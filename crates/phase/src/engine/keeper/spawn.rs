//! The engine's side of a keeper: [`spawn`] starts a keeper detached in a
//! group of its own, hands it its specification and environment, and waits
//! for its handshake. The module docs of [`crate::engine::keeper`] state
//! the program, arguments, environment, streams, deadline and end.

use std::ffi::OsString;
use std::io::{self, BufRead as _, BufReader, Read, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use rustix::process::Signal;

use crate::clock::now;
use crate::engine::keeper::KeeperArgs;
use crate::engine::keeper::MOST_HANDSHAKE_BYTES;
use crate::engine::keeper::args::{APP, PROCESS, STATE_DIR, WORKSPACE};
use crate::engine::keeper::spec::{HANDSHAKE_PREFIX, Handshake, KeeperSpec};
use crate::env::AppliedEnvironment;
use crate::error::PhaseError;
use crate::record::process::{Processes, Start, System};
use crate::record::run::HostProcess;

/// The program that runs a keeper, and the words of its hidden subcommand
/// that come before the keeper's arguments, such as `mxv` and `phase keep`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeeperCommand {
    /// The program's path.
    program: PathBuf,
    /// The words before the keeper's arguments.
    leading: Vec<OsString>,
}

impl KeeperCommand {
    /// Returns the command that runs `program` with `leading` before the
    /// keeper's arguments.
    pub(crate) const fn new(program: PathBuf, leading: Vec<OsString>) -> Self {
        Self { program, leading }
    }
}

/// What [`spawn`] needs to start one keeper.
pub(crate) struct Launch<'a> {
    /// The program that runs the keeper.
    pub(crate) command: &'a KeeperCommand,
    /// The keeper's arguments.
    pub(crate) args: &'a KeeperArgs,
    /// The keeper's specification.
    pub(crate) spec: &'a KeeperSpec,
    /// The process's resolved environment, which the keeper holds.
    pub(crate) environment: &'a dyn AppliedEnvironment,
    /// The variables that the keeper inherits from the caller: `PATH` and
    /// `HOME`.
    pub(crate) inherited: &'a [(OsString, OsString)],
    /// When the keeper must have started the app.
    pub(crate) deadline: Instant,
}

/// A keeper that started its app.
#[derive(Debug)]
pub(crate) struct Kept {
    /// The keeper, which leads its own group.
    pub(crate) keeper: HostProcess,
    /// The app, which leads its own group.
    pub(crate) app: HostProcess,
    /// The keeper's child handle, which the engine may drop to leave the
    /// keeper running, or poll to learn its exit.
    #[expect(
        clippy::disallowed_types,
        reason = "crates/phase/src/engine/keeper/spawn.rs holds the keeper it started (PROC-1)"
    )]
    child: std::process::Child,
}

impl Kept {
    /// Returns the keeper's exit status once it has exited, reaping it, or
    /// `None` while it runs.
    pub(crate) fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }
}

/// Starts the keeper that `launch` describes and waits for its handshake
/// until the deadline.
///
/// # Errors
///
/// Returns [`PhaseError::KeeperUnstartable`] when the keeper cannot start,
/// refuses its specification, or gives no handshake before the deadline,
/// and [`PhaseError::ProcessUnstartable`] when the keeper reports that the
/// app cannot start. In each case the keeper is killed and reaped.
pub(crate) fn spawn(launch: &Launch<'_>) -> Result<Kept, PhaseError> {
    let args = launch.args;
    let keeper_failed = |reason: String| PhaseError::KeeperUnstartable {
        app: args.app().clone(),
        process: args.process().clone(),
        reason,
    };
    let spec = serde_json::to_vec(launch.spec)
        .map_err(|error| keeper_failed(format!("the specification cannot be written: {error}")))?;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/keeper/spawn.rs starts keepers (PROC-1)"
    )]
    let mut command = Command::new(&launch.command.program);
    command
        .args(&launch.command.leading)
        .args(command_line(args))
        .env_clear()
        .envs(launch.inherited.iter().map(|(name, value)| (name, value)))
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    launch.environment.apply(&mut command);
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/keeper/spawn.rs starts keepers (PROC-1)"
    )]
    let spawned = command.spawn();
    let mut child =
        spawned.map_err(|error| keeper_failed(format!("the keeper cannot start: {error}")))?;
    let pid = child.id();
    let started = System.start_time(pid);
    let boot_id = System.boot_id();
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        kill(&mut child);
        return Err(keeper_failed(
            "the keeper has no standard streams".to_owned(),
        ));
    };
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // A keeper that ended before reading gives EPIPE here, and its
        // handshake or its missing handshake says why.
        let _ = stdin.write_all(&spec);
        drop(stdin);
        let _ = sender.send(read_handshake(stdout));
    });
    let handshake = receiver.recv_timeout(launch.deadline.saturating_duration_since(now()));
    let keeper = match (started, boot_id) {
        (Ok(Start::At(started_at_us)), Ok(boot_id)) => HostProcess {
            pid,
            started_at_us,
            group: pid,
            boot_id,
        },
        _ => {
            kill(&mut child);
            return Err(keeper_failed(format!(
                "the start time of the keeper {pid} is unreadable"
            )));
        }
    };
    match handshake {
        Ok(Ok(Some(Handshake::Started(app)))) => Ok(Kept { keeper, app, child }),
        Ok(Ok(Some(Handshake::Unstartable(reason)))) => {
            kill(&mut child);
            Err(PhaseError::ProcessUnstartable {
                app: args.app().clone(),
                process: args.process().clone(),
                reason,
            })
        }
        Ok(Ok(Some(Handshake::Refused(reason)))) => {
            kill(&mut child);
            Err(keeper_failed(format!(
                "the keeper refused its specification: {reason}"
            )))
        }
        Ok(Ok(None)) => {
            let status = kill(&mut child);
            Err(keeper_failed(format!(
                "the keeper ended without a handshake, with {status}"
            )))
        }
        Ok(Err(error)) => {
            kill(&mut child);
            Err(keeper_failed(format!(
                "the keeper's handshake cannot be read: {error}"
            )))
        }
        Err(_) => {
            kill(&mut child);
            Err(keeper_failed(
                "the keeper gave no handshake before the deadline".to_owned(),
            ))
        }
    }
}

/// Sends `KILL` to the keeper's group, reaps it, and describes how it
/// ended.
#[expect(
    clippy::disallowed_types,
    reason = "crates/phase/src/engine/keeper/spawn.rs holds the keeper it started (PROC-1)"
)]
fn kill(child: &mut std::process::Child) -> String {
    if let Some(group) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, Signal::KILL);
    }
    child.wait().map_or_else(
        |error| format!("an unreadable status: {error}"),
        |status| status.to_string(),
    )
}

/// Returns the keeper's arguments as its command line holds them, which
/// [`KeeperArgs::parse`] reads back.
pub(crate) fn command_line(args: &KeeperArgs) -> Vec<OsString> {
    vec![
        WORKSPACE.into(),
        args.workspace().as_str().into(),
        APP.into(),
        args.app().as_str().into(),
        PROCESS.into(),
        args.process().as_str().into(),
        STATE_DIR.into(),
        args.state_dir().as_os_str().to_owned(),
    ]
}

/// Reads lines from `input` until the handshake line, reading at most
/// [`MOST_HANDSHAKE_BYTES`] bytes, and returns the handshake, or `None`
/// when the input ends or the limit is reached first.
///
/// # Errors
///
/// Returns the error of a read that fails.
pub(crate) fn read_handshake(input: impl Read) -> io::Result<Option<Handshake>> {
    let mut reader = BufReader::new(input.take(MOST_HANDSHAKE_BYTES));
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        if let Some(json) = line.trim_end().strip_prefix(HANDSHAKE_PREFIX) {
            return serde_json::from_str(json)
                .map(Some)
                .map_err(io::Error::other);
        }
    }
}
