//! `exec`: runs a one-off command, such as a delivery script or a test run,
//! in the context of one process of an app.
//!
//! The module has two parts. The first builds the command in the
//! process's context. A host process's command receives the process's
//! resolved environment, checked as a start checks it, its working
//! directory, `PATH` and `HOME`, and, when its app is sandboxed, the same
//! sandbox through the same launcher that the process's keeper uses, with
//! the environment that the keeper gives a sandboxed app. A container
//! process's command is a `docker exec` into the process's running
//! container. The second part, [`run`], runs a built command with the
//! caller's streams, or with streams the caller gives, until it ends or
//! the caller cancels, and returns its exit status.
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
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal};
use wait_timeout::ChildExt as _;

use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::container::exec::{ContainerExec, exec_command};
use crate::engine::keeper::{KeeperCommand, check_inherited, check_resolved};
use crate::engine::run::{SandboxScope, Settings, app_ports, process_dir, process_sandbox};
use crate::env::EnvironmentSource;
use crate::error::{NotRunnable, PhaseError};
use crate::name::{AppName, ProcessName, VariableName};
use crate::plan::{AppPlan, Plan, ProcessPlan, RunnerKind};
use crate::record::store::RunStore;
use crate::sandbox::{launcher_command, sandboxed_environment};
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
    /// The variables that the command receives beside the process's own,
    /// such as the thread counts that a job gets from a machine-wide
    /// scheduler. A variable that the process's plan already declares keeps
    /// its declared value. A value here is passed on `docker exec`'s command
    /// line for a container process, so it must hold no secret.
    pub variables: Vec<(VariableName, String)>,
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
            variables: Vec::new(),
        }
    }

    /// Returns the request with `variables` added to the command's
    /// environment, as [`ExecRequest::variables`] describes.
    #[must_use]
    pub fn with_variables(mut self, variables: Vec<(VariableName, String)>) -> Self {
        self.variables = variables;
        self
    }
}

/// The standard streams of an `exec`'s command.
#[derive(Debug)]
#[non_exhaustive]
pub enum ExecIo {
    /// The command shares the caller's standard input, output and error.
    ///
    /// An unsandboxed host command, and the `docker exec` of a container
    /// command, run in the caller's process group. When the caller runs in
    /// the foreground of a terminal, they do too, so the terminal sends
    /// Ctrl-C and Ctrl-Z to the caller and the command alike. A container's
    /// `docker exec` passes them on. The caller should then catch `SIGINT`
    /// rather than ignore it, because a command inherits an ignored
    /// signal, and should not set the cancel flag from that handler,
    /// because the command has received the Ctrl-C itself. A cancel stops
    /// only the command's own process, so processes that it started may
    /// survive it.
    ///
    /// A sandboxed host command runs in a session of its own, which its
    /// launcher starts before it applies the sandbox, so the command has no
    /// controlling terminal and cannot push input into the caller's shell.
    /// The terminal's Ctrl-C then reaches only the caller, so the caller
    /// should set the cancel flag on `SIGINT`, and `exec` ends the
    /// command's whole process group. [`Phase::exec_sandboxed`] tells the
    /// caller which case applies. `exec` refuses a sandboxed host command
    /// with [`PhaseError::ExecNeedsTerminal`] when `terminal` holds, or
    /// when any of the caller's standard input, output and error is a
    /// terminal, which `exec` checks itself. A sandboxed command that
    /// inherited the terminal could read what the user types into it, and
    /// one that runs without it would not get the terminal it expects.
    ///
    /// A command that the cancel flag kills may leave the terminal in the
    /// raw mode that it set, which the caller can reset.
    ///
    /// [`Phase::exec_sandboxed`]: crate::Phase::exec_sandboxed
    Inherit {
        /// Whether the caller's standard input is a terminal. A container
        /// command then runs with a terminal of its own, through `docker
        /// exec --tty`, and a sandboxed host command is refused. The host
        /// program knows this. phase reads no standard stream, and checks
        /// only whether each is a terminal before a sandboxed command.
        terminal: bool,
    },
    /// The command reads and writes these streams, such as a file or a
    /// pipe, and `None` is the null device. It runs in a process group of
    /// its own, which no terminal signals. A cancel ends the whole group. A
    /// sandboxed host command's group leads a session of its own.
    ///
    /// `exec` refuses a sandboxed host command with
    /// [`PhaseError::ExecNeedsTerminal`] when any of these is a terminal.
    /// Its own session takes away the controlling terminal but leaves the
    /// open descriptor, through which the command could read what the user
    /// types. The streams are descriptors because a [`Stdio`] cannot be
    /// asked whether it is a terminal.
    Streams {
        /// The command's standard input.
        stdin: Option<OwnedFd>,
        /// The command's standard output.
        stdout: Option<OwnedFd>,
        /// The command's standard error.
        stderr: Option<OwnedFd>,
    },
}

/// Where the command that [`run`] starts stands towards the caller's
/// session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Session {
    /// The command runs where its streams put it: in the caller's process
    /// group with [`ExecIo::Inherit`], and in a group of its own with
    /// [`ExecIo::Streams`].
    Caller,
    /// The command starts a session of its own, as the sandbox's launcher
    /// does, so it leads its own process group and has no controlling
    /// terminal.
    Own,
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
    if is_sandboxed(app) && reaches_terminal(&request.io) {
        return Err(PhaseError::ExecNeedsTerminal {
            app: app.name.clone(),
            process: process.name.clone(),
        });
    }
    // The lock is taken and released at once, so `exec` runs after an
    // operation in progress and blocks none while its command runs.
    drop(
        RunStore::new(exec.settings.state_dir)
            .lock(plan.id(), deadline.saturating_duration_since(now()))?,
    );
    let failed = |error: io::Error| unstartable(app, process, error.to_string());
    match app.runner {
        RunnerKind::Host => {
            let (mut command, session) = host_command(
                &exec.settings,
                plan,
                app,
                process,
                &request.run,
                env,
                deadline,
            )?;
            add_variables(&mut command, &request.variables);
            run(
                command,
                session,
                request.io,
                cancel,
                exec.settings.kill_wait,
            )
            .map_err(failed)
        }
        RunnerKind::Container => {
            // The plan names every variable that mxv's own side declares. A
            // variable that only the image sets is unknown here, so a job
            // variable of that name replaces the image's value. Reading the
            // image's `Env` before each `exec` would close this.
            let variables: Vec<&(VariableName, String)> = request
                .variables
                .iter()
                .filter(|(name, _)| !process.env.contains(name.as_str()))
                .collect();
            let prepared = exec_command(&ContainerExec {
                state_dir: exec.settings.state_dir,
                workspace: plan.id(),
                app: &app.name,
                process: &process.name,
                run: &request.run,
                variables: &variables,
                terminal: matches!(request.io, ExecIo::Inherit { terminal: true }),
                docker_environment: exec.docker_environment,
            })?;
            let status = run(
                prepared.command,
                Session::Caller,
                request.io,
                cancel,
                exec.settings.kill_wait,
            )
            .map_err(failed)?;
            // docker exits 125 when it fails itself, and 126 or 127 when it
            // cannot run the command, which it also does when the
            // container stopped or went since the inspection.
            if matches!(status.code(), Some(125..=127)) && !prepared.container.runs()? {
                return Err(PhaseError::NotRunning {
                    app: app.name.clone(),
                    process: process.name.clone(),
                });
            }
            Ok(status)
        }
    }
}

/// Sets each of `variables` on `command`, except one that `command`'s
/// environment already sets, so a variable that the process declares, or
/// that phase sets itself, such as `PATH` or a sandbox's `TMPDIR`, keeps its
/// value.
fn add_variables(command: &mut Command, variables: &[(VariableName, String)]) {
    let set: BTreeSet<OsString> = command
        .get_envs()
        .filter(|(_, value)| value.is_some())
        .map(|(name, _)| name.to_os_string())
        .collect();
    for (name, value) in variables {
        if !set.contains(OsStr::new(name.as_str())) {
            command.env(name.as_str(), value);
        }
    }
}

/// Reports whether a command with the streams `io` would hold a terminal.
fn reaches_terminal(io: &ExecIo) -> bool {
    use std::io::IsTerminal;
    match io {
        ExecIo::Inherit { terminal } => *terminal || caller_has_terminal(),
        ExecIo::Streams {
            stdin,
            stdout,
            stderr,
        } => [stdin, stdout, stderr]
            .into_iter()
            .flatten()
            .any(IsTerminal::is_terminal),
    }
}

/// Reports whether any of the caller's standard input, output and error is
/// a terminal. A sandboxed command that inherited one could read what the
/// user types into it, so `exec` checks the streams themselves rather than
/// trust the caller's flag.
fn caller_has_terminal() -> bool {
    use std::io::IsTerminal as _;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/exec.rs asks whether the caller's standard streams are terminals, and reads and writes none of them"
    )]
    let streams = (std::io::stdin(), std::io::stdout(), std::io::stderr());
    streams.0.is_terminal() || streams.1.is_terminal() || streams.2.is_terminal()
}

/// Reports whether `exec` runs a command in the context of `process` of
/// `app` in `plan` under the sandbox, in a session of its own.
///
/// # Errors
///
/// Returns [`PhaseError::UnknownTarget`] when the plan lacks the app or the
/// process.
pub(crate) fn sandboxed(
    plan: &Plan,
    app: &AppName,
    process: &ProcessName,
) -> Result<bool, PhaseError> {
    let (app, _) = target(plan, app, process)?;
    Ok(is_sandboxed(app))
}

/// Reports whether the processes of `app` run under the sandbox: it is a
/// host app that declares permissions, on a machine that offers a sandbox.
/// [`process_sandbox`] builds a sandbox for exactly these.
fn is_sandboxed(app: &AppPlan) -> bool {
    app.runner == RunnerKind::Host
        && app.permissions.is_some()
        && matches!(
            crate::sandbox::platform(),
            crate::sandbox::Platform::Available(_)
        )
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
/// sandbox, which receives the environment of a sandboxed app as the
/// keeper gives it, and which starts a session of its own. The command does not start here.
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
) -> Result<(Command, Session), PhaseError> {
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
        None if is_sandboxed(app) => {
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
    let session = match &sandbox {
        Some(spec) => {
            sandboxed_environment(&mut command, &spec.tmp);
            Session::Own
        }
        None => Session::Caller,
    };
    Ok((command, session))
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
/// A command whose `session` is [`Session::Own`] leads its own process
/// group once it has started its session. Otherwise the command runs in the
/// caller's process group with [`ExecIo::Inherit`], and in a group of its
/// own with [`ExecIo::Streams`]. When `cancel` is set, `run` sends `TERM`
/// to the command's group when it has one of its own, and to the command,
/// waits up to `kill_wait`, then sends `KILL` the same way, and returns the
/// status that the signal gave. When the command exits and other members of
/// its group remain, `run` waits for them until `kill_wait` has passed since
/// the `TERM`, sends `KILL` to the group, and only then returns. A command
/// in the caller's group receives the signals alone, so processes that it
/// started may survive it.
///
/// # Errors
///
/// Returns the error of starting the command, such as a program that does
/// not exist, or of waiting for it.
pub(crate) fn run(
    mut command: Command,
    session: Session,
    io: ExecIo,
    cancel: &Cancel,
    kill_wait: Duration,
) -> io::Result<ExitStatus> {
    let own_group = match io {
        ExecIo::Inherit { .. } => session == Session::Own,
        ExecIo::Streams {
            stdin,
            stdout,
            stderr,
        } => {
            let stream = |given: Option<OwnedFd>| given.map_or_else(Stdio::null, Stdio::from);
            command
                .stdin(stream(stdin))
                .stdout(stream(stdout))
                .stderr(stream(stderr));
            // A process that leads its group cannot start a session, so a
            // command that starts its own stays out of a group here.
            if session == Session::Caller {
                command.process_group(0);
            }
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
            // reports. A command that starts its own session may not have
            // started it yet, so the command itself receives the signal
            // too.
            if own_group {
                let _ = rustix::process::kill_process_group(pid, signal);
            }
            let _ = rustix::process::kill_process(pid, signal);
        }
    };
    let mut killed_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.wait_timeout(POLL_INTERVAL)? {
            if own_group
                && cancel.is_cancelled()
                && let Some(group) = pid
            {
                end_group(group, killed_at, kill_wait);
            }
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

/// Ends what remains of the process group `group` after its leader exited
/// on a cancel: it sends `TERM` unless `killed_at` says that `run` already
/// did, waits until that time for the group to empty, and otherwise sends
/// `KILL`. A group that has emptied can have its number reused, but only
/// within one poll of it emptying, and `run` looks again before each signal.
fn end_group(group: Pid, killed_at: Option<Instant>, kill_wait: Duration) {
    let kill_at = killed_at.unwrap_or_else(|| {
        // A group that has already gone reports that to the wait below.
        let _ = rustix::process::kill_process_group(group, Signal::TERM);
        now() + kill_wait
    });
    while rustix::process::test_kill_process_group(group).is_ok() {
        if now() >= kill_at {
            let _ = rustix::process::kill_process_group(group, Signal::KILL);
            return;
        }
        pause(POLL_INTERVAL);
    }
}
