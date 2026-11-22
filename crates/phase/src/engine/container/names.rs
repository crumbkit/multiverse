//! The names of docker objects: a [`Scope`] and the names that it gives a
//! workspace's compose project, containers and volumes, and the names and
//! ids that the engine reads back from docker.
//!
//! Each type holds text that its constructor checked, so a function that
//! takes one never receives a name that docker would read as an option or
//! that would split at the wrong `.`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::PathBuf;

use serde_json::{Map, Value};

#[cfg(doc)]
use crate::name::MOST_NAME_BYTES;
use crate::name::{AppName, WorkspaceId, is_unit_name};

/// The prefix of the label keys that phase gives the docker objects it
/// creates, such as `mxv.` in `mxv.workspace-id`. A program that links
/// phase under its own name gives phase its own prefix with
/// [`Phase::with_labels`](crate::Phase::with_labels), so its objects carry
/// its name, and phase finds, sweeps and removes only the objects whose
/// labels start with its prefix. Without one, phase uses [`LabelPrefix::mxv`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LabelPrefix(Cow<'static, str>);

impl LabelPrefix {
    /// The most bytes of the name of a prefix, before its `.`. The longest
    /// label key is the name, the `.` and `workspace-id`, 45 bytes, far
    /// below docker's 255-byte limit on a label key.
    pub const MOST_NAME_BYTES: usize = 32;

    /// Returns the prefix `<name>.`, such as `mxv.` for `mxv`.
    ///
    /// # Errors
    ///
    /// Returns [`BadLabelPrefix`] when `name` is not 1 to
    /// [`LabelPrefix::MOST_NAME_BYTES`] bytes of lowercase ASCII letters,
    /// digits and `-`, starting with a letter.
    pub fn new(name: &str) -> Result<Self, BadLabelPrefix> {
        let valid = (1..=Self::MOST_NAME_BYTES).contains(&name.len())
            && name
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase())
            && name.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            });
        if valid {
            Ok(Self(Cow::Owned(format!("{name}."))))
        } else {
            Err(BadLabelPrefix(name.to_owned()))
        }
    }

    /// Returns mxv's prefix, `mxv.`, which phase uses unless it is given
    /// another.
    #[must_use]
    pub const fn mxv() -> Self {
        Self(Cow::Borrowed("mxv."))
    }

    /// Returns the prefix as text, such as `mxv.`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the key of the label `label` under this prefix, such as
    /// `mxv.workspace-id`.
    pub(crate) fn key(&self, label: Label) -> String {
        format!("{}{}", self.0, label.suffix())
    }
}

/// The text that [`LabelPrefix::new`] refused as the name of a prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadLabelPrefix(String);

impl fmt::Display for BadLabelPrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the label prefix {:?} is refused: a label prefix is 1 to {} bytes of lowercase ASCII letters, digits and `-`, starting with a letter",
            self.0,
            LabelPrefix::MOST_NAME_BYTES
        )
    }
}

impl std::error::Error for BadLabelPrefix {}

impl Default for LabelPrefix {
    fn default() -> Self {
        Self::mxv()
    }
}

impl fmt::Display for LabelPrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A label that phase gives the docker objects it creates. Its key is a
/// [`LabelPrefix`] followed by the label's suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Label {
    /// The label that names a container's name in the manifest.
    Container,
    /// The label that names a volume's name in the manifest.
    Volume,
    /// The label that names the stable id of an object's workspace, which
    /// phase's `sweep` matches an object by.
    WorkspaceId,
    /// The label that names the absolute path of the run record of an
    /// object's workspace.
    RunRecord,
    /// The label that names an object's app.
    App,
    /// The label that names the role of a short-lived container that
    /// phase runs for itself, apart from an app's processes.
    Role,
    /// The label of a container whose process receives secret files.
    Secrets,
}

impl Label {
    /// Returns the part of the label's key after the prefix, such as
    /// `workspace-id`.
    const fn suffix(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::WorkspaceId => "workspace-id",
            Self::RunRecord => "run-record",
            Self::App => "app",
            Self::Role => "role",
            Self::Secrets => "secrets",
        }
    }
}

/// The value of [`Label::Role`] on a helper of the volume-owner step, which
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
    /// The prefix of the keys of the labels that name the workspace.
    pub(crate) labels: LabelPrefix,
}

impl Identity {
    /// Reports whether `labels`, an object's labels, name this workspace:
    /// the object's `<prefix>workspace-id` label equals the workspace's
    /// stable id and its `<prefix>run-record` label equals the path of the
    /// workspace's run record. Each value must equal the expected one
    /// exactly, with nothing trimmed. The run record is the lossy UTF-8
    /// string that [`Scope::labels`] writes, so a state directory reached
    /// by another path than the one that made an object finds that object
    /// foreign, and leaves it alone.
    pub(crate) fn names(&self, labels: &BTreeMap<String, String>) -> bool {
        let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
        is(&self.labels.key(Label::WorkspaceId), self.id.as_str())
            && is(
                &self.labels.key(Label::RunRecord),
                &self.run_record.to_string_lossy(),
            )
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
/// workspace shares, are `phase-<hash>.<volume>`. Under the prefix `mxv.`,
/// its containers carry the labels `mxv.workspace-id`, `mxv.run-record`,
/// `mxv.app` and `mxv.container`, and its volumes carry `mxv.workspace-id`,
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

    /// Returns the labels of the scope's object `object` under `label`,
    /// [`Label::Container`] or [`Label::Volume`]: the workspace's stable
    /// id, run record and object labels, and the app label to a container.
    pub(super) fn labels(&self, label: Label, object: &str) -> Value {
        let mut labels = Map::new();
        let mut put = |key: Label, value: &str| {
            labels.insert(
                self.identity.labels.key(key),
                Value::String(value.to_owned()),
            );
        };
        put(label, object);
        put(Label::WorkspaceId, self.identity.id.as_str());
        put(
            Label::RunRecord,
            &self.identity.run_record.to_string_lossy(),
        );
        if label == Label::Container {
            put(Label::App, self.app.as_str());
        }
        Value::Object(labels)
    }

    /// Returns the labels of a volume-owner helper, as `(label, value)`
    /// pairs in label order: the workspace's stable id, its run record and
    /// the app, [`Label::Role`] set to [`VOLUME_OWNER_ROLE`], and the
    /// volume that the helper mounts when it mounts one. A helper carries
    /// no `<prefix>container` label, so nothing that finds an app's
    /// containers by their process finds it.
    pub(super) fn helper_labels(&self, volume: Option<&VolumeKey>) -> BTreeMap<String, String> {
        let key = |label: Label| self.identity.labels.key(label);
        let mut labels = BTreeMap::new();
        labels.insert(key(Label::App), self.app.as_str().to_owned());
        labels.insert(
            key(Label::WorkspaceId),
            self.identity.id.as_str().to_owned(),
        );
        labels.insert(
            key(Label::RunRecord),
            self.identity.run_record.to_string_lossy().into_owned(),
        );
        labels.insert(key(Label::Role), VOLUME_OWNER_ROLE.to_owned());
        if let Some(volume) = volume {
            labels.insert(key(Label::Volume), volume.as_str().to_owned());
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
    /// as [`Label::Container`], the object `object`. The object names the
    /// workspace's stable id and the run record of this scope's state
    /// directory, and a container also names its app. Each value must equal
    /// the expected one exactly, with nothing trimmed. The run record is
    /// the lossy UTF-8 string that [`Scope::labels`] writes, so a state
    /// directory reached by another path than the one that made an object
    /// finds that object foreign, and keeps it.
    pub(super) fn owns(&self, labels: &str, label: Label, object: &str) -> bool {
        let Ok(Some(labels)) = serde_json::from_str::<Option<BTreeMap<String, String>>>(labels)
        else {
            return false;
        };
        let is = |key: &str, value: &str| labels.get(key).is_some_and(|found| found == value);
        self.identity.names(&labels)
            && (label != Label::Container
                || is(&self.identity.labels.key(Label::App), self.app.as_str()))
            && is(&self.identity.labels.key(label), object)
    }
}
