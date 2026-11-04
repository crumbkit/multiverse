//! The docker backend of a manifest's containers, which runs the `docker`
//! program found on `PATH`.
//!
//! Every container and volume that mxv creates carries the label
//! `mxv.workspace=<workspace>`, and a container also carries
//! `mxv.container=<name>`, so mxv finds them again without a record of its
//! own. A container is named `mxv-<workspace>.<name>` and a volume
//! `mxv-<workspace>.<volume>`. A workspace name may hold `-` and `.`, and a
//! container or volume name holds neither `.` nor anything after the last
//! `.`, so two workspaces never share a name.
//!
//! Each docker command runs with a deadline, after which mxv kills it and
//! fails with [`Error::DockerFailed`].

use std::ffi::OsStr;
use std::io::{self, Read as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::error::Error;
use crate::manifest::Container;
use crate::name::WorkspaceName;

#[cfg(test)]
mod tests;

/// The program that this backend runs.
const DOCKER: &str = "docker";

/// The deadline of `docker run`, which may pull the image first.
pub const PULL_DEADLINE: Duration = Duration::from_secs(600);

/// The deadline of every other docker command, apart from `docker stop`,
/// whose deadline is its grace plus this.
pub const DOCKER_DEADLINE: Duration = Duration::from_secs(30);

/// The most bytes of each output stream that a docker command keeps.
const MOST_OUTPUT_BYTES: u64 = 1024 * 1024;

/// The label that names a container's or a volume's workspace.
pub const WORKSPACE_LABEL: &str = "mxv.workspace";

/// The label that names a container's name in the manifest.
pub const CONTAINER_LABEL: &str = "mxv.container";

/// How long a readiness probe waits for a published port to close a
/// connection, which tells a port that nothing inside the container
/// listens on from one that something does.
const PROBE_WAIT: Duration = Duration::from_millis(300);

/// What `docker container inspect` reports about a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContainerState {
    /// The container runs, or is paused or restarting.
    Running,
    /// The container has stopped, with its exit code.
    Exited(i32),
}

/// What a finished docker command printed and how it exited.
struct Finished {
    /// The command's exit status.
    status: ExitStatus,
    /// The kept bytes of standard output, as text.
    stdout: String,
    /// The kept bytes of standard error, as text.
    stderr: String,
}

impl Finished {
    /// Returns standard output when the command succeeded, or the failure
    /// of `operation`.
    fn ok(self, operation: &'static str) -> Result<String, Error> {
        if self.status.success() {
            Ok(self.stdout)
        } else {
            Err(Error::DockerFailed {
                operation,
                message: last_line(&self.stderr),
            })
        }
    }
}

/// Returns the last line of `text` that is not empty.
fn last_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Returns the name of the container `container` of the workspace
/// `workspace`.
#[must_use]
pub fn container_name(workspace: &WorkspaceName, container: &str) -> String {
    format!("mxv-{}.{container}", workspace.as_str())
}

/// Returns the name of the volume `volume` of the workspace `workspace`.
#[must_use]
pub fn volume_name(workspace: &WorkspaceName, volume: &str) -> String {
    format!("mxv-{}.{volume}", workspace.as_str())
}

/// Runs `docker <args>` with `deadline`, and returns what it printed and
/// how it exited.
fn run<I, S>(args: I, operation: &'static str, deadline: Duration) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/docker.rs is the module that runs docker (PROC-1)"
    )]
    let mut command = Command::new(DOCKER);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/docker.rs is the module that runs docker (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/docker.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(Error::DockerMissing { source });
        }
        Err(source) => {
            return Err(Error::DockerFailed {
                operation,
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
    let finished = run(
        ["version", "--format", "{{.Server.Version}}"],
        "version",
        DOCKER_DEADLINE,
    )?;
    if finished.status.success() && !finished.stdout.trim().is_empty() {
        Ok(())
    } else {
        Err(Error::DockerUnavailable {
            message: last_line(&finished.stderr),
        })
    }
}

/// Returns the state of the container `name`, or `None` when no container
/// has the name.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn state(name: &str) -> Result<Option<ContainerState>, Error> {
    let finished = run(
        [
            "container",
            "inspect",
            "--format",
            "{{.State.Status}} {{.State.ExitCode}}",
            name,
        ],
        "container inspect",
        DOCKER_DEADLINE,
    )?;
    if !finished.status.success() {
        if finished.stderr.contains("No such container") {
            return Ok(None);
        }
        check()?;
        return Err(Error::DockerFailed {
            operation: "container inspect",
            message: last_line(&finished.stderr),
        });
    }
    let printed = finished.stdout.trim();
    let (status, code) = printed.split_once(' ').unwrap_or((printed, "0"));
    Ok(Some(match status {
        "running" | "paused" | "restarting" => ContainerState::Running,
        _ => ContainerState::Exited(code.parse().unwrap_or_default()),
    }))
}

/// Returns the manifest names of the containers of `workspace` that exist,
/// running or stopped, in name order. When docker is missing or its daemon
/// does not answer, no container of mxv can run, so it returns none.
pub(crate) fn listed(workspace: &WorkspaceName) -> Vec<String> {
    let filter = format!("label={WORKSPACE_LABEL}={}", workspace.as_str());
    let format = format!("{{{{.Label \"{CONTAINER_LABEL}\"}}}}");
    let Ok(printed) = run(
        ["ps", "--all", "--filter", &filter, "--format", &format],
        "ps",
        DOCKER_DEADLINE,
    )
    .and_then(|finished| finished.ok("ps")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = printed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The arguments of one `docker run`.
pub(crate) struct Run<'a> {
    /// The workspace.
    pub(crate) workspace: &'a WorkspaceName,
    /// The container's name in the manifest.
    pub(crate) name: &'a str,
    /// The container as the manifest declares it.
    pub(crate) container: &'a Container,
    /// The host port of each published port, in the order of
    /// [`Container::ports`].
    pub(crate) host_ports: &'a [u16],
}

/// Returns the arguments of `docker run` for `spec`.
pub(crate) fn run_args(spec: &Run<'_>) -> Vec<String> {
    let mut args = vec![
        "run".to_owned(),
        "--detach".to_owned(),
        "--name".to_owned(),
        container_name(spec.workspace, spec.name),
        "--label".to_owned(),
        format!("{WORKSPACE_LABEL}={}", spec.workspace.as_str()),
        "--label".to_owned(),
        format!("{CONTAINER_LABEL}={}", spec.name),
    ];
    for (published, host) in spec.container.ports.iter().zip(spec.host_ports) {
        args.push("--publish".to_owned());
        args.push(format!("127.0.0.1:{host}:{}/tcp", published.container));
    }
    for (variable, value) in &spec.container.env {
        args.push("--env".to_owned());
        args.push(format!("{variable}={value}"));
    }
    for (volume, path) in &spec.container.volumes {
        args.push("--mount".to_owned());
        args.push(format!(
            "type=volume,source={},target={path}",
            volume_name(spec.workspace, volume)
        ));
    }
    args.push("--".to_owned());
    args.push(spec.container.image.clone());
    if let Some(command) = &spec.container.command {
        args.extend(command.iter().cloned());
    }
    args
}

/// Creates the volumes of `spec` that do not exist, with the workspace's
/// label, then starts the container.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`] when docker
/// cannot create a volume, pull the image or start the container.
pub(crate) fn start(spec: &Run<'_>) -> Result<(), Error> {
    check()?;
    let label = format!("{WORKSPACE_LABEL}={}", spec.workspace.as_str());
    for volume in spec.container.volumes.keys() {
        run(
            [
                "volume",
                "create",
                "--label",
                &label,
                &volume_name(spec.workspace, volume),
            ],
            "volume create",
            DOCKER_DEADLINE,
        )?
        .ok("volume create")?;
    }
    run(run_args(spec), "run", PULL_DEADLINE)?.ok("run")?;
    Ok(())
}

/// Stops the container `name` with SIGTERM, then SIGKILL after `grace`,
/// and removes it with its anonymous volumes. Its named volumes stay.
/// Removing a container that does not exist succeeds.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn remove(name: &str, grace: Duration) -> Result<(), Error> {
    let seconds = grace.as_secs().to_string();
    let stopped = run(
        ["stop", "--time", &seconds, name],
        "stop",
        grace + DOCKER_DEADLINE,
    )?;
    if !stopped.status.success() && !stopped.stderr.contains("No such container") {
        check()?;
        return stopped.ok("stop").map(drop);
    }
    let removed = run(["rm", "--force", "--volumes", name], "rm", DOCKER_DEADLINE)?;
    if removed.status.success() || removed.stderr.contains("No such container") {
        Ok(())
    } else {
        removed.ok("rm").map(drop)
    }
}

/// Returns the volumes of `workspace`, in name order. When docker is
/// missing or its daemon does not answer, it returns none.
pub(crate) fn volumes(workspace: &WorkspaceName) -> Vec<String> {
    let filter = format!("label={WORKSPACE_LABEL}={}", workspace.as_str());
    let Ok(printed) = run(
        ["volume", "ls", "--quiet", "--filter", &filter],
        "volume ls",
        DOCKER_DEADLINE,
    )
    .and_then(|finished| finished.ok("volume ls")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = printed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

/// Deletes the volumes `names`.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn remove_volumes(names: &[String]) -> Result<(), Error> {
    if names.is_empty() {
        return Ok(());
    }
    check()?;
    let mut args = vec!["volume".to_owned(), "rm".to_owned(), "--".to_owned()];
    args.extend(names.iter().cloned());
    run(args, "volume rm", DOCKER_DEADLINE)?.ok("volume rm")?;
    Ok(())
}

/// Reports whether something inside a container answers on the host port
/// `port`. Docker's port proxy accepts a connection to a published port
/// before anything in the container listens, and then closes it at once,
/// so the probe counts the port as answering only when a connection stays
/// open for [`PROBE_WAIT`] or the container sends data.
pub(crate) fn answers(port: u16) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE_WAIT) else {
        return false;
    };
    if stream.set_read_timeout(Some(PROBE_WAIT)).is_err() {
        return false;
    }
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(read) => read > 0,
        Err(error) => matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
    }
}
