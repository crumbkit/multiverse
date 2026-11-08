//! The volume-owner step, which gives the root directory of each workspace
//! volume that a container app mounts to the user that the app runs as,
//! after the app's container is created and before it starts. Docker
//! creates a named volume owned by root, and copies the owner of the image
//! directory at the mount path onto an empty volume when `docker create`
//! runs, so a process that runs as another user could not write it.
//!
//! The owner comes from the same rule as the owner of the secret files,
//! [`secrets::chosen_owner`]: the app's `owner`, or else its run-as user,
//! or else the image's `USER`. `owner = "keep"` leaves every volume as it
//! is. A plan's `owner` of `0:0` gives the volumes to root, but a run-as
//! user or an image `USER` that is root leaves them as they are. A user
//! that is a name, such as `postgres`, is resolved to numbers by running
//! `id -u` and `id -g` in the app's image as that user, because the
//! helper that sets the owner runs busybox, whose `/etc/passwd` is not
//! the image's. A name that resolves to uid 0 is refused: only a plan's
//! numeric `owner` can name root.
//!
//! The image's author controls the text of its `USER`, which may hold a
//! newline or a terminal escape sequence, so no error repeats it. An error
//! about a user from the image names the image's `USER` as its source, and
//! an error about the plan's run-as user prints it quoted. Docker's own
//! line can quote the user too, when `docker start` cannot find it in the
//! image's `/etc/passwd`, so a refused creation or start of the `id`
//! helper keeps none of Docker's text when the user came from the image.
//!
//! Each helper is a container that phase creates, starts, waits for and
//! removes: `docker create`, `docker start`, `docker wait` and `docker
//! rm`. Each of those prints only Docker's own text, so a failure carries
//! Docker's line as the other container commands do, apart from the `id`
//! helper's creation and start for a user from the image, as above. phase
//! never reads
//! what a helper writes, apart from the standard output of `id`, which
//! counts only when it is one number, and which no error repeats. A
//! helper runs with no network, every capability dropped (the chown
//! helper keeps only `CHOWN`), `no-new-privileges` and a read-only root
//! filesystem, and the chown helper mounts only its volume, at `/volume`.
//! It changes the owner of the volume's root directory alone, never
//! recursively.
//!
//! Every helper carries the workspace's labels, its app's label and
//! `mxv.role=volume-owner`, and the chown helper also names its volume
//! with `mxv.volume`. A run of phase that ends between a helper's
//! creation and its removal leaves the helper behind. [`remove_helpers`]
//! finds such helpers by their labels and removes them: `up` does so
//! before it sets any owner. `down`, a sweep and the removal of a
//! workspace's volumes remove them too, so a leftover helper never keeps a
//! volume in use.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::time::Instant;

use crate::cancel::Cancel;
use crate::clock::now;
use crate::engine::container::ContainerError;
use crate::engine::container::command::{
    DOCKER, DOCKER_DEADLINE, check_with, quick_with, run_program_until,
};
use crate::engine::container::names::{
    APP_LABEL, ContainerId, ROLE_LABEL, Scope, VOLUME_OWNER_ROLE, VolumeKey, WORKSPACE_ID_LABEL,
};
use crate::engine::container::secrets::{self, run_as};
use crate::error::{PhaseError, VolumeOwnerProblem};
use crate::name::{AppName, WorkspaceId};
use crate::plan::AppPlan;
use crate::plan::container::KEEP_OWNER;
use crate::record::DockerCause;
use crate::record::run::{UNCHANGED_ID, VolumeOwner};

#[cfg(test)]
mod tests;

/// The image of the helper that sets a volume's owner: busybox 1.37.0,
/// pinned by the digest of its multi-platform index on Docker Hub, so a
/// moved tag cannot change what runs as root with a workspace's volume.
///
/// To update it, run `docker buildx imagetools inspect busybox:<version>`
/// and copy the `Digest:` line of the index, then pull the image by that
/// digest and run the volume-owner tests. CI pulls the same image by
/// digest in `.github/workflows/ci.yml`, so keep the two in step.
pub(crate) const HELPER_IMAGE: &str =
    "busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec";

/// The path inside the chown helper at which it mounts its volume.
const MOUNT: &str = "/volume";

/// The owner that the volume-owner step wants for an app's volumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Wanted {
    /// The step leaves the volumes as they are: the plan says `keep`, or
    /// the app runs as root and the plan names no owner.
    Skip,
    /// The step gives each volume to this numeric owner.
    Owner(VolumeOwner),
    /// The app runs as this user, which names a user or group, so the step
    /// resolves it to numbers in the app's image first.
    Resolve(RunAs),
}

/// A user that the app's containers run as and that names a user or group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunAs {
    /// The user, in Docker's `USER[:GROUP]` form.
    pub(crate) user: String,
    /// Whether the user is the image's `USER`, whose text no error repeats.
    pub(crate) from_image: bool,
}

impl RunAs {
    /// Returns the error that says `problem` stopped phase resolving this
    /// user for `app`. It holds the user's text only when the plan named
    /// the user.
    pub(crate) fn error(self, app: AppName, problem: VolumeOwnerProblem) -> PhaseError {
        PhaseError::VolumeOwner {
            app,
            user: (!self.from_image).then_some(self.user),
            problem,
        }
    }
}

/// Reports whether the owner of `app`'s volumes depends on its image's
/// `USER`: the plan names neither an owner nor a run-as user.
pub(crate) const fn needs_image_user(app: &AppPlan) -> bool {
    app.container.owner.is_none() && app.container.user.is_none()
}

/// Returns the owner that the volumes of `app` should have, when its image
/// runs as `image_user`, which is empty for root, as the module docs say.
pub(crate) fn wanted(app: &AppPlan, image_user: &str) -> Wanted {
    let explicit = app.container.owner.as_deref();
    if explicit == Some(KEEP_OWNER) {
        return Wanted::Skip;
    }
    let Some(text) = secrets::chosen_owner(explicit, run_as(app, image_user)) else {
        return Wanted::Skip;
    };
    match text.parse::<VolumeOwner>() {
        // Only the plan's own owner gives a volume to root. A run-as user
        // or an image `USER` of root means the app can write the volume
        // as Docker made it.
        Ok(owner) if owner.is_root() && explicit.is_none() => Wanted::Skip,
        Ok(owner) => Wanted::Owner(owner),
        Err(_) => Wanted::Resolve(RunAs {
            user: text,
            from_image: explicit.is_none() && app.container.user.is_none(),
        }),
    }
}

/// Returns the arguments of the `docker create` of a helper of `scope`
/// that runs `entrypoint` with `args` in `image` as `user`, under the
/// helpers' restrictions, with `capability` added back and `volume`
/// mounted when they are given.
fn create_args(
    labels: BTreeMap<String, String>,
    user: &str,
    capability: Option<&str>,
    volume: Option<String>,
    entrypoint: &str,
    image: &str,
    args: &[&str],
) -> Vec<String> {
    let mut argv: Vec<String> = ["create", "--pull", "never"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for (label, value) in labels {
        argv.push("--label".to_owned());
        argv.push(format!("{label}={value}"));
    }
    argv.extend(
        ["--network", "none", "--cap-drop", "ALL"]
            .into_iter()
            .map(str::to_owned),
    );
    if let Some(capability) = capability {
        argv.extend(["--cap-add".to_owned(), capability.to_owned()]);
    }
    argv.extend(
        [
            "--security-opt",
            "no-new-privileges",
            "--read-only",
            "--user",
            user,
        ]
        .into_iter()
        .map(str::to_owned),
    );
    if let Some(volume) = volume {
        argv.extend(["--volume".to_owned(), volume]);
    }
    argv.extend(
        ["--entrypoint", entrypoint, "--", image]
            .into_iter()
            .map(str::to_owned),
    );
    argv.extend(args.iter().map(|arg| (*arg).to_owned()));
    argv
}

/// Returns the arguments of the `docker create` of the helper that gives
/// the root directory of the volume `key` of `scope` to `owner`.
pub(crate) fn chown_args(scope: &Scope, key: &VolumeKey, owner: &VolumeOwner) -> Vec<String> {
    let owner = owner.to_string();
    create_args(
        scope.helper_labels(Some(key)),
        "0:0",
        Some("CHOWN"),
        Some(format!("{}:{MOUNT}", scope.volume_name(key))),
        "chown",
        HELPER_IMAGE,
        &[&owner, MOUNT],
    )
}

/// Returns the arguments of the `docker create` of the helper that runs
/// `id <flag>` in `image` as `user`, for an app of `scope`.
pub(crate) fn id_args(scope: &Scope, image: &str, user: &str, flag: &str) -> Vec<String> {
    create_args(
        scope.helper_labels(None),
        user,
        None,
        None,
        "id",
        image,
        &[flag],
    )
}

/// Returns the number that `id -u` or `id -g` printed as `printed`, or
/// `None` when it printed anything else than one decimal number on one
/// line, or [`UNCHANGED_ID`], which `chown` would read as no change.
pub(crate) fn parse_id(printed: &str) -> Option<u32> {
    let line = printed.strip_suffix('\n').unwrap_or(printed);
    if line.is_empty() || !line.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    line.parse().ok().filter(|id| *id != UNCHANGED_ID)
}

/// Reports whether `labels`, as the JSON that `docker container inspect`
/// prints, name a volume-owner helper of the workspace `workspace`, and of
/// `app` when `app` is given. Each value must equal the expected one
/// exactly.
pub(crate) fn helper_of(workspace: &WorkspaceId, app: Option<&AppName>, labels: &str) -> bool {
    let Some(labels) = serde_json::from_str::<Option<BTreeMap<String, String>>>(labels)
        .ok()
        .flatten()
    else {
        return false;
    };
    let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
    is(WORKSPACE_ID_LABEL, workspace.as_str())
        && is(ROLE_LABEL, VOLUME_OWNER_ROLE)
        && app.is_none_or(|app| is(APP_LABEL, app.as_str()))
}

/// Returns Docker's line when `error` says that Docker refused to create
/// or start a helper.
fn refused_line(error: &ContainerError) -> Option<&str> {
    match error {
        ContainerError::Failed {
            operation: "volume owner create" | "volume owner start",
            cause: DockerCause::Refused,
            message,
            ..
        } => Some(message),
        _ => None,
    }
}

/// How a helper ended.
struct Ran {
    /// The helper's exit status, as `docker wait` printed it.
    status: i64,
    /// What the helper wrote to its standard output, when the caller asked
    /// for it and the helper exited with status 0.
    stdout: Option<String>,
}

/// Creates a helper with the `docker create` arguments `args`, starts it,
/// waits until it exits or until `deadline` or `cancel`, reads its
/// standard output when `read_stdout` asks for it, and removes it. The
/// helper is removed however the steps after its creation end.
fn run_helper(
    args: &[String],
    read_stdout: bool,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Ran, ContainerError> {
    let created = quick_with(DOCKER.as_ref(), args, "volume owner create")?
        .checked("volume owner create", None)?;
    let id = ContainerId::new(created.trim()).map_err(|_| {
        ContainerError::unexpected(
            "volume owner create",
            None,
            "docker printed a container id that is not 64 hex digits",
        )
    })?;
    let ran = run_created(&id, read_stdout, deadline, cancel);
    let removed = remove_helper(DOCKER.as_ref(), &id);
    let ran = ran?;
    removed?;
    Ok(ran)
}

/// Starts the created helper `id` and waits for it, as [`run_helper`]
/// says.
fn run_created(
    id: &ContainerId,
    read_stdout: bool,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Ran, ContainerError> {
    let program: &OsStr = DOCKER.as_ref();
    quick_with(program, ["start", "--", id.as_str()], "volume owner start")?
        .checked("volume owner start", None)?;
    let wait = deadline
        .saturating_duration_since(now())
        .min(DOCKER_DEADLINE);
    let printed = run_program_until(
        program,
        ["wait", "--", id.as_str()],
        "volume owner wait",
        wait,
        cancel,
    )?
    .checked("volume owner wait", None)?;
    let status = printed.trim().parse::<i64>().map_err(|_| {
        ContainerError::unexpected(
            "volume owner wait",
            None,
            "docker wait printed no exit status",
        )
    })?;
    let stdout = if read_stdout && status == 0 {
        Some(helper_stdout(id)?)
    } else {
        None
    };
    Ok(Ran { status, stdout })
}

/// Returns what the exited helper `id` wrote to its standard output.
/// `docker logs` passes on what the helper wrote to its standard error
/// too, so phase reads none of it, and a failure says only how `docker
/// logs` exited.
fn helper_stdout(id: &ContainerId) -> Result<String, ContainerError> {
    let program: &OsStr = DOCKER.as_ref();
    let finished = quick_with(program, ["logs", "--", id.as_str()], "volume owner logs")?;
    if finished.status.success() {
        return Ok(finished.stdout);
    }
    check_with(program)?;
    let status = finished
        .status
        .code()
        .map_or_else(|| "a signal".to_owned(), |code| format!("status {code}"));
    Err(ContainerError::unexpected(
        "volume owner logs",
        None,
        format!("docker logs of the volume-owner helper exited with {status}"),
    ))
}

/// Removes the helper `id` with the `docker` program `program`, and the
/// anonymous volumes that Docker made for it. A helper that Docker no
/// longer has counts as removed.
fn remove_helper(program: &OsStr, id: &ContainerId) -> Result<(), ContainerError> {
    let finished = quick_with(
        program,
        ["rm", "--force", "--volumes", "--", id.as_str()],
        "volume owner rm",
    )?;
    if finished.missing() {
        return Ok(());
    }
    finished.checked_with(program, "volume owner rm", None)?;
    Ok(())
}

/// Gives the root directory of the volume `key` of `scope` to `owner`,
/// whatever owner it has now, with a helper that must finish before `deadline`, unless `cancel` ends
/// it first.
///
/// # Errors
///
/// Returns the errors of the docker commands, and
/// [`ContainerError::Failed`] when `chown` exits with a status other than
/// 0, which the message gives.
pub(crate) fn set_owner(
    scope: &Scope,
    key: &VolumeKey,
    owner: &VolumeOwner,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<(), ContainerError> {
    let ran = run_helper(&chown_args(scope, key, owner), false, deadline, cancel)?;
    if ran.status == 0 {
        return Ok(());
    }
    Err(ContainerError::unexpected(
        "volume owner",
        None,
        format!(
            "the helper that gives the volume {} to {owner} exited with status {}",
            scope.volume_name(key),
            ran.status
        ),
    ))
}

/// Resolves `user`, which names a user or group, to a numeric owner by
/// running `id -u` and `id -g` in `image` as that user, for an app of
/// `scope`. The outer result holds a Docker failure, and the inner one the
/// reason the user could not be an owner, which holds nothing that `id`
/// printed.
///
/// # Errors
///
/// Returns the errors of the docker commands.
pub(crate) fn resolve(
    scope: &Scope,
    image: &str,
    user: &RunAs,
    deadline: Instant,
    cancel: &Cancel,
) -> Result<Result<VolumeOwner, VolumeOwnerProblem>, ContainerError> {
    let id = |flag: &str| -> Result<Result<u32, VolumeOwnerProblem>, ContainerError> {
        let ran = match run_helper(
            &id_args(scope, image, &user.user, flag),
            true,
            deadline,
            cancel,
        ) {
            Ok(ran) => ran,
            Err(error) => {
                let Some(line) = refused_line(&error) else {
                    return Err(error);
                };
                return Ok(Err(VolumeOwnerProblem::IdNotRun {
                    docker_line: (!user.from_image).then(|| line.to_owned()),
                }));
            }
        };
        if ran.status != 0 {
            return Ok(Err(VolumeOwnerProblem::IdFailed { status: ran.status }));
        }
        Ok(ran
            .stdout
            .as_deref()
            .and_then(parse_id)
            .ok_or(VolumeOwnerProblem::IdUnreadable))
    };
    let uid = match id("-u")? {
        Ok(0) => return Ok(Err(VolumeOwnerProblem::ResolvesToRoot)),
        Ok(uid) => uid,
        Err(problem) => return Ok(Err(problem)),
    };
    Ok(id("-g")?.map(|gid| VolumeOwner {
        uid,
        gid: Some(gid),
    }))
}

/// Removes every volume-owner helper of the workspace `workspace`, or of
/// its app `app` when it is given, that the `docker` program `program`
/// finds by its labels. A helper that Docker no longer has is skipped.
///
/// # Errors
///
/// Returns the errors of the docker commands.
pub(crate) fn remove_helpers_with(
    program: &OsStr,
    workspace: &WorkspaceId,
    app: Option<&AppName>,
) -> Result<(), ContainerError> {
    let listed = quick_with(
        program,
        [
            "container",
            "ls",
            "--all",
            "--quiet",
            "--no-trunc",
            "--filter",
            &format!("label={WORKSPACE_ID_LABEL}={workspace}"),
            "--filter",
            &format!("label={ROLE_LABEL}={VOLUME_OWNER_ROLE}"),
        ],
        "container ls",
    )?
    .checked_with(program, "container ls", None)?;
    for line in listed.lines() {
        let Ok(id) = ContainerId::new(line.trim()) else {
            continue;
        };
        let inspected = quick_with(
            program,
            [
                "container",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                "--",
                id.as_str(),
            ],
            "container inspect",
        )?;
        if inspected.missing() {
            continue;
        }
        let labels = inspected.checked_with(program, "container inspect", None)?;
        if helper_of(workspace, app, labels.trim()) {
            remove_helper(program, &id)?;
        }
    }
    Ok(())
}

/// Removes the volume-owner helpers of the workspace `workspace`, or of its
/// app `app`, with the `docker` program on `PATH`, as
/// [`remove_helpers_with`] does.
///
/// # Errors
///
/// Returns the errors of [`remove_helpers_with`].
pub(crate) fn remove_helpers(
    workspace: &WorkspaceId,
    app: Option<&AppName>,
) -> Result<(), ContainerError> {
    remove_helpers_with(DOCKER.as_ref(), workspace, app)
}
