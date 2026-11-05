//! The containers and volumes that the engine acts on, and the handles
//! that prove it checked their labels.
//!
//! An [`OwnedContainer`] or [`OwnedVolume`] exists only after a call here
//! inspected the object and found that its `mxv.store`, `mxv.workspace`
//! and `mxv.container` or `mxv.volume` labels name the scope and the
//! object. That call is the only place that fails with
//! [`Error::Foreign`]. Stopping, removing, deleting and following
//! the log of an object take its handle, so nothing outside this module
//! acts on an object whose labels it did not check.
//!
//! An anonymous volume, which docker creates for an image's `VOLUME` path,
//! carries no label that names a scope. An [`AnonymousVolume`] therefore
//! comes from the inspection of the [`OwnedContainer`] that mounts it.
//! After the container is removed, the workspace's volume record, which
//! the engine writes only from such values, is what names the volume, and
//! [`inspect_anonymous`] checks that docker still labels it anonymous.

use std::io::{self, Read as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::container::ContainerError as Error;
use crate::engine::container::command::{DOCKER, DOCKER_DEADLINE, quick, run, run_until};
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::{
    CONTAINER_LABEL, ContainerId, Scope, ServiceName, VOLUME_LABEL, VolumeKey, VolumeName,
};

/// The label that docker gives the anonymous volumes of an image's
/// `VOLUME` paths.
const ANONYMOUS_LABEL: &str = "com.docker.volume.anonymous";

/// How long a readiness probe waits for a published port to close a
/// connection, which tells a port that nothing inside the container
/// listens on from one that something does.
const PROBE_WAIT: Duration = Duration::from_millis(300);

/// Returns the Go template that prints the labels at `path`, which differs
/// between containers and other objects, as one line of JSON for
/// [`Scope::owns`]. JSON escapes every tab and newline in a label's value,
/// so no value can reach another field or line.
fn labels_format(path: &str) -> String {
    format!("{{{{json {path}}}}}")
}

/// What `docker container inspect` reports about a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerState {
    /// The container runs, or is paused or restarting.
    Running,
    /// docker created the container and has not started it.
    Created,
    /// The container has stopped, with the exit code that docker reports.
    Exited(i32),
    /// docker printed a status that phase does not know, such as
    /// `removing`, or an exit code that is not a number, so phase cannot
    /// tell whether the container runs.
    Unknown,
}

impl ContainerState {
    /// Returns the state that docker prints as `<status> <exit code>`. A
    /// `dead` container is one that docker failed to remove after it
    /// stopped, so it has exited.
    pub(crate) fn parse(printed: &str) -> Self {
        let mut fields = printed.split_whitespace();
        match (fields.next(), fields.next(), fields.next()) {
            (Some("running" | "paused" | "restarting"), _, None) => Self::Running,
            (Some("created"), _, None) => Self::Created,
            (Some("exited" | "dead"), Some(code), None) => {
                code.parse().map_or(Self::Unknown, Self::Exited)
            }
            _ => Self::Unknown,
        }
    }
}

/// Docker's exact status of a container, where phase's container runner
/// needs more than [`ContainerState`] says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    /// The container runs.
    Running,
    /// Docker is restarting the container after it exited.
    Restarting,
    /// Any other status, such as `paused`, `created` or `exited`.
    Other,
}

impl Status {
    /// Returns the status that docker prints as `<status> <exit code>`.
    fn parse(printed: &str) -> Self {
        match printed.split_whitespace().next() {
            Some("running") => Self::Running,
            Some("restarting") => Self::Restarting,
            _ => Self::Other,
        }
    }
}

/// A container whose labels name its scope and service, as an inspection
/// found them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedContainer {
    /// The scope that the labels name.
    scope: Scope,
    /// The service that the labels name.
    service: ServiceName,
    /// The container's id.
    id: ContainerId,
}

impl OwnedContainer {
    /// Returns the service that the container's labels name.
    #[must_use]
    pub const fn service(&self) -> &ServiceName {
        &self.service
    }

    /// Returns the container's id.
    pub(crate) const fn id(&self) -> &ContainerId {
        &self.id
    }
}

/// What an inspection of an owned container reported.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Inspected {
    /// The container.
    pub(crate) container: OwnedContainer,
    /// Its state.
    pub state: ContainerState,
    /// Its exact status.
    pub(crate) status: Status,
    /// How many times Docker has restarted it under its restart policy.
    pub(crate) restarts: u32,
    /// When it last started, as docker prints it, for `docker logs
    /// --since`.
    pub(crate) started: String,
    /// The volumes that it mounts and that carry no name of the scope,
    /// which are the anonymous volumes of its image.
    pub(crate) anonymous: Vec<AnonymousVolume>,
}

/// A named volume of the scope whose labels name the scope and the
/// volume, as an inspection found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnedVolume {
    /// The volume's name.
    name: VolumeName,
}

/// An anonymous volume that an owned container mounts, as the container's
/// inspection found it, or that the workspace's volume record names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AnonymousVolume {
    /// The volume's name, which docker chose.
    name: VolumeName,
}

impl AnonymousVolume {
    /// Returns the volume's name.
    pub(crate) const fn name(&self) -> &VolumeName {
        &self.name
    }
}

/// A volume of a workspace that a removal may delete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Volume {
    /// A named volume of the scope.
    Named(OwnedVolume),
    /// An anonymous volume of one of the scope's containers.
    Anonymous(AnonymousVolume),
}

impl Volume {
    /// Returns the volume's name.
    pub(crate) const fn name(&self) -> &VolumeName {
        match self {
            Self::Named(volume) => &volume.name,
            Self::Anonymous(volume) => &volume.name,
        }
    }
}

/// Returns the Go template of `docker container inspect` that prints what
/// [`mint_container`] reads.
fn container_format() -> String {
    format!(
        "{}\n{{{{.Id}}}}\n{{{{.State.Status}}}} {{{{.State.ExitCode}}}}\n{{{{.State.StartedAt}}}}\n{{{{range .Mounts}}}}{{{{if eq .Type \"volume\"}}}}{{{{.Name}}}} {{{{end}}}}{{{{end}}}}\n{{{{.RestartCount}}}}",
        labels_format(".Config.Labels")
    )
}

/// Returns the container of `service` in `scope` that `docker container
/// inspect` of `target` printed as `printed`, in the format of
/// [`container_format`].
///
/// # Errors
///
/// Returns [`Error::Foreign`] when the labels do not name `scope` and
/// `service`, and [`Error::Failed`] when docker printed no container
/// id.
pub(super) fn mint_container(
    scope: &Scope,
    service: &ServiceName,
    target: &str,
    printed: &str,
) -> Result<Inspected, Error> {
    let mut lines = printed.lines();
    if !scope.owns(
        lines.next().unwrap_or_default(),
        CONTAINER_LABEL,
        service.as_str(),
    ) {
        return Err(Error::Foreign {
            kind: "container",
            name: target.to_owned(),
        });
    }
    let printed_id = lines.next().unwrap_or_default().trim();
    let id = ContainerId::new(printed_id).map_err(|_| Error::Failed {
        operation: "container inspect",
        container: Some(service.as_str().to_owned()),
        message: format!("docker printed {printed_id:?} as the container's id"),
    })?;
    let printed_state = lines.next().unwrap_or_default();
    let state = ContainerState::parse(printed_state);
    let status = Status::parse(printed_state);
    let started = lines.next().unwrap_or_default().trim().to_owned();
    let prefix = scope.volume_prefix();
    let anonymous = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .filter(|volume| !volume.starts_with(&prefix))
        .filter_map(|volume| VolumeName::new(volume).ok())
        .map(|name| AnonymousVolume { name })
        .collect();
    let restarts = lines
        .next()
        .and_then(|line| line.trim().parse().ok())
        .unwrap_or_default();
    Ok(Inspected {
        container: OwnedContainer {
            scope: scope.clone(),
            service: service.clone(),
            id,
        },
        state,
        status,
        restarts,
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

/// Returns the seconds that `docker stop` waits after the stop signal for
/// `container`, as it was created with, or `None` when it has Docker's
/// default or docker does not say.
///
/// Docker omits `StopTimeout` from a container's config when it was
/// created without one, and a template that names the missing key fails.
/// The config is therefore read as JSON, apart from [`container_format`].
pub(crate) fn stop_timeout(container: &OwnedContainer) -> Option<u64> {
    let finished = quick(
        [
            "container",
            "inspect",
            "--format",
            "{{json .Config}}",
            "--",
            container.id.as_str(),
        ],
        "container inspect",
    )
    .ok()?;
    if !finished.status.success() {
        return None;
    }
    stop_timeout_of(&finished.stdout)
}

/// Returns the `StopTimeout` of a container's config, which docker printed
/// as JSON in `printed`.
pub(super) fn stop_timeout_of(printed: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(printed)
        .ok()?
        .get("StopTimeout")?
        .as_u64()
}

/// Inspects the container with the id `id`, which mxv recorded as the
/// container of `service` in `scope`, or returns `None` when docker no
/// longer has it.
///
/// # Errors
///
/// Returns [`Error::Foreign`] when the container's labels do not name
/// `scope` and `service`, and the errors of `docker version` and
/// [`Error::Failed`].
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
/// Returns [`Error::Foreign`] when the container's labels no longer name
/// its scope and service, the errors of checking that docker answers, and
/// [`Error::Failed`].
pub fn reinspect(container: &OwnedContainer) -> Result<Option<Inspected>, Error> {
    inspect_id(&container.scope, &container.service, &container.id)
}

/// Stops `container` with SIGTERM, then SIGKILL after `grace`. A container
/// that docker no longer has counts as stopped.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`Error::Failed`].
pub(crate) fn stop(container: &OwnedContainer, grace: Duration) -> Result<(), Error> {
    let seconds = stop_seconds(grace);
    let stopped = run(
        stop_args(seconds, container),
        ComposeEnv::default(),
        "stop",
        Duration::from_secs(seconds) + DOCKER_DEADLINE,
    )?;
    if !stopped.missing() {
        stopped.checked("stop", Some(container.service.as_str()))?;
    }
    Ok(())
}

/// Stops `container` as [`stop`] does, and stops waiting for docker with
/// [`Error::Cancelled`] once `cancel` is set.
///
/// # Errors
///
/// Returns the errors of [`stop`] and [`Error::Cancelled`].
pub(crate) fn stop_until(
    container: &OwnedContainer,
    grace: Duration,
    cancel: &Cancel,
) -> Result<(), Error> {
    let seconds = stop_seconds(grace);
    let stopped = run_until(
        stop_args(seconds, container),
        "stop",
        Duration::from_secs(seconds) + DOCKER_DEADLINE,
        cancel,
    )?;
    if !stopped.missing() {
        stopped.checked("stop", Some(container.service.as_str()))?;
    }
    Ok(())
}

/// Pulls `image` unless docker already has it, within `deadline`, and
/// stops the pull with [`Error::Cancelled`] once `cancel` is set.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`Error::Failed`] when the
/// pull fails or misses its deadline.
pub(crate) fn ensure_image(image: &str, deadline: Duration, cancel: &Cancel) -> Result<(), Error> {
    let found = quick(
        ["image", "inspect", "--format", "{{.Id}}", "--", image],
        "image inspect",
    )?;
    if !found.missing() {
        found.checked("image inspect", None)?;
        return Ok(());
    }
    run_until(["pull", "--quiet", "--", image], "pull", deadline, cancel)?.checked("pull", None)?;
    Ok(())
}

/// Returns the whole seconds that `docker stop --time` gives a container
/// for `wait`: `wait` rounded up, and at least one second, so a wait under
/// a second never stops the container at once.
pub(crate) fn stop_seconds(wait: Duration) -> u64 {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    seconds.max(1)
}

/// Returns the arguments of `docker stop` of `container` that waits
/// `seconds` after the stop signal.
fn stop_args(seconds: u64, container: &OwnedContainer) -> [String; 5] {
    [
        "stop".to_owned(),
        "--time".to_owned(),
        seconds.to_string(),
        "--".to_owned(),
        container.id.as_str().to_owned(),
    ]
}

/// Removes `container`. Its named and anonymous volumes stay. A container
/// that docker no longer has counts as removed.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`Error::Failed`].
pub(crate) fn remove(container: OwnedContainer) -> Result<(), Error> {
    let removed = quick(["rm", "--force", "--", container.id.as_str()], "rm")?;
    if !removed.missing() {
        removed.checked("rm", Some(container.service.as_str()))?;
    }
    Ok(())
}

/// The Go template of `docker volume inspect` that prints `anonymous` for
/// a volume with docker's [`ANONYMOUS_LABEL`].
fn anonymous_format() -> String {
    format!(
        "{{{{range $key, $value := .Labels}}}}{{{{if eq $key \"{ANONYMOUS_LABEL}\"}}}}anonymous{{{{end}}}}{{{{end}}}}"
    )
}

/// Runs `docker volume inspect` of `name` with `format`, and returns what
/// it printed, or `None` when the volume does not exist.
fn inspect_volume_with(name: &VolumeName, format: &str) -> Result<Option<String>, Error> {
    let finished = quick(
        ["volume", "inspect", "--format", format, "--", name.as_str()],
        "volume inspect",
    )?;
    if finished.missing() {
        return Ok(None);
    }
    finished.checked("volume inspect", None).map(Some)
}

/// Returns the named volume `name` of `scope` that `docker volume inspect`
/// printed as `printed`, with the labels' format of a volume.
///
/// # Errors
///
/// Returns [`Error::Foreign`] when `name` is not a name of the scope,
/// or its labels do not name the scope and the volume.
pub(super) fn mint_volume(
    scope: &Scope,
    name: &VolumeName,
    printed: &str,
) -> Result<OwnedVolume, Error> {
    let owned = name
        .as_str()
        .strip_prefix(&scope.volume_prefix())
        .is_some_and(|volume| scope.owns(printed.trim_end_matches('\n'), VOLUME_LABEL, volume));
    if owned {
        Ok(OwnedVolume { name: name.clone() })
    } else {
        Err(Error::Foreign {
            kind: "volume",
            name: name.as_str().to_owned(),
        })
    }
}

/// Returns the anonymous volume `name` that `docker volume inspect`
/// printed as `printed`, in the format of [`anonymous_format`].
///
/// # Errors
///
/// Returns [`Error::Foreign`] when docker does not label the volume
/// anonymous.
pub(super) fn mint_anonymous(name: &VolumeName, printed: &str) -> Result<AnonymousVolume, Error> {
    if printed.trim() == "anonymous" {
        Ok(AnonymousVolume { name: name.clone() })
    } else {
        Err(Error::Foreign {
            kind: "volume",
            name: name.as_str().to_owned(),
        })
    }
}

/// Inspects the named volume `name` and returns it as a volume of `scope`,
/// or `None` when it does not exist.
///
/// # Errors
///
/// Returns [`Error::Foreign`] as [`mint_volume`] describes, and the
/// errors of `docker version` and [`Error::Failed`].
pub(crate) fn inspect_volume(
    scope: &Scope,
    name: &VolumeName,
) -> Result<Option<OwnedVolume>, Error> {
    let format = labels_format(".Labels");
    inspect_volume_with(name, &format)?
        .map(|printed| mint_volume(scope, name, &printed))
        .transpose()
}

/// Inspects the volume `name`, which an [`AnonymousVolume`] named when the
/// workspace's volume record was written, and returns it when docker still
/// has it and labels it anonymous, or `None` when it does not exist.
///
/// # Errors
///
/// Returns [`Error::Foreign`] as [`mint_anonymous`] describes, and
/// the errors of `docker version` and [`Error::Failed`].
pub(super) fn inspect_anonymous(name: &VolumeName) -> Result<Option<AnonymousVolume>, Error> {
    inspect_volume_with(name, &anonymous_format())?
        .map(|printed| mint_anonymous(name, &printed))
        .transpose()
}

/// Inspects `volume` again, and returns it when it still exists and is the
/// scope's, or `None` when it does not exist.
///
/// # Errors
///
/// Returns the errors of [`inspect_volume`] and [`inspect_anonymous`].
pub(crate) fn reinspect_volume(scope: &Scope, volume: &Volume) -> Result<Option<Volume>, Error> {
    Ok(match volume {
        Volume::Named(named) => inspect_volume(scope, &named.name)?.map(Volume::Named),
        Volume::Anonymous(anonymous) => inspect_anonymous(&anonymous.name)?.map(Volume::Anonymous),
    })
}

/// Returns the named volume `name` of `scope`, after creating it with the
/// scope's labels for the volume `key` when it does not exist. The volume
/// is inspected again after its creation, because `docker volume create`
/// succeeds for a volume that another program created meanwhile.
///
/// # Errors
///
/// Returns [`Error::Foreign`] when a volume with the name lacks the
/// scope's labels, and the errors of `docker version` and
/// [`Error::Failed`].
pub(crate) fn ensure_volume(scope: &Scope, key: &VolumeKey) -> Result<OwnedVolume, Error> {
    let name = scope.volume_name(key);
    if let Some(volume) = inspect_volume(scope, &name)? {
        return Ok(volume);
    }
    let mut args = vec!["volume".to_owned(), "create".to_owned()];
    if let serde_json::Value::Object(labels) = scope.labels(VOLUME_LABEL, key.as_str()) {
        for (label, value) in labels {
            args.push("--label".to_owned());
            args.push(format!("{label}={}", value.as_str().unwrap_or_default()));
        }
    }
    args.extend(["--".to_owned(), name.as_str().to_owned()]);
    quick(args, "volume create")?.checked("volume create", None)?;
    inspect_volume(scope, &name)?.ok_or_else(|| {
        Error::failed(
            "volume create",
            None,
            format!("docker created the volume {name} and no longer has it"),
        )
    })
}

/// Deletes `volume`. A volume that no longer exists counts as deleted.
///
/// # Errors
///
/// Returns the errors of `docker version` and
/// [`Error::Failed`].
pub(crate) fn delete_volume(volume: Volume) -> Result<(), Error> {
    let finished = quick(["volume", "rm", "--", volume.name().as_str()], "volume rm")?;
    if !finished.missing() {
        finished.checked("volume rm", None)?;
    }
    Ok(())
}

/// Returns the arguments of the `docker logs` that follows `container`
/// from `since`, when docker last started it, or from its first line
/// without `since`. The host starts the follower, which ends when the
/// container stops.
pub(crate) fn follow_args(container: &OwnedContainer, since: Option<&str>) -> Vec<String> {
    let mut argv = vec![DOCKER.to_owned(), "logs".to_owned(), "--follow".to_owned()];
    if let Some(since) = since {
        argv.extend(["--since".to_owned(), since.to_owned()]);
    }
    argv.extend(["--".to_owned(), container.id.as_str().to_owned()]);
    argv
}

/// Reports whether something inside a container answers on the host port
/// `port`. Docker's port proxy accepts a connection to a published port
/// before anything in the container listens, and then closes it at once,
/// so the probe counts the port as answering only when a connection stays
/// open for 300 ms or the container sends data.
#[must_use]
pub fn answers(port: u16) -> bool {
    answers_within(port, PROBE_WAIT + PROBE_WAIT, &Cancel::new())
}

/// Reports whether something inside a container answers on the host port
/// `port`, as [`answers`] describes, within `timeout`. A connection that
/// stays open until `timeout` ends, when that comes before 300 ms, counts
/// as answering. It reports no answer once `cancel` is set.
pub(crate) fn answers_within(port: u16, timeout: Duration, cancel: &Cancel) -> bool {
    let began = now();
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, PROBE_WAIT.min(timeout)) else {
        return false;
    };
    let hold = PROBE_WAIT.min(timeout.saturating_sub(began.elapsed()));
    let held = now();
    let mut byte = [0_u8; 1];
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        let left = hold.saturating_sub(held.elapsed());
        if left.is_zero() {
            return true;
        }
        if stream
            .set_read_timeout(Some(left.min(ANSWER_POLL)))
            .is_err()
        {
            return false;
        }
        match stream.read(&mut byte) {
            Ok(read) => return read > 0,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return false,
        }
    }
}

/// The longest that [`answers_within`] reads before it checks its cancel
/// flag again.
const ANSWER_POLL: Duration = Duration::from_millis(50);
