//! The operations that mxv and other hosts call.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::engine::container::DockerCli;
use crate::engine::containers::Unbuilt;
use crate::engine::keeper::KeeperCommand;
use crate::engine::run::{Engine, Settings};
use crate::env::EnvironmentSource;
use crate::error::{Operation, PhaseError};
use crate::logs::{LineSink, LogRequest};
use crate::name::{AppName, WorkspaceId};
use crate::plan::Plan;
use crate::record::Swept;
use crate::record::process::System;
use crate::record::store::RunStore;
use crate::record::sweep::{self, Reach, Waits};
use crate::report::Report;
use crate::select::Selection;
use crate::status::Status;

#[cfg(test)]
mod tests;

/// How long an operation waits for the workspace's lifecycle lock, which
/// another `up`, `down` or `restart` holds, before it fails, unless the
/// builder states another wait.
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(5);

/// How long `up` watches a process without a readiness probe before it
/// counts the process as started, unless the builder states another wait. A
/// process that exits within it fails to start.
pub const DEFAULT_START_GRACE: Duration = Duration::from_millis(500);

/// How long phase waits after `KILL` before it reports a process as
/// unstoppable, unless the builder states another wait.
pub const DEFAULT_KILL_WAIT: Duration = Duration::from_secs(5);

/// Runs workspaces' apps. It keeps each workspace's run record, logs and
/// lock under its state directory, keyed by the workspace's stable id, and
/// keeps no global state.
#[derive(Clone, Debug)]
pub struct Phase {
    /// The directory that holds phase's state, such as the run record of
    /// each workspace at `runs/<workspace-id>.json`.
    state_dir: PathBuf,
    /// How long an operation waits for the lifecycle lock.
    lock_wait: Duration,
    /// How long `up` watches a process without a probe.
    start_grace: Duration,
    /// How long phase waits after `KILL`.
    kill_wait: Duration,
    /// The program that runs keepers, or `None` until the builder names
    /// one.
    keeper: Option<KeeperCommand>,
    /// The variables that keepers and their apps inherit from the program
    /// that links phase: only ever `PATH` and `HOME`.
    inherited: BTreeMap<String, String>,
}

/// The deadlines of one `up`. A deadline that cuts off a start makes that
/// app's outcome uncertain, and phase does not retry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpDeadlines {
    /// When building every selected checkout must have finished.
    pub build: Instant,
    /// When every selected process must be ready.
    pub ready: Instant,
}

impl UpDeadlines {
    /// Returns the deadlines that end building at `build` and readiness at
    /// `ready`.
    #[must_use]
    pub const fn new(build: Instant, ready: Instant) -> Self {
        Self { build, ready }
    }
}

impl Phase {
    /// Returns a `Phase` that keeps its state under `state_dir`, such as
    /// `~/.local/state/mxv/phase`, with [`DEFAULT_LOCK_WAIT`],
    /// [`DEFAULT_START_GRACE`] and [`DEFAULT_KILL_WAIT`].
    #[must_use]
    pub const fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            lock_wait: DEFAULT_LOCK_WAIT,
            start_grace: DEFAULT_START_GRACE,
            kill_wait: DEFAULT_KILL_WAIT,
            keeper: None,
            inherited: BTreeMap::new(),
        }
    }

    /// Returns this `Phase` with `program` as the program that runs
    /// keepers, followed by `leading`, the words of its hidden subcommand,
    /// such as `mxv` and `phase keep`. `up` cannot start a host process
    /// until the builder names it.
    #[must_use]
    pub fn with_keeper(mut self, program: PathBuf, leading: Vec<OsString>) -> Self {
        self.keeper = Some(KeeperCommand::new(program, leading));
        self
    }

    /// Returns this `Phase` with `inherited` as the variables that keepers
    /// and their apps inherit, which the program that links phase reads in
    /// `main`. phase reads no environment itself. Only `PATH` and `HOME`
    /// may be inherited, and `up` fails a host process's start when
    /// `inherited` names another variable.
    #[must_use]
    pub fn with_inherited(mut self, inherited: BTreeMap<String, String>) -> Self {
        self.inherited = inherited;
        self
    }

    /// Returns this `Phase` with `wait` as how long an operation waits for
    /// the lifecycle lock.
    #[must_use]
    pub const fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// Returns this `Phase` with `grace` as how long `up` watches a process
    /// without a readiness probe.
    #[must_use]
    pub const fn with_start_grace(mut self, grace: Duration) -> Self {
        self.start_grace = grace;
        self
    }

    /// Returns this `Phase` with `wait` as how long phase waits after
    /// `KILL`.
    #[must_use]
    pub const fn with_kill_wait(mut self, wait: Duration) -> Self {
        self.kill_wait = wait;
        self
    }

    /// Returns the directory that holds phase's state.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Returns the path of the run record of the workspace `workspace`,
    /// `<state_dir>/runs/<workspace>.json`, which the run store computes.
    /// A host labels a workspace's Docker objects with it.
    #[must_use]
    pub fn run_record(&self, workspace: &WorkspaceId) -> PathBuf {
        RunStore::new(&self.state_dir).path(workspace)
    }

    /// Returns how long an operation waits for the lifecycle lock.
    #[must_use]
    pub const fn lock_wait(&self) -> Duration {
        self.lock_wait
    }

    /// Returns how long `up` watches a process without a readiness probe.
    #[must_use]
    pub const fn start_grace(&self) -> Duration {
        self.start_grace
    }

    /// Returns how long phase waits after `KILL`.
    #[must_use]
    pub const fn kill_wait(&self) -> Duration {
        self.kill_wait
    }

    /// Starts the selected apps of `plan`, and the apps they depend on, in
    /// dependency order, resolving each process's environment through `env`
    /// just before it starts. When `cancel` is set, `up` stops what it
    /// started and returns.
    ///
    /// `up` takes the workspace's lifecycle lock, waiting up to the lock
    /// wait, and writes every process and container it will start into the
    /// run record before it starts any. For each host process it checks
    /// that nothing holds the process's port, starts the process's keeper,
    /// and waits until the readiness probe passes, before
    /// `deadlines.ready` and the process's ready timeout. A process
    /// without a probe is watched for the start grace instead. A process
    /// that ends meanwhile fails to start. A process that the run record
    /// names as running is left alone.
    ///
    /// When a process fails to start, `up` stops what it started of that
    /// app, starts no further app, and reports which apps started. An app
    /// whose program comes from a release or a build, or that runs in
    /// containers, is reported as errored with
    /// [`PhaseError::NotRunnable`]'s code, since a later release does that
    /// work, and the apps that depend on it do not start.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::UnknownTarget`] when `select` names what the
    /// plan lacks, [`PhaseError::Busy`] when another operation holds the
    /// lock, the errors of reading and writing the run record, and
    /// [`PhaseError::Environment`] when a value cannot be resolved. `up`
    /// resolves each process's environment just before it starts the
    /// process, so it may already have started earlier apps, and it stops
    /// everything it started before it returns any error.
    pub fn up(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadlines: UpDeadlines,
        cancel: &Cancel,
    ) -> Result<Report, PhaseError> {
        self.engine().up(plan, select, env, deadlines, cancel)
    }

    /// Resolves the environment of the selected processes of `plan` that
    /// run again through `env`, then stops each one and starts it with its
    /// new environment, before `deadline`. A selected app with no running
    /// process is reported as not running, and `restart` starts nothing it
    /// depends on. Its stops give up at `deadline`, and a stop that
    /// `deadline` cuts off reports the app's stop as uncertain. When
    /// `cancel` is set, `restart` stops what it started of the app it was
    /// restarting, leaves the apps it already restarted running, and
    /// reports the apps it had not reached as untouched.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Phase::up`]. When a value cannot be
    /// resolved, `restart` has stopped nothing.
    pub fn restart(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Report, PhaseError> {
        self.engine().restart(plan, select, env, deadline, cancel)
    }

    /// Stops the selected apps of the workspace whose stable id is
    /// `workspace`, in reverse dependency order, before `deadline`. The run
    /// record holds the order, so `down` needs no plan.
    ///
    /// `down` never signals a process that phase cannot prove is its own.
    /// Such a process, and one that survives `KILL`, keeps its entry in the
    /// run record, and the report notes it. The run record is removed once
    /// everything it names is stopped.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Busy`] when another operation holds the lock,
    /// the errors of reading and writing the run record, and
    /// [`PhaseError::ProcessTable`] when phase cannot read or signal
    /// processes.
    pub fn down(
        &self,
        workspace: &WorkspaceId,
        select: &Selection,
        deadline: Instant,
    ) -> Result<Report, PhaseError> {
        self.engine().down(workspace, select, deadline)
    }

    /// Returns the engine that carries out `up`, `down` and `restart` on
    /// this machine.
    fn engine(&self) -> Engine<'_> {
        Engine {
            settings: Settings {
                state_dir: &self.state_dir,
                lock_wait: self.lock_wait,
                start_grace: self.start_grace,
                kill_wait: self.kill_wait,
                keeper: self.keeper.as_ref(),
                inherited: &self.inherited,
            },
            host: &System,
            containers: &Unbuilt,
        }
    }

    /// Returns the status of the workspace whose stable id is `workspace`.
    /// With the workspace's `plan`, the rows follow the plan and carry the
    /// sources, runners and addresses that only the plan gives. Without
    /// one, as when no plan can be built, the rows are the record's live
    /// entries. Either way `status` checks each live process with the
    /// process table and each live container with Docker, and reports an
    /// entry that the plan lacks. It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::RecordUnreadable`] or
    /// [`PhaseError::NewerRecord`] when the record cannot be used, and
    /// [`PhaseError::ProcessTable`] when phase cannot read processes.
    pub fn status(
        &self,
        workspace: &WorkspaceId,
        plan: Option<&Plan>,
    ) -> Result<Status, PhaseError> {
        let sources = crate::status::Sources {
            state_dir: &self.state_dir,
            processes: &System,
            docker: &DockerCli::new(),
        };
        crate::status::status(&RunStore::new(&self.state_dir), workspace, plan, sources)
    }

    /// Passes the lines of the requested log of the workspace whose stable
    /// id is `workspace` to `sink`, from the log's start, and with
    /// `request.follow` each later line until the sink breaks or nothing
    /// can write to the log any more. It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::UnknownTarget`] when phase knows no log of the
    /// process, [`PhaseError::LogUnreadable`] when the log cannot be read,
    /// and [`PhaseError::NotBuilt`] for a container's log and for
    /// `request.since`, which phase does not carry out yet.
    pub fn logs(
        &self,
        workspace: &WorkspaceId,
        request: &LogRequest,
        sink: &mut dyn LineSink,
    ) -> Result<(), PhaseError> {
        crate::logs::logs(
            &RunStore::new(&self.state_dir),
            &self.state_dir,
            workspace,
            request,
            sink,
            &System,
        )
    }

    /// Stops what the run record of the workspace whose stable id is
    /// `workspace` names and phase no longer supervises, and removes the
    /// record once everything it names is stopped. A workspace with no
    /// record has nothing to sweep.
    ///
    /// `sweep` takes the workspace's lifecycle lock, waiting up to the lock
    /// wait. It stops a host process only when the process's pid and start
    /// time both match the record, and reports
    /// [`SweepOutcome::PidReused`] when another process has the pid. When
    /// phase may not read the process that has the pid, as inside a
    /// sandbox, it reports [`SweepOutcome::Unknown`] and leaves the process
    /// and its record entry alone. It sends `TERM` to the process's group
    /// when the process leads its group, and to the process alone
    /// otherwise, waits up to the kill wait, then sends `KILL` and waits
    /// again. It removes the record's containers, networks and run volumes,
    /// and leaves volumes that hold the workspace's data.
    ///
    /// With `dry_run`, `sweep` reads the record without the lock, reports
    /// what it would stop, and changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Busy`] when another operation holds the lock
    /// for all of the lock wait, [`PhaseError::RecordUnreadable`] or
    /// [`PhaseError::NewerRecord`] when the record cannot be used,
    /// [`PhaseError::Docker`] when the record names a Docker object and
    /// Docker does not answer or refuses, [`PhaseError::ProcessTable`] when
    /// phase cannot read or signal processes, and [`PhaseError::StateUnwritable`] when the
    /// record cannot be written. A failed sweep keeps the record, with each
    /// object's state as far as the sweep got.
    ///
    /// [`SweepOutcome::PidReused`]: crate::SweepOutcome::PidReused
    /// [`SweepOutcome::Unknown`]: crate::SweepOutcome::Unknown
    pub fn sweep(&self, workspace: &WorkspaceId, dry_run: bool) -> Result<Swept, PhaseError> {
        let store = RunStore::new(&self.state_dir);
        let waits = Waits {
            lock: self.lock_wait,
            kill: self.kill_wait,
        };
        let reach = Reach {
            docker: &DockerCli::new(),
            processes: &System,
            host: &System,
        };
        sweep::sweep(&store, workspace, dry_run, reach, waits)
    }

    /// Runs `run`, as separate values that no shell reads, with the
    /// environment and sandbox of `app` in `plan`, and returns its exit
    /// status. It first waits for an `up` in progress to finish, until
    /// `deadline`. When `cancel` is set, `exec` stops the command.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Environment`] when a value cannot be resolved,
    /// and [`PhaseError::NotBuilt`] until phase carries the operation out.
    #[expect(unused_variables, reason = "phase does not carry out `exec` yet")]
    pub fn exec(
        &self,
        plan: &Plan,
        app: &AppName,
        run: &[String],
        env: &dyn EnvironmentSource,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<ExitStatus, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Exec,
        })
    }
}
