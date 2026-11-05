//! The JSON form of a [`ProcessPlan`], which writes the process's kind as a
//! plain `kind` field beside a `once` field that only a oneshot has.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::probe::Probe;
use super::restart::{Restart, Stop};
use super::{ProcessKind, ProcessPlan, SecretDelivery, default_ready_timeout};
use crate::name::{PortName, ProcessName, VolumeName};

/// The name of a process's kind in JSON.
#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum KindName {
    /// [`ProcessKind::LongRunning`].
    #[default]
    LongRunning,
    /// [`ProcessKind::Oneshot`].
    Oneshot,
}

/// The JSON form of a [`ProcessPlan`]. Each field but `kind` and `once` is
/// the field of the same name, documented there.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessFile {
    /// The process's name.
    name: ProcessName,
    /// Whether the process keeps running or runs to completion.
    #[serde(default)]
    kind: KindName,
    /// Whether a oneshot runs only on the first `up`. Only a oneshot has it,
    /// and it is false when a oneshot omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    once: Option<bool>,
    /// The command.
    run: Vec<String>,
    /// The processes this one waits for.
    #[serde(default)]
    depends: Vec<ProcessName>,
    /// The working directory.
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// The variables' names.
    #[serde(default)]
    env: BTreeSet<String>,
    /// The listen port.
    #[serde(default)]
    port: Option<PortName>,
    /// The published ports.
    #[serde(default)]
    publish: BTreeMap<PortName, NonZeroU16>,
    /// The volumes.
    #[serde(default)]
    volumes: BTreeMap<VolumeName, PathBuf>,
    /// The readiness probe.
    #[serde(default)]
    ready: Option<Probe>,
    /// The ready timeout.
    #[serde(
        default = "default_ready_timeout",
        rename = "ready_timeout_ms",
        with = "crate::millis"
    )]
    ready_timeout: Duration,
    /// The restart policy.
    #[serde(default)]
    restart: Restart,
    /// The stop handling.
    #[serde(default)]
    stop: Stop,
    /// How a container process receives its secrets.
    #[serde(default, skip_serializing_if = "SecretDelivery::is_file")]
    secret_delivery: SecretDelivery,
    /// The owner of the secret files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret_owner: Option<String>,
}

/// A process that is not a oneshot states `once`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnceWithoutOneshot;

impl fmt::Display for OnceWithoutOneshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a process that is not a oneshot states `once`")
    }
}

impl std::error::Error for OnceWithoutOneshot {}

impl TryFrom<ProcessFile> for ProcessPlan {
    type Error = OnceWithoutOneshot;

    fn try_from(file: ProcessFile) -> Result<Self, OnceWithoutOneshot> {
        let kind = match (file.kind, file.once) {
            (KindName::LongRunning, Some(_)) => return Err(OnceWithoutOneshot),
            (KindName::LongRunning, None) => ProcessKind::LongRunning,
            (KindName::Oneshot, once) => ProcessKind::Oneshot {
                once: once.unwrap_or(false),
            },
        };
        Ok(Self {
            name: file.name,
            kind,
            run: file.run,
            depends: file.depends,
            cwd: file.cwd,
            env: file.env,
            port: file.port,
            publish: file.publish,
            volumes: file.volumes,
            ready: file.ready,
            ready_timeout: file.ready_timeout,
            restart: file.restart,
            stop: file.stop,
            secret_delivery: file.secret_delivery,
            secret_owner: file.secret_owner,
        })
    }
}

impl From<ProcessPlan> for ProcessFile {
    fn from(process: ProcessPlan) -> Self {
        let (kind, once) = match process.kind {
            ProcessKind::LongRunning => (KindName::LongRunning, None),
            ProcessKind::Oneshot { once } => (KindName::Oneshot, Some(once)),
        };
        Self {
            name: process.name,
            kind,
            once,
            run: process.run,
            depends: process.depends,
            cwd: process.cwd,
            env: process.env,
            port: process.port,
            publish: process.publish,
            volumes: process.volumes,
            ready: process.ready,
            ready_timeout: process.ready_timeout,
            restart: process.restart,
            stop: process.stop,
            secret_delivery: process.secret_delivery,
            secret_owner: process.secret_owner,
        }
    }
}
