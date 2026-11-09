//! The `docker exec` that runs a one-off command in the running container
//! of a process for [`Phase::exec`](crate::Phase::exec). The container
//! already holds the process's variables and secret files, so the command
//! receives them from Docker, and phase passes no value of the process's
//! variables on the command line or in the `docker` program's environment.
//! The only values on the command line are the caller's
//! [`ExecRequest::variables`](crate::ExecRequest::variables), which hold no
//! secret.

use std::collections::BTreeMap;
use std::process::Command;

use crate::engine::container::command::DOCKER;
use crate::engine::container::names::{self, Identity, Scope};
use crate::engine::container::objects::{ContainerState, inspect_id};
use crate::engine::container::runner::docker_error;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, VariableName, WorkspaceId};
use crate::record::run::EntryState;
use crate::record::store::RunStore;

#[cfg(test)]
mod tests;

/// Where the command of [`exec_command`] runs and what it runs.
pub(crate) struct ContainerExec<'a> {
    /// phase's state directory, which holds the run record.
    pub(crate) state_dir: &'a std::path::Path,
    /// The workspace's stable id.
    pub(crate) workspace: &'a WorkspaceId,
    /// The app.
    pub(crate) app: &'a AppName,
    /// The process, which is the container's Compose service.
    pub(crate) process: &'a ProcessName,
    /// The command and its arguments, each a separate value.
    pub(crate) run: &'a [String],
    /// The variables that the command receives beside the container's own.
    pub(crate) variables: &'a [&'a (VariableName, String)],
    /// Whether the caller's standard input is a terminal, which gives the
    /// command a terminal in the container with `--tty`.
    pub(crate) terminal: bool,
    /// The only variables of the `docker` program's environment, or `None`
    /// for the host program's whole environment.
    pub(crate) docker_environment: Option<&'a BTreeMap<String, String>>,
}

/// The `docker exec` of one command, and the container it runs in, which
/// the caller inspects again when the command's status may be docker's.
pub(crate) struct ContainerCommand {
    /// The `docker exec` command, which has not started.
    pub(crate) command: Command,
    /// The container, as the inspection before the command found it.
    pub(crate) container: Recheck,
}

/// A container that `exec` inspected, which it can inspect again.
pub(crate) struct Recheck {
    /// The app's scope.
    scope: Scope,
    /// The container's service.
    service: names::ServiceName,
    /// The container's id.
    id: names::ContainerId,
}

impl Recheck {
    /// Reports whether docker still has the container and it runs.
    ///
    /// # Errors
    ///
    /// Returns [`PhaseError::Docker`] when the inspection fails.
    pub(crate) fn runs(&self) -> Result<bool, PhaseError> {
        let inspected = inspect_id(&self.scope, &self.service, &self.id)
            .map_err(|error| docker_error(&error))?;
        Ok(inspected.is_some_and(|inspected| inspected.state == ContainerState::Running))
    }
}

/// Returns the `docker exec --interactive` command that runs `exec.run` in
/// the container that the run record names for the process, after it
/// inspects the container with the app's scope. The command does not start
/// here: the caller runs it.
///
/// # Errors
///
/// Returns [`PhaseError::NotRunning`] when the record names no running
/// container of the process, when docker no longer has it, or when it is
/// not running, and [`PhaseError::Docker`] when the inspection fails, such
/// as for a container whose labels name another workspace or app.
pub(crate) fn exec_command(exec: &ContainerExec<'_>) -> Result<ContainerCommand, PhaseError> {
    let not_running = || PhaseError::NotRunning {
        app: exec.app.clone(),
        process: exec.process.clone(),
    };
    let store = RunStore::new(exec.state_dir);
    let record = store.read(exec.workspace)?.ok_or_else(not_running)?;
    let id = record
        .containers
        .iter()
        .find(|entry| {
            &entry.app == exec.app
                && &entry.process == exec.process
                && entry.state == EntryState::Running
        })
        .and_then(|entry| entry.id.clone())
        .ok_or_else(not_running)?;
    let unusable = |reason: String| PhaseError::ExecUnstartable {
        app: exec.app.clone(),
        process: exec.process.clone(),
        reason,
    };
    let service = names::ServiceName::new(exec.process.as_str())
        .map_err(|_| unusable("the process's name is not a name that Docker can use".to_owned()))?;
    let docker_id = names::ContainerId::new(id.as_str()).map_err(|_| {
        unusable("the run record names the container by an id that phase cannot use".to_owned())
    })?;
    let scope = Scope::app(
        Identity {
            id: exec.workspace.clone(),
            run_record: store.path(exec.workspace),
        },
        exec.app,
    );
    let inspected = inspect_id(&scope, &service, &docker_id)
        .map_err(|error| docker_error(&error))?
        .ok_or_else(not_running)?;
    if inspected.state != ContainerState::Running {
        return Err(not_running());
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/exec.rs builds the docker exec of Phase::exec, which the engine's exec module runs (PROC-1)"
    )]
    let mut command = Command::new(DOCKER);
    if let Some(environment) = exec.docker_environment {
        command.env_clear().envs(environment);
    }
    command.args(exec_args(
        inspected.container.id().as_str(),
        exec.variables,
        exec.run,
        exec.terminal,
    ));
    Ok(ContainerCommand {
        command,
        container: Recheck {
            scope,
            service,
            id: docker_id,
        },
    })
}

/// Returns the arguments of `docker` that run `run` in the container `id`
/// with each of `variables`, and `--tty` when `terminal` holds. The
/// command's words follow the container's id, so docker reads none of them
/// as its own flags.
fn exec_args(
    id: &str,
    variables: &[&(VariableName, String)],
    run: &[String],
    terminal: bool,
) -> Vec<String> {
    let mut args = vec!["exec".to_owned(), "--interactive".to_owned()];
    if terminal {
        args.push("--tty".to_owned());
    }
    for (name, value) in variables {
        args.push("--env".to_owned());
        args.push(format!("{name}={value}"));
    }
    args.push("--".to_owned());
    args.push(id.to_owned());
    args.extend(run.iter().cloned());
    args
}
