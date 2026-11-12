//! The data volumes of a workspace's apps, which phase's `down` and `sweep`
//! leave and a removal of the workspace deletes, and the memory of its
//! containers that a reclaim measures.

use std::collections::BTreeMap;
use std::path::Path;

use crate::engine::container::command::quick;
use crate::engine::container::names::{
    Identity, RUN_RECORD_LABEL, Scope, VOLUME_LABEL, VolumeKey, VolumeName, WORKSPACE_ID_LABEL,
};
use crate::engine::container::objects::{
    OwnedVolume, delete_volume, inspect_volume, reinspect_volume,
};
use crate::engine::container::{ContainerError, volume_owner};
use crate::name::{AppName, WorkspaceId};
use crate::phase::Phase;

#[cfg(test)]
mod tests;

/// The volumes of a workspace that a removal deletes, which [`app_volumes`]
/// found and checked, in name order, and the scope that they belong to.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkspaceVolumes {
    /// The scope of the volumes, or `None` for a workspace without them.
    scope: Option<Scope>,
    /// The volumes.
    volumes: Vec<OwnedVolume>,
}

impl WorkspaceVolumes {
    /// Returns the volumes' docker names, in name order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.volumes
            .iter()
            .map(|volume| volume.name().as_str().to_owned())
            .collect()
    }
}

/// The app that the scope of a workspace's app volumes names. The volumes
/// of a workspace's apps are shared by every app, and neither their names
/// nor their labels name an app, so any app's scope finds them.
const VOLUMES_APP: &str = "volumes";

/// Returns the scope that finds the data volumes of the apps of the
/// workspace whose stable id is `id`, which `phase` keeps.
fn app_volume_scope(phase: &Phase, id: &WorkspaceId) -> Result<Scope, ContainerError> {
    let app = AppName::try_from(VOLUMES_APP.to_owned()).map_err(|error| {
        ContainerError::failed("volume scope", None, format!("{VOLUMES_APP}: {error}"))
    })?;
    let identity = Identity {
        id: id.clone(),
        run_record: phase.run_record(id),
    };
    Ok(Scope::app(identity, &app))
}

/// Returns the docker name of the data volume `volume` of the apps of the
/// workspace whose stable id is `id`, `phase-<hash>.<volume>`, which a
/// container app creates when it first mounts the volume.
#[must_use]
pub fn app_volume_name(phase: &Phase, id: &WorkspaceId, volume: &VolumeKey) -> Option<String> {
    app_volume_scope(phase, id)
        .ok()
        .map(|scope| scope.volume_name(volume).as_str().to_owned())
}

/// Returns the labels, as `name=value` pairs in name order, that phase
/// gives the data volume `volume` of the apps of the workspace whose stable
/// id is `id`. A volume created with exactly these labels under
/// [`app_volume_name`] is one that phase mounts as its own.
#[must_use]
pub fn app_volume_labels(phase: &Phase, id: &WorkspaceId, volume: &VolumeKey) -> Vec<String> {
    let Ok(scope) = app_volume_scope(phase, id) else {
        return Vec::new();
    };
    match scope.labels(VOLUME_LABEL, volume.as_str()) {
        serde_json::Value::Object(labels) => labels
            .into_iter()
            .map(|(label, value)| format!("{label}={}", value.as_str().unwrap_or_default()))
            .collect(),
        _ => Vec::new(),
    }
}

/// Returns each workspace id that docker labels a data volume of `phase`
/// with, and the names of those volumes, from one `docker volume ls`, in
/// id order and each id's names in name order. A volume counts as
/// `phase`'s when its run-record label names `phase`'s run record of its
/// id, so the volumes that another state directory's phase made, which may
/// share the docker daemon, are left out. [`app_volumes`] checks each
/// volume again before it is deleted.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn labelled_volumes(phase: &Phase) -> Result<Vec<(WorkspaceId, Vec<String>)>, ContainerError> {
    let format = format!(
        "{{{{.Name}}}}\t{{{{.Label \"{WORKSPACE_ID_LABEL}\"}}}}\t{{{{.Label \"{RUN_RECORD_LABEL}\"}}}}"
    );
    let listed = quick(
        [
            "volume",
            "ls",
            "--filter",
            &format!("label={VOLUME_LABEL}"),
            "--format",
            format.as_str(),
        ],
        "volume ls",
    )?
    .checked("volume ls", None)?;
    let mut found: BTreeMap<WorkspaceId, Vec<String>> = BTreeMap::new();
    for line in listed.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(name), Some(id), Some(run_record)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(id) = WorkspaceId::try_from(id.to_owned()) else {
            continue;
        };
        if Path::new(run_record) == phase.run_record(&id) {
            found.entry(id).or_default().push(name.to_owned());
        }
    }
    Ok(found
        .into_iter()
        .map(|(id, mut names)| {
            names.sort();
            (id, names)
        })
        .collect())
}

/// Returns the data volumes of the apps of the workspace whose stable id
/// is `id`: each volume that docker labels with the id as a volume of the
/// workspace, under a name of the workspace's apps, in name order. phase's
/// `down` and `sweep` leave these volumes, and a removal of the workspace
/// deletes them with [`delete_app_volumes`].
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn app_volumes(phase: &Phase, id: &WorkspaceId) -> Result<WorkspaceVolumes, ContainerError> {
    let scope = app_volume_scope(phase, id)?;
    let filter = format!("label={WORKSPACE_ID_LABEL}={id}");
    let listed = quick(
        [
            "volume",
            "ls",
            "--quiet",
            "--filter",
            filter.as_str(),
            "--filter",
            &format!("label={VOLUME_LABEL}"),
        ],
        "volume ls",
    )?
    .checked("volume ls", None)?;
    let mut volumes = BTreeMap::new();
    for line in listed.lines() {
        let Ok(name) = VolumeName::new(line.trim()) else {
            continue;
        };
        match inspect_volume(&scope, &name) {
            Ok(Some(volume)) => {
                volumes.insert(name, volume);
            }
            Ok(None) | Err(ContainerError::Foreign { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(WorkspaceVolumes {
        scope: (!volumes.is_empty()).then_some(scope),
        volumes: volumes.into_values().collect(),
    })
}

/// Deletes `volumes`, which [`app_volumes`] returned, after it checked each
/// one again. A volume that no longer exists counts as deleted.
///
/// # Errors
///
/// Returns the errors
/// of `docker version` and [`ContainerError::Failed`].
pub fn delete_app_volumes(volumes: WorkspaceVolumes) -> Result<(), ContainerError> {
    let Some(scope) = volumes.scope.as_ref() else {
        return Ok(());
    };
    // A volume-owner helper that an interrupted `up` left behind keeps
    // its volume in use, and Docker refuses to delete a volume in use.
    volume_owner::remove_helpers(scope.workspace_id(), None)?;
    for volume in &volumes.volumes {
        if let Some(volume) = reinspect_volume(scope, volume)? {
            delete_volume(volume)?;
        }
    }
    Ok(())
}

/// Reports whether docker has a volume named `name`, whoever owns it.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn volume_exists(name: &str) -> Result<bool, ContainerError> {
    let finished = quick(
        ["volume", "inspect", "--format", "{{.Name}}", "--", name],
        "volume inspect",
    )?;
    if finished.missing() {
        return Ok(false);
    }
    finished.checked("volume inspect", None).map(|_| true)
}

/// Returns the resident memory of each running container of `ids`, by
/// id, as `docker stats --no-stream` measures it. A container that is not
/// running, or whose memory docker does not report, is left out. It runs
/// no docker command when `ids` is empty.
///
/// # Errors
///
/// Returns the errors of `docker version` and [`ContainerError::Failed`].
pub fn container_memory(ids: &[String]) -> Result<BTreeMap<String, u64>, ContainerError> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut args = vec![
        "stats",
        "--no-stream",
        "--no-trunc",
        "--format",
        "{{.ID}}\t{{.MemUsage}}",
    ];
    args.extend(ids.iter().map(String::as_str));
    let listed = quick(args, "stats")?.checked("stats", None)?;
    Ok(listed.lines().filter_map(stats_line).collect())
}

/// Returns the container id and the resident bytes of one line of
/// `docker stats --format '{{.ID}}\t{{.MemUsage}}'`, such as
/// `0123abcd\t12.5MiB / 7.6GiB`, or `None` when docker reports no usage.
pub(crate) fn stats_line(line: &str) -> Option<(String, u64)> {
    let (id, usage) = line.split_once('\t')?;
    let used = usage.split('/').next()?.trim();
    let split = used.find(|char: char| !(char.is_ascii_digit() || char == '.'))?;
    let (number, unit) = used.split_at(split);
    let scale: u64 = match unit.trim() {
        "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        "kB" | "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole: u64 = whole.parse().ok()?;
    // Docker prints at most a few decimal places, so the fraction is read
    // as a whole number of its own and scaled, without floating point.
    let digits = u32::try_from(fraction.len()).ok()?;
    let fraction: u64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().ok()?
    };
    let bytes = whole
        .checked_mul(scale)?
        .checked_add(fraction.checked_mul(scale)? / 10_u64.checked_pow(digits)?)?;
    Some((id.trim().to_owned(), bytes))
}
