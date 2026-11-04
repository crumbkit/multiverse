//! What the host program supplies to the container engine: its durable
//! files, its locks, its port check and its log followers.
//!
//! The engine decides which files, locks and followers a workspace's
//! containers need, and the host program keeps them in its own state
//! directory with its own errors. mxv supplies them from its store and its
//! process module, so the files, locks and followers that its other
//! commands read keep their form.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::engine::container::ContainerError;
use crate::name::{WorkspaceId, WorkspaceName};

/// A JSON file of the run directory whose version the host checks.
pub trait Versioned {
    /// The file's version, or `None` for a file without one.
    fn version(&self) -> Option<u32>;
}

/// A set of names has no version.
impl Versioned for BTreeSet<String> {
    fn version(&self) -> Option<u32> {
        None
    }
}

/// A detached `docker logs --follow` that the host starts for a
/// container, from [`super::Lifecycle`].
#[derive(Debug)]
#[non_exhaustive]
pub struct Follow<'a> {
    /// The container's name in the manifest, for errors.
    pub name: &'a str,
    /// The `docker logs` program and its arguments.
    pub argv: &'a [String],
    /// The directory that the follower starts in.
    pub cwd: &'a Path,
    /// The log that the follower appends to.
    pub log: &'a Path,
    /// The file that the follower writes its exit code to.
    pub exit_file: &'a Path,
    /// The file that records the follower, so that
    /// [`Host::follower_alive`] finds it.
    pub record: &'a Path,
}

/// The host program's side of the container engine.
pub trait Host {
    /// The host program's error, which every failure of the engine's own
    /// steps converts into.
    type Error: From<ContainerError>;

    /// A held lock, which the host releases when it drops.
    type Lock;

    /// Reads the JSON file at `path`, or returns `None` when it does not
    /// exist.
    ///
    /// # Errors
    ///
    /// Returns the host's error for a file that cannot be read, does not
    /// parse, or has a version that the host does not read.
    fn read_json<T: DeserializeOwned + Versioned>(
        &self,
        path: &Path,
    ) -> Result<Option<T>, Self::Error>;

    /// Writes `value` in JSON to the file at `path`, replacing it whole.
    ///
    /// # Errors
    ///
    /// Returns the host's error when the file cannot be written.
    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), Self::Error>;

    /// Deletes the file at `path`, and succeeds when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns the host's error when the file cannot be deleted.
    fn remove_file(&self, path: &Path) -> Result<(), Self::Error>;

    /// Moves the log at `log` to `older`, replacing an older log there. A
    /// missing log succeeds.
    ///
    /// # Errors
    ///
    /// Returns the host's error when the move fails.
    fn rotate_log(&self, log: &Path, older: &Path) -> Result<(), Self::Error>;

    /// Takes the exclusive lock on the file at `path`, waiting up to
    /// `wait` while another holder has it.
    ///
    /// # Errors
    ///
    /// Returns the host's error when another holder keeps the lock for all
    /// of `wait`, or the file cannot be locked.
    fn lock(&self, path: &Path, wait: Duration) -> Result<Self::Lock, Self::Error>;

    /// Returns the stable id that the record of `workspace` holds on disk.
    /// When the record holds none, the host mints one and saves the record
    /// with it first. The engine labels docker objects only with an id that
    /// this returned, through [`super::SavedWorkspaceId`], so no object is
    /// ever labelled with an id that the record does not hold.
    ///
    /// # Errors
    ///
    /// Returns the host's error when the record cannot be read or written,
    /// or the workspace has no record.
    fn saved_workspace_id(&self, workspace: &WorkspaceName) -> Result<WorkspaceId, Self::Error>;

    /// Reports whether something on the host holds the TCP port `port`.
    fn port_taken(&self, port: u16) -> bool;

    /// Reports whether the follower that the file `record` names still
    /// runs. A missing record names no follower.
    ///
    /// # Errors
    ///
    /// Returns the host's error when the record cannot be read.
    fn follower_alive(&self, record: &Path) -> Result<bool, Self::Error>;

    /// Starts the detached follower `follow`, writes its record and leaves
    /// it running.
    ///
    /// # Errors
    ///
    /// Returns the host's error when the follower cannot start or its
    /// record cannot be written. A follower whose record cannot be written
    /// is stopped.
    fn follow(&self, follow: &Follow<'_>) -> Result<(), Self::Error>;
}
