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
use crate::engine::container::app::{SECRET_FILES, SECRETS_LABEL};
use crate::engine::container::command::{DOCKER, DOCKER_DEADLINE, quick, run, run_until};
use crate::engine::container::compose::ComposeEnv;
use crate::engine::container::names::{
    CONTAINER_LABEL, ContainerId, Scope, ServiceName, VOLUME_LABEL, VolumeKey, VolumeName,
};
use crate::engine::container::secrets::{IMAGE_CONFIG_FORMAT, ImageConfig};

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
    /// The container runs.
    Running,
    /// The container is paused: its processes are frozen and keep their
    /// resources.
    Paused,
    /// The container exited and Docker is restarting it under its restart
    /// policy.
    Restarting,
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
            (Some("running"), _, None) => Self::Running,
            (Some("paused"), _, None) => Self::Paused,
            (Some("restarting"), _, None) => Self::Restarting,
            (Some("created"), _, None) => Self::Created,
            (Some("exited" | "dead"), Some(code), None) => {
                code.parse().map_or(Self::Unknown, Self::Exited)
            }
            _ => Self::Unknown,
        }
    }

    /// Reports whether the container runs or is paused, so it takes a stop
    /// signal. A container that Docker is restarting does not: `docker
    /// stop` first waits out Docker's restart delay.
    #[must_use]
    pub const fn runs(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }

    /// Reports whether the container is up: it runs, is paused, or Docker
    /// is restarting it.
    #[must_use]
    pub const fn is_up(self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::Restarting)
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
    /// How many times Docker has restarted it under its restart policy.
    pub(crate) restarts: u32,
    /// Whether it receives secret files: its labels carry the secrets
    /// label, so it mounts the tmpfs at `/run/secrets` and starts through
    /// phase's wrapper. A container without it has no tmpfs, so it cannot
    /// receive a secret file without being recreated.
    pub(crate) files: bool,
    /// The user that it runs as, in Docker's `USER[:GROUP]` form, which is
    /// empty for root: the app's run-as user, or else the user of its image
    /// when it was created. A later image under the same tag does not
    /// change it. An inspection whose user cannot be read fails with
    /// [`DockerCause::Unexpected`](crate::DockerCause::Unexpected).
    pub(crate) user: String,
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
        "{}\n{{{{.Id}}}}\n{{{{.State.Status}}}} {{{{.State.ExitCode}}}}\n{{{{.State.StartedAt}}}}\n{{{{range .Mounts}}}}{{{{if eq .Type \"volume\"}}}}{{{{.Name}}}} {{{{end}}}}{{{{end}}}}\n{{{{.RestartCount}}}}\n{{{{json .Config.User}}}}",
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
/// `service`, and [`Error::Failed`] when docker printed no container id or
/// a user that is not a JSON string.
pub(super) fn mint_container(
    scope: &Scope,
    service: &ServiceName,
    target: &str,
    printed: &str,
) -> Result<Inspected, Error> {
    let mut lines = printed.lines();
    let labels = lines.next().unwrap_or_default();
    if !scope.owns(labels, CONTAINER_LABEL, service.as_str()) {
        return Err(Error::Foreign {
            kind: "container",
            name: target.to_owned(),
        });
    }
    let printed_id = lines.next().unwrap_or_default().trim();
    let id = ContainerId::new(printed_id).map_err(|_| {
        Error::unexpected(
            "container inspect",
            Some(service.as_str()),
            format!("docker printed {printed_id:?} as the container's id"),
        )
    })?;
    let printed_state = lines.next().unwrap_or_default();
    let state = ContainerState::parse(printed_state);
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
    // An empty user means root, so a user that cannot be read is an error
    // and never counts as root.
    let printed_user = lines.next().unwrap_or_default().trim();
    let user = serde_json::from_str::<String>(printed_user).map_err(|_| {
        Error::unexpected(
            "container inspect",
            Some(service.as_str()),
            format!("docker printed {printed_user:?} as the container's user"),
        )
    })?;
    Ok(Inspected {
        container: OwnedContainer {
            scope: scope.clone(),
            service: service.clone(),
            id,
        },
        state,
        restarts,
        files: has_secret_files(labels),
        user,
        started,
        anonymous,
    })
}

/// Reports whether `labels`, a container's labels printed as JSON, carry
/// the label of a container that receives secret files.
fn has_secret_files(labels: &str) -> bool {
    serde_json::from_str::<Option<std::collections::BTreeMap<String, String>>>(labels)
        .ok()
        .flatten()
        .is_some_and(|labels| labels.get(SECRETS_LABEL).map(String::as_str) == Some(SECRET_FILES))
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
/// created without one, and a template that names the missing key as a
/// field fails, so the shared [`container_format`] leaves it out. This
/// reads the one key through `index`, which prints `null` for a missing
/// key, and never the rest of the config, which holds the container's
/// environment values.
pub(crate) fn stop_timeout(container: &OwnedContainer) -> Option<u64> {
    let finished = quick(
        [
            "container",
            "inspect",
            "--format",
            STOP_TIMEOUT_FORMAT,
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

/// The Go template of `docker container inspect` that prints a
/// container's `StopTimeout` as JSON, or `null` when it has none.
pub(super) const STOP_TIMEOUT_FORMAT: &str = "{{json (index .Config \"StopTimeout\")}}";

/// Returns the `StopTimeout` that docker printed with
/// [`STOP_TIMEOUT_FORMAT`] in `printed`.
pub(super) fn stop_timeout_of(printed: &str) -> Option<u64> {
    serde_json::from_str::<Option<u64>>(printed.trim()).ok()?
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
    run_until(["pull", "--quiet", "--", image], "pull", deadline, cancel)
        .and_then(|pulled| pulled.checked("pull", None))
        .map_err(Error::in_pull)?;
    Ok(())
}

/// Returns the entrypoint, command and user of `image`, which docker has.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`Error::Failed`] when
/// docker cannot inspect the image or prints something else.
pub(crate) fn image_config(image: &str) -> Result<ImageConfig, Error> {
    let printed = quick(
        [
            "image",
            "inspect",
            "--format",
            IMAGE_CONFIG_FORMAT,
            "--",
            image,
        ],
        "image inspect",
    )?
    .checked("image inspect", None)?;
    ImageConfig::parse(&printed).ok_or_else(|| {
        Error::unexpected(
            "image inspect",
            None,
            "docker printed an entrypoint, command and user that phase cannot read",
        )
    })
}

/// Returns the whole seconds that `docker stop --time` gives a container
/// for `wait`: `wait` rounded up, and at least one second, so a wait under
/// a second never stops the container at once.
pub(crate) fn stop_seconds(wait: Duration) -> u64 {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    seconds.max(1)
}

/// Turns off Docker's restart policy of `container`, so Docker does not
/// restart it while phase stops and removes it. A container that docker
/// no longer has needs nothing.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`Error::Failed`].
pub(crate) fn no_restart(container: &OwnedContainer) -> Result<(), Error> {
    let updated = quick(
        ["update", "--restart", "no", "--", container.id.as_str()],
        "update",
    )?;
    if !updated.missing() {
        updated.checked("update", Some(container.service.as_str()))?;
    }
    Ok(())
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

/// What happens to a container's anonymous volumes when [`remove`]
/// removes the container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnonymousVolumes {
    /// The anonymous volumes stay, for a caller that records them and
    /// deletes them later.
    Keep,
    /// docker removes the anonymous volumes with the container, as
    /// `docker rm --volumes` does. A named volume always stays.
    Remove,
}

/// Returns the arguments of the `docker rm` of the container `id`, which
/// removes its anonymous volumes with it when `anonymous` says so.
pub(crate) fn rm_args(id: &str, anonymous: AnonymousVolumes) -> Vec<&str> {
    let mut args = vec!["rm", "--force"];
    if anonymous == AnonymousVolumes::Remove {
        args.push("--volumes");
    }
    args.extend(["--", id]);
    args
}

/// Removes `container`, and its anonymous volumes when `anonymous` says
/// so. Its named volumes always stay. A container that docker no longer
/// has counts as removed.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`Error::Failed`].
pub(crate) fn remove(container: OwnedContainer, anonymous: AnonymousVolumes) -> Result<(), Error> {
    let removed = quick(rm_args(container.id.as_str(), anonymous), "rm")?;
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
        Error::unexpected(
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
/// `port`, as [`answers`] describes, within `timeout`. The connection gets
/// at most 300 ms, and must then stay open for the full 300 ms or bring
/// data, so a `timeout` too short to leave 300 ms after the connection
/// gives no answer. [`crate::plan::probe::LEAST_PUBLISHED_PROBE_TIMEOUT`]
/// is the shortest timeout that the plan allows for this check. It reports
/// no answer once `cancel` is set.
pub(crate) fn answers_within(port: u16, timeout: Duration, cancel: &Cancel) -> bool {
    let began = now();
    let connect = PROBE_WAIT.min(timeout.saturating_sub(PROBE_WAIT));
    if connect.is_zero() {
        return false;
    }
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&address, connect) else {
        return false;
    };
    if timeout.saturating_sub(began.elapsed()) < PROBE_WAIT {
        return false;
    }
    let hold = PROBE_WAIT;
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

/// Passes the log of the container `id` to `line`, as `docker logs`
/// prints it: its last `lines` lines, or the whole log when `lines` is
/// `None`, and with `follow` each later line until the container stops or
/// `line` breaks. `line` receives whether the container wrote the line to
/// standard error.
///
/// docker prints its own errors on standard error, among the container's
/// lines. phase recognises only the lines that start with one of
/// [`DOCKER_ERROR_PREFIXES`] as possibly docker's, and a container could
/// print such a line too. A recognised line waits up to [`HOLD`]. When
/// docker exits with code 1, as it does after it prints its own error,
/// and the line is the last of standard error, the line is the failure's
/// message and never reaches `line`. Otherwise the line reaches `line` as
/// the container's own: when another line of standard error follows, when
/// [`HOLD`] passes, or when docker exits in any other way. Every other line
/// reaches `line` at once.
///
/// # Errors
///
/// Returns [`Error::Missing`] when no `docker` program is on `PATH`, and
/// [`Error::Failed`] when docker cannot start or exits with a failure,
/// such as for a container that docker no longer has. The failure's message
/// is a recognised line, under the rule above, or else docker's exit
/// status. A line that does not start with a recognised prefix never
/// becomes the message, so a line that the container wrote, which may
/// carry a secret, becomes it only when it imitates docker's own error
/// and docker then exits with code 1.
pub(crate) fn logs(
    id: &crate::record::ContainerId,
    lines: Option<usize>,
    follow: bool,
    line: &mut dyn FnMut(bool, &str) -> std::ops::ControlFlow<()>,
) -> Result<(), Error> {
    logs_with(super::command::DOCKER.as_ref(), id, lines, follow, line)
}

/// The starts of the lines that docker prints for its own errors during
/// `docker logs`, which phase keeps from the container's log when docker
/// then fails. A line that starts with only `Error:` is not among them,
/// because containers print such lines too.
pub(crate) const DOCKER_ERROR_PREFIXES: [&str; 4] = [
    "Error response from daemon:",
    "Error: No such container:",
    "error from daemon in stream:",
    "Cannot connect to the Docker daemon",
];

/// The longest that [`logs`] holds back a line that may be docker's own
/// error before it passes the line as the container's.
pub(crate) const HOLD: Duration = Duration::from_millis(250);

/// The exit code of the docker client after it prints its own error.
const DOCKER_ERROR_EXIT: i32 = 1;

/// Runs [`logs`] with `program` as the `docker` program.
fn logs_with(
    program: &std::ffi::OsStr,
    id: &crate::record::ContainerId,
    lines: Option<usize>,
    follow: bool,
    line: &mut dyn FnMut(bool, &str) -> std::ops::ControlFlow<()>,
) -> Result<(), Error> {
    use std::ops::ControlFlow;
    let tail = lines.map_or_else(|| "all".to_owned(), |lines| lines.to_string());
    let mut args = vec!["logs".to_owned(), "--tail".to_owned(), tail];
    if follow {
        args.push("--follow".to_owned());
    }
    args.extend(["--".to_owned(), id.as_str().to_owned()]);
    // The recognised line of standard error that may be docker's own
    // error, with when it arrived.
    let mut held: Option<(String, std::time::Instant)> = None;
    // A read without follow ends once docker has printed the log, so a
    // docker that prints nothing for the deadline of a docker command hangs.
    let idle = (!follow).then_some(super::command::DOCKER_DEADLINE);
    let status = {
        let mut pass = |event: Option<(bool, &str)>| {
            // A held line that has waited its whole hold is the
            // container's, and so is one that another line of standard
            // error follows.
            let release = match event {
                Some((true, _)) => true,
                _ => held
                    .as_ref()
                    .is_some_and(|(_, since)| since.elapsed() >= HOLD),
            };
            if release
                && let Some((earlier, _)) = held.take()
                && line(true, &earlier).is_break()
            {
                return ControlFlow::Break(());
            }
            match event {
                None => ControlFlow::Continue(()),
                Some((false, text)) => line(false, text),
                Some((true, text)) if is_docker_error(text) => {
                    held = Some((text.to_owned(), crate::clock::now()));
                    ControlFlow::Continue(())
                }
                Some((true, text)) => line(true, text),
            }
        };
        super::command::stream_with_ticks(program, args, "logs", idle, Some(HOLD), &mut pass)
    };
    let held = held.map(|(text, _)| text);
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            if let Some(earlier) = held {
                let _ = line(true, &earlier);
            }
            return Err(error);
        }
    };
    match (status, held) {
        (Some(status), Some(earlier)) if status.code() == Some(DOCKER_ERROR_EXIT) => {
            Err(Error::failed("logs", Some(id.as_str()), earlier))
        }
        (Some(status), held) => {
            if let Some(earlier) = held {
                let _ = line(true, &earlier);
            }
            if status.success() {
                Ok(())
            } else {
                Err(Error::failed(
                    "logs",
                    Some(id.as_str()),
                    format!("docker logs exited with {status}"),
                ))
            }
        }
        // The sink broke, so it takes no more lines.
        (None, _) => Ok(()),
    }
}

/// Reports whether `text` starts like a line that docker prints for its
/// own error, as [`DOCKER_ERROR_PREFIXES`] lists.
fn is_docker_error(text: &str) -> bool {
    DOCKER_ERROR_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

#[cfg(test)]
mod tests;
