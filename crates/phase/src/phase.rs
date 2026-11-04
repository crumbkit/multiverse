//! The operations that mxv and other hosts call.

use std::path::Path;
use std::process::ExitStatus;
use std::time::Instant;

use crate::env::EnvironmentSource;
use crate::error::{Operation, PhaseError};
use crate::logs::{LineSink, LogRequest};
use crate::name::AppName;
use crate::plan::Plan;
use crate::record::Swept;
use crate::report::Report;
use crate::select::Selection;
use crate::status::Status;

#[cfg(test)]
mod tests;

/// Runs a workspace's apps. Each operation takes the workspace's directory
/// or plan, and phase keeps no global state.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Phase {}

/// The deadlines of one `up`. A deadline that cuts off a start makes that
/// app's outcome uncertain, and phase does not retry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpDeadlines {
    /// When building every selected checkout must have finished.
    pub build: Instant,
    /// When every selected process must be ready.
    pub ready: Instant,
}

impl Phase {
    /// Returns a `Phase`.
    #[must_use]
    pub const fn new() -> Self {
        Self {}
    }

    /// Starts the selected apps of `plan`, and the apps they depend on, in
    /// dependency order, resolving each app's environment through `env`.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::PermissionsGrew`] when a selected app asks for
    /// permissions beyond its grant, [`PhaseError::Environment`] when a
    /// value cannot be resolved, and [`PhaseError::NotBuilt`] until phase
    /// carries the operation out.
    #[expect(unused_variables, reason = "phase does not carry out `up` yet")]
    pub fn up(
        &self,
        plan: &Plan,
        select: &Selection,
        env: &dyn EnvironmentSource,
        deadlines: UpDeadlines,
    ) -> Result<Report, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Up,
        })
    }

    /// Stops the selected apps of the workspace in `workspace`, in reverse
    /// dependency order, before `deadline`.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `down` yet")]
    pub fn down(
        &self,
        workspace: &Path,
        select: &Selection,
        deadline: Instant,
    ) -> Result<Report, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Down,
        })
    }

    /// Returns the status of the workspace in `workspace`. It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `status` yet")]
    pub fn status(&self, workspace: &Path) -> Result<Status, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Status,
        })
    }

    /// Passes the lines of the requested log to `sink`. It takes no lock.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `logs` yet")]
    pub fn logs(
        &self,
        workspace: &Path,
        request: &LogRequest,
        sink: &mut dyn LineSink,
    ) -> Result<(), PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Logs,
        })
    }

    /// Stops what the run record of the workspace in `workspace` names and
    /// phase no longer supervises. With `dry_run`, it reports what it would
    /// stop and stops nothing.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::NotBuilt`] until phase carries the operation
    /// out.
    #[expect(unused_variables, reason = "phase does not carry out `sweep` yet")]
    pub fn sweep(&self, workspace: &Path, dry_run: bool) -> Result<Swept, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Sweep,
        })
    }

    /// Runs `run`, as separate values that no shell reads, with the
    /// environment and sandbox of `app` in `plan`, and returns its exit
    /// status. It first waits for an `up` in progress to finish, until
    /// `deadline`.
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
    ) -> Result<ExitStatus, PhaseError> {
        Err(PhaseError::NotBuilt {
            operation: Operation::Exec,
        })
    }
}
