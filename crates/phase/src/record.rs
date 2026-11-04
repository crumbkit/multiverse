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
        /// When the process started, in microseconds since the Unix epoch.
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
    /// `started_at_us` microseconds since the Unix epoch.
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

/// What `sweep` found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Swept {
    /// One entry per process or container that the run record names.
    pub items: Vec<SweptItem>,
}

impl Swept {
    /// Returns the result of a sweep that found `items`.
    #[must_use]
    pub const fn new(items: Vec<SweptItem>) -> Self {
        Self { items }
    }
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
    /// How phase found it.
    pub handle: Handle,
    /// What `sweep` did.
    pub outcome: SweepOutcome,
}

impl SweptItem {
    /// Returns the item for `process` of `app`, found by `handle`, to which
    /// `sweep` did `outcome`.
    #[must_use]
    pub const fn new(
        app: AppName,
        process: ProcessName,
        handle: Handle,
        outcome: SweepOutcome,
    ) -> Self {
        Self {
            app,
            process,
            handle,
            outcome,
        }
    }
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
