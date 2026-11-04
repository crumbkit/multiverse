//! The docker backend of a manifest's containers, which runs the `docker`
//! program found on `PATH`.
//!
//! One docker daemon can serve several mxv state directories, such as two
//! people's or two test runs', and each of them may have a workspace with
//! the same name. Each state directory therefore has a store id, a random
//! 32-digit hex number that [`crate::Runs`] keeps in it, and a [`Scope`]
//! names one workspace of one store. Every container and volume that mxv
//! creates carries three labels: `mxv.store=<id>`, `mxv.workspace=<name>`,
//! and `mxv.container=<name>` or `mxv.volume=<name>`. A container is named
//! `mxv-<id8>-<workspace>.<name>` and a volume `mxv-<id8>-<workspace>.<volume>`,
//! where `<id8>` is the first 8 digits of the store id. A workspace name
//! may hold `-` and `.`, and a container or volume name holds no `.`, so
//! the part after the last `.` is always the container or volume.
//!
//! mxv lists containers and volumes only with filters on both the store
//! and the workspace label. Before it stops, removes, adopts or reuses an
//! object that it found by name, it reads the object's labels, and it
//! fails with [`Error::DockerForeign`] when they do not name this store,
//! workspace and object. mxv never touches such an object.
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

/// The label that names the store id of a container's or a volume's
/// state directory.
pub const STORE_LABEL: &str = "mxv.store";

/// The label that names a container's or a volume's workspace.
pub const WORKSPACE_LABEL: &str = "mxv.workspace";

/// The label that names a container's name in the manifest.
pub const CONTAINER_LABEL: &str = "mxv.container";

/// The label that names a volume's name in the manifest.
pub const VOLUME_LABEL: &str = "mxv.volume";

/// The number of digits of the store id that container and volume names
/// hold.
const SHORT_ID: usize = 8;

/// How long a readiness probe waits for a published port to close a
/// connection, which tells a port that nothing inside the container
/// listens on from one that something does.
const PROBE_WAIT: Duration = Duration::from_millis(300);

/// One workspace of one store: the labels and names of its containers and
/// volumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The store id.
    store: String,
    /// The workspace.
    workspace: WorkspaceName,
}

impl Scope {
    /// Returns the scope of the workspace `workspace` in the store with the
    /// id `store`.
    #[must_use]
    pub fn new(store: &str, workspace: &WorkspaceName) -> Self {
        Self {
            store: store.to_owned(),
            workspace: workspace.clone(),
        }
    }

    /// Returns the scope's workspace.
    #[must_use]
    pub fn workspace(&self) -> &WorkspaceName {
        &self.workspace
    }

    /// Returns the prefix of every container and volume name of the
    /// scope, `mxv-<id8>-<workspace>.`.
    fn prefix(&self) -> String {
        let short = self.store.get(..SHORT_ID).unwrap_or(&self.store);
        format!("mxv-{short}-{}.", self.workspace.as_str())
    }

    /// Returns the name of the container `container`.
    #[must_use]
    pub fn container_name(&self, container: &str) -> String {
        format!("{}{container}", self.prefix())
    }

    /// Returns the name of the volume `volume`.
    #[must_use]
    pub fn volume_name(&self, volume: &str) -> String {
        format!("{}{volume}", self.prefix())
    }

    /// Returns the `docker ps` and `docker volume ls` filters that match
    /// the scope's objects.
    fn filters(&self) -> [String; 4] {
        [
            "--filter".to_owned(),
            format!("label={STORE_LABEL}={}", self.store),
            "--filter".to_owned(),
            format!("label={WORKSPACE_LABEL}={}", self.workspace.as_str()),
        ]
    }

    /// Reports whether `labels`, as [`LABELS_FORMAT`] prints them, name
    /// this scope and the object `object`.
    fn owns(&self, labels: &str, object: &str) -> bool {
        let mut fields = labels.split('\t');
        fields.next() == Some(self.store.as_str())
            && fields.next() == Some(self.workspace.as_str())
            && fields.next() == Some(object)
    }
}

/// What `docker container inspect` reports about a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContainerState {
    /// The container runs, or is paused or restarting.
    Running,
    /// docker created the container and has not started it.
    Created,
    /// The container has stopped, with the exit code that docker reports.
    Exited(i32),
}

/// A container of the scope that docker inspected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inspected {
    /// The container's id, which names it even if another container takes
    /// its name later.
    pub(crate) id: String,
    /// Its state.
    pub(crate) state: ContainerState,
    /// When it last started, as docker prints it, for `docker logs
    /// --since`.
    pub(crate) started: String,
    /// The volumes that it mounts and that carry no name of the scope,
    /// which are the anonymous volumes of its image.
    pub(crate) anonymous: Vec<String>,
}

/// The Go template that prints an object's store, workspace and object
/// labels, separated by tabs, for [`Scope::owns`]. `%s` stands for the
/// object label, `mxv.container` or `mxv.volume`, and `%l` for the path
/// of the labels, which differs between containers and volumes.
const LABELS_FORMAT: &str =
    r#"{{index %l "mxv.store"}}	{{index %l "mxv.workspace"}}	{{index %l "%s"}}"#;

/// Returns [`LABELS_FORMAT`] for the labels at `path` and the object label
/// `label`.
fn labels_format(path: &str, label: &str) -> String {
    LABELS_FORMAT.replace("%l", path).replace("%s", label)
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
    /// of `operation` on the object `container`.
    fn ok(self, operation: &'static str, container: Option<&str>) -> Result<String, Error> {
        if self.status.success() {
            Ok(self.stdout)
        } else {
            Err(failed(operation, container, &self.stderr))
        }
    }

    /// Reports whether docker said that the object does not exist.
    fn missing(&self) -> bool {
        !self.status.success()
            && (self.stderr.contains("No such container")
                || self.stderr.contains("No such volume")
                || self.stderr.contains("no such volume")
                || self.stderr.contains("No such object"))
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

/// Runs `docker <args>` and returns its standard output. When docker
/// fails, it checks whether the daemon answers, so a daemon that does not
/// answer fails as [`Error::DockerUnavailable`].
fn output<I, S>(args: I, operation: &'static str, container: Option<&str>) -> Result<String, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let finished = run(args, operation, DOCKER_DEADLINE)?;
    if !finished.status.success() {
        check()?;
    }
    finished.ok(operation, container)
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

/// Returns the lines of `text` that are not empty, trimmed, in name order
/// and without repeats.
fn names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
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
            message: message(&finished.stderr),
        })
    }
}

/// Returns the container `container` of `scope`, or `None` when no
/// container has its name.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] when a container has the name and its
/// labels do not name `scope` and `container`, and the errors of [`check`]
/// and [`Error::DockerFailed`].
pub(crate) fn inspect(scope: &Scope, container: &str) -> Result<Option<Inspected>, Error> {
    let name = scope.container_name(container);
    let format = format!(
        "{}\n{{{{.Id}}}}\n{{{{.State.Status}}}} {{{{.State.ExitCode}}}}\n{{{{.State.StartedAt}}}}\n{{{{range .Mounts}}}}{{{{if eq .Type \"volume\"}}}}{{{{.Name}}}} {{{{end}}}}{{{{end}}}}",
        labels_format(".Config.Labels", CONTAINER_LABEL)
    );
    let finished = run(
        ["container", "inspect", "--format", &format, "--", &name],
        "container inspect",
        DOCKER_DEADLINE,
    )?;
    if finished.missing() {
        return Ok(None);
    }
    if !finished.status.success() {
        check()?;
    }
    let printed = finished.ok("container inspect", Some(container))?;
    let mut lines = printed.lines();
    if !scope.owns(lines.next().unwrap_or_default(), container) {
        return Err(Error::DockerForeign {
            kind: "container",
            name,
        });
    }
    let id = lines.next().unwrap_or_default().trim().to_owned();
    let state = lines.next().unwrap_or_default().trim();
    let (status, code) = state.split_once(' ').unwrap_or((state, "0"));
    let state = match status {
        "running" | "paused" | "restarting" => ContainerState::Running,
        "created" => ContainerState::Created,
        _ => ContainerState::Exited(code.parse().unwrap_or_default()),
    };
    let started = lines.next().unwrap_or_default().trim().to_owned();
    let prefix = scope.prefix();
    let anonymous = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .filter(|volume| !volume.starts_with(&prefix))
        .map(str::to_owned)
        .collect();
    Ok(Some(Inspected {
        id,
        state,
        started,
        anonymous,
    }))
}

/// Returns the manifest names of the containers of `scope` that exist,
/// running or stopped, in name order.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn listed(scope: &Scope) -> Result<Vec<String>, Error> {
    let format = format!("{{{{.Label \"{CONTAINER_LABEL}\"}}}}");
    let mut args = vec!["ps".to_owned(), "--all".to_owned()];
    args.extend(scope.filters());
    args.extend(["--format".to_owned(), format]);
    Ok(names(&output(args, "ps", None)?))
}

/// The arguments of one `docker run`.
pub(crate) struct Run<'a> {
    /// The workspace and store.
    pub(crate) scope: &'a Scope,
    /// The container's name in the manifest.
    pub(crate) name: &'a str,
    /// The container as the manifest declares it.
    pub(crate) container: &'a Container,
    /// The host port of each published port, in the order of
    /// [`Container::ports`].
    pub(crate) host_ports: &'a [u16],
}

/// Returns the `--label` arguments that name `scope` and the object
/// `object` under the label `label`.
fn label_args(scope: &Scope, label: &str, object: &str) -> [String; 6] {
    [
        "--label".to_owned(),
        format!("{STORE_LABEL}={}", scope.store),
        "--label".to_owned(),
        format!("{WORKSPACE_LABEL}={}", scope.workspace.as_str()),
        "--label".to_owned(),
        format!("{label}={object}"),
    ]
}

/// Returns the arguments of `docker run` for `spec`.
pub(crate) fn run_args(spec: &Run<'_>) -> Vec<String> {
    let mut args = vec![
        "run".to_owned(),
        "--detach".to_owned(),
        "--name".to_owned(),
        spec.scope.container_name(spec.name),
    ];
    args.extend(label_args(spec.scope, CONTAINER_LABEL, spec.name));
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
            spec.scope.volume_name(volume)
        ));
    }
    args.push("--".to_owned());
    args.push(spec.container.image.clone());
    if let Some(command) = &spec.container.command {
        args.extend(command.iter().cloned());
    }
    args
}

/// Creates the volume `volume` of `scope` with its labels when it does not
/// exist, and checks that the volume with its name carries them.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] for a volume with the name and other
/// labels, and the errors of [`check`] and [`Error::DockerFailed`].
fn ensure_volume(scope: &Scope, volume: &str) -> Result<(), Error> {
    let name = scope.volume_name(volume);
    let format = labels_format(".Labels", VOLUME_LABEL);
    let inspected = run(
        ["volume", "inspect", "--format", &format, "--", &name],
        "volume inspect",
        DOCKER_DEADLINE,
    )?;
    if inspected.missing() {
        let mut args = vec!["volume".to_owned(), "create".to_owned()];
        args.extend(label_args(scope, VOLUME_LABEL, volume));
        args.extend(["--".to_owned(), name.clone()]);
        output(args, "volume create", None)?;
        // `volume create` succeeds for a volume that another program
        // created meanwhile, without its labels, so the labels are read
        // again.
        return ensure_volume(scope, volume);
    }
    if !inspected.status.success() {
        check()?;
    }
    let labels = inspected.ok("volume inspect", None)?;
    if scope.owns(labels.trim_end_matches('\n'), volume) {
        Ok(())
    } else {
        Err(Error::DockerForeign {
            kind: "volume",
            name,
        })
    }
}

/// Creates the volumes of `spec` that do not exist, with the scope's
/// labels, then starts the container.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] when a volume or a container with a
/// name of the scope lacks its labels, the errors of [`check`], and
/// [`Error::DockerFailed`] when docker cannot create a volume, pull the
/// image or start the container.
pub(crate) fn start(spec: &Run<'_>) -> Result<(), Error> {
    check()?;
    for volume in spec.container.volumes.keys() {
        ensure_volume(spec.scope, volume)?;
    }
    let finished = run(run_args(spec), "run", PULL_DEADLINE)?;
    if !finished.status.success() && finished.stderr.contains("Conflict") {
        // Another program created a container with the name after mxv
        // removed its own, so the name is foreign.
        inspect(spec.scope, spec.name)?;
    }
    finished.ok("run", Some(spec.name))?;
    Ok(())
}

/// Stops the container `container` of `scope` with SIGTERM, then SIGKILL
/// after `grace`, and removes it. Its named and anonymous volumes stay.
/// It returns the container's anonymous volumes, or `None` when no
/// container has its name.
///
/// # Errors
///
/// Returns the errors of [`inspect`], [`check`] and [`Error::DockerFailed`].
pub(crate) fn remove(
    scope: &Scope,
    container: &str,
    grace: Duration,
) -> Result<Option<Vec<String>>, Error> {
    let Some(inspected) = inspect(scope, container)? else {
        return Ok(None);
    };
    let seconds = grace.as_secs().to_string();
    let stopped = run(
        ["stop", "--time", &seconds, "--", &inspected.id],
        "stop",
        grace + DOCKER_DEADLINE,
    )?;
    if !stopped.missing() {
        if !stopped.status.success() {
            check()?;
        }
        stopped.ok("stop", Some(container))?;
    }
    let removed = run(
        ["rm", "--force", "--", &inspected.id],
        "rm",
        DOCKER_DEADLINE,
    )?;
    if !removed.missing() {
        if !removed.status.success() {
            check()?;
        }
        removed.ok("rm", Some(container))?;
    }
    Ok(Some(inspected.anonymous))
}

/// Returns the volumes of `scope` that docker lists, in name order.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn volumes(scope: &Scope) -> Result<Vec<String>, Error> {
    let mut args = vec!["volume".to_owned(), "ls".to_owned(), "--quiet".to_owned()];
    args.extend(scope.filters());
    Ok(names(&output(args, "volume ls", None)?))
}

/// Deletes the volumes `names`. A volume that no longer exists counts as
/// deleted.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn remove_volumes(names: &[String]) -> Result<(), Error> {
    for name in names {
        let finished = run(["volume", "rm", "--", name], "volume rm", DOCKER_DEADLINE)?;
        if !finished.missing() {
            if !finished.status.success() {
                check()?;
            }
            finished.ok("volume rm", None)?;
        }
    }
    Ok(())
}

/// Returns the arguments of the `docker logs` that follows the container
/// with the id `id` from `since`, when docker last started it, or from its
/// first line without `since`.
pub(crate) fn follow_args(id: &str, since: Option<&str>) -> Vec<String> {
    let mut argv = vec![DOCKER.to_owned(), "logs".to_owned(), "--follow".to_owned()];
    if let Some(since) = since {
        argv.extend(["--since".to_owned(), since.to_owned()]);
    }
    argv.extend(["--".to_owned(), id.to_owned()]);
    argv
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
