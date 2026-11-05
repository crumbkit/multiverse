//! Which manifest a workspace reads.
//!
//! A workspace may hold a manifest of its own at `<workspace>/mxv.toml`,
//! beside its `AGENTS.md` and above its checkouts, which `mxv new` writes
//! from `--manifest` or from a template.
//!
//! A workspace with one checkout reads exactly one manifest, from the
//! first of these that exists:
//!
//! 1. the workspace manifest, [`Workspace::manifest_file`];
//! 2. the `mxv.toml` at the root of the checkout;
//! 3. none, and the workspace is bare.
//!
//! The two files are never merged. Paths in either, such as a process's
//! `cwd` or a `disposable` glob, are relative to the checkout.
//!
//! A workspace with several checkouts reads a merged manifest, as
//! `crates/mxv/docs/decisions/multi-repository-manifests.md` describes:
//! the workspace manifest with its names as written and its paths relative
//! to the workspace's directory, and each checkout's own `mxv.toml` with
//! each process, container, port and volume renamed `<prefix>-<name>`, where the
//! prefix is [`checkout_prefix`] of the checkout's folder. Two equal names
//! fail with [`ManifestProblem::NameClash`]. Each process of the merged
//! manifest has an absolute `cwd`, so it runs in its own checkout.
//!
//! The workspace manifest is trusted as the person's own, because only
//! the person writes `<workspace>/`: mxv copies the file there, and a
//! sandbox that runs agents in the checkout must not let them write the
//! workspace's directory. Its `disposable` globs then count like the
//! config file's, which [`Workspace::manifest_trusted`] decides. A file
//! that is a symbolic link, or a hard link of another file, could be the
//! repository's, so it is read but not trusted. A record whose checkout
//! holds the workspace manifest's path has none, as
//! [`Workspace::checkout_holds_manifest_file`] reports.
//! [`ManifestInputs`] names the files, for a tool that reads them as
//! separate inputs even when the workspace manifest wins.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::manifest::{Joined, MANIFEST_FILE, Manifest, ManifestError, ManifestProblem};
use crate::name::MOST_NAME_BYTES;
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
    /// The workspace has several checkouts, and merges the workspace
    /// manifest and each checkout's `mxv.toml`.
    Merged,
    /// Neither file exists, so the workspace is bare.
    None,
}

/// The manifest files of a workspace that exist, as separate inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestInputs {
    /// The workspace manifest, when it exists.
    pub workspace: Option<PathBuf>,
    /// The checkout's `mxv.toml`, when it exists, for a workspace with one
    /// checkout or for the checkout that the inputs were asked for.
    pub repository: Option<PathBuf>,
    /// For a workspace with several checkouts, the `mxv.toml` of each
    /// checkout that has one, in the record's order. It is empty for a
    /// workspace with one checkout.
    pub checkouts: Vec<PathBuf>,
    /// Whether the workspace has several checkouts, so it merges its files.
    pub merged: bool,
}

impl ManifestInputs {
    /// Returns which file the workspace reads: for a workspace with one
    /// checkout, the workspace manifest when it exists, or else the
    /// checkout's `mxv.toml`, or else none; for a workspace with several
    /// checkouts, the merge of every file, or none when no file exists.
    #[must_use]
    pub fn source(&self) -> ManifestSource {
        if self.merged {
            if self.workspace.is_some() || !self.checkouts.is_empty() {
                ManifestSource::Merged
            } else {
                ManifestSource::None
            }
        } else if self.workspace.is_some() {
            ManifestSource::Workspace
        } else if self.repository.is_some() {
            ManifestSource::Repository
        } else {
            ManifestSource::None
        }
    }

    /// Returns the path of the first file that the workspace reads, or
    /// `None` when it reads none. For a merged manifest, it is the
    /// workspace manifest when it exists, or else the first checkout's
    /// file, and [`ManifestInputs::files`] names every file.
    #[must_use]
    pub fn chosen(&self) -> Option<&Path> {
        if self.merged {
            return self
                .workspace
                .as_deref()
                .or_else(|| self.checkouts.first().map(PathBuf::as_path));
        }
        self.workspace.as_deref().or(self.repository.as_deref())
    }

    /// Returns every file that the workspace reads: the one file of a
    /// workspace with one checkout, or every file of a merged manifest.
    #[must_use]
    pub fn files(&self) -> Vec<&Path> {
        if self.merged {
            self.workspace
                .as_deref()
                .into_iter()
                .chain(self.checkouts.iter().map(PathBuf::as_path))
                .collect()
        } else {
            self.chosen().into_iter().collect()
        }
    }
}

/// Returns the prefix of the names that the `mxv.toml` of the checkout
/// whose folder is `folder` declares, in a workspace with several
/// checkouts: the folder name in lower case, with each character other
/// than an ASCII letter, a digit, `-` or `_` written as `-`. The folder
/// `Web.App` has the prefix `web-app`.
#[must_use]
pub fn checkout_prefix(folder: &str) -> String {
    folder
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Returns the name that `name`, declared by the manifest of a checkout
/// with the prefix `prefix`, has in a merged manifest.
#[must_use]
pub fn prefixed(prefix: &str, name: &str) -> String {
    format!("{prefix}-{name}")
}

/// Reports whether something is at `path`, without following a symbolic
/// link, so a link that leads nowhere still counts and fails when it is
/// read, instead of letting the next file take its place.
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// The names that a merged manifest holds so far, each with the file that
/// declares it, for finding a clash.
#[derive(Default)]
struct Declared {
    /// The processes and containers.
    units: BTreeMap<String, PathBuf>,
    /// The ports.
    ports: BTreeMap<String, PathBuf>,
    /// The volumes, which phase names after the workspace alone, so two
    /// files that mount one name would share the volume's data.
    volumes: BTreeMap<String, PathBuf>,
}

impl Declared {
    /// Adds the names of `manifest`, read from `path`, or fails on the
    /// first name that another file declares or that is longer than a
    /// manifest name may be. Containers of one file may share a volume.
    fn add(&mut self, manifest: &Manifest, path: &Path) -> Result<(), ManifestError> {
        let units = manifest.processes.keys().chain(manifest.containers.keys());
        declare(&mut self.units, units, path)?;
        declare(&mut self.ports, manifest.ports.iter(), path)?;
        let volumes: BTreeSet<&String> = manifest
            .containers
            .values()
            .flat_map(|container| container.volumes.keys())
            .collect();
        declare(&mut self.volumes, volumes.into_iter(), path)
    }
}

/// Adds each of `added`, declared by the file `path`, to `names`, or fails
/// on the first name that `names` already holds or that is longer than a
/// manifest name may be.
fn declare<'a>(
    names: &mut BTreeMap<String, PathBuf>,
    added: impl Iterator<Item = &'a String>,
    path: &Path,
) -> Result<(), ManifestError> {
    let failure = |problem| ManifestError {
        path: path.to_path_buf(),
        line: None,
        problem,
    };
    for name in added {
        if name.len() > MOST_NAME_BYTES {
            return Err(failure(ManifestProblem::Invalid {
                message: format!(
                    "the merged name `{name}` is longer than {MOST_NAME_BYTES} bytes, so give its checkout a shorter folder name with `--as`"
                ),
            }));
        }
        if let Some(other) = names.get(name) {
            return Err(failure(ManifestProblem::NameClash {
                name: name.clone(),
                other: other.clone(),
            }));
        }
        names.insert(name.clone(), path.to_path_buf());
    }
    Ok(())
}

/// Moves the units, ports and warnings of `from` into `into`, with each
/// process's `cwd` and each write path of its permissions other than
/// `$STATE` made absolute below `root`.
fn absorb(into: &mut Manifest, from: Manifest, root: &Path) {
    for (name, mut process) in from.processes {
        process.cwd = if process.cwd == Path::new(".") {
            root.to_path_buf()
        } else {
            root.join(&process.cwd)
        };
        if let Some(permissions) = &mut process.permissions {
            permissions.write = permissions
                .write
                .iter()
                .map(|path| {
                    if path == crate::manifest::STATE {
                        path.clone()
                    } else {
                        root.join(path).display().to_string()
                    }
                })
                .collect();
        }
        into.processes.insert(name, process);
    }
    into.containers.extend(from.containers);
    into.ports.extend(from.ports);
    into.warnings.extend(from.warnings);
}

impl Workspace {
    /// Returns the path of the workspace manifest, `<workspace>/mxv.toml`,
    /// whether or not it exists.
    #[must_use]
    pub fn manifest_file(&self) -> PathBuf {
        self.path.join(MANIFEST_FILE)
    }

    /// Reports whether the workspace has more than one checkout, so it
    /// reads a merged manifest whose checkouts' names carry a prefix.
    #[must_use]
    pub fn has_several_checkouts(&self) -> bool {
        self.checkouts.len() > 1
    }

    /// Returns the checkout whose `mxv.toml` a workspace with one checkout
    /// reads, and that the paths of either manifest are relative to: the
    /// first checkout.
    #[must_use]
    pub fn manifest_checkout(&self) -> Option<&Checkout> {
        self.checkouts.first()
    }

    /// Returns the directory that the paths of the workspace manifest are
    /// relative to: the workspace's directory for a workspace with several
    /// checkouts, or else the checkout, or else the workspace's directory.
    #[must_use]
    pub fn manifest_root(&self) -> &Path {
        if self.has_several_checkouts() {
            return &self.path;
        }
        self.manifest_checkout()
            .map_or(self.path.as_path(), |checkout| checkout.path.as_path())
    }

    /// Returns the workspace manifest's path when it exists and no
    /// checkout holds it.
    fn workspace_manifest_input(&self) -> Option<PathBuf> {
        Some(self.manifest_file())
            .filter(|path| exists(path) && !self.checkout_holds_manifest_file())
    }

    /// Returns the workspace's manifest files that exist, for `checkout`,
    /// or for [`Workspace::manifest_checkout`] when `checkout` is `None`.
    /// For a workspace with several checkouts, it also names each
    /// checkout's file in [`ManifestInputs::checkouts`].
    #[must_use]
    pub fn manifest_inputs_for(&self, checkout: Option<&Checkout>) -> ManifestInputs {
        let workspace = self.workspace_manifest_input();
        let repository = checkout
            .or_else(|| self.manifest_checkout())
            .map(|checkout| checkout.path.join(MANIFEST_FILE))
            .filter(|path| exists(path));
        let merged = self.has_several_checkouts();
        let checkouts = if merged {
            self.checkouts
                .iter()
                .map(|checkout| checkout.path.join(MANIFEST_FILE))
                .filter(|path| exists(path))
                .collect()
        } else {
            Vec::new()
        };
        ManifestInputs {
            workspace,
            repository,
            checkouts,
            merged,
        }
    }

    /// Reports whether the workspace manifest is the person's own: it
    /// exists, no checkout holds its path, and it is a regular file with
    /// no other hard link, so no repository can have written it. Only a
    /// trusted workspace manifest's `disposable` globs are deleted without
    /// asking.
    #[must_use]
    pub fn manifest_trusted(&self) -> bool {
        if self.checkout_holds_manifest_file() {
            return false;
        }
        fs::symlink_metadata(self.manifest_file()).is_ok_and(|metadata| {
            metadata.file_type().is_file() && crate::disk::links(&metadata) == 1
        })
    }

    /// Returns the workspace's manifest files that exist, as separate
    /// inputs.
    #[must_use]
    pub fn manifest_inputs(&self) -> ManifestInputs {
        self.manifest_inputs_for(None)
    }

    /// Returns the manifest that the workspace reads, as the module docs
    /// describe, or `None` when the workspace is bare.
    ///
    /// # Errors
    ///
    /// Returns the [`ManifestError`] of a file, which never falls back to
    /// another one, and for a workspace with several checkouts, a
    /// [`ManifestProblem::NameClash`] or a name that is too long.
    pub fn manifest(&self) -> Result<Option<Manifest>, ManifestError> {
        self.manifest_for(None)
    }

    /// Returns the manifest that `checkout` reads as [`Workspace::manifest`]
    /// does, or that [`Workspace::manifest_checkout`] reads when `checkout`
    /// is `None`, with its paths checked against that checkout. In a
    /// workspace with several checkouts, it returns the checkout's own
    /// `mxv.toml`, with its names and paths as written, for a `checkout`,
    /// and the merged manifest for `None`.
    ///
    /// # Errors
    ///
    /// Returns the [`ManifestError`] of the chosen file.
    pub fn manifest_for(
        &self,
        checkout: Option<&Checkout>,
    ) -> Result<Option<Manifest>, ManifestError> {
        if self.has_several_checkouts() {
            return match checkout {
                Some(checkout) => Manifest::read(&checkout.path),
                None => self.merged_manifest(),
            };
        }
        let checkout = checkout.or_else(|| self.manifest_checkout());
        let inputs = self.manifest_inputs_for(checkout);
        let Some(path) = inputs.chosen() else {
            return Ok(None);
        };
        let root = checkout.map_or(self.path.as_path(), |checkout| checkout.path.as_path());
        Manifest::read_file(path, root)
    }

    /// Returns the workspace manifest alone, with its paths checked
    /// against [`Workspace::manifest_root`], or `None` when it does not
    /// exist or a checkout holds it.
    ///
    /// # Errors
    ///
    /// Returns the [`ManifestError`] of the file.
    pub fn workspace_manifest(&self) -> Result<Option<Manifest>, ManifestError> {
        let Some(path) = self.workspace_manifest_input() else {
            return Ok(None);
        };
        let mut joined = Joined::default();
        if self.has_several_checkouts() {
            // A checkout's file that does not parse adds no names, and
            // fails on its own wherever the merged manifest is read.
            for (_, renamed) in self.renamed_checkout_manifests().into_iter().flatten() {
                joined.add(&renamed);
            }
            self.join_checkouts(&mut joined);
        }
        Manifest::read_file_joining(&path, self.manifest_root(), &joined)
    }

    /// Returns each checkout's own `mxv.toml` that exists, with its names
    /// prefixed, and the checkout, in the record's order, or the error of
    /// a file that cannot be read.
    fn renamed_checkout_manifests(&self) -> Vec<Result<(&Checkout, Manifest), ManifestError>> {
        self.checkouts
            .iter()
            .filter_map(|checkout| match Manifest::read(&checkout.path) {
                Ok(Some(own)) => {
                    let prefix = checkout_prefix(&checkout.name);
                    Some(Ok((checkout, own.renamed(&|name| prefixed(&prefix, name)))))
                }
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Returns the merged manifest of a workspace with several checkouts,
    /// or `None` when no file exists.
    fn merged_manifest(&self) -> Result<Option<Manifest>, ManifestError> {
        self.merged_from(None)
    }

    /// Returns the merged manifest that the workspace, which has several
    /// checkouts, would read with `text` as its workspace manifest, read
    /// from the checkouts as they are on disk. It checks a workspace
    /// manifest before it is written, so a manifest that clashes or that
    /// refers to a name no file declares is never installed.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspace::manifest`].
    pub fn merged_manifest_with(&self, text: &str) -> Result<Manifest, ManifestError> {
        self.merged_from(Some(text))?.ok_or_else(|| ManifestError {
            path: self.manifest_file(),
            line: None,
            problem: ManifestProblem::Invalid {
                message: "the merged manifest is empty".to_owned(),
            },
        })
    }

    /// Adds the folder of each of the workspace's checkouts to `joined`.
    fn join_checkouts(&self, joined: &mut Joined) {
        for checkout in &self.checkouts {
            if let Some(folder) = checkout.path.file_name() {
                joined.add_checkout(&folder.to_string_lossy());
            }
        }
    }

    /// Returns the merged manifest with `text`, when it is given, in place
    /// of the workspace manifest's file.
    fn merged_from(&self, text: Option<&str>) -> Result<Option<Manifest>, ManifestError> {
        let mut checkouts = Vec::new();
        let mut joined = Joined::default();
        for read in self.renamed_checkout_manifests() {
            let (checkout, renamed) = read?;
            joined.add(&renamed);
            checkouts.push((checkout.path.join(MANIFEST_FILE), checkout, renamed));
        }
        self.join_checkouts(&mut joined);
        let shared = match (text, self.workspace_manifest_input()) {
            (Some(text), _) => {
                let path = self.manifest_file();
                let manifest = Manifest::parse_at(text, &path, &self.path, &joined)?;
                Some((path, manifest))
            }
            (None, Some(path)) => Manifest::read_file_joining(&path, &self.path, &joined)?
                .map(|manifest| (path, manifest)),
            (None, None) => None,
        };
        let mut declared = Declared::default();
        let mut merged: Option<Manifest> = None;
        if let Some((path, manifest)) = shared {
            declared.add(&manifest, &path)?;
            let into = merged.insert(emptied(&manifest));
            // The workspace manifest's own globs stay, relative to the
            // workspace's directory.
            into.disposable.clone_from(&manifest.disposable);
            into.clone_on_new.clone_from(&manifest.clone_on_new);
            absorb(into, manifest, &self.path);
        }
        for (path, checkout, renamed) in checkouts {
            declared.add(&renamed, &path)?;
            let into = merged.get_or_insert_with(|| emptied(&renamed));
            absorb(into, renamed, &checkout.path);
        }
        Ok(merged)
    }
}

/// Returns `manifest` without its units, ports, globs and warnings, as the
/// start of a merged manifest.
fn emptied(manifest: &Manifest) -> Manifest {
    let mut empty = manifest.clone();
    empty.processes.clear();
    empty.containers.clear();
    empty.ports.clear();
    empty.warnings.clear();
    empty.disposable.clear();
    empty.clone_on_new.clear();
    empty
}
