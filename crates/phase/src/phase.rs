//! The operations that mxv and other hosts call.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::cancel::Cancel;
use crate::env::EnvironmentSource;
use crate::error::{Operation, PhaseError};
use crate::logs::{LineSink, LogRequest};
use crate::name::{AppName, WorkspaceId};
use crate::plan::Plan;
use crate::record::Swept;
use crate::record::docker::Unreachable;
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
        }
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
    /// dependency order, resolving each app's environment through `env`.
    /// When `cancel` is set, `up` stops what it started and returns.
    ///
    /// An app whose permissions grew beyond its grant does not start, and
    /// the report gives it [`AppOutcome::AwaitingGrant`]. The other apps
    /// start.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Environment`] when a value cannot be resolved,
    /// and [`PhaseError::NotBuilt`] until phase carries the operation out.
    ///
    /// [`AppOutcome::AwaitingGrant`]: crate::AppOutcome::AwaitingGrant
    #[expect(unused_variables, reason = "phase does not carry out `up` yet")]
    pub fn up(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadlines: UpDeadlines,
        cancel: &Cancel,
    ) -> Result<Report, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Up,
        })
    }

    /// Resolves the environment of the selected apps of `plan` again
    /// through `env`, then restarts their processes, before `deadline`.
    /// When `cancel` is set, `restart` stops what it started and returns.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Environment`] when a value cannot be resolved,
    /// and [`PhaseError::NotBuilt`] until phase carries the operation out.
    #[expect(unused_variables, reason = "phase does not carry out `restart` yet")]
    pub fn restart(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadline: Instant,
        cancel: &Cancel,
    ) -> Result<Report, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Restart,
        })
    }

    /// Stops the selected apps of the workspace whose stable id is
    /// `workspace`, in reverse dependency order, before `deadline`. The run
    /// record holds the order.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `down` yet")]
    pub fn down(
        &self,
        workspace: &WorkspaceId,
        select: &Selection,
        deadline: Instant,
    ) -> Result<Report, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Down,
        })
    }

    /// Returns the status of the workspace whose stable id is `workspace`.
    /// It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `status` yet")]
    pub fn status(&self, workspace: &WorkspaceId) -> Result<Status, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Status,
        })
    }

    /// Passes the lines of the requested log of the workspace whose stable
    /// id is `workspace` to `sink`. It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `logs` yet")]
    pub fn logs(
        &self,
        workspace: &WorkspaceId,
        request: &LogRequest,
        sink: &mut dyn LineSink,
    ) -> Result<(), PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Logs,
        })
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
        // The container module supplies the Docker that runs `docker` when
        // it lands. Until then a record that names a Docker object makes
        // `sweep` fail with `PhaseError::Docker`.
        let store = RunStore::new(&self.state_dir);
        let waits = Waits {
            lock: self.lock_wait,
            kill: self.kill_wait,
        };
        let reach = Reach {
            docker: &Unreachable,
            processes: &System,
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
