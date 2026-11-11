//! `exec` of a workspace's app, which runs a one-off command in the context
//! of a running unit through phase's `exec`.
//!
//! [`Runs::exec_target`] finds the unit and builds the workspace's plan
//! the way `up` builds it, so the command receives the same variables,
//! directory and sandbox as the unit's process. The caller then reads
//! [`Exec::sandboxed`] to choose how it handles Ctrl-C, and runs the
//! command with [`Exec::run`].

use std::fmt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use phase::{
    AppName, Cancel, ExecIo, ExecRequest, PhaseError, ProcessName, StatusRow, WorkspaceId,
};

use super::{app, main_process, state_of};
use crate::error::Error;
use crate::manifest::Manifest;
use crate::phase_plan::{self, Built, Unrunnable};
use crate::record::Workspace;
use crate::runs::{ProcessState, Runs};

#[cfg(test)]
mod tests;

/// How long `exec` and a build wait for an operation in progress to release
/// phase's lifecycle lock, and for the unit's variables to resolve. When it
/// passes, the command does not start and `exec` fails with phase's
/// `phase::lock::busy` or `phase::environment`. It does not cover the
/// running command: `exec` runs without a deadline, and a build's deadline
/// is its own.
pub(super) const EXEC_WAIT: Duration = Duration::from_secs(600);

/// The outcome of [`Runs::exec_target`].
#[derive(Debug)]
pub enum ExecTarget<'a> {
    /// The unit runs, and [`Exec::run`] runs a command in its context.
    Ready(Exec<'a>),
    /// Neither the manifest nor a run record knows a unit with the name.
    UnknownProcess,
    /// The unit is known and does not run, so `exec` runs nothing.
    NotRunning,
    /// phase cannot run the manifest.
    Unrunnable(Unrunnable),
    /// The unit is a host process whose permissions the grant of the
    /// workspace's repository does not cover, so `exec` runs nothing and
    /// asks no question.
    NotGranted(super::PermissionAsk),
}

/// A command's context: the running unit of a workspace and the plan that
/// `up` would build for it.
pub struct Exec<'a> {
    /// The run files and the phase that run the command.
    runs: &'a Runs,
    /// The workspace's stable id.
    id: WorkspaceId,
    /// The workspace's plan and its variables.
    built: Built,
    /// The unit's app.
    app: AppName,
    /// The unit's one process.
    process: ProcessName,
    /// Whether the command runs under the unit's sandbox.
    sandboxed: bool,
}

impl fmt::Debug for Exec<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Exec")
            .field("id", &self.id)
            .field("app", &self.app)
            .field("process", &self.process)
            .field("sandboxed", &self.sandboxed)
            .finish_non_exhaustive()
    }
}

/// How a command that [`Exec::run`] ran, or tried to run, ended.
#[derive(Debug)]
pub enum ExecEnd {
    /// The command ran and ended with this status: its exit code, or the
    /// signal that ended it.
    Exited(ExitStatus),
    /// The unit's container stopped or went before the command started,
    /// or while `docker exec` failed, so the command did not run.
    NotRunning,
    /// The command could not start, such as when its program does not
    /// exist in a host process's context or its sandbox cannot be built.
    Unstartable {
        /// Why, in phase's words, which hold no variable's value.
        reason: String,
    },
    /// The unit runs under the sandbox, and the command's standard input,
    /// output or error is a terminal. phase does not pass a terminal to a
    /// sandboxed command, so the command did not run.
    NeedsTerminal,
}

/// Reports whether the unit of phase's row `row` runs, so a command can
/// run in its context: it is starting, running or ready.
fn running(row: &StatusRow) -> bool {
    matches!(
        state_of(row),
        ProcessState::Running { .. } | ProcessState::Starting { .. }
    )
}

impl Runs {
    /// Returns the context of the running unit `unit` of `workspace`,
    /// whose manifest is `manifest`, in which [`Exec::run`] runs a
    /// command, or the outcome that says why no command can run there.
    ///
    /// The plan is the one that `up` builds from the same manifest and
    /// ports, so a host command receives the same variables, directory and
    /// sandbox as the unit's process. The variables that phase passes
    /// through, such as `PATH`, come from the environment of the caller of
    /// `exec`, which can differ from the one that ran `up`. A unit name
    /// of a workspace with several checkouts carries its checkout's
    /// prefix, as `up` and `logs` take it.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Runs::status`], and [`Error::Phase`] when
    /// phase cannot tell whether the unit runs under the sandbox.
    pub fn exec_target<'a>(
        &'a self,
        workspace: &Workspace,
        manifest: &Manifest,
        unit: &str,
    ) -> Result<ExecTarget<'a>, Error> {
        let declared = manifest.unit(unit).is_some();
        // A workspace without a stable id has never run under phase.
        let Some(id) = workspace.id.clone() else {
            return Ok(if declared {
                ExecTarget::NotRunning
            } else {
                ExecTarget::UnknownProcess
            });
        };
        // `exec` builds the plan once, and phase's status and the command
        // both use it.
        let built = phase_plan::build(&self.inputs(workspace, &id, manifest));
        let row = self
            .phase
            .status(&id, built.as_ref().ok().map(|built| &built.plan))
            .map_err(|error| self.phase_error(&id, error))?
            .rows
            .into_iter()
            .find(|row| row.app.as_str() == unit);
        if !declared && row.is_none() {
            return Ok(ExecTarget::UnknownProcess);
        }
        let (Some(app), Some(process), Some(true)) =
            (app(unit), main_process(), row.as_ref().map(running))
        else {
            return Ok(ExecTarget::NotRunning);
        };
        let built = match built {
            Ok(built) => built,
            Err(unrunnable) => return Ok(ExecTarget::Unrunnable(unrunnable)),
        };
        // A command runs with its unit's permissions, so a unit whose
        // permissions grew since a person granted them runs no command.
        if manifest.processes.contains_key(unit) {
            match self.ask_of(workspace, &id, manifest, &[unit.to_owned()])? {
                Ok(None) => {}
                Ok(Some(ask)) => return Ok(ExecTarget::NotGranted(ask)),
                Err(unrunnable) => return Ok(ExecTarget::Unrunnable(unrunnable)),
            }
        }
        // A unit that only phase's run record names, and the manifest no
        // longer declares, is in no plan, so no command can run in its
        // context, and `up` would not start it either.
        let sandboxed = match self.phase.exec_sandboxed(&built.plan, &app, &process) {
            Ok(sandboxed) => sandboxed,
            Err(PhaseError::UnknownTarget { .. }) => return Ok(ExecTarget::UnknownProcess),
            Err(error) => return Err(self.phase_error(&id, error)),
        };
        Ok(ExecTarget::Ready(Exec {
            runs: self,
            id,
            built,
            app,
            process,
            sandboxed,
        }))
    }
}

impl Exec<'_> {
    /// Reports whether the command runs under the unit's sandbox, in a
    /// session of its own. The terminal's Ctrl-C then reaches only the
    /// caller, which sets the cancel flag that it passes to
    /// [`Exec::run`]. Otherwise the command shares the caller's process
    /// group and receives Ctrl-C itself, and the caller catches `SIGINT`
    /// without setting the flag.
    #[must_use]
    pub const fn sandboxed(&self) -> bool {
        self.sandboxed
    }

    /// Runs `command`, as separate values that no shell reads, in the
    /// unit's context with the caller's standard streams, and returns how
    /// it ended. `terminal` says whether the caller gives the command a
    /// terminal, which mxv does when its standard input and output are
    /// both terminals: a container's command then gets a terminal of its own
    /// through `docker exec --tty`, and a sandboxed host command is
    /// refused with [`ExecEnd::NeedsTerminal`]. When `cancel` is set,
    /// phase stops the command with `TERM`, and with `KILL` after its kill
    /// wait.
    ///
    /// A host command runs outside phase's record, so `mxv down` does not
    /// stop it, and it outlives the unit's process when it runs longer. A
    /// sandboxed command whose sandbox the launcher cannot apply exits with
    /// 78 after it writes why to standard error.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Busy`] when an `up`, `down` or `restart` of the
    /// workspace holds phase's lock for longer than `exec` waits, the
    /// docker errors, and [`Error::Phase`] for another failure of phase,
    /// such as a variable that cannot be resolved.
    pub fn run(
        self,
        command: Vec<String>,
        terminal: bool,
        cancel: &Cancel,
    ) -> Result<ExecEnd, Error> {
        let request = ExecRequest::new(
            self.app,
            self.process,
            command,
            ExecIo::Inherit { terminal },
        );
        let deadline = Instant::now() + EXEC_WAIT;
        match self.runs.phase.exec(
            &self.built.plan,
            request,
            &self.built.environment,
            deadline,
            cancel,
        ) {
            Ok(status) => Ok(ExecEnd::Exited(status)),
            Err(PhaseError::NotRunning { .. }) => Ok(ExecEnd::NotRunning),
            Err(PhaseError::ExecUnstartable { reason, .. }) => Ok(ExecEnd::Unstartable { reason }),
            Err(PhaseError::ExecNeedsTerminal { .. }) => Ok(ExecEnd::NeedsTerminal),
            Err(error) => Err(self.runs.phase_error(&self.id, error)),
        }
    }
}
