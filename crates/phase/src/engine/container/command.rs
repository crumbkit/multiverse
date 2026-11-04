//! Running the `docker` program: the one place in phase that starts it and
//! waits for it, with a deadline, and reads what it printed.

use std::ffi::OsStr;
use std::io::{self, Read as _};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::engine::container::ContainerError as Error;
use crate::engine::container::compose::ComposeEnv;

/// The program that this backend runs.
pub(crate) const DOCKER: &str = "docker";

/// The deadline of `docker compose up`, which may pull the image first.
pub(crate) const PULL_DEADLINE: Duration = Duration::from_secs(600);

/// The deadline of every other docker command, apart from `docker stop`,
/// whose deadline is its grace plus this.
pub(crate) const DOCKER_DEADLINE: Duration = Duration::from_secs(30);

/// The most bytes of each output stream that a docker command keeps.
const MOST_OUTPUT_BYTES: u64 = 1024 * 1024;

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
            Err(Error::failed(operation, container, message(&self.stderr)))
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
            "No such volume",
            "no such volume",
            "No such object",
        ]
        .iter()
        .any(|said| answer.contains(said))
            || (answer.starts_with("network ") && answer.ends_with(" not found"));
    }
    line.to_ascii_lowercase()
        .starts_with("error: no such object:")
}

/// Returns the line of docker's standard error that says what failed: the
/// first line that starts with `docker:` or `Error`, which is where docker
/// puts its own message, and otherwise the last line that is not empty.
pub(crate) fn message(stderr: &str) -> String {
    let mut lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    lines
        .clone()
        .find(|line| line.starts_with("docker:") || line.starts_with("Error"))
        .or_else(|| lines.next_back())
        .unwrap_or_default()
        .to_owned()
}

/// Runs `docker <args>` with `deadline` and the variables `env` on top of
/// the host program's own environment, and returns what it printed and how
/// it exited.
pub(super) fn run<I, S>(
    args: I,
    env: ComposeEnv,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_program(DOCKER.as_ref(), args, env, operation, deadline)
}

/// Runs `<program> <args>`, where `program` is the `docker` program, as
/// [`run`] does. It is the only place where `env`'s values leave their
/// [`ComposeEnv`].
pub(super) fn run_program<I, S>(
    program: &OsStr,
    args: I,
    env: ComposeEnv,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
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
        .envs(env.into_vars())
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
        reason = "crates/phase/src/engine/container/command.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(Error::Missing { source });
        }
        Err(source) => {
            return Err(Error::failed(operation, None, source.to_string()));
        }
    };
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let status = match child.wait_timeout(deadline) {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            // The child is killed and reaped whatever the wait reported, so
            // no zombie remains.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::failed(
                operation,
                None,
                format!(
                    "docker did not finish within {} seconds, so mxv stopped it",
                    deadline.as_secs()
                ),
            ));
        }
    };
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
