//! The `docker exec` that runs a one-off command in the running container
//! of a process for [`Phase::exec`](crate::Phase::exec). The container
//! already holds the process's variables and secret files, so the command
//! receives them from Docker, and phase passes no value of a variable on
//! the command line or in the `docker` program's environment.

use std::collections::BTreeMap;
use std::process::Command;

use crate::engine::container::command::DOCKER;
use crate::engine::container::names::{self, Identity, Scope};
use crate::engine::container::objects::{ContainerState, inspect_id};
use crate::engine::container::runner::docker_error;
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
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
    /// Whether the caller's standard input is a terminal, which gives the
    /// command a terminal in the container with `--tty`.
    pub(crate) terminal: bool,
    /// The only variables of the `docker` program's environment, or `None`
    /// for the host program's whole environment.
    pub(crate) docker_environment: Option<&'a BTreeMap<String, String>>,
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
pub(crate) fn exec_command(exec: &ContainerExec<'_>) -> Result<Command, PhaseError> {
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
        exec.run,
        exec.terminal,
    ));
    Ok(command)
}

/// Returns the arguments of `docker` that run `run` in the container `id`,
/// with `--tty` when `terminal` holds. The command's words follow the
/// container's id, so docker reads none of them as its own flags.
fn exec_args(id: &str, run: &[String], terminal: bool) -> Vec<String> {
    let mut args = vec!["exec".to_owned(), "--interactive".to_owned()];
    if terminal {
        args.push("--tty".to_owned());
    }
    args.push("--".to_owned());
    args.push(id.to_owned());
    args.extend(run.iter().cloned());
    args
}
