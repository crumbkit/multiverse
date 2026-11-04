//! The paths of a workspace's run directory.
//!
//! A workspace's run directory is `<runs>/<workspace>/`. Each process or
//! container `<name>` of the workspace has files `<name>.<extension>`
//! there, such as its log `<name>.log`, and `.lock` is the lock that `up`
//! and `down` hold. [`RunDir`] is the one place that builds these paths,
//! for the container engine and for mxv's host processes.

use std::path::{Path, PathBuf};

use crate::name::WorkspaceName;

#[cfg(test)]
mod tests;

/// The run directory of one workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunDir(PathBuf);

impl RunDir {
    /// Returns the run directory of the workspace `workspace` in the runs
    /// directory `runs`.
    #[must_use]
    pub fn new(runs: &Path, workspace: &WorkspaceName) -> Self {
        Self(runs.join(workspace.as_str()))
    }

    /// Returns the directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Returns the path of the file of the process or container `unit`
    /// with `extension`, `<unit>.<extension>`.
    #[must_use]
    pub fn file(&self, unit: &str, extension: &str) -> PathBuf {
        self.0.join(format!("{unit}.{extension}"))
    }

    /// Returns the log of the process or container `unit`, `<unit>.log`.
    #[must_use]
    pub fn log(&self, unit: &str) -> PathBuf {
        self.file(unit, "log")
    }

    /// Returns the older log of the process or container `unit`,
    /// `<unit>.log.1`, which holds the log of its previous start or the
    /// part of its log that a cap moved aside.
    #[must_use]
    pub fn older_log(&self, unit: &str) -> PathBuf {
        self.file(unit, "log.1")
    }

    /// Returns the lock file of the workspace, `.lock`.
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.0.join(".lock")
    }
}
