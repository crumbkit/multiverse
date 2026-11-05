//! The operations that mxv and other hosts call.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::engine::container::{ComposeRunner, DockerCli};
use crate::engine::exec::{ExecRequest, ExecSettings};
use crate::engine::keeper::KeeperCommand;
use crate::engine::run::{Engine, Settings};
use crate::env::EnvironmentSource;
use crate::error::PhaseError;
use crate::logs::{LineSink, LogRequest};
use crate::name::WorkspaceId;
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
    /// The only variables that the `docker exec` which writes a container's
    /// secret receives, or `None` for the host program's whole
    /// environment.
    docker_environment: Option<BTreeMap<String, String>>,
    /// The paths that sandboxed apps may never read or write, besides those
    /// that phase always protects.
    protected: Vec<PathBuf>,
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
            docker_environment: None,
            protected: Vec::new(),
        }
    }

    /// Returns this `Phase` with `protected` as paths that a sandboxed app
    /// may never read or write, such as the host program's own state and
    /// data directories. phase always protects its own state directory,
    /// `~/.ssh`, the Keychains, the directory of the agent's socket and
    /// the workspace directory outside the app's checkout, and refuses to
    /// start a sandboxed app whose allowed paths would reach one.
    #[must_use]
    pub fn with_protected(mut self, protected: Vec<PathBuf>) -> Self {
        self.protected = protected;
        self
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

    /// Returns this `Phase` with `vars`, which the program that links phase
    /// reads in `main`, as the only environment of the `docker exec` that
    /// writes a container's secret file. phase keeps `PATH`, `HOME` and the
    /// variables whose names start with `DOCKER_`, which choose the docker
    /// program, its configuration and its daemon, and drops the rest.
    /// Without it, that `docker exec` inherits the host program's
    /// environment, as phase's other `docker` commands do. The secret's
    /// value reaches neither environment, because phase writes it to the
    /// command's standard input.
    #[must_use]
    pub fn with_docker_environment(mut self, vars: BTreeMap<String, String>) -> Self {
        self.docker_environment = Some(
            vars.into_iter()
                .filter(|(name, _)| name == "PATH" || name == "HOME" || name.starts_with("DOCKER_"))
                .collect(),
        );
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
    /// Each container app is one Docker Compose project, and `up` starts
    /// its processes one at a time through `docker compose up`, waits for
    /// each to be ready, and records each container by its id. A process
    /// receives its variables that are not secrets through private
    /// references on its own `docker compose up` child, and no value
    /// reaches the compose file, an argument or a label. After its
    /// container starts, `up` writes each of its secrets to a file in the
    /// container's `/run/secrets` tmpfs through `docker exec`, with the
    /// value on standard input, before it waits for the process to be
    /// ready. It refuses a process whose secret files would belong to root
    /// with [`PhaseError::SecretOwnerUnknown`]. A process whose plan sets
    /// [`SecretDelivery::Env`](crate::SecretDelivery::Env) receives its
    /// secrets through private references instead.
    ///
    /// When a process fails to start, `up` stops what it started of that
    /// app, starts no further app, and reports which apps started. An app
    /// whose program comes from a release or a build, and a container app
    /// whose process runs once or names a working directory, is reported
    /// as errored with [`PhaseError::NotRunnable`]'s code, since a later
    /// release does that work, and the apps that depend on it do not start.
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
        self.engine(&self.containers())
            .up(plan, select, env, deadlines, cancel)
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
        self.engine(&self.containers())
            .restart(plan, select, env, deadline, cancel)
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
        self.engine(&self.containers())
            .down(workspace, select, deadline)
    }

    /// Returns the runner of container apps on this machine.
    fn containers(&self) -> ComposeRunner {
        ComposeRunner::new(self.state_dir.clone(), self.start_grace)
            .with_docker_environment(self.docker_environment.clone())
    }

    /// Returns the engine that carries out `up`, `down` and `restart` on
    /// this machine, with `containers` as its runner of container apps.
    fn engine<'a>(&'a self, containers: &'a ComposeRunner) -> Engine<'a> {
        Engine {
            settings: Settings {
                state_dir: &self.state_dir,
                lock_wait: self.lock_wait,
                start_grace: self.start_grace,
                kill_wait: self.kill_wait,
                keeper: self.keeper.as_ref(),
                inherited: &self.inherited,
                protected: &self.protected,
            },
            host: &System,
            containers,
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
    /// id is `workspace` to `sink`, from the log's start or only its last
    /// `request.lines` lines, and with `request.follow` each later line
    /// until the sink breaks or nothing can write to the log any more. A
    /// host process's last lines come from at most the log's last
    /// [`crate::logs::MOST_TAIL_BYTES`] bytes, so a request for more lines
    /// than those bytes hold passes fewer. A container's log comes from
    /// `docker logs`, after phase inspects the container with its app's
    /// labels. A container that has not started, that phase stopped, that
    /// docker no longer has, or that the record names by a name or id phase
    /// cannot use passes no lines, even with `request.follow`, and gives
    /// the sink the [`crate::LogNote`] that says which. It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::UnknownTarget`] when phase knows no log of the
    /// process, [`PhaseError::LogUnreadable`] when the log cannot be read,
    /// [`PhaseError::Docker`] when `docker logs` fails, and
    /// [`PhaseError::NotBuilt`] for `request.since`, which phase does not
    /// carry out yet.
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

    /// Runs `request.run`, as separate values that no shell reads, in the
    /// context of `request.process` of `request.app` in `plan`, such as a
    /// delivery script or a test run, and returns its exit status: its
    /// code, or the signal that ended it.
    ///
    /// For a host process, the command runs in the process's directory
    /// with the inherited `PATH` and `HOME` and the process's environment,
    /// which `exec` resolves through `env` before `deadline` and checks
    /// for the process's required variables as `up` does. An optional
    /// variable without a value is unset. When the app is sandboxed, the
    /// command runs under the process's own sandbox, through the launcher
    /// that the process's keeper uses, with the sandbox's `TMPDIR`, so it
    /// may read, write, listen and connect only where the process may. An
    /// app that is not sandboxed runs its command unsandboxed. The process
    /// need not be running.
    ///
    /// For a container process, the command runs in the process's
    /// container through `docker exec --interactive`, with `--tty` when
    /// `request.io` says the caller's standard input is a terminal. The
    /// container must be running, and its labels must name the app's
    /// workspace and app. The command receives the variables and secret
    /// files that the container already holds, and no value reaches the
    /// command line. `exec` never starts a container.
    ///
    /// `exec` writes nothing to the run record, and starts and stops no
    /// app. It first waits for an `up`, `down`, `restart` or `sweep` in
    /// progress to finish, until `deadline`, and then releases the
    /// lifecycle lock before the command starts, so it never blocks those
    /// operations while the command runs. With [`ExecIo::Inherit`](crate::ExecIo::Inherit) the
    /// command shares the caller's streams and process group, so Ctrl-C at
    /// the terminal reaches it directly. When `cancel` is set, `exec` stops
    /// the command with `TERM`, and with `KILL` after the kill wait.
    ///
    /// A sandboxed command whose sandbox the launcher cannot apply exits
    /// with [`SANDBOX_REFUSED`](crate::keeper::SANDBOX_REFUSED) after it
    /// writes why to standard error.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::UnknownTarget`] when the plan lacks the app
    /// or the process, [`PhaseError::Busy`] when an operation holds the
    /// lock until `deadline`, [`PhaseError::NotRunnable`] for a host app
    /// whose program comes from a release or a build,
    /// [`PhaseError::Environment`] when a value cannot be resolved or a
    /// required variable is missing, [`PhaseError::NotRunning`] when the
    /// process's container is not running, [`PhaseError::Docker`] when
    /// Docker fails or the container belongs to another workspace or app,
    /// and [`PhaseError::ExecUnstartable`] when the sandbox cannot be built
    /// or the command cannot start.
    pub fn exec(
        &self,
        plan: &Plan,
        request: ExecRequest,
        env: &dyn EnvironmentSource,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<ExitStatus, PhaseError> {
        let containers = self.containers();
        let engine = self.engine(&containers);
        crate::engine::exec::exec(
            &ExecSettings {
                settings: engine.settings,
                docker_environment: self.docker_environment.as_ref(),
            },
            plan,
            request,
            env,
            deadline,
            cancel,
        )
    }
}
