//! Running the `docker` program: the one place in mxv that starts it and
//! waits for it, with a deadline, and reads what it printed.

use std::ffi::OsStr;
use std::io::{self, Read as _};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::docker::compose::ComposeEnv;
use crate::error::Error;

/// The program that this backend runs.
pub(super) const DOCKER: &str = "docker";

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
            Err(failed(operation, container, &self.stderr))
        }
    }

    /// Reports whether docker said that the object does not exist.
    pub(super) fn missing(&self) -> bool {
        !self.status.success()
            && [
                "No such container",
                "No such volume",
                "no such volume",
                "No such object",
                "not found",
            ]
            .iter()
            .any(|said| self.stderr.contains(said))
    }

    /// Returns standard output when the command succeeded. When it failed,
    /// it checks whether the daemon answers, and returns the failure of
    /// `operation` on `container`.
    pub(super) fn checked(
        self,
        operation: &'static str,
        container: Option<&str>,
    ) -> Result<String, Error> {
        if !self.status.success() {
            check()?;
        }
        self.ok(operation, container)
    }
}

/// Returns the failure of `operation` on `container` whose standard error
/// is `stderr`.
fn failed(operation: &'static str, container: Option<&str>, stderr: &str) -> Error {
    Error::DockerFailed {
        operation,
        container: container.map(str::to_owned),
        message: message(stderr),
    }
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
/// mxv's own environment, and returns what it printed and how it exited.
/// It is the only place where `env`'s values leave their [`ComposeEnv`].
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
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/docker/command.rs is the module that runs docker (PROC-1)"
    )]
    let mut command = Command::new(DOCKER);
    command
        .args(args)
        .env("LC_ALL", "C")
        .envs(env.into_vars())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/docker/command.rs is the module that runs docker (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/docker/command.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(Error::DockerMissing { source });
        }
        Err(source) => {
            return Err(Error::DockerFailed {
                operation,
                container: None,
                message: source.to_string(),
            });
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
            return Err(Error::DockerFailed {
                operation,
                container: None,
                message: format!(
                    "docker did not finish within {} seconds, so mxv stopped it",
                    deadline.as_secs()
                ),
            });
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
    run(args, ComposeEnv::default(), operation, DOCKER_DEADLINE)
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
/// Returns [`Error::DockerMissing`] when no `docker` program is on `PATH`,
/// and [`Error::DockerUnavailable`] when the daemon does not answer.
pub(crate) fn check() -> Result<(), Error> {
    let finished = quick(["version", "--format", "{{.Server.Version}}"], "version")?;
    if finished.status.success() && !finished.stdout.trim().is_empty() {
        Ok(())
    } else {
        Err(Error::DockerUnavailable {
            message: message(&finished.stderr),
        })
    }
}

/// Checks that docker's `compose` plugin is installed.
///
/// # Errors
///
/// Returns [`Error::DockerComposeMissing`] when `docker compose version`
/// fails, and [`Error::DockerMissing`].
pub(crate) fn check_compose() -> Result<(), Error> {
    let finished = quick(["compose", "version"], "compose version")?;
    if finished.status.success() {
        Ok(())
    } else {
        Err(Error::DockerComposeMissing {
            message: message(&finished.stderr),
        })
    }
}
