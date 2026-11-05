//! `exec`: runs a one-off command, such as a delivery script or a test run,
//! in the context of one process of an app.
//!
//! The module has two parts. The first builds the command in the
//! process's context. A host process's command receives the process's
//! resolved environment, checked as a start checks it, its working
//! directory, `PATH` and `HOME`, and, when its app is sandboxed, the same
//! sandbox through the same launcher that the process's keeper uses, with
//! its `TMPDIR`. A container process's command is a `docker exec` into the
//! process's running container. The second part, [`run`], runs a built
//! command with the caller's streams, or with streams the caller gives,
//! until it ends or the caller cancels, and returns its exit status.
//!
//! `run` knows nothing of apps, and the builders know nothing of terminals,
//! so a later caller without a terminal, such as an init step that `up`
//! runs once a process's dependencies are ready, can build the same
//! command and run it with its output in a log.
//!
//! `exec` is no lifecycle operation. It writes nothing to the run record,
//! starts and stops no app, and does not hold the lifecycle lock while the
//! command runs. It takes the lock once and releases it at once, so it
//! waits for an `up`, `down`, `restart` or `sweep` in progress to finish
//! and then sees the containers that operation left. Holding the lock
//! while the command runs would block every `up` and `down` of the
//! workspace, and every keeper's record of a restart, for as long as a
//! test run or a shell lasts. A shared lock would block them the same way,
//! since they all take it exclusively.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use wait_timeout::ChildExt as _;

use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now};
use crate::engine::container::exec::{ContainerExec, exec_command};
use crate::engine::keeper::{KeeperCommand, check_inherited, check_resolved};
use crate::engine::run::{SandboxScope, Settings, app_ports, process_dir, process_sandbox};
use crate::env::EnvironmentSource;
use crate::error::{NotRunnable, PhaseError};
use crate::name::{AppName, ProcessName};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::store::RunStore;
use crate::sandbox::launcher_command;
use crate::source::Source;

#[cfg(test)]
mod tests;

/// The command that [`Phase::exec`](crate::Phase::exec) runs, and the
/// process whose context it runs in.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecRequest {
    /// The app.
    pub app: AppName,
    /// The process of the app whose context the command runs in.
    pub process: ProcessName,
    /// The command's program and arguments, each a separate value that no
    /// shell reads.
    pub run: Vec<String>,
    /// The command's standard streams.
    pub io: ExecIo,
}

impl ExecRequest {
    /// Returns the request to run `run` in the context of `process` of
    /// `app`, with `io` as its streams.
    #[must_use]
    pub const fn new(app: AppName, process: ProcessName, run: Vec<String>, io: ExecIo) -> Self {
        Self {
            app,
            process,
            run,
            io,
        }
    }
}

/// The standard streams of an `exec`'s command.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExecIo {
    /// The command shares the caller's standard input, output and error,
    /// and runs in the caller's process group. When the caller runs in the
    /// foreground of a terminal, the command does too, so the terminal
    /// sends Ctrl-C and Ctrl-Z to both. A container's `docker exec`
    /// passes them on.
    ///
    /// The caller should catch `SIGINT` while the command runs, rather than
    /// ignore it, because a command inherits an ignored signal, and should
    /// not set the `exec`'s cancel flag from that handler, because the
    /// command has received the Ctrl-C itself and decides what it means.
    Inherit {
        /// Whether the caller's standard input is a terminal. A container
        /// command then runs with a terminal of its own, through `docker
        /// exec --tty`. The host program knows this, and phase reads no
        /// standard stream.
        terminal: bool,
    },
    /// The command reads and writes these streams, such as
    /// [`Stdio::null`] or a file, and runs in a process group of its own,
    /// which no terminal signals.
    Streams {
        /// The command's standard input.
        stdin: Stdio,
        /// The command's standard output.
        stdout: Stdio,
        /// The command's standard error.
        stderr: Stdio,
    },
}

/// What `exec` needs besides its request: the settings that a keeper's
/// start also uses, and the environment of the `docker` program.
pub(crate) struct ExecSettings<'a> {
    /// The settings of the engine.
    pub(crate) settings: Settings<'a>,
    /// The only variables of the `docker` program's environment, or `None`
    /// for the host program's whole environment.
    pub(crate) docker_environment: Option<&'a BTreeMap<String, String>>,
}

/// Runs `request` in its process's context, as the module docs say, and
/// returns the command's exit status.
///
/// # Errors
///
/// Returns [`PhaseError::UnknownTarget`] when the plan lacks the app or the
/// process, [`PhaseError::Busy`] when an operation holds the lifecycle lock
/// until `deadline`, the errors of [`host_command`] and of
/// [`exec_command`], and [`PhaseError::ExecUnstartable`] when the command
/// cannot start or phase cannot wait for it.
pub(crate) fn exec(
    exec: &ExecSettings<'_>,
    plan: &Plan,
    request: ExecRequest,
    env: &dyn EnvironmentSource,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<ExitStatus, PhaseError> {
    let (app, process) = target(plan, &request.app, &request.process)?;
    if request.run.is_empty() {
        return Err(unstartable(app, process, "the command is empty".to_owned()));
    }
    // The lock is taken and released at once, so `exec` runs after an
    // operation in progress and blocks none while its command runs.
    drop(
        RunStore::new(exec.settings.state_dir)
            .lock(plan.id(), deadline.saturating_duration_since(now()))?,
    );
    let command = match app.runner {
        RunnerKind::Host => host_command(
            &exec.settings,
            plan,
            app,
            process,
            &request.run,
            env,
            deadline,
        )?,
        RunnerKind::Container => exec_command(&ContainerExec {
            state_dir: exec.settings.state_dir,
            workspace: plan.id(),
            app: &app.name,
            process: &process.name,
            run: &request.run,
            terminal: matches!(request.io, ExecIo::Inherit { terminal: true }),
            docker_environment: exec.docker_environment,
        })?,
    };
    run(command, request.io, cancel, exec.settings.kill_wait)
        .map_err(|error| unstartable(app, process, error.to_string()))
}

/// Returns the app and the process that `app` and `process` name in
/// `plan`.
fn target<'p>(
    plan: &'p Plan,
    app: &AppName,
    process: &ProcessName,
) -> Result<(&'p AppPlan, &'p ProcessPlan), PhaseError> {
    let unknown = || PhaseError::UnknownTarget {
        target: format!("{app}/{process}"),
    };
    let app = plan.app(app).ok_or_else(unknown)?;
    let process = app
        .processes
        .iter()
        .find(|found| &found.name == process)
        .ok_or_else(unknown)?;
    Ok((app, process))
}

/// Returns the failure to start a command in the context of `process` of
/// `app`, for `reason`, which holds no environment value.
fn unstartable(app: &AppPlan, process: &ProcessPlan, reason: String) -> PhaseError {
    PhaseError::ExecUnstartable {
        app: app.name.clone(),
        process: process.name.clone(),
        reason,
    }
}

/// Returns the command that runs `run` in the context of the host
/// `process` of `app`: in the process's directory, with only the inherited
/// `PATH` and `HOME` and the process's environment, which it resolves
/// through `env` before `deadline` and checks as a start does. When the
/// app is sandboxed, the command is the launcher of the process's own
/// sandbox, which receives the sandbox's `TMPDIR`. The command does not
/// start here.
///
/// # Errors
///
/// Returns [`PhaseError::NotRunnable`] for an app whose program comes from
/// a release or a build, [`PhaseError::Environment`] when the environment
/// cannot be resolved or lacks a required variable, and
/// [`PhaseError::ExecUnstartable`] when the inherited or resolved names
/// cannot be passed on or the sandbox cannot be built.
pub(crate) fn host_command(
    settings: &Settings<'_>,
    plan: &Plan,
    app: &AppPlan,
    process: &ProcessPlan,
    run: &[String],
    env: &dyn EnvironmentSource,
    deadline: Instant,
) -> Result<Command, PhaseError> {
    let refuse = |reason| PhaseError::NotRunnable {
        app: app.name.clone(),
        reason,
    };
    match &app.source {
        Source::Release { .. } => return Err(refuse(NotRunnable::Release)),
        Source::Checkout { build: Some(_), .. } => return Err(refuse(NotRunnable::CheckoutBuild)),
        Source::Checkout { build: None, .. } | Source::Image { .. } => {}
    }
    check_inherited(settings.inherited)
        .map_err(|error| unstartable(app, process, error.to_string()))?;
    let environment =
        crate::env::resolve_required(env, &app.name, process, deadline).map_err(|source| {
            PhaseError::Environment {
                app: app.name.clone(),
                source,
            }
        })?;
    check_resolved(&environment.names()).map_err(|reason| unstartable(app, process, reason))?;
    let sandbox = match settings.keeper {
        Some(keeper) => sandbox(settings, plan, app, process, keeper)?,
        // Without a keeper program there is no launcher, so an app that
        // needs the sandbox cannot run, and phase refuses it rather than
        // run it unsandboxed.
        None if app.permissions.is_some()
            && matches!(
                crate::sandbox::platform(),
                crate::sandbox::Platform::Available(_)
            ) =>
        {
            return Err(unstartable(
                app,
                process,
                "the program that links phase gave no keeper program, which launches the sandbox"
                    .to_owned(),
            ));
        }
        None => None,
    };
    let mut command = match &sandbox {
        Some(spec) => launcher_command(spec, run).map_err(|reason| {
            unstartable(
                app,
                process,
                format!("the sandbox cannot be passed to the launcher: {reason}"),
            )
        })?,
        None => {
            let (program, arguments) = run
                .split_first()
                .ok_or_else(|| unstartable(app, process, "the command is empty".to_owned()))?;
            #[expect(
                clippy::disallowed_methods,
                reason = "crates/phase/src/engine/exec.rs builds the command of Phase::exec, which run starts (PROC-1)"
            )]
            let mut command = Command::new(program);
            command.args(arguments);
            command
        }
    };
    command
        .current_dir(process_dir(app, process))
        .env_clear()
        .envs(settings.inherited);
    environment.apply(&mut command);
    if let Some(spec) = &sandbox {
        command.env("TMPDIR", &spec.tmp);
    }
    Ok(command)
}

/// Returns the sandbox of `process` of `app`, as its keeper builds it, or
/// `None` when the process runs unsandboxed.
fn sandbox(
    settings: &Settings<'_>,
    plan: &Plan,
    app: &AppPlan,
    process: &ProcessPlan,
    keeper: &KeeperCommand,
) -> Result<Option<crate::sandbox::SandboxSpec>, PhaseError> {
    let ports: BTreeMap<AppName, BTreeSet<u16>> = app_ports(plan);
    let scope = SandboxScope {
        workspace: plan.id(),
        workspace_paths: plan.workspace_paths(),
        ports: &ports,
    };
    process_sandbox(settings, &scope, app, process, keeper).map_err(|error| {
        unstartable(
            app,
            process,
            format!("its sandbox cannot be built: {error}"),
        )
    })
}

/// Starts `command` with `io` as its streams, waits for it to end, and
/// returns its exit status: its code, or the signal that ended it.
///
/// With [`ExecIo::Inherit`] the command runs in the caller's process
/// group, and with [`ExecIo::Streams`] in a group of its own. When `cancel`
/// is set, `run` sends the command `TERM`, or its group `TERM` when it has
/// one, waits up to `kill_wait`, then sends `KILL` the same way, and
/// returns the status that the signal gave.
///
/// # Errors
///
/// Returns the error of starting the command, such as a program that does
/// not exist, or of waiting for it.
pub(crate) fn run(
    mut command: Command,
    io: ExecIo,
    cancel: &Cancel,
    kill_wait: Duration,
) -> io::Result<ExitStatus> {
    let own_group = match io {
        ExecIo::Inherit { .. } => false,
        ExecIo::Streams {
            stdin,
            stdout,
            stderr,
        } => {
            command
                .stdin(stdin)
                .stdout(stdout)
                .stderr(stderr)
                .process_group(0);
            true
        }
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/exec.rs starts the command of Phase::exec and waits for it (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/phase/src/engine/exec.rs holds the command of Phase::exec until it ends (PROC-1)"
    )]
    let mut child: std::process::Child = spawned?;
    // `command` holds the environment's values, so it goes before the wait.
    drop(command);
    let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let signal = |signal: Signal| {
        if let Some(pid) = pid {
            // The command may have ended meanwhile, which the next wait
            // reports.
            let _ = if own_group {
                rustix::process::kill_process_group(pid, signal)
            } else {
                rustix::process::kill_process(pid, signal)
            };
        }
    };
    let mut killed_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.wait_timeout(POLL_INTERVAL)? {
            return Ok(status);
        }
        match killed_at {
            None if cancel.is_cancelled() => {
                signal(Signal::TERM);
                killed_at = Some(now() + kill_wait);
            }
            Some(at) if now() >= at => {
                signal(Signal::KILL);
                killed_at = Some(now() + kill_wait);
            }
            _ => {}
        }
    }
}
