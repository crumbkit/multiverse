//! The containers and volumes that mxv acts on, and the handles that prove
//! it checked their labels.
//!
//! An [`OwnedContainer`] or [`OwnedVolume`] exists only after a call here
//! inspected the object and found that its `mxv.store`, `mxv.workspace`
//! and `mxv.container` or `mxv.volume` labels name the scope and the
//! object. That call is the only place that fails with
//! [`Error::DockerForeign`]. Stopping, removing, deleting and following
//! the log of an object take its handle, so nothing outside this module
//! acts on an object whose labels it did not check.

use std::io::{self, Read as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use crate::docker::command::{DOCKER, DOCKER_DEADLINE, quick, run};
use crate::docker::compose::ComposeEnv;
use crate::docker::names::{
    CONTAINER_LABEL, ContainerId, Scope, ServiceName, VOLUME_LABEL, VolumeName,
};
use crate::error::Error;
use crate::process::{self, Launch, Launched};

/// The label that docker gives the anonymous volumes of an image's
/// `VOLUME` paths.
const ANONYMOUS_LABEL: &str = "com.docker.volume.anonymous";

/// How long a readiness probe waits for a published port to close a
/// connection, which tells a port that nothing inside the container
/// listens on from one that something does.
const PROBE_WAIT: Duration = Duration::from_millis(300);

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

/// A container whose labels name its scope and service, as an inspection
/// found them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnedContainer {
    /// The scope that the labels name.
    scope: Scope,
    /// The service that the labels name.
    service: ServiceName,
    /// The container's id.
    id: ContainerId,
}

impl OwnedContainer {
    /// Returns the service that the container's labels name.
    pub(crate) const fn service(&self) -> &ServiceName {
        &self.service
    }

    /// Returns the container's id.
    pub(crate) const fn id(&self) -> &ContainerId {
        &self.id
    }
}

/// What an inspection of an owned container reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inspected {
    /// The container.
    pub(crate) container: OwnedContainer,
    /// Its state.
    pub(crate) state: ContainerState,
    /// When it last started, as docker prints it, for `docker logs
    /// --since`.
    pub(crate) started: String,
    /// The volumes that it mounts and that carry no name of the scope,
    /// which are the anonymous volumes of its image.
    pub(crate) anonymous: Vec<VolumeName>,
}

/// A volume that does not carry a name of the scope and carries docker's
/// [`ANONYMOUS_LABEL`], or a named volume of the scope whose labels name
/// the scope and the volume, as an inspection found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnedVolume {
    /// The volume's name.
    name: VolumeName,
}

/// Returns the Go template of `docker container inspect` that prints what
/// [`mint_container`] reads.
fn container_format() -> String {
    format!(
        "{}\n{{{{.Id}}}}\n{{{{.State.Status}}}} {{{{.State.ExitCode}}}}\n{{{{.State.StartedAt}}}}\n{{{{range .Mounts}}}}{{{{if eq .Type \"volume\"}}}}{{{{.Name}}}} {{{{end}}}}{{{{end}}}}",
        labels_format(".Config.Labels", CONTAINER_LABEL)
    )
}

/// Returns the container of `service` in `scope` that `docker container
/// inspect` of `target` printed as `printed`, in the format of
/// [`container_format`].
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] when the labels do not name `scope` and
/// `service`, and [`Error::DockerFailed`] when docker printed no container
/// id.
pub(super) fn mint_container(
    scope: &Scope,
    service: &ServiceName,
    target: &str,
    printed: &str,
) -> Result<Inspected, Error> {
    let mut lines = printed.lines();
    if !scope.owns(lines.next().unwrap_or_default(), service.as_str()) {
        return Err(Error::DockerForeign {
            kind: "container",
            name: target.to_owned(),
        });
    }
    let printed_id = lines.next().unwrap_or_default().trim();
    let id = ContainerId::new(printed_id).map_err(|_| Error::DockerFailed {
        operation: "container inspect",
        container: Some(service.as_str().to_owned()),
        message: format!("docker printed {printed_id:?} as the container's id"),
    })?;
    let state = ContainerState::parse(lines.next().unwrap_or_default());
    let started = lines.next().unwrap_or_default().trim().to_owned();
    let prefix = scope.prefix();
    let anonymous = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .filter(|volume| !volume.starts_with(&prefix))
        .filter_map(|volume| VolumeName::new(volume).ok())
        .collect();
    Ok(Inspected {
        container: OwnedContainer {
            scope: scope.clone(),
            service: service.clone(),
            id,
        },
        state,
        started,
        anonymous,
    })
}

/// Inspects the container that `target`, its id or its name, names, and
/// returns it as the container of `service` in `scope`, or `None` when no
/// container has `target`.
fn inspect(scope: &Scope, service: &ServiceName, target: &str) -> Result<Option<Inspected>, Error> {
    let format = container_format();
    let finished = quick(
        ["container", "inspect", "--format", &format, "--", target],
        "container inspect",
    )?;
    if finished.missing() {
        return Ok(None);
    }
    let printed = finished.checked("container inspect", Some(service.as_str()))?;
    mint_container(scope, service, target, &printed).map(Some)
}

/// Inspects the container with the id `id`, which mxv recorded as the
/// container of `service` in `scope`, or returns `None` when docker no
/// longer has it.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] when the container's labels do not name
/// `scope` and `service`, and the errors of [`super::command::check`] and
/// [`Error::DockerFailed`].
pub(crate) fn inspect_id(
    scope: &Scope,
    service: &ServiceName,
    id: &ContainerId,
) -> Result<Option<Inspected>, Error> {
    inspect(scope, service, id.as_str())
}

/// Inspects the container that has the name of `service` in `scope`, or
/// returns `None` when no container has the name.
///
/// # Errors
///
/// Returns the errors of [`inspect_id`].
pub(crate) fn inspect_named(
    scope: &Scope,
    service: &ServiceName,
) -> Result<Option<Inspected>, Error> {
    inspect(scope, service, scope.container_name(service).as_str())
}

/// Inspects `container` again, or returns `None` when docker no longer has
/// it.
///
/// # Errors
///
/// Returns the errors of [`inspect_id`].
pub(crate) fn reinspect(container: &OwnedContainer) -> Result<Option<Inspected>, Error> {
    inspect_id(&container.scope, &container.service, &container.id)
}

/// Stops `container` with SIGTERM, then SIGKILL after `grace`. A container
/// that docker no longer has counts as stopped.
///
/// # Errors
///
/// Returns the errors of [`super::command::check`] and [`Error::DockerFailed`].
pub(crate) fn stop(container: &OwnedContainer, grace: Duration) -> Result<(), Error> {
    let seconds = grace.as_secs().to_string();
    let stopped = run(
        ["stop", "--time", &seconds, "--", container.id.as_str()],
        ComposeEnv::default(),
        "stop",
        grace + DOCKER_DEADLINE,
    )?;
    if !stopped.missing() {
        stopped.checked("stop", Some(container.service.as_str()))?;
    }
    Ok(())
}

/// Removes `container`. Its named and anonymous volumes stay. A container
/// that docker no longer has counts as removed.
///
/// # Errors
///
/// Returns the errors of [`super::command::check`] and [`Error::DockerFailed`].
pub(crate) fn remove(container: OwnedContainer) -> Result<(), Error> {
    let removed = quick(["rm", "--force", "--", container.id.as_str()], "rm")?;
    if !removed.missing() {
        removed.checked("rm", Some(container.service.as_str()))?;
    }
    Ok(())
}

/// Returns the Go template of `docker volume inspect` that prints what
/// [`mint_volume`] reads for the volume `name` of `scope`.
fn volume_format(scope: &Scope, name: &VolumeName) -> String {
    if name.as_str().starts_with(&scope.prefix()) {
        labels_format(".Labels", VOLUME_LABEL)
    } else {
        format!(
            "{{{{range $key, $value := .Labels}}}}{{{{if eq $key \"{ANONYMOUS_LABEL}\"}}}}anonymous{{{{end}}}}{{{{end}}}}"
        )
    }
}

/// Returns the volume `name` of `scope` that `docker volume inspect`
/// printed as `printed`, in the format of [`volume_format`].
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] for a named volume of the scope whose
/// labels do not name the scope and the volume, and for any other volume
/// without docker's anonymous label.
pub(super) fn mint_volume(
    scope: &Scope,
    name: &VolumeName,
    printed: &str,
) -> Result<OwnedVolume, Error> {
    let owned = match name.as_str().strip_prefix(&scope.prefix()) {
        Some(volume) => scope.owns(printed.trim_end_matches('\n'), volume),
        None => printed.trim() == "anonymous",
    };
    if owned {
        Ok(OwnedVolume { name: name.clone() })
    } else {
        Err(Error::DockerForeign {
            kind: "volume",
            name: name.as_str().to_owned(),
        })
    }
}

/// Inspects the volume `name` and returns it as a volume of `scope`, or
/// `None` when it does not exist.
///
/// # Errors
///
/// Returns [`Error::DockerForeign`] as [`mint_volume`] describes, and the
/// errors of [`super::command::check`] and [`Error::DockerFailed`].
pub(crate) fn inspect_volume(
    scope: &Scope,
    name: &VolumeName,
) -> Result<Option<OwnedVolume>, Error> {
    let format = volume_format(scope, name);
    let finished = quick(
        [
            "volume",
            "inspect",
            "--format",
            &format,
            "--",
            name.as_str(),
        ],
        "volume inspect",
    )?;
    if finished.missing() {
        return Ok(None);
    }
    let printed = finished.checked("volume inspect", None)?;
    mint_volume(scope, name, &printed).map(Some)
}

/// Deletes `volume`. A volume that no longer exists counts as deleted.
///
/// # Errors
///
/// Returns the errors of [`super::command::check`] and [`Error::DockerFailed`].
pub(crate) fn delete_volume(volume: OwnedVolume) -> Result<(), Error> {
    let finished = quick(["volume", "rm", "--", volume.name.as_str()], "volume rm")?;
    if !finished.missing() {
        finished.checked("volume rm", None)?;
    }
    Ok(())
}

/// The files of a log follower.
#[derive(Debug)]
pub(crate) struct FollowFiles<'a> {
    /// The directory that the follower starts in.
    pub(crate) cwd: &'a Path,
    /// The log that the follower appends to.
    pub(crate) log: &'a Path,
    /// The file that the follower's shell writes its exit code to.
    pub(crate) exit_file: &'a Path,
}

/// Returns the arguments of the `docker logs` that follows the container
/// with the id `id` from `since`, when docker last started it, or from its
/// first line without `since`.
pub(super) fn follow_args(id: &ContainerId, since: Option<&str>) -> Vec<String> {
    let mut argv = vec![DOCKER.to_owned(), "logs".to_owned(), "--follow".to_owned()];
    if let Some(since) = since {
        argv.extend(["--since".to_owned(), since.to_owned()]);
    }
    argv.extend(["--".to_owned(), id.as_str().to_owned()]);
    argv
}

/// Starts a detached `docker logs --follow` of `container` from `since`,
/// when docker last started it, or from its first line without `since`,
/// through the process module, with `files`. The follower runs in a
/// process group of its own and ends when the container stops.
///
/// # Errors
///
/// Returns the errors of [`process::launch`].
pub(crate) fn follow_logs(
    container: &OwnedContainer,
    since: Option<&str>,
    files: &FollowFiles<'_>,
) -> Result<Launched, Error> {
    let argv = follow_args(&container.id, since);
    process::launch(&Launch {
        name: container.service.as_str(),
        argv: &argv,
        cwd: files.cwd,
        env: &[],
        log: files.log,
        exit_file: files.exit_file,
    })
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
