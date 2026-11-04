//! Which manifest a workspace reads.
//!
//! A workspace may hold a manifest of its own at `<workspace>/mxv.toml`,
//! beside its `AGENTS.md` and above its checkout, which `mxv new` writes
//! from `--manifest` or from a template. Every command reads exactly one
//! manifest, from the first of these that exists:
//!
//! 1. the workspace manifest, [`Workspace::manifest_file`];
//! 2. the `mxv.toml` at the root of the checkout;
//! 3. none, and the workspace is bare.
//!
//! The two files are never merged. Paths in either, such as a process's
//! `cwd` or a `disposable` glob, are relative to the checkout.
//! [`ManifestInputs`] names both files, for a tool that reads them as
//! separate inputs even when the workspace manifest wins.

use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::{MANIFEST_FILE, Manifest, ManifestError};
use crate::record::{Checkout, Workspace};

#[cfg(test)]
mod tests;

/// Which file a workspace's manifest comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestSource {
    /// The workspace manifest at `<workspace>/mxv.toml`.
    Workspace,
    /// The `mxv.toml` at the root of the workspace's checkout.
    Repository,
    /// Neither file exists, so the workspace is bare.
    None,
}

/// The two manifest files of a workspace that exist, as separate inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestInputs {
    /// The workspace manifest, when it exists.
    pub workspace: Option<PathBuf>,
    /// The checkout's `mxv.toml`, when it exists.
    pub repository: Option<PathBuf>,
}

impl ManifestInputs {
    /// Returns which file the workspace reads: the workspace manifest when
    /// it exists, or else the checkout's `mxv.toml`, or else none.
    #[must_use]
    pub const fn source(&self) -> ManifestSource {
        if self.workspace.is_some() {
            ManifestSource::Workspace
        } else if self.repository.is_some() {
            ManifestSource::Repository
        } else {
            ManifestSource::None
        }
    }

    /// Returns the path of the file that the workspace reads, or `None`
    /// when it reads none.
    #[must_use]
    pub fn chosen(&self) -> Option<&Path> {
        self.workspace.as_deref().or(self.repository.as_deref())
    }
}

/// Reports whether something is at `path`, without following a symbolic
/// link, so a link that leads nowhere still counts and fails when it is
/// read, instead of letting the next file take its place.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

impl Workspace {
    /// Returns the path of the workspace manifest, `<workspace>/mxv.toml`,
    /// whether or not it exists.
    #[must_use]
    pub fn manifest_file(&self) -> PathBuf {
        self.path.join(MANIFEST_FILE)
    }

    /// Returns the checkout whose `mxv.toml` the workspace reads, and that
    /// the paths of either manifest are relative to: the first checkout.
    /// A workspace holds one checkout for now.
    #[must_use]
    pub fn manifest_checkout(&self) -> Option<&Checkout> {
        self.checkouts.first()
    }

    /// Returns the workspace's manifest files that exist, for `checkout`,
    /// or for [`Workspace::manifest_checkout`] when `checkout` is `None`.
    #[must_use]
    pub fn manifest_inputs_for(&self, checkout: Option<&Checkout>) -> ManifestInputs {
        let workspace = Some(self.manifest_file()).filter(|path| exists(path));
        let repository = checkout
            .or_else(|| self.manifest_checkout())
            .map(|checkout| checkout.path.join(MANIFEST_FILE))
            .filter(|path| exists(path));
        ManifestInputs {
            workspace,
            repository,
        }
    }

    /// Returns the workspace's manifest files that exist, as separate
    /// inputs.
    #[must_use]
    pub fn manifest_inputs(&self) -> ManifestInputs {
        self.manifest_inputs_for(None)
    }

    /// Returns the one manifest that the workspace reads, as the module
    /// docs describe, or `None` when the workspace is bare.
    ///
    /// # Errors
    ///
    /// Returns the [`ManifestError`] of the chosen file, which never falls
    /// back to the other one.
    pub fn manifest(&self) -> Result<Option<Manifest>, ManifestError> {
        self.manifest_for(None)
    }

    /// Returns the manifest that `checkout` reads as [`Workspace::manifest`]
    /// does, or that [`Workspace::manifest_checkout`] reads when `checkout`
    /// is `None`, with its paths checked against that checkout.
    ///
    /// # Errors
    ///
    /// Returns the [`ManifestError`] of the chosen file.
    pub fn manifest_for(
        &self,
        checkout: Option<&Checkout>,
    ) -> Result<Option<Manifest>, ManifestError> {
        let checkout = checkout.or_else(|| self.manifest_checkout());
        let inputs = self.manifest_inputs_for(checkout);
        let Some(path) = inputs.chosen() else {
            return Ok(None);
        };
        let root = checkout.map_or(self.path.as_path(), |checkout| checkout.path.as_path());
        Manifest::read_file(path, root)
    }
}
