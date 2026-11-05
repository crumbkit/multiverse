//! Running the `docker` program: the one place in phase that starts it and
//! waits for it, with a deadline, and reads what it printed.

use std::ffi::OsStr;
use std::io::{self, Read as _, Write as _};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::container::ContainerError as Error;
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::ContainerId;
use crate::engine::probe::{CommandEnd, ProbeExec, run_bounded};
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

/// The variable of the shell that writes a secret into a container which
/// holds the secret's value. The shell hands the value to `docker exec` on
/// standard input, and the `docker` program does not inherit it.
const SECRET_VARIABLE: &str = "MXV_SECRET";

/// The value of one secret of a process's resolved environment, which
/// reaches the shell of [`write_secret`] as [`SECRET_VARIABLE`] and no
/// other variable of the environment.
struct SecretValue<'a> {
    /// The process's resolved environment.
    environment: &'a dyn AppliedEnvironment,
    /// The name of the secret.
    name: &'a str,
}

impl ChildEnv for SecretValue<'_> {
    fn set_on(self, command: &mut Command) {
        command.env_remove(SECRET_VARIABLE);
        let name = self.name;
        self.environment.apply_as(command, &|variable| {
            (variable == name).then(|| SECRET_VARIABLE.to_owned())
        });
    }
}

/// The script of the host's shell that runs `docker` with its arguments
/// and the value of [`SECRET_VARIABLE`] on standard input. `printf` is a
/// built-in command of the shell, so the value is in no argument list,
/// and `docker` starts without the variable. A shell that did not receive
/// the variable fails with status 64 and says so.
const SECRET_FEED: &str = r#"if [ -z "${MXV_SECRET+set}" ]; then
  echo 'the environment source supplied no value for the secret' >&2
  exit 64
fi
printf '%s' "$MXV_SECRET" | { unset MXV_SECRET; exec "$0" "$@"; }
"#;

/// Writes the secret `name` of `environment` into the container `id`
/// with `docker <args>`, where `args` are [`super::secrets::write_args`],
/// within `deadline` or until `cancel` is set. The host's `/bin/sh`
/// feeds the value to `docker exec` on standard input, so it never
/// appears in an argument list, and its error holds what `docker` printed
/// on standard error, which the writer never fills with the value.
pub(super) fn write_secret(
    args: &[String],
    environment: &dyn AppliedEnvironment,
    name: &str,
    deadline: Duration,
    cancel: &Cancel,
) -> Result<Finished, Error> {
    let shell = ["-c", SECRET_FEED, DOCKER]
        .into_iter()
        .map(str::to_owned)
        .chain(args.iter().cloned());
    run_full(
        "/bin/sh".as_ref(),
        shell,
        SecretValue { environment, name },
        None,
        "secret write",
        Until {
            deadline,
            cancel: Some(cancel),
        },
    )
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
            return Err(Error::failed(operation, None, source.to_string()));
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
