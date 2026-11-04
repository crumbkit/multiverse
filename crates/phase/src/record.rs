//! The run record and sweep.
//!
//! phase writes a run record (`phase/run/v1`) for each workspace, keyed by
//! its stable id, before it creates anything. The record names every
//! keeper and app process by pid, process start time and process group,
//! every container, network and volume by id, and the hash of the plan that
//! started them. It holds no environment value. This module will write the
//! record atomically (DUR-1), take the workspace's lifecycle lock (DUR-2),
//! and remove only what the record and the current owner agree on (DUR-4).
//! `sweep` reads the record and stops what a crashed run left behind.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, ProcessName};

/// How phase finds a running process: by pid for a host process, or by
/// container id for a container process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Handle {
    /// A host process's pid.
    Pid(u32),
    /// A container's immutable id.
    Container(ContainerId),
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
        if text.len() == 64
            && text
                .chars()
                .all(|character| matches!(character, '0'..='9' | 'a'..='f'))
        {
            Ok(Self(text))
        } else {
            Err(BadContainerId)
        }
    }
}

impl From<ContainerId> for String {
    fn from(id: ContainerId) -> Self {
        id.0
    }
}

// Only phase builds a sweep result, so it has no constructor. It is
// non-exhaustive because LIB-9 says adding a field to an exhaustive public
// type breaks every dependant that builds or matches it.
/// What `sweep` found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Swept {
    /// One entry per process or container that the run record names.
    pub items: Vec<SweptItem>,
}

// Only phase builds a swept item, so it has no constructor. It is
// non-exhaustive because LIB-9 says adding a field to an exhaustive public
// type breaks every dependant that builds or matches it.
/// One process or container that the run record names, and what `sweep`
/// did to it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SweptItem {
    /// The app.
    pub app: AppName,
    /// The process.
    pub process: ProcessName,
    /// How phase found it.
    pub handle: Handle,
    /// What `sweep` did.
    pub outcome: SweepOutcome,
}

/// What `sweep` did to one recorded process or container.
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
}
