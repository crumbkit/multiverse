//! The names of docker objects: a [`Scope`] and the names that it gives a
//! workspace's compose project, containers and volumes, and the names and
//! ids that the engine reads back from docker.
//!
//! Each type holds text that its constructor checked, so a function that
//! takes one never receives a name that docker would read as an option or
//! that would split at the wrong `.`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::PathBuf;

use serde_json::{Map, Value};

#[cfg(doc)]
use crate::name::MOST_NAME_BYTES;
use crate::name::{AppName, WorkspaceId, is_unit_name};

/// The label that names a container's name in the manifest.
pub(crate) const CONTAINER_LABEL: &str = "mxv.container";

/// The label that names a volume's name in the manifest.
pub(crate) const VOLUME_LABEL: &str = "mxv.volume";

/// The label that names the stable id of an object's workspace, which
/// phase's `sweep` matches an object by.
pub(crate) const WORKSPACE_ID_LABEL: &str = "mxv.workspace-id";

/// The label that names the absolute path of the run record of an
/// object's workspace.
pub(crate) const RUN_RECORD_LABEL: &str = "mxv.run-record";

/// The label that names an object's app. Until mxv names apps, a
/// workspace's app is named after the workspace.
pub(crate) const APP_LABEL: &str = "mxv.app";

/// The label that names the role of a short-lived container that phase
/// runs for itself, apart from an app's processes.
pub(crate) const ROLE_LABEL: &str = "mxv.role";

/// The value of [`ROLE_LABEL`] on a helper of the volume-owner step, which
/// sets the owner of a workspace volume or reads the numeric ids of an
/// image's user.
pub(crate) const VOLUME_OWNER_ROLE: &str = "volume-owner";

/// The text of a name that a constructor refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadDockerName(pub(crate) String);

/// Implements [`AsRef<OsStr>`], [`fmt::Display`] and comparison with text
/// for a name type that wraps a `String`, and `as_str` when the type is
/// followed by `as_str`.
macro_rules! name_type {
    ($name:ident, as_str) => {
        impl $name {
            /// Returns the name as text.
            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        name_type!($name);
    };
    ($name:ident, pub as_str) => {
        impl $name {
            /// Returns the name as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        name_type!($name);
    };
    ($name:ident) => {
        impl AsRef<OsStr> for $name {
            fn as_ref(&self) -> &OsStr {
                self.0.as_ref()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
    };
}

/// The name of a workspace app's compose project, `phase-<hash>-<app>`,
/// which only [`Scope::project`] builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectName(String);

name_type!(ProjectName, as_str);

/// The name of a manifest container, which is also its compose service's
/// name: 1 to [`MOST_NAME_BYTES`] ASCII letters, digits, `-` and `_`,
/// starting with a letter or a digit. These are the names that a manifest
/// accepts, and none holds a `.`, so the part of a container or volume
/// name after the scope's `.` is always the service or volume.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceName(String);

name_type!(ServiceName, pub as_str);

impl ServiceName {
    /// Returns the service name `text`.
    ///
    /// # Errors
    ///
    /// Returns [`BadDockerName`] when `text` breaks the rules of the type.
    pub fn new(text: &str) -> Result<Self, BadDockerName> {
        if is_unit_name(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadDockerName(text.to_owned()))
        }
    }
}

/// The name of a named volume in the manifest, such as `data`, which
/// follows the rule of a [`ServiceName`]. Its docker name is
/// `phase-<hash>.<volume>`, which its scope builds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VolumeKey(String);

name_type!(VolumeKey, pub as_str);

impl VolumeKey {
    /// Returns the key of the plan's volume `volume`, whose name follows
    /// the same rule.
    pub(crate) fn of(volume: &crate::name::VolumeName) -> Self {
        Self(volume.as_str().to_owned())
    }

    /// Returns the volume name `text` of a manifest.
    ///
    /// # Errors
    ///
    /// Returns [`BadDockerName`] when `text` breaks the rules of a
    /// [`ServiceName`].
    pub fn new(text: &str) -> Result<Self, BadDockerName> {
        if is_unit_name(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadDockerName(text.to_owned()))
        }
    }
}

/// The docker name of a container, `<project>.<service>`, which only
/// [`Scope::container_name`] builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerName(String);

name_type!(ContainerName, as_str);

/// The docker name of a volume, `phase-<hash>.<volume>` for a named volume
/// of a scope. It holds an ASCII letter or digit, then one or more letters, digits, `_`, `.` and
/// `-`, which is the rule that docker applies to volume names, so docker
/// never reads it as an option.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct VolumeName(String);

name_type!(VolumeName, as_str);

impl VolumeName {
    /// Returns the volume name `text`, such as one that a run record holds.
    ///
    /// # Errors
    ///
    /// Returns [`BadDockerName`] when `text` breaks the rules of the type.
    pub(crate) fn new(text: &str) -> Result<Self, BadDockerName> {
        let valid = text.len() >= 2
            && text
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric())
            && text
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadDockerName(text.to_owned()))
        }
    }
}

/// The id of a container: the 64 lowercase hex digits that docker prints
/// as `.Id`. It names the container even after another container takes
/// the container's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerId(String);

name_type!(ContainerId, as_str);

impl ContainerId {
    /// Returns the container id `text`.
    ///
    /// # Errors
    ///
    /// Returns [`BadDockerName`] when `text` is not 64 lowercase hex
    /// digits.
    pub(crate) fn new(text: &str) -> Result<Self, BadDockerName> {
        crate::record::hex_id(text.to_owned())
            .map(Self)
            .map_err(|_| BadDockerName(text.to_owned()))
    }
}

/// The stable id of a workspace and the path of its run record, which
/// label the objects that a scope creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The workspace's stable id.
    pub(crate) id: WorkspaceId,
    /// The absolute path of the workspace's run record.
    pub(crate) run_record: PathBuf,
}

impl Identity {
    /// Reports whether `labels`, an object's labels, name this workspace:
    /// the object's `mxv.workspace-id` label equals the workspace's stable
    /// id and its `mxv.run-record` label equals the path of the
    /// workspace's run record. Each value must equal the expected one
    /// exactly, with nothing trimmed. The run record is the lossy UTF-8
    /// string that [`Scope::labels`] writes, so a state directory reached
    /// by another path than the one that made an object finds that object
    /// foreign, and leaves it alone.
    pub(crate) fn names(&self, labels: &BTreeMap<String, String>) -> bool {
        let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
        is(WORKSPACE_ID_LABEL, self.id.as_str())
            && is(RUN_RECORD_LABEL, &self.run_record.to_string_lossy())
    }
}

/// The objects of one app of a workspace, which are one compose project of
/// phase's. A scope gives the project its name and its containers and
/// volumes their names and labels. Two scopes are equal when they name the
/// same workspace id and app.
///
/// A scope names its project `phase-<hash>-<app>`, where `<hash>` is 16 hex
/// digits of a hash of the workspace's stable id. Its containers are
/// `<project>.<process>`, and its volumes, which every app of the
/// workspace shares, are `phase-<hash>.<volume>`. Its containers carry the
/// labels `mxv.workspace-id`, `mxv.run-record`, `mxv.app` and
/// `mxv.container`, and its volumes carry `mxv.workspace-id`,
/// `mxv.run-record` and `mxv.volume`.
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    /// The app.
    app: AppName,
    /// The workspace's stable id and run record, which label the objects
    /// that the scope creates.
    identity: Identity,
}

impl PartialEq for Scope {
    fn eq(&self, other: &Self) -> bool {
        self.app == other.app && self.identity.id == other.identity.id
    }
}

impl Eq for Scope {}

/// Returns 16 hex digits of the 64-bit FNV-1a hash of `text`, which names
/// a workspace in a compose project's name. A workspace's stable id may be
/// up to 64 bytes, and the hash keeps the names of its objects short.
fn short_hash(text: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let hash = text.bytes().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    });
    format!("{hash:016x}")
}

impl Scope {
    /// Returns the scope of the app `app` of the workspace that `identity`
    /// names, whose objects are labelled with `identity`.
    pub(crate) fn app(identity: Identity, app: &AppName) -> Self {
        Self {
            app: app.clone(),
            identity,
        }
    }

    /// Returns the name of the scope's compose project, `phase-<hash>-<app>`.
    pub(crate) fn project(&self) -> ProjectName {
        ProjectName(format!(
            "phase-{}-{}",
            short_hash(self.identity.id.as_str()),
            self.app
        ))
    }

    /// Returns the prefix of every container name of the scope,
    /// `<project>.`.
    pub(super) fn prefix(&self) -> String {
        format!("{}.", self.project())
    }

    /// Returns the prefix of every named volume of the scope,
    /// `phase-<hash>.`, whose volumes every app of the workspace shares.
    pub(super) fn volume_prefix(&self) -> String {
        format!("phase-{}.", short_hash(self.identity.id.as_str()))
    }

    /// Returns the name of the container of the service `service`,
    /// `<project>.<service>`.
    pub(crate) fn container_name(&self, service: &ServiceName) -> ContainerName {
        ContainerName(format!("{}{service}", self.prefix()))
    }

    /// Returns the docker name of the volume `volume`, its key after the
    /// scope's volume prefix.
    pub(crate) fn volume_name(&self, volume: &VolumeKey) -> VolumeName {
        VolumeName(format!("{}{volume}", self.volume_prefix()))
    }

    /// Returns the labels of the scope's object `object` under `label`:
    /// the workspace's stable id, run record and object labels, and the
    /// app label to a container.
    pub(super) fn labels(&self, label: &str, object: &str) -> Value {
        let mut labels = Map::new();
        let mut put = |key: &str, value: &str| {
            labels.insert(key.to_owned(), Value::String(value.to_owned()));
        };
        put(label, object);
        put(WORKSPACE_ID_LABEL, self.identity.id.as_str());
        put(
            RUN_RECORD_LABEL,
            &self.identity.run_record.to_string_lossy(),
        );
        if label == CONTAINER_LABEL {
            put(APP_LABEL, self.app.as_str());
        }
        Value::Object(labels)
    }

    /// Returns the labels of a volume-owner helper, as `(label, value)`
    /// pairs in label order: the workspace's stable id, its run record and
    /// the app, [`ROLE_LABEL`] set to [`VOLUME_OWNER_ROLE`], and the volume
    /// that the helper mounts when it mounts one. A helper carries no
    /// `mxv.container` label, so nothing that finds an app's containers by
    /// their process finds it.
    pub(super) fn helper_labels(&self, volume: Option<&VolumeKey>) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert(APP_LABEL.to_owned(), self.app.as_str().to_owned());
        labels.insert(
            WORKSPACE_ID_LABEL.to_owned(),
            self.identity.id.as_str().to_owned(),
        );
        labels.insert(
            RUN_RECORD_LABEL.to_owned(),
            self.identity.run_record.to_string_lossy().into_owned(),
        );
        labels.insert(ROLE_LABEL.to_owned(), VOLUME_OWNER_ROLE.to_owned());
        if let Some(volume) = volume {
            labels.insert(VOLUME_LABEL.to_owned(), volume.as_str().to_owned());
        }
        labels
    }

    /// Returns the stable id and run record that label the scope's
    /// objects.
    pub(crate) const fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Reports whether `labels`, an object's labels as the JSON that
    /// [`super::objects`] prints, name this scope and, under `label`, such
    /// as `mxv.container`, the object `object`. The object names the
    /// workspace's stable id and the run record of this scope's state
    /// directory, and a container also names its app. Each value must equal
    /// the expected one exactly, with nothing trimmed. The run record is
    /// the lossy UTF-8 string that [`Scope::labels`] writes, so a state
    /// directory reached by another path than the one that made an object
    /// finds that object foreign, and keeps it.
    pub(super) fn owns(&self, labels: &str, label: &str, object: &str) -> bool {
        let Ok(Some(labels)) = serde_json::from_str::<Option<BTreeMap<String, String>>>(labels)
        else {
            return false;
        };
        let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
        self.identity.names(&labels)
            && (label != CONTAINER_LABEL || is(APP_LABEL, self.app.as_str()))
            && is(label, object)
    }
}
