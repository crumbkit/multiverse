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

/// How long `exec` waits for an `up`, `down` or `restart` of the workspace
/// to release phase's lifecycle lock, and for the unit's variables to
/// resolve. When it passes, the command does not start and `exec` fails
/// with phase's `phase::lock::busy` or `phase::environment`. The command
/// itself runs without a deadline.
const EXEC_WAIT: Duration = Duration::from_secs(600);

/// The outcome of [`Runs::exec_target`].
#[derive(Debug)]
pub enum ExecTarget<'a> {
    /// The unit runs, and [`Exec::run`] runs a command in its context.
    Ready(Exec<'a>),
    /// Neither the manifest nor a run record knows a unit with the name.
    UnknownProcess,
    /// The unit is known and does not run, so `exec` runs nothing.
    NotRunning,
    /// Processes or containers that mxv started before phase ran the
    /// workspace's apps may still run, so `exec` runs nothing until
    /// `mxv down` stops them and `mxv up` starts the workspace under phase.
    LegacyRunning {
        /// The processes and containers that may still run.
        processes: Vec<String>,
    },
    /// phase cannot run the manifest.
    Unrunnable(Unrunnable),
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
#[non_exhaustive]
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
    /// The plan is the one that `up` builds from the same manifest, ports
    /// and caller's environment, so a host command receives the same
    /// variables, directory and sandbox as the unit's process. A unit name
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
        if self.has_legacy(&workspace.name)? {
            let processes = self.legacy_running(workspace)?;
            if !processes.is_empty() {
                return Ok(ExecTarget::LegacyRunning { processes });
            }
        }
        let rows = self.phase_rows(workspace, Some(manifest))?;
        let known = manifest.unit(unit).is_some() || rows.contains_key(unit);
        if !known {
            return Ok(ExecTarget::UnknownProcess);
        }
        let (Some(id), Some(app), Some(process), Some(true)) = (
            workspace.id.clone(),
            app(unit),
            main_process(),
            rows.get(unit).map(running),
        ) else {
            return Ok(ExecTarget::NotRunning);
        };
        let built = match phase_plan::build(&self.inputs(workspace, &id, manifest)) {
            Ok(built) => built,
            Err(unrunnable) => return Ok(ExecTarget::Unrunnable(unrunnable)),
        };
        // A unit that only phase's run record names, and the manifest no
        // longer declares, is in no plan, so no command can run in its
        // context.
        let sandboxed = match self.phase.exec_sandboxed(&built.plan, &app, &process) {
            Ok(sandboxed) => sandboxed,
            Err(PhaseError::UnknownTarget { .. }) => return Ok(ExecTarget::NotRunning),
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
    /// it ended. `terminal` says whether the caller's standard input is a
    /// terminal: a container's command then gets a terminal of its own
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
