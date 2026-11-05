//! Running the `docker` program: the one place in phase that starts it and
//! waits for it, with a deadline, and reads what it printed.

use std::ffi::OsStr;
use std::io::{self, Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use rustix::process::{Pid, Signal};
use wait_timeout::ChildExt as _;

use crate::cancel::Cancel;
use crate::clock::{now, pause};
use crate::engine::container::ContainerError as Error;
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::ContainerId;
use crate::engine::container::objects::DOCKER_ERROR_PREFIXES;
use crate::engine::container::secrets::SecretBytes;
use crate::engine::probe::{CommandEnd, ProbeExec, exited, run_bounded};
use crate::env::AppliedEnvironment;

/// The variables that a docker command receives on top of the host
/// program's own environment. Neither implementation implements `Debug`,
/// `Display` or `Serialize`, and each sets its values on the `Command`
/// only.
pub(crate) trait ChildEnv {
    /// Sets the variables on `command`.
    fn set_on(self, command: &mut Command);
}

impl ChildEnv for ComposeEnv {
    fn set_on(self, command: &mut Command) {
        command.envs(self.into_vars());
    }
}

/// The values of a process's resolved environment that reach a `docker
/// compose up` child, each under the private name that `reference`
/// returns for the variable's own name. A variable for which `reference`
/// returns `None` is left out. The child inherits none of the names in
/// `cleared`, which are every private name that the compose file refers
/// to.
pub(crate) struct Referenced<'a> {
    /// The process's resolved environment.
    pub(crate) environment: &'a dyn AppliedEnvironment,
    /// The private name of each variable that reaches the child.
    pub(crate) reference: &'a dyn Fn(&str) -> Option<String>,
    /// The private names that the child does not inherit.
    pub(crate) cleared: &'a [String],
}

impl ChildEnv for Referenced<'_> {
    fn set_on(self, command: &mut Command) {
        for name in self.cleared {
            command.env_remove(name);
        }
        self.environment.apply_as(command, self.reference);
    }
}

/// How long the feeder of a secret pauses between two bytes, which a test
/// sets through [`FeedPause`] to catch the processes of a write while they
/// run. It is zero outside tests.
#[cfg(test)]
static FEED_PAUSE_MILLIS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The pause between two bytes of every secret's feed while a test holds
/// it, which ends when it is dropped, so no other write is slowed.
#[cfg(test)]
pub(crate) struct FeedPause;

#[cfg(test)]
impl FeedPause {
    /// Pauses `millis` milliseconds between two bytes of a feed until the
    /// returned value is dropped.
    pub(crate) fn set(millis: u64) -> Self {
        FEED_PAUSE_MILLIS.store(millis, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}

#[cfg(test)]
impl Drop for FeedPause {
    fn drop(&mut self) {
        FEED_PAUSE_MILLIS.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Writes `input` to `stdin` and closes it. In a test that holds a
/// `FeedPause`, it writes one byte at a time with that pause.
fn feed(mut stdin: std::process::ChildStdin, input: &SecretBytes) {
    #[cfg(test)]
    {
        let millis = FEED_PAUSE_MILLIS.load(std::sync::atomic::Ordering::Relaxed);
        if millis > 0 {
            for byte in input.framed() {
                if stdin.write_all(&[*byte]).is_err() {
                    return;
                }
                let _ = stdin.flush();
                crate::clock::pause(Duration::from_millis(millis));
            }
            return;
        }
    }
    let _ = stdin.write_all(input.framed());
}

/// Why [`secret_exec`] stopped `docker` before it exited.
enum Stop {
    /// The caller cancelled the write.
    Cancelled,
    /// The deadline passed.
    Late,
    /// phase could not wait for `docker`.
    WaitFailed(io::Error),
}

/// Runs `docker <args>`, where `args` are a `docker exec --interactive`
/// that stages a secret, with `input`, the secret's framed bytes, on its
/// standard input, which it then closes, as [`secret_exec`] does.
///
/// # Errors
///
/// Returns the errors of [`secret_exec`].
pub(super) fn write_secret(
    args: &[String],
    input: SecretBytes,
    environment: Option<&std::collections::BTreeMap<String, String>>,
    until: (Duration, &Cancel),
) -> Result<ExitStatus, Error> {
    secret_exec(args, Some(input), environment, "secret write", until)
}

/// Runs `docker <args>`, where `args` are a `docker exec` of one of the
/// secret scripts, with `input` on its standard input when there is one,
/// within the deadline of `until` or until its cancel flag is set, and
/// returns how it exited.
///
/// The standard output and the standard error of `docker` go to
/// `/dev/null`, and phase never reads them. A `docker exec` passes on
/// what the process inside the container writes, and that process, which
/// the container's image supplies as `/bin/sh`, receives the secret's
/// value. So the only fact that phase takes from the exec is its exit
/// status, and a failure's message is phase's own sentence about that
/// status.
///
/// `docker` starts in a process group of its own. With `environment`, it
/// receives those variables alone, which the host program chose, and
/// otherwise the host program's environment, as phase's other `docker`
/// commands do. No secret's value is in either. When the deadline passes
/// or the caller cancels, phase kills the group, reaps `docker`, and joins
/// the thread that fed it, whose pipe the kill closed. The feeder's next
/// write to the closed pipe then fails with `EPIPE` and the feeder ends,
/// because Rust's runtime ignores `SIGPIPE` in every program it starts, so
/// the write does not kill the host program.
///
/// # Errors
///
/// Returns [`Error::Missing`] when no `docker` program is on `PATH`,
/// [`Error::Cancelled`], and [`Error::Failed`] when the deadline passes or
/// phase cannot wait for `docker`.
pub(super) fn secret_exec(
    args: &[String],
    input: Option<SecretBytes>,
    environment: Option<&std::collections::BTreeMap<String, String>>,
    operation: &'static str,
    until: (Duration, &Cancel),
) -> Result<ExitStatus, Error> {
    let (deadline, cancel) = until;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
    )]
    let mut command = Command::new(DOCKER);
    if let Some(environment) = environment {
        command.env_clear().envs(environment);
    }
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/phase/src/engine/container/command.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(Error::Missing { source });
        }
        Err(source) => return Err(Error::unexpected(operation, None, source.to_string())),
    };
    let pid = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    let feeder = child
        .stdin
        .take()
        .zip(input)
        .map(|(stdin, input)| thread::spawn(move || feed(stdin, &input)));
    let ends = now() + deadline;
    let stopped = loop {
        match exited(pid) {
            Ok(true) => break None,
            Ok(false) => {}
            Err(error) => break Some(Stop::WaitFailed(error)),
        }
        if cancel.is_cancelled() {
            break Some(Stop::Cancelled);
        }
        if now() >= ends {
            break Some(Stop::Late);
        }
        pause(CANCEL_POLL.min(ends.saturating_duration_since(now())));
    };
    // The group is killed however the write ended, so nothing that docker
    // started outlives it. docker stays unreaped until then, so its pid,
    // which is the group's id, is not reused in between.
    if let Some(group) = pid {
        let _ = rustix::process::kill_process_group(group, Signal::KILL);
    }
    let reaped = child.wait();
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    match (stopped, reaped) {
        (Some(Stop::Cancelled), _) => Err(Error::Cancelled { operation }),
        (Some(Stop::WaitFailed(error)), _) => Err(Error::unexpected(
            operation,
            None,
            format!("phase could not wait for docker: {error}"),
        )),
        (Some(Stop::Late), _) => Err(Error::timed_out(
            operation,
            format!(
                "docker did not finish within {} seconds, so phase stopped it",
                deadline.as_secs()
            ),
        )),
        (None, Ok(status)) => Ok(status),
        (None, Err(source)) => Err(Error::unexpected(operation, None, source.to_string())),
    }
}

/// The program that this backend runs.
pub(crate) const DOCKER: &str = "docker";

/// The deadline of `docker compose up`, which may pull the image first.
pub(crate) const PULL_DEADLINE: Duration = Duration::from_secs(600);

/// The deadline of every other docker command, apart from `docker stop`,
/// whose deadline is its grace plus this.
pub(crate) const DOCKER_DEADLINE: Duration = Duration::from_secs(30);

/// The most bytes of each output stream that a docker command keeps.
pub(super) const MOST_OUTPUT_BYTES: u64 = 1024 * 1024;

/// What a finished docker command printed and how it exited.
pub(super) struct Finished {
    /// The command's exit status.
    pub(super) status: ExitStatus,
    /// The kept bytes of standard output, as text.
    pub(super) stdout: String,
    /// The kept bytes of standard error, as text.
    pub(super) stderr: String,
}

impl Finished {
    /// Returns standard output when the command succeeded, or the failure
    /// of `operation` on the object `container`.
    pub(super) fn ok(
        self,
        operation: &'static str,
        container: Option<&str>,
    ) -> Result<String, Error> {
        if self.status.success() {
            Ok(self.stdout)
        } else {
            Err(Error::failed(operation, container, message(&self.stderr))
                .with_docker_line(docker_line(&self.stderr)))
        }
    }

    /// Reports whether docker's daemon answered that the object does not
    /// exist. Only the daemon's own answers count: a line that starts with
    /// `Error response from daemon:` and says `No such container`, `No
    /// such volume`, `No such object` or `network <name> not found`, or the
    /// `no such object` line that `docker inspect` prints after the daemon
    /// answered for every kind of object. Any other failure, such as an
    /// unknown context or a daemon that does not answer, is no answer
    /// about the object, so it never counts as missing.
    pub(super) fn missing(&self) -> bool {
        !self.status.success() && self.stderr.lines().any(says_missing)
    }

    /// Returns standard output when the command succeeded. When it failed,
    /// it checks whether the daemon answers, and returns the failure of
    /// `operation` on `container`.
    pub(super) fn checked(
        self,
        operation: &'static str,
        container: Option<&str>,
    ) -> Result<String, Error> {
        self.checked_with(DOCKER.as_ref(), operation, container)
    }

    /// Returns standard output when the command succeeded. When it failed,
    /// it checks whether the daemon of `program` answers, and returns the
    /// failure of `operation` on `container`.
    pub(super) fn checked_with(
        self,
        program: &OsStr,
        operation: &'static str,
        container: Option<&str>,
    ) -> Result<String, Error> {
        if !self.status.success() {
            check_with(program)?;
        }
        self.ok(operation, container)
    }
}

/// The start of every answer that docker's daemon sends back as an error.
const DAEMON_ANSWER: &str = "Error response from daemon:";

/// Reports whether `line` of docker's standard error is the daemon's
/// answer that an object does not exist, as [`Finished::missing`]
/// describes.
pub(super) fn says_missing(line: &str) -> bool {
    let line = line.trim();
    if let Some(answer) = line.strip_prefix(DAEMON_ANSWER) {
        let answer = answer.trim();
        return [
            "No such container",
            "No such image",
            "No such volume",
            "no such volume",
            "No such object",
        ]
        .iter()
        .any(|said| answer.contains(said))
            || (answer.starts_with("network ") && answer.ends_with(" not found"));
    }
    let line = line.to_ascii_lowercase();
    line.starts_with("error: no such object:") || line.starts_with("error: no such image:")
}

/// Returns the line of docker's standard error that says what failed: the
/// first line that starts with `docker:` or `Error`, which is where docker
/// puts its own message, and otherwise the last line that is not empty. It
/// keeps the line to [`crate::MOST_DOCKER_MESSAGE_BYTES`] bytes.
pub(crate) fn message(stderr: &str) -> String {
    let mut lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    lines
        .clone()
        .find(|line| line.starts_with("docker:") || line.starts_with("Error"))
        .or_else(|| lines.next_back())
        .map(|line| crate::record::docker::bounded(line.to_owned()))
        .unwrap_or_default()
}

/// The starts of the lines that the docker client prints for its own
/// errors, beyond [`DOCKER_ERROR_PREFIXES`]. Docker 29 prints `failed to
/// connect to the docker API at` where older clients print `Cannot connect
/// to the Docker daemon`.
const CLIENT_ERROR_PREFIXES: [&str; 1] = ["failed to connect to the docker API at"];

/// Reports whether `line`, as docker printed it, starts with one of
/// Docker's own error forms: [`DOCKER_ERROR_PREFIXES`] or
/// [`CLIENT_ERROR_PREFIXES`]. A line that starts with only `Error:`,
/// `docker:` or white space is not one of them, because containers print
/// such lines too.
fn is_docker_form(line: &str) -> bool {
    DOCKER_ERROR_PREFIXES
        .iter()
        .chain(CLIENT_ERROR_PREFIXES.iter())
        .any(|prefix| line.starts_with(prefix))
}

/// Returns Docker's own error line from `stderr`, the standard error of a
/// docker command other than `docker compose up` and `docker logs`: its
/// last line that is not empty, and only when every line that is not
/// empty is in one of Docker's own forms, as [`is_docker_form`] says.
///
/// The commands that reach it print only Docker's own lines on standard
/// error. A `docker exec` into a container never reaches it: phase runs
/// the secret scripts through [`secret_exec`], which discards what they
/// print, and a probe's command, whose output goes to `/dev/null`. The rule
/// that every line must be in Docker's forms keeps a line that some other
/// program wrote from becoming Docker's line all the same.
pub(super) fn docker_line(stderr: &str) -> Option<String> {
    let mut lines = stderr.lines().filter(|line| !line.trim().is_empty());
    if !lines.clone().all(is_docker_form) {
        return None;
    }
    lines.next_back().map(|line| line.trim_end().to_owned())
}

/// Returns Docker's own error line from `stderr`, the standard error of a
/// `docker compose up --detach`: its last line that is not empty, and only
/// when that line is in one of Docker's own forms, as [`is_docker_form`]
/// says. Compose prints its progress lines first, such as ` Container
/// phase-web-1 Starting`, and Docker's error last. A detached `compose up`
/// does not pass on what a container writes.
pub(super) fn compose_docker_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .filter(|line| is_docker_form(line))
        .map(|line| line.trim_end().to_owned())
}

/// Runs `docker <args>` with `deadline` and the variables `env` on top of
/// the host program's own environment, and returns what it printed and how
/// it exited.
pub(super) fn run<I, S, E>(
    args: I,
    env: E,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: ChildEnv,
{
    run_program(DOCKER.as_ref(), args, env, operation, deadline)
}

/// Runs `docker <args>` as [`run`] does, with `input` on its standard
/// input, such as a compose file that `docker compose --file -` reads,
/// and stops it with [`Error::Cancelled`] once `cancel` is set.
pub(super) fn run_input<I, S, E>(
    args: I,
    env: E,
    input: Vec<u8>,
    operation: &'static str,
    deadline: Duration,
    cancel: &Cancel,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: ChildEnv,
{
    run_full(
        DOCKER.as_ref(),
        args,
        env,
        Some(input),
        operation,
        Until {
            deadline,
            cancel: Some(cancel),
        },
    )
}

/// Runs `docker <args>` with `deadline` and no added variables, and stops
/// it with [`Error::Cancelled`] once `cancel` is set.
pub(super) fn run_until<I, S>(
    args: I,
    operation: &'static str,
    deadline: Duration,
    cancel: &Cancel,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_program_until(DOCKER.as_ref(), args, operation, deadline, cancel)
}

/// Runs `<program> <args>`, where `program` is the `docker` program, as
/// [`run_until`] does.
pub(super) fn run_program_until<I, S>(
    program: &OsStr,
    args: I,
    operation: &'static str,
    deadline: Duration,
    cancel: &Cancel,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_full(
        program,
        args,
        ComposeEnv::default(),
        None,
        operation,
        Until {
            deadline,
            cancel: Some(cancel),
        },
    )
}

/// How long a docker command may run: until its deadline passes, or until
/// its caller cancels.
struct Until<'a> {
    /// The time the command may take.
    deadline: Duration,
    /// The flag that stops the command, or `None`.
    cancel: Option<&'a Cancel>,
}

/// The longest a command waits before it checks its cancel flag again.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Runs `<program> <args>`, where `program` is the `docker` program, as
/// [`run`] does.
pub(super) fn run_program<I, S, E>(
    program: &OsStr,
    args: I,
    env: E,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: ChildEnv,
{
    run_full(
        program,
        args,
        env,
        None,
        operation,
        Until {
            deadline,
            cancel: None,
        },
    )
}

/// Runs `<program> <args>` as [`run_program`] does, with `input` on its
/// standard input, or with no input when `input` is `None`, until `until`
/// ends it. It is the only place where `env`'s values leave their
/// [`ChildEnv`].
fn run_full<I, S, E>(
    program: &OsStr,
    args: I,
    env: E,
    input: Option<Vec<u8>>,
    operation: &'static str,
    until: Until<'_>,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: ChildEnv,
{
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
    )]
    let mut command = Command::new(program);
    command.args(args).env("LC_ALL", "C");
    env.set_on(&mut command);
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/phase/src/engine/container/command.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(Error::Missing { source });
        }
        Err(source) => {
            return Err(Error::unexpected(operation, None, source.to_string()));
        }
    };
    // The input is written on a thread of its own, so a child that writes
    // before it reads never blocks on a full pipe.
    let feeder = child.stdin.take().zip(input).map(|(mut stdin, input)| {
        thread::spawn(move || {
            let _ = stdin.write_all(&input);
        })
    });
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let Until { deadline, cancel } = until;
    let began = now();
    let waited = loop {
        let left = deadline.saturating_sub(began.elapsed());
        let slice = match cancel {
            Some(_) => left.min(CANCEL_POLL),
            None => left,
        };
        match child.wait_timeout(slice) {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if cancel.is_some_and(Cancel::is_cancelled) => break Err(true),
            Ok(None) if !left.is_zero() && began.elapsed() < deadline => {}
            Ok(None) | Err(_) => break Err(false),
        }
    };
    let status = match waited {
        Ok(status) => status,
        Err(cancelled) => {
            // The child is killed and reaped whatever the wait reported, so
            // no zombie remains.
            let _ = child.kill();
            let _ = child.wait();
            if let Some(feeder) = feeder {
                let _ = feeder.join();
            }
            if cancelled {
                return Err(Error::Cancelled { operation });
            }
            return Err(Error::timed_out(
                operation,
                format!(
                    "docker did not finish within {} seconds, so mxv stopped it",
                    deadline.as_secs()
                ),
            ));
        }
    };
    if let Some(feeder) = feeder {
        let _ = feeder.join();
    }
    Ok(Finished {
        status,
        stdout: joined(stdout),
        stderr: joined(stderr),
    })
}

/// Runs `docker <args>` with [`DOCKER_DEADLINE`] and no added variables.
pub(super) fn quick<I, S>(args: I, operation: &'static str) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    quick_with(DOCKER.as_ref(), args, operation)
}

/// Runs `<program> <args>`, where `program` is the `docker` program, as
/// [`quick`] does.
pub(super) fn quick_with<I, S>(
    program: &OsStr,
    args: I,
    operation: &'static str,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_program(
        program,
        args,
        ComposeEnv::default(),
        operation,
        DOCKER_DEADLINE,
    )
}

/// Reads `stream` on a thread of its own: the first [`MOST_OUTPUT_BYTES`]
/// bytes into a buffer, and the rest into nothing, so docker never blocks
/// on a full pipe.
fn drain<R: io::Read + Send + 'static>(mut stream: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut stream).take(MOST_OUTPUT_BYTES).read_to_end(&mut kept);
        let _ = io::copy(&mut stream, &mut io::sink());
        kept
    })
}

/// Returns the text that a drain thread read.
fn joined(handle: Option<thread::JoinHandle<Vec<u8>>>) -> String {
    let bytes = handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Checks that docker is installed and that its daemon answers.
///
/// # Errors
///
/// Returns [`Error::Missing`] when no `docker` program is on `PATH`,
/// and [`Error::Unavailable`] when the daemon does not answer.
pub(crate) fn check() -> Result<(), Error> {
    check_with(DOCKER.as_ref())
}

/// Checks that the `docker` program `program` runs and that its daemon
/// answers, as [`check`] does.
pub(super) fn check_with(program: &OsStr) -> Result<(), Error> {
    let finished = quick_with(
        program,
        ["version", "--format", "{{.Server.Version}}"],
        "version",
    )?;
    if finished.status.success() && !finished.stdout.trim().is_empty() {
        Ok(())
    } else {
        Err(Error::Unavailable {
            message: message(&finished.stderr),
            docker_line: docker_line(&finished.stderr),
        })
    }
}

/// Checks that docker's `compose` plugin is installed.
///
/// # Errors
///
/// Returns [`Error::ComposeMissing`] when `docker compose version`
/// fails, and [`Error::Missing`].
pub(crate) fn check_compose() -> Result<(), Error> {
    let finished = quick(["compose", "version"], "compose version")?;
    if finished.status.success() {
        Ok(())
    } else {
        Err(Error::ComposeMissing {
            message: message(&finished.stderr),
        })
    }
}

/// Runs a container process's probe commands inside its container with
/// `docker exec`, which gives the command the container's own environment.
/// When a command times out or is cancelled, phase kills the local
/// `docker` client, and the command inside the container may keep running.
#[derive(Debug)]
pub(crate) struct DockerExec {
    /// The container.
    pub(crate) id: ContainerId,
}

impl ProbeExec for DockerExec {
    fn exec(&self, run: &[String], timeout: Duration, cancel: &Cancel) -> io::Result<CommandEnd> {
        if run.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the probe's command is empty",
            ));
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
        )]
        let mut command = Command::new(DOCKER);
        command
            .args(["exec", "--", self.id.as_str()])
            .args(run)
            .env("LC_ALL", "C");
        run_bounded(command, timeout, cancel)
    }
}

/// One line that a streamed docker command printed: standard output's when
/// the flag is false, standard error's when it is true.
type StreamedLine = (bool, String);

/// How many lines the readers of [`stream_with_ticks`] may hold for the
/// calling thread. A reader whose line does not fit waits, so a slow `event`
/// slows docker down instead of growing phase's memory. Each line holds at
/// most [`MOST_LINE_BYTES`], so the queue holds at most 16 MiB. Each of the
/// two readers holds one more line while it waits, and the calling thread
/// holds the line it passes, so a stream holds about 19 MiB at most.
const STREAMED_LINES: usize = 16;

/// The most bytes of one line that [`stream_with_ticks`] passes at once. A
/// longer line comes out in parts of at most this many bytes, as the host
/// log follower splits one. A part never ends inside a UTF-8 character:
/// the bytes of a character that the bound cuts start the next part.
pub(crate) const MOST_LINE_BYTES: usize = 1024 * 1024;

/// Runs `<program> <args>` as [`stream_with_ticks`] does without ticks,
/// and passes each line to `line` with whether it came from standard
/// error.
#[cfg(test)]
fn stream_program<I, S>(
    program: &OsStr,
    args: I,
    operation: &'static str,
    idle: Option<Duration>,
    line: &mut dyn FnMut(bool, &str) -> std::ops::ControlFlow<()>,
) -> Result<Option<ExitStatus>, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    stream_with_ticks(program, args, operation, idle, None, &mut |event| {
        event.map_or(std::ops::ControlFlow::Continue(()), |(is_stderr, text)| {
            line(is_stderr, text)
        })
    })
}

/// What [`stream_with_ticks`] passes its caller: a line with whether it
/// came from standard error, or `None` when the program printed nothing
/// for the tick.
pub(super) type StreamEvent<'a> = Option<(bool, &'a str)>;

/// Runs `<program> <args>`, where `program` is the `docker` program, with
/// no input, and passes each line it prints to `event` as `Some`, with whether it
/// printed it on standard error, until it exits or `event` breaks, which
/// kills it. It returns how the program exited, or `None` when `event`
/// broke. A line is read as UTF-8, with each byte that is not replaced,
/// and without its line ending. With `idle`, the program is killed when it
/// prints nothing for that long, as docker does when its daemon hangs. A
/// follow of a container's log passes no `idle`, because it lasts as long
/// as the container runs or the reader reads.
///
/// With `tick`, it also passes `None` to `event` each time the program
/// prints nothing for that long, so the caller can act on a quiet stream,
/// such as by passing a line that it held back.
///
/// `event` runs on the calling thread only, so it is never called while a
/// read of either stream blocks: each stream is read on a thread of its
/// own, which hands the calling thread complete lines.
///
/// The program stays in the caller's process group, so a signal that the
/// terminal sends to the foreground group, such as SIGINT from Ctrl-C,
/// reaches it too and it ends with the caller. When `event` breaks, the
/// idle deadline passes or `event` panics, phase kills the program by its
/// process id, reaps it, and waits at most [`READER_WAIT`] for both
/// readers to end. `docker logs` starts no program of its own, so killing
/// it ends everything the stream started. A `docker` wrapper that does not
/// exec leaves its child running with the pipes after the wrapper is
/// killed. phase then leaves a reader that has not ended. The reader ends
/// at the container's next line, because nothing receives it, and the
/// child ends when it writes the line after that to the closed pipe.
///
/// # Errors
///
/// Returns [`Error::Missing`] when `program` does not exist,
/// [`Error::Failed`] with the timed-out cause when the program was idle for
/// `idle`, and [`Error::Failed`] when it cannot start or cannot be waited
/// for.
pub(super) fn stream_with_ticks<I, S>(
    program: &OsStr,
    args: I,
    operation: &'static str,
    idle: Option<Duration>,
    tick: Option<Duration>,
    event: &mut dyn FnMut(StreamEvent<'_>) -> std::ops::ControlFlow<()>,
) -> Result<Option<ExitStatus>, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
    )]
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/container/command.rs is the module that runs docker (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/phase/src/engine/container/command.rs holds the child and owns its cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(Error::Missing { source });
        }
        Err(source) => return Err(Error::failed(operation, None, source.to_string())),
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel::<StreamedLine>(STREAMED_LINES);
    let readers: Vec<thread::JoinHandle<()>> = [
        child
            .stdout
            .take()
            .map(|out| read_lines(out, false, sender.clone())),
        child
            .stderr
            .take()
            .map(|err| read_lines(err, true, sender.clone())),
    ]
    .into_iter()
    .flatten()
    .collect();
    // Only the readers hold senders now, so the receiver ends once both
    // streams have closed.
    drop(sender);
    let mut streaming = Streaming {
        child,
        receiver: Some(receiver),
        readers,
        finished: false,
    };
    let mut broke = false;
    let mut idled = false;
    if let Some(receiver) = &streaming.receiver {
        use std::sync::mpsc::RecvTimeoutError;
        let mut last_line = now();
        loop {
            let wait = [
                idle.map(|idle| idle.saturating_sub(last_line.elapsed())),
                tick,
            ]
            .into_iter()
            .flatten()
            .min();
            let received = match wait {
                Some(wait) => receiver.recv_timeout(wait),
                None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            let passed = match received {
                Ok((is_stderr, text)) => {
                    last_line = now();
                    event(Some((is_stderr, &text)))
                }
                Err(RecvTimeoutError::Timeout) => {
                    if idle.is_some_and(|idle| last_line.elapsed() >= idle) {
                        idled = true;
                        break;
                    }
                    event(None)
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if passed.is_break() {
                broke = true;
                break;
            }
        }
    }
    match streaming.finish(broke || idled) {
        _ if idled => Err(Error::timed_out(
            operation,
            format!(
                "docker printed nothing for {} seconds, so phase stopped it",
                idle.unwrap_or_default().as_secs()
            ),
        )),
        Ok(_) if broke => Ok(None),
        Ok(status) => Ok(Some(status)),
        Err(source) => Err(Error::failed(operation, None, source.to_string())),
    }
}

/// The program that [`stream_with_ticks`] runs, with the readers of its
/// streams, which [`Streaming::finish`] ends, or dropping it ends when the
/// caller unwinds.
struct Streaming {
    /// The program, which shares the caller's process group.
    #[expect(
        clippy::disallowed_types,
        reason = "crates/phase/src/engine/container/command.rs holds the child and owns its cleanup (PROC-1)"
    )]
    child: std::process::Child,
    /// The lines that the readers pass, until `finish` drops it.
    receiver: Option<std::sync::mpsc::Receiver<StreamedLine>>,
    /// The readers of standard output and standard error.
    readers: Vec<thread::JoinHandle<()>>,
    /// Whether `finish` has run.
    finished: bool,
}

impl Streaming {
    /// Kills the program with SIGKILL when `kill`, reaps the program,
    /// drops the receiver so that a reader that waits to send gives up, and
    /// joins the readers that end within [`READER_WAIT`]. The program stays
    /// unreaped until it is killed, so its process id is not reused in
    /// between. A program whose streams both closed is reaped without a
    /// kill, so its exit status is its own. A reader that is still blocked,
    /// because a child of the program holds its pipe, is left to end when
    /// that child closes the pipe.
    fn finish(&mut self, kill: bool) -> io::Result<ExitStatus> {
        self.finished = true;
        if kill {
            let _ = self.child.kill();
        }
        let waited = self.child.wait();
        drop(self.receiver.take());
        let started = now();
        while self.readers.iter().any(|reader| !reader.is_finished())
            && started.elapsed() < READER_WAIT
        {
            pause(READER_POLL);
        }
        for reader in self.readers.drain(..) {
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
        waited
    }
}

/// The longest that [`Streaming::finish`] waits for the readers once the
/// program is reaped.
const READER_WAIT: Duration = Duration::from_secs(1);

/// How often [`Streaming::finish`] checks whether the readers have ended.
const READER_POLL: Duration = Duration::from_millis(10);

impl Drop for Streaming {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish(true);
        }
    }
}

/// Reads `stream` on a thread of its own and sends each line, without its
/// line ending, to `sender`, marked with `is_stderr`. A line longer than
/// [`MOST_LINE_BYTES`] goes in parts. The thread ends when the stream
/// closes or the receiver is gone.
fn read_lines<R: io::Read + Send + 'static>(
    stream: R,
    is_stderr: bool,
    sender: std::sync::mpsc::SyncSender<StreamedLine>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        use std::io::BufRead as _;
        let mut reader = io::BufReader::new(stream);
        let mut bytes = Vec::new();
        // The start of a UTF-8 character that the bound cut from the last
        // part, which starts the next one.
        let mut carried = Vec::new();
        loop {
            bytes.clear();
            bytes.append(&mut carried);
            let room = MOST_LINE_BYTES - bytes.len();
            let room = u64::try_from(room).unwrap_or(u64::MAX);
            match (&mut reader).take(room).read_until(b'\n', &mut bytes) {
                Ok(0) if bytes.is_empty() => return,
                Ok(_) => {}
                Err(_) => return,
            }
            if bytes.len() >= MOST_LINE_BYTES && bytes.last() != Some(&b'\n') {
                carried = bytes.split_off(cut_character_start(&bytes));
            }
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if sender.send((is_stderr, text)).is_err() {
                return;
            }
        }
    })
}

/// Returns where the UTF-8 character that `bytes` ends inside starts, or
/// the length of `bytes` when it ends with a whole character or with bytes
/// that are not UTF-8.
fn cut_character_start(bytes: &[u8]) -> usize {
    let from = bytes.len().saturating_sub(3);
    let Some((lead, &byte)) = bytes
        .iter()
        .enumerate()
        .skip(from)
        .rev()
        .find(|(_, byte)| *byte & 0b1100_0000 != 0b1000_0000)
    else {
        return bytes.len();
    };
    let width = match byte {
        0b1100_0000..=0b1101_1111 => 2,
        0b1110_0000..=0b1110_1111 => 3,
        0b1111_0000..=0b1111_0111 => 4,
        _ => 1,
    };
    if lead + width > bytes.len() && lead > 0 {
        lead
    } else {
        bytes.len()
    }
}

#[cfg(test)]
mod tests;
