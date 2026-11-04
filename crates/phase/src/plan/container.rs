//! The options of a container app that the host program supplies (decision
//! P14): the host program builds the image, and phase runs it with the
//! app's checkout bind-mounted, the cache volumes the plan names, and the
//! user the plan names.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::name::VolumeName;

/// The bind mounts, cache volumes and run-as user of a container app. They
/// apply to every process of the app.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ContainerOptions {
    /// The host directories mounted into each container, such as the app's
    /// checkout.
    #[serde(default)]
    pub mounts: Vec<BindMount>,
    /// The workspace's cache volumes, each mounted at an absolute path
    /// inside each container, such as a package manager's download cache.
    /// A cache volume survives `down` like any other volume.
    #[serde(default)]
    pub caches: BTreeMap<VolumeName, PathBuf>,
    /// The user that the containers run as, in Docker's `USER[:GROUP]`
    /// form, such as `1000:1000`, or `None` to keep the image's user.
    #[serde(default)]
    pub user: Option<String>,
}

/// A host directory mounted into a container.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct BindMount {
    /// The absolute path of the host directory.
    pub source: PathBuf,
    /// The absolute path inside the container.
    pub target: PathBuf,
    /// Whether the container may only read the directory.
    pub read_only: bool,
}

impl BindMount {
    /// Returns the mount of the host directory `source` at `target` inside
    /// the container, which the container may only read when `read_only`.
    #[must_use]
    pub const fn new(source: PathBuf, target: PathBuf, read_only: bool) -> Self {
        Self {
            source,
            target,
            read_only,
        }
    }
}
