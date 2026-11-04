//! The docker backend of a manifest's containers, which runs the `docker`
//! program found on `PATH` and its `compose` plugin.
//!
//! One docker daemon can serve several mxv state directories, such as two
//! people's or two test runs', and each of them may have a workspace with
//! the same name. Each state directory therefore has a store id, a random
//! 32-digit hex number that [`crate::Runs`] keeps in it, and a [`Scope`]
//! names one workspace of one store.
//!
//! The containers of one workspace form one Docker Compose project, named
//! `mxv-<id8>-<workspace>`, where `<id8>` is the first 8 digits of the store
//! id. A compose project name allows only lowercase letters, digits, `-`
//! and `_`, so the workspace's `_` is written `__` and its `.` is written
//! `_d`, which keeps two workspace names from sharing a project. Each
//! manifest container is one service of the project. mxv writes the
//! project's compose file into the workspace's run directory, and starts
//! one service at a time with `docker compose up`. The compose file names
//! each environment variable and refers to its value by a variable of the
//! `docker compose` process, so the values exist only in that process's
//! environment and in the container.
//!
//! The container of the service `<name>` is named `<project>.<name>`, its
//! named volume `<volume>` is `<project>.<volume>`, and Compose names the
//! project's network `<project>_default`. Neither the project nor a
//! manifest name holds a `.`, so the part after the `.` is always the
//! container or volume. Every container, volume and network carries three
//! labels: `mxv.store=<id>`, `mxv.workspace=<name>`, and
//! `mxv.container=<name>`, `mxv.volume=<name>` or `mxv.network=default`.
//! Compose adds labels of its own.
//!
//! mxv records the id of each container it started and the name of each
//! volume. Before it stops, removes, adopts or reuses a container, volume
//! or network, it reads the object's labels, and it fails with
//! [`Error::DockerForeign`] when they do not name this store, workspace and
//! object. mxv never touches such an object.
//!
//! Each docker command runs with a deadline, after which mxv kills it and
//! fails with [`Error::DockerFailed`].

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, Read as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value, json};
use wait_timeout::ChildExt as _;

use crate::error::Error;
use crate::manifest::Container;
use crate::name::WorkspaceName;

#[cfg(test)]
mod tests;

/// The program that this backend runs.
const DOCKER: &str = "docker";

/// The deadline of `docker compose up`, which may pull the image first.
pub const PULL_DEADLINE: Duration = Duration::from_secs(600);

/// The deadline of every other docker command, apart from `docker stop`,
/// whose deadline is its grace plus this.
pub const DOCKER_DEADLINE: Duration = Duration::from_secs(30);

/// The most bytes of each output stream that a docker command keeps.
const MOST_OUTPUT_BYTES: u64 = 1024 * 1024;

/// The label that names the store id of an object's state directory.
pub const STORE_LABEL: &str = "mxv.store";

/// The label that names an object's workspace.
pub const WORKSPACE_LABEL: &str = "mxv.workspace";

/// The label that names a container's name in the manifest.
pub const CONTAINER_LABEL: &str = "mxv.container";

/// The label that names a volume's name in the manifest.
pub const VOLUME_LABEL: &str = "mxv.volume";

/// The label that names a project's network, whose value is `default`.
pub const NETWORK_LABEL: &str = "mxv.network";

/// The compose name of the project's one network.
const NETWORK: &str = "default";

/// The number of digits of the store id that project names hold.
const SHORT_ID: usize = 8;

/// How long a readiness probe waits for a published port to close a
/// connection, which tells a port that nothing inside the container
/// listens on from one that something does.
const PROBE_WAIT: Duration = Duration::from_millis(300);

/// One workspace of one store: its compose project and the labels and
/// names of its containers, volumes and network.
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

    /// Returns the name of the scope's compose project,
    /// `mxv-<id8>-<workspace>`, with the workspace's `_` written `__` and
    /// its `.` written `_d`.
    #[must_use]
    pub fn project(&self) -> String {
        let short = self.store.get(..SHORT_ID).unwrap_or(&self.store);
        let workspace = self
            .workspace
            .as_str()
            .replace('_', "__")
            .replace('.', "_d");
        format!("mxv-{short}-{workspace}")
    }

    /// Returns the prefix of every container and volume name of the
    /// scope, `<project>.`.
    fn prefix(&self) -> String {
        format!("{}.", self.project())
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

    /// Returns the name of the project's network, which Compose gives it.
    #[must_use]
    pub fn network_name(&self) -> String {
        format!("{}_{NETWORK}", self.project())
    }

    /// Returns the labels of the scope's object `object` under `label`.
    fn labels(&self, label: &str, object: &str) -> Value {
        json!({
            STORE_LABEL: self.store,
            WORKSPACE_LABEL: self.workspace.as_str(),
            label: object,
        })
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

impl ContainerState {
    /// Returns the state that docker prints as `<status> <exit code>`.
    pub(crate) fn parse(printed: &str) -> Self {
        let printed = printed.trim();
        let (status, code) = printed.split_once(' ').unwrap_or((printed, "0"));
        match status {
            "running" | "paused" | "restarting" => Self::Running,
            "created" => Self::Created,
            _ => Self::Exited(code.trim().parse().unwrap_or_default()),
        }
    }
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
/// object label, such as `mxv.container`, and `%l` for the path of the
/// labels, which differs between containers and other objects.
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
            && [
                "No such container",
                "No such volume",
                "no such volume",
                "No such object",
                "No such network",
                "not found",
            ]
            .iter()
            .any(|said| self.stderr.contains(said))
    }

    /// Returns standard output when the command succeeded. When it failed,
    /// it checks whether the daemon answers, and returns the failure of
    /// `operation` on `container`.
    fn checked(self, operation: &'static str, container: Option<&str>) -> Result<String, Error> {
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
fn run<I, S>(
    args: I,
    env: &[(String, String)],
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
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
        .envs(env.iter().map(|(name, value)| (name, value)))
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

/// Runs `docker <args>` with [`DOCKER_DEADLINE`] and no added variables.
fn quick<I, S>(args: I, operation: &'static str) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run(args, &[], operation, DOCKER_DEADLINE)
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

/// Returns the container `container` of `scope` that `target`, its id or
/// its name, names, or `None` when no container has it.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] when the container's labels do not name
/// `scope` and `container`, and the errors of [`check`] and
/// [`Error::DockerFailed`].
pub(crate) fn inspect(
    scope: &Scope,
    container: &str,
    target: &str,
) -> Result<Option<Inspected>, Error> {
    let format = format!(
        "{}\n{{{{.Id}}}}\n{{{{.State.Status}}}} {{{{.State.ExitCode}}}}\n{{{{.State.StartedAt}}}}\n{{{{range .Mounts}}}}{{{{if eq .Type \"volume\"}}}}{{{{.Name}}}} {{{{end}}}}{{{{end}}}}",
        labels_format(".Config.Labels", CONTAINER_LABEL)
    );
    let finished = quick(
        ["container", "inspect", "--format", &format, "--", target],
        "container inspect",
    )?;
    if finished.missing() {
        return Ok(None);
    }
    let printed = finished.checked("container inspect", Some(container))?;
    let mut lines = printed.lines();
    if !scope.owns(lines.next().unwrap_or_default(), container) {
        return Err(Error::DockerForeign {
            kind: "container",
            name: target.to_owned(),
        });
    }
    let id = lines.next().unwrap_or_default().trim().to_owned();
    let state = ContainerState::parse(lines.next().unwrap_or_default());
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

/// Writes `$` as `$$`, which Compose reads back as `$`, so it interpolates
/// nothing in `text`.
fn literal(text: &str) -> String {
    text.replace('$', "$$")
}

/// Returns the variable of the `docker compose` process that holds the
/// value of the `variable`th variable of the `service`th service, in the
/// order of the manifest's containers and of their variables.
fn variable_of(service: usize, variable: usize) -> String {
    format!("MXV_ENV_{service}_{variable}")
}

/// Returns the compose file of `scope` for `containers`, the manifest's
/// containers, whose ports `ports` holds by port name. A port that `ports`
/// lacks is not published. The file holds no value of a container's
/// variables: each variable refers to a variable of the `docker compose`
/// process, which [`compose_env`] returns.
pub(crate) fn compose_file(
    scope: &Scope,
    containers: &BTreeMap<String, Container>,
    ports: &BTreeMap<String, u16>,
) -> Value {
    let mut services = Map::new();
    let mut volumes = Map::new();
    for (service, (name, container)) in containers.iter().enumerate() {
        let environment: Map<String, Value> = container
            .env
            .keys()
            .enumerate()
            .map(|(variable, key)| {
                (
                    key.clone(),
                    Value::String(format!("${{{}:-}}", variable_of(service, variable))),
                )
            })
            .collect();
        let published: Vec<Value> = container
            .ports
            .iter()
            .filter_map(|published| {
                ports.get(&published.port).map(|host| {
                    Value::String(format!("127.0.0.1:{host}:{}/tcp", published.container))
                })
            })
            .collect();
        let mounts: Vec<Value> = container
            .volumes
            .iter()
            .map(|(volume, path)| {
                json!({"type": "volume", "source": volume, "target": literal(path)})
            })
            .collect();
        for volume in container.volumes.keys() {
            volumes.insert(
                volume.clone(),
                json!({
                    "name": scope.volume_name(volume),
                    "labels": scope.labels(VOLUME_LABEL, volume),
                }),
            );
        }
        let mut definition = Map::new();
        definition.insert("image".to_owned(), Value::String(literal(&container.image)));
        definition.insert(
            "container_name".to_owned(),
            Value::String(scope.container_name(name)),
        );
        definition.insert("labels".to_owned(), scope.labels(CONTAINER_LABEL, name));
        if let Some(command) = &container.command {
            definition.insert(
                "command".to_owned(),
                command.iter().map(|argument| literal(argument)).collect(),
            );
        }
        definition.insert("environment".to_owned(), Value::Object(environment));
        definition.insert("ports".to_owned(), Value::Array(published));
        definition.insert("volumes".to_owned(), Value::Array(mounts));
        services.insert(name.clone(), Value::Object(definition));
    }
    json!({
        "services": services,
        "volumes": volumes,
        "networks": {
            NETWORK: {"labels": scope.labels(NETWORK_LABEL, NETWORK)},
        },
    })
}

/// Returns the variables of the `docker compose` process that start the
/// container `name` of `containers`: the values of the container's
/// variables, under the names that [`compose_file`] refers to.
pub(crate) fn compose_env(
    containers: &BTreeMap<String, Container>,
    name: &str,
) -> Vec<(String, String)> {
    containers
        .iter()
        .enumerate()
        .filter(|(_, (service, _))| service.as_str() == name)
        .flat_map(|(service, (_, container))| {
            container
                .env
                .values()
                .enumerate()
                .map(move |(variable, value)| (variable_of(service, variable), value.clone()))
        })
        .collect()
}

/// Checks that the volume `volume` of `scope` does not exist or carries
/// the scope's labels, so Compose may create or mount it.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] for a volume with the name and other
/// labels, and the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn check_volume(scope: &Scope, volume: &str) -> Result<(), Error> {
    match volume_owned(scope, &scope.volume_name(volume))? {
        Some(false) => Err(Error::DockerForeign {
            kind: "volume",
            name: scope.volume_name(volume),
        }),
        Some(true) | None => Ok(()),
    }
}

/// Reports whether the volume `name` exists and is the scope's: `None`
/// when it does not exist, `Some(true)` when it is a named volume of the
/// scope with its labels or an anonymous volume, which carries no name of
/// the scope, and `Some(false)` for a volume with a name of the scope and
/// other labels.
fn volume_owned(scope: &Scope, name: &str) -> Result<Option<bool>, Error> {
    let format = labels_format(".Labels", VOLUME_LABEL);
    let finished = quick(
        ["volume", "inspect", "--format", &format, "--", name],
        "volume inspect",
    )?;
    if finished.missing() {
        return Ok(None);
    }
    let labels = finished.checked("volume inspect", None)?;
    let owned = match name.strip_prefix(&scope.prefix()) {
        Some(volume) => scope.owns(labels.trim_end_matches('\n'), volume),
        None => true,
    };
    Ok(Some(owned))
}

/// Reports whether the volume `name`, which mxv recorded, exists and is
/// the scope's, as [`volume_owned`] describes.
///
/// # Errors
///
/// Returns the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn volume_exists(scope: &Scope, name: &str) -> Result<bool, Error> {
    Ok(volume_owned(scope, name)? == Some(true))
}

/// Starts the service `name` of the compose file `file` with `docker
/// compose up`, with the variables `env` that [`compose_env`] returned.
/// The project directory is `dir`, which holds no other file that Compose
/// reads.
///
/// The caller has checked the compose plugin with [`check_compose`].
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] when a container with the name of the service lacks the scope's labels, the
/// errors of [`check`], and [`Error::DockerFailed`] when docker cannot
/// pull the image or start the container.
pub(crate) fn compose_up(
    scope: &Scope,
    dir: &Path,
    file: &Path,
    name: &str,
    env: &[(String, String)],
) -> Result<(), Error> {
    let project = scope.project();
    let args: [&OsStr; 13] = [
        "compose".as_ref(),
        "--project-name".as_ref(),
        project.as_ref(),
        "--project-directory".as_ref(),
        dir.as_os_str(),
        "--file".as_ref(),
        file.as_os_str(),
        "up".as_ref(),
        "--detach".as_ref(),
        "--no-deps".as_ref(),
        "--quiet-pull".as_ref(),
        "--".as_ref(),
        name.as_ref(),
    ];
    let finished = run(args, env, "compose up", PULL_DEADLINE)?;
    if !finished.status.success() && finished.stderr.contains("Conflict") {
        // Another program created a container with the name after mxv
        // removed its own, so the name is foreign.
        inspect(scope, name, &scope.container_name(name))?;
    }
    finished.checked("compose up", Some(name))?;
    Ok(())
}

/// Stops the container `container` of `scope` that `target`, its id or its
/// name, names, with SIGTERM, then SIGKILL after `grace`, and removes it.
/// Its named and anonymous volumes stay. It returns the container's
/// anonymous volumes, or `None` when no container has `target`.
///
/// # Errors
///
/// Returns the errors of [`inspect`], [`check`] and [`Error::DockerFailed`].
pub(crate) fn remove(
    scope: &Scope,
    container: &str,
    target: &str,
    grace: Duration,
) -> Result<Option<Vec<String>>, Error> {
    let Some(inspected) = inspect(scope, container, target)? else {
        return Ok(None);
    };
    let seconds = grace.as_secs().to_string();
    let stopped = run(
        ["stop", "--time", &seconds, "--", &inspected.id],
        &[],
        "stop",
        grace + DOCKER_DEADLINE,
    )?;
    if !stopped.missing() {
        stopped.checked("stop", Some(container))?;
    }
    let removed = quick(["rm", "--force", "--", &inspected.id], "rm")?;
    if !removed.missing() {
        removed.checked("rm", Some(container))?;
    }
    Ok(Some(inspected.anonymous))
}

/// Removes the network of `scope` after it checked its labels. A network
/// that does not exist, or that a container still uses, stays.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] for a network with the name and other
/// labels, the errors of [`check`], and [`Error::DockerFailed`].
pub(crate) fn remove_network(scope: &Scope) -> Result<(), Error> {
    let name = scope.network_name();
    let format = labels_format(".Labels", NETWORK_LABEL);
    let inspected = quick(
        ["network", "inspect", "--format", &format, "--", &name],
        "network inspect",
    )?;
    if inspected.missing() {
        return Ok(());
    }
    let labels = inspected.checked("network inspect", None)?;
    if !scope.owns(labels.trim_end_matches('\n'), NETWORK) {
        return Err(Error::DockerForeign {
            kind: "network",
            name,
        });
    }
    let removed = quick(["network", "rm", "--", &name], "network rm")?;
    if removed.missing() || removed.stderr.contains("active endpoints") {
        return Ok(());
    }
    removed.checked("network rm", None)?;
    Ok(())
}

/// Deletes the volumes `names` of `scope`, after it checked the labels of
/// each named volume. A volume that no longer exists counts as deleted.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] for a named volume with other labels,
/// the errors of [`check`] and [`Error::DockerFailed`].
pub(crate) fn remove_volumes(scope: &Scope, names: &[String]) -> Result<(), Error> {
    for name in names {
        match volume_owned(scope, name)? {
            None => continue,
            Some(false) => {
                return Err(Error::DockerForeign {
                    kind: "volume",
                    name: name.clone(),
                });
            }
            Some(true) => {}
        }
        let finished = quick(["volume", "rm", "--", name], "volume rm")?;
        if !finished.missing() {
            finished.checked("volume rm", None)?;
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
