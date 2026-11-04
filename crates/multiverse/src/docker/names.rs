//! The names of docker objects: a [`Scope`] and the names that it gives a
//! workspace's compose project, containers and volumes, and the names and
//! ids that mxv reads back from docker.
//!
//! Each type holds text that its constructor checked, so a function that
//! takes one never receives a name that docker would read as an option or
//! that would split at the wrong `.`.

use std::ffi::OsStr;
use std::fmt;

use serde_json::{Value, json};

use crate::name::{MOST_NAME_BYTES, WorkspaceName};

/// The label that names the store id of an object's state directory.
pub(crate) const STORE_LABEL: &str = "mxv.store";

/// The label that names an object's workspace.
pub(crate) const WORKSPACE_LABEL: &str = "mxv.workspace";

/// The label that names a container's name in the manifest.
pub(crate) const CONTAINER_LABEL: &str = "mxv.container";

/// The label that names a volume's name in the manifest.
pub(crate) const VOLUME_LABEL: &str = "mxv.volume";

/// The number of digits of the store id that project names hold.
const SHORT_ID: usize = 8;

/// The number of hex digits of a container id.
const ID_DIGITS: usize = 64;

/// The text of a name that a constructor refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BadDockerName(pub(crate) String);

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

/// The name of a workspace's compose project, `mxv-<id8>-<workspace>`,
/// which only [`Scope::project`] builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectName(String);

name_type!(ProjectName);

/// The name of a manifest container, which is also its compose service's
/// name: 1 to [`MOST_NAME_BYTES`] ASCII letters, digits, `-` and `_`,
/// starting with a letter or a digit. These are the names that a manifest
/// accepts, and none holds a `.`, so the part of a container or volume
/// name after the scope's `.` is always the service or volume.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ServiceName(String);

name_type!(ServiceName, as_str);

impl ServiceName {
    /// Returns the service name `text`.
    ///
    /// # Errors
    ///
    /// Returns [`BadDockerName`] when `text` breaks the rules of the type.
    pub(crate) fn new(text: &str) -> Result<Self, BadDockerName> {
        let valid = text.len() <= MOST_NAME_BYTES
            && text
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphanumeric())
            && text
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character));
        if valid {
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
        let valid = text.len() == ID_DIGITS
            && text
                .chars()
                .all(|digit| digit.is_ascii_digit() || ('a'..='f').contains(&digit));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(BadDockerName(text.to_owned()))
        }
    }
}

/// One workspace of one store: its compose project and the labels and
/// names of its containers and volumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scope {
    /// The store id.
    store: String,
    /// The workspace.
    workspace: WorkspaceName,
}

impl Scope {
    /// Returns the scope of the workspace `workspace` in the store with the
    /// id `store`.
    pub(crate) fn new(store: &str, workspace: &WorkspaceName) -> Self {
        Self {
            store: store.to_owned(),
            workspace: workspace.clone(),
        }
    }

    /// Returns the name of the scope's compose project,
    /// `mxv-<id8>-<workspace>`, with the workspace's `_` written `__` and
    /// its `.` written `_d`.
    pub(crate) fn project(&self) -> ProjectName {
        let short = self.store.get(..SHORT_ID).unwrap_or(&self.store);
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

    /// Returns the docker name of the scope's object `object`,
    /// `<project>.<object>`.
    pub(super) fn object_name(&self, object: &str) -> String {
        format!("{}{object}", self.prefix())
    }

    /// Returns the name of the container of the service `service`.
    pub(crate) fn container_name(&self, service: &ServiceName) -> ContainerName {
        ContainerName(self.object_name(service.as_str()))
    }

    /// Returns the name of the volume `volume`, a volume name of the
    /// manifest.
    pub(crate) fn volume_name(&self, volume: &str) -> VolumeName {
        VolumeName(self.object_name(volume))
    }

    /// Returns the labels of the scope's object `object` under `label`.
    pub(super) fn labels(&self, label: &str, object: &str) -> Value {
        json!({
            STORE_LABEL: self.store,
            WORKSPACE_LABEL: self.workspace.as_str(),
            label: object,
        })
    }

    /// Reports whether `labels`, as [`super::objects`] prints them with
    /// tabs between the store, workspace and object labels, name this
    /// scope and the object `object`.
    pub(super) fn owns(&self, labels: &str, object: &str) -> bool {
        let mut fields = labels.split('\t');
        fields.next() == Some(self.store.as_str())
            && fields.next() == Some(self.workspace.as_str())
            && fields.next() == Some(object)
    }
}
