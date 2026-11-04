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

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[cfg(doc)]
use crate::name::MOST_NAME_BYTES;
use crate::name::{WorkspaceId, WorkspaceName, is_unit_name};

/// The label that names the store id of an object's state directory.
pub(crate) const STORE_LABEL: &str = "mxv.store";

/// The label that names an object's workspace.
pub(crate) const WORKSPACE_LABEL: &str = "mxv.workspace";

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

/// The number of digits of the store id that project names hold.
const SHORT_ID: usize = 8;

/// The number of hex digits of a store id.
const STORE_DIGITS: usize = 32;

/// Reports whether `text` is `digits` lowercase hex digits.
fn is_hex(text: &str, digits: usize) -> bool {
    text.len() == digits
        && text
            .chars()
            .all(|digit| digit.is_ascii_digit() || ('a'..='f').contains(&digit))
}

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

/// The id of a state directory, 32 lowercase hex digits, which labels its
/// containers and volumes. One docker daemon can serve several state
/// directories, and the id tells their objects apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct StoreId(String);

name_type!(StoreId, as_str);

impl StoreId {
    /// Returns the store id `text`.
    ///
    /// # Errors
    ///
    /// Returns [`BadDockerName`] when `text` is not 32 lowercase hex
    /// digits.
    pub(crate) fn new(text: &str) -> Result<Self, BadDockerName> {
        if is_hex(text, STORE_DIGITS) {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadDockerName(text.to_owned()))
        }
    }
}

impl TryFrom<String> for StoreId {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        Self::new(&text).map_err(|_| format!("{text:?} is not 32 lowercase hex digits"))
    }
}

impl From<StoreId> for String {
    fn from(id: StoreId) -> Self {
        id.0
    }
}

/// The name of a workspace's compose project, `mxv-<id8>-<workspace>`,
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
/// `<project>.<volume>`, which its scope builds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VolumeKey(String);

name_type!(VolumeKey, pub as_str);

impl VolumeKey {
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

/// The docker name of a volume: `<project>.<volume>` for a named volume of
/// a scope, or the name that docker gave an anonymous volume. It holds an
/// ASCII letter or digit, then one or more letters, digits, `_`, `.` and
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
#[derive(Clone, Debug)]
pub(crate) struct Identity {
    /// The workspace's stable id.
    pub(crate) id: WorkspaceId,
    /// The absolute path of the workspace's run record.
    pub(crate) run_record: PathBuf,
}

/// One workspace of one store: its compose project and the labels and
/// names of its containers and volumes. Two scopes are equal when they
/// name the same store and workspace.
#[derive(Clone, Debug)]
pub(crate) struct Scope {
    /// The store id.
    store: StoreId,
    /// The workspace.
    workspace: WorkspaceName,
    /// The workspace's stable id and run record, which label the objects
    /// that the scope creates, or `None` for a scope that only finds and
    /// checks objects.
    identity: Option<Identity>,
}

impl PartialEq for Scope {
    fn eq(&self, other: &Self) -> bool {
        self.store == other.store && self.workspace == other.workspace
    }
}

impl Eq for Scope {}

impl Scope {
    /// Returns the scope of the workspace `workspace` in the store with the
    /// id `store`.
    pub(crate) fn new(store: &StoreId, workspace: &WorkspaceName) -> Self {
        Self {
            store: store.clone(),
            workspace: workspace.clone(),
            identity: None,
        }
    }

    /// Returns the scope, whose objects are also labelled with `identity`.
    pub(crate) fn identified(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Returns the scope's store id.
    pub(crate) const fn store(&self) -> &StoreId {
        &self.store
    }

    /// Returns the scope's workspace.
    pub(crate) const fn workspace(&self) -> &WorkspaceName {
        &self.workspace
    }

    /// Returns the name of the scope's compose project,
    /// `mxv-<id8>-<workspace>`, with the workspace's `_` written `__` and
    /// its `.` written `_d`.
    pub(crate) fn project(&self) -> ProjectName {
        let short = self.store.0.get(..SHORT_ID).unwrap_or(&self.store.0);
        let workspace = self
            .workspace
            .as_str()
            .replace('_', "__")
            .replace('.', "_d");
        ProjectName(format!("mxv-{short}-{workspace}"))
    }

    /// Returns the prefix of every container and volume name of the
    /// scope, `<project>.`.
    pub(super) fn prefix(&self) -> String {
        format!("{}.", self.project())
    }

    /// Returns the name of the container of the service `service`,
    /// `<project>.<service>`.
    pub(crate) fn container_name(&self, service: &ServiceName) -> ContainerName {
        ContainerName(format!("{}{service}", self.prefix()))
    }

    /// Returns the docker name of the manifest's volume `volume`,
    /// `<project>.<volume>`.
    pub(crate) fn volume_name(&self, volume: &VolumeKey) -> VolumeName {
        VolumeName(format!("{}{volume}", self.prefix()))
    }

    /// Returns the labels of the scope's object `object` under `label`:
    /// the store, workspace and object labels, and the workspace's stable
    /// id, run record and app when the scope has its identity.
    pub(super) fn labels(&self, label: &str, object: &str) -> Value {
        let mut labels = Map::new();
        let mut put = |key: &str, value: &str| {
            labels.insert(key.to_owned(), Value::String(value.to_owned()));
        };
        put(STORE_LABEL, self.store.as_str());
        put(WORKSPACE_LABEL, self.workspace.as_str());
        put(label, object);
        if let Some(identity) = &self.identity {
            put(WORKSPACE_ID_LABEL, identity.id.as_str());
            put(RUN_RECORD_LABEL, &identity.run_record.to_string_lossy());
            put(APP_LABEL, self.workspace.as_str());
        }
        Value::Object(labels)
    }

    /// Reports whether `labels`, an object's labels as the JSON that
    /// [`super::objects`] prints, name this scope's store and workspace
    /// and, under `label`, such as `mxv.container`, the object `object`.
    /// Each value must equal the expected one exactly, with nothing
    /// trimmed.
    pub(super) fn owns(&self, labels: &str, label: &str, object: &str) -> bool {
        let Ok(Some(labels)) = serde_json::from_str::<Option<BTreeMap<String, String>>>(labels)
        else {
            return false;
        };
        let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
        is(STORE_LABEL, self.store.as_str())
            && is(WORKSPACE_LABEL, self.workspace.as_str())
            && is(label, object)
    }
}
