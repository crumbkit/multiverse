//! The run record and sweep.
//!
//! phase writes a run record (`phase/run/v1`) for each workspace, keyed by
//! its stable id, before it creates anything. The record names every
//! keeper and app process by pid, process start time and process group,
//! every container, network and volume by id, and the hash of the plan that
//! started them. It holds no environment value. The `store` module writes
//! the record atomically (DUR-1) and takes the workspace's lifecycle lock
//! (DUR-2), and `sweep` reads the record and stops what a crashed run left
//! behind, removing only what the record names and still matches (DUR-4).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, ProcessName};

pub(crate) mod docker;
pub(crate) mod process;
pub(crate) mod run;
pub(crate) mod store;
pub(crate) mod sweep;
#[cfg(test)]
pub(crate) mod testing;

pub use crate::record::docker::{DockerError, MOST_DOCKER_MESSAGE_BYTES};

#[cfg(test)]
mod tests;

/// How phase finds a running process: by pid and start time for a host
/// process, or by container id for a container process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
#[non_exhaustive]
pub enum Handle {
    /// A host process, which phase matches by its pid and its start time
    /// together, so a reused pid is never mistaken for it.
    #[non_exhaustive]
    Pid {
        /// The process id.
        pid: u32,
        /// When the process started, in microseconds: since the Unix epoch
        /// on macOS, and since the machine booted on Linux, where the
        /// kernel counts a process's start in clock ticks. phase only
        /// compares it with the start time it reads again.
        started_at_us: u64,
    },
    /// A container, by its immutable id.
    #[non_exhaustive]
    Container {
        /// The container's id.
        id: ContainerId,
    },
}

impl Handle {
    /// Returns the handle of the host process `pid` that started at
    /// `started_at_us`, in the unit that [`Handle::Pid`] describes.
    #[must_use]
    pub const fn pid(pid: u32, started_at_us: u64) -> Self {
        Self::Pid { pid, started_at_us }
    }

    /// Returns the handle of the container `id`.
    #[must_use]
    pub const fn container(id: ContainerId) -> Self {
        Self::Container { id }
    }
}

/// The immutable id of a Docker container: 64 lowercase hexadecimal digits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContainerId(String);

impl ContainerId {
    /// Returns the id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The text is not 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadContainerId;

impl fmt::Display for BadContainerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a container id is 64 lowercase hexadecimal digits")
    }
}

impl std::error::Error for BadContainerId {}

impl TryFrom<String> for ContainerId {
    type Error = BadContainerId;

    fn try_from(text: String) -> Result<Self, BadContainerId> {
        hex_id(text).map(Self).map_err(|BadHex| BadContainerId)
    }
}

/// The text is not 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BadHex;

impl fmt::Display for BadHex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the text is not 64 lowercase hexadecimal digits")
    }
}

/// Returns `text` when it is 64 lowercase hexadecimal digits, the form of a
/// Docker id and of a 32-byte digest.
pub(crate) fn hex_id(text: String) -> Result<String, BadHex> {
    if text.len() == 64
        && text
            .chars()
            .all(|character| matches!(character, '0'..='9' | 'a'..='f'))
    {
        Ok(text)
    } else {
        Err(BadHex)
    }
}

/// The most bytes of a [`DockerName`]. A longer name is refused.
pub const MOST_DOCKER_NAME_BYTES: usize = 128;

/// The name that phase gives a Docker container, network or volume, such
/// as `mxv-0b9f3c8e-db-server-1`. It holds 1 to
/// [`MOST_DOCKER_NAME_BYTES`] ASCII letters, digits, `_`, `.` and `-`, and
/// starts with a letter or a digit, which is the rule Docker applies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DockerName(String);

impl DockerName {
    /// Returns the name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DockerName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The text breaks the rule of a [`DockerName`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadDockerName;

impl fmt::Display for BadDockerName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a Docker name holds 1 to {MOST_DOCKER_NAME_BYTES} ASCII letters, digits, `_`, `.` and `-`, and starts with a letter or a digit"
        )
    }
}

impl std::error::Error for BadDockerName {}

impl TryFrom<String> for DockerName {
    type Error = BadDockerName;

    fn try_from(text: String) -> Result<Self, BadDockerName> {
        let starts_well = text
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric());
        let rest_well = text.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
        });
        if starts_well && rest_well && text.len() <= MOST_DOCKER_NAME_BYTES {
            Ok(Self(text))
        } else {
            Err(BadDockerName)
        }
    }
}

impl std::str::FromStr for DockerName {
    type Err = BadDockerName;

    fn from_str(text: &str) -> Result<Self, BadDockerName> {
        Self::try_from(text.to_owned())
    }
}

impl From<DockerName> for String {
    fn from(name: DockerName) -> Self {
        name.0
    }
}

impl From<ContainerId> for String {
    fn from(id: ContainerId) -> Self {
        id.0
    }
}

/// What `sweep` found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Swept {
    /// One entry per keeper, process or container that the run record
    /// names.
    pub items: Vec<SweptItem>,
    /// One entry per network and volume that the run record names.
    pub objects: Vec<SweptObject>,
}

impl Swept {
    /// Returns the result of a sweep that found `items` and `objects`.
    #[must_use]
    pub const fn new(items: Vec<SweptItem>, objects: Vec<SweptObject>) -> Self {
        Self { items, objects }
    }
}

/// Whether a swept host process is an app's process or the keeper of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Role {
    /// The keeper of the process.
    Keeper,
    /// The process, on the host or in a container.
    Process,
}

/// One process or container that the run record names, and what `sweep`
/// did to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SweptItem {
    /// The app.
    pub app: AppName,
    /// The process.
    pub process: ProcessName,
    /// Whether the item is the process or its keeper.
    pub role: Role,
    /// How phase found it, or `None` when the run record names it as
    /// planned and nothing of it was found.
    pub handle: Option<Handle>,
    /// What `sweep` did.
    pub outcome: SweepOutcome,
}

impl SweptItem {
    /// Returns the item for `process` of `app` in `role`, found by
    /// `handle`, to which `sweep` did `outcome`.
    #[must_use]
    pub const fn new(
        app: AppName,
        process: ProcessName,
        role: Role,
        handle: Option<Handle>,
        outcome: SweepOutcome,
    ) -> Self {
        Self {
            app,
            process,
            role,
            handle,
            outcome,
        }
    }
}

/// A kind of Docker object that is not a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ObjectKind {
    /// A Docker network.
    Network,
    /// A Docker volume.
    Volume,
}

/// One network or volume that the run record names, and what `sweep` did
/// to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SweptObject {
    /// The object's kind.
    pub kind: ObjectKind,
    /// The object's name.
    pub name: DockerName,
    /// What `sweep` did.
    pub outcome: SweepOutcome,
}

impl SweptObject {
    /// Returns the object of `kind` named `name`, to which `sweep` did
    /// `outcome`.
    #[must_use]
    pub const fn new(kind: ObjectKind, name: DockerName, outcome: SweepOutcome) -> Self {
        Self {
            kind,
            name,
            outcome,
        }
    }
}

/// What `sweep` did to one recorded process or Docker object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SweepOutcome {
    /// `sweep` stopped it and checked that it is gone.
    Stopped,
    /// `sweep --dry-run` would stop it.
    WouldStop,
    /// It was already gone.
    Gone,
    /// The pid is in use by a process with another start time, so it is not
    /// the recorded process, and `sweep` left it alone.
    PidReused,
    /// phase may not read the process that has the pid, as inside a
    /// sandbox, so it cannot tell whether that is the recorded process.
    /// `sweep` left it alone, and the run record keeps it unchanged.
    ///
    /// `sweep` also reports this for a host entry whose recorded pid or
    /// group is 0 or 1, which phase never signals. Such an entry stays in
    /// the run record, and every later `sweep` reports it again, until
    /// someone removes it from the record by hand.
    Unknown,
    /// It was still there after `KILL` and the kill wait, or after Docker
    /// removed it, so the run record keeps it and `sweep` can try again.
    Unstoppable,
    /// It is a volume that holds the workspace's data, which `sweep`
    /// leaves. Only `reset` removes it.
    Kept,
}
