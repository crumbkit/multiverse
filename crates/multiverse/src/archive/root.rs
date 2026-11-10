//! The extra entries of a workspace's directory, which `mxv archive` copies
//! into the archive and `mxv restore` writes back. mxv writes each
//! checkout's folder, an `AGENTS.md` that nobody edited and an empty
//! `.deadband-workspace`, and the workspace manifest has a place of its own
//! in the archive, so an extra entry is anything else, including an edited
//! `AGENTS.md`.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::archive::{Archive, Archives, MANIFEST_COPY, QUARANTINE_SUFFIX, ROOT_DIR, quarantine};
use crate::error::Error;
use crate::manifest::MANIFEST_FILE;
use crate::marker::MARKER_FILE;
use crate::record::{AGENTS_FILE, Workspace};
use crate::store;
use crate::workspaces::{Entry, aside};

/// What one extra entry of a workspace's directory is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RootKind {
    /// A file, whose copy has this digest and these permission bits.
    File {
        /// The SHA-256 digest of its bytes, in lowercase hex.
        digest: String,
        /// Its permission bits.
        mode: u32,
    },
    /// A symbolic link, which is copied as a link.
    Link {
        /// What the link names.
        target: String,
    },
    /// A directory.
    Directory {
        /// Its permission bits.
        mode: u32,
    },
}

/// One extra entry of a workspace's directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RootEntry {
    /// The entry's path, relative to the workspace's directory.
    pub path: String,
    /// What the entry is.
    #[serde(flatten)]
    pub kind: RootKind,
}

/// An entry of a workspace's directory that mxv cannot copy.
#[derive(Debug)]
pub(super) struct Special {
    /// The entry's path relative to the directory, as text.
    pub(super) name: String,
    /// The entry's path on the disk.
    path: PathBuf,
}

/// What a scan of a workspace's directory found.
#[derive(Debug, Default)]
pub(super) struct Scanned {
    /// Each entry that mxv can copy, parents before their children, with
    /// its digest still empty.
    pub(super) entries: Vec<RootEntry>,
    /// Each entry that mxv cannot copy, such as a socket or a name that is
    /// not UTF-8.
    pub(super) special: Vec<Special>,
    /// Each file that mxv cannot read and each directory that it cannot
    /// read or empty, which would stop an archive partway.
    pub(super) blocked: Vec<String>,
}

impl Scanned {
    /// Returns the path of each entry.
    pub(super) fn paths(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }
}

/// Reports whether `name`, an entry of the workspace's directory, belongs
/// to mxv or to a checkout of `workspace`. `generated` is the text that mxv
/// writes as `AGENTS.md`, so a file that differs from it is the person's.
fn is_own(workspace: &Workspace, name: &str, path: &Path, generated: &str) -> bool {
    if name == AGENTS_FILE {
        // Opening a FIFO blocks until something writes to it, so only a
        // regular file is read.
        return is_regular(path) && fs::read(path).is_ok_and(|text| text == generated.as_bytes());
    }
    // `mxv restore` writes the marker again. One that holds bytes, a
    // directory or a link of its name is the person's, and is copied.
    if name == MARKER_FILE {
        return crate::marker::is_own(path);
    }
    // The workspace manifest is copied by itself only when it is a file. A
    // link stays a link, as any other entry does.
    // An unreadable one goes through the scan, which refuses it at the plan.
    if name == MANIFEST_FILE
        && matches!(Entry::at(path), Ok(Entry::Present))
        && path.is_file()
        && readable(path)
    {
        return true;
    }
    workspace.checkouts.iter().any(|checkout| {
        checkout.path == path
            || quarantine(&checkout.path).is_some_and(|held| held == path)
            || aside(&checkout.path).is_some_and(|doomed| doomed == path)
    }) || name.ends_with(QUARANTINE_SUFFIX)
}

/// Returns the extra entries of the directory of `workspace`, where mxv
/// writes `generated` as `AGENTS.md`. A workspace whose checkout is the
/// directory itself, as in an old record, has none.
pub(super) fn scan(workspace: &Workspace, generated: &str) -> Result<Scanned, Error> {
    let mut scanned = Scanned::default();
    let holds_itself = workspace
        .checkouts
        .iter()
        .any(|checkout| checkout.path == workspace.path);
    if holds_itself || Entry::at(&workspace.path)? == Entry::Absent {
        return Ok(scanned);
    }
    walk(workspace, generated, &workspace.path, "", &mut scanned)?;
    Ok(scanned)
}

/// Reports whether mxv can read the file at `path`.
fn readable(path: &Path) -> bool {
    fs::File::open(path).is_ok()
}

/// Adds the entries below `dir`, whose path relative to the workspace's
/// directory is `prefix`, to `scanned`.
fn walk(
    workspace: &Workspace,
    generated: &str,
    dir: &Path,
    prefix: &str,
    scanned: &mut Scanned,
) -> Result<(), Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        found.push(entry.map_err(unreadable)?);
    }
    found.sort_by_key(fs::DirEntry::file_name);
    for entry in found {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            let name = format!("{prefix}{}", name.to_string_lossy());
            // A special entry is deleted as one file. A directory whose name
            // is not UTF-8 cannot be, so it stops the archive at the plan.
            if path.is_dir() && !path.is_symlink() {
                scanned.blocked.push(name);
            } else {
                scanned.special.push(Special { name, path });
            }
            continue;
        };
        if prefix.is_empty() && is_own(workspace, name, &path, generated) {
            continue;
        }
        let relative = format!("{prefix}{name}");
        let metadata = fs::symlink_metadata(&path).map_err(unreadable)?;
        let mode = metadata.permissions().mode() & 0o7777;
        let kind = if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(unreadable)?;
            let Some(target) = target.to_str() else {
                scanned.special.push(Special {
                    name: relative,
                    path,
                });
                continue;
            };
            RootKind::Link {
                target: target.to_owned(),
            }
        } else if metadata.is_dir() {
            if mode & 0o700 != 0o700 {
                scanned.blocked.push(relative);
                continue;
            }
            RootKind::Directory { mode }
        } else if metadata.is_file() {
            if !readable(&path) {
                scanned.blocked.push(relative);
                continue;
            }
            RootKind::File {
                digest: String::new(),
                mode,
            }
        } else {
            scanned.special.push(Special {
                name: relative,
                path,
            });
            continue;
        };
        let is_dir = matches!(kind, RootKind::Directory { .. });
        scanned.entries.push(RootEntry {
            path: relative.clone(),
            kind,
        });
        if is_dir {
            walk(
                workspace,
                generated,
                &path,
                &format!("{relative}/"),
                scanned,
            )?;
        }
    }
    Ok(())
}

/// Puts `entry` into `known` in place of an entry of the same path.
pub(super) fn merge(known: &mut Vec<RootEntry>, entry: RootEntry) {
    known.retain(|kept| kept.path != entry.path);
    known.push(entry);
}

/// Copies the workspace manifest and the extra entries of `workspace` into
/// the archive's directory, and records them in `archive`. A file that an
/// earlier run copied and whose original is gone stays recorded.
pub(super) fn copy_in(
    archives: &Archives,
    archive: &mut Archive,
    workspace: &Workspace,
    generated: &str,
) -> Result<(), Error> {
    let dir = archives.dir_of(&archive.name);
    let scanned = scan(workspace, generated)?;
    let root = dir.join(ROOT_DIR);
    store::private_dir(&root)?;
    let mut created = vec![root.clone()];
    for mut entry in scanned.entries {
        let from = workspace.path.join(&entry.path);
        let to = root.join(&entry.path);
        match &mut entry.kind {
            RootKind::File { digest, mode } => {
                *digest = store::copy_file(&from, &to, *mode)?;
            }
            RootKind::Directory { .. } => {
                fs::create_dir_all(&to).map_err(|source| Error::Unwritable {
                    path: to.clone(),
                    source,
                })?;
                created.extend(to.parent().map(Path::to_path_buf));
            }
            RootKind::Link { .. } => {}
        }
        merge(&mut archive.root, entry);
    }
    // A file's own sync does not make the directories above it durable.
    for directory in created {
        store::sync_dir(&directory);
    }
    archive
        .root
        .sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = workspace.manifest_file();
    if !workspace.path.eq(&manifest) && is_regular(&manifest) {
        archive.manifest = Some(store::copy_file(
            &manifest,
            &dir.join(MANIFEST_COPY),
            0o600,
        )?);
        archive.manifest_mode = fs::symlink_metadata(&manifest)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o7777);
    }
    Ok(())
}

/// Returns one sentence for each difference between the workspace's
/// directory and what `archive` copied from it, so a file that changed
/// after its copy is not deleted.
pub(super) fn differences(
    archive: &Archive,
    workspace: &Workspace,
    generated: &str,
) -> Result<Vec<String>, Error> {
    let scanned = scan(workspace, generated)?;
    let mut found = Vec::new();
    for entry in &scanned.entries {
        let Some(copied) = archive.root.iter().find(|kept| kept.path == entry.path) else {
            found.push(format!("new path: {}", entry.path));
            continue;
        };
        let same = match (&entry.kind, &copied.kind) {
            (RootKind::File { mode, .. }, RootKind::File { digest, mode: kept }) => {
                mode == kept && store::file_digest(&workspace.path.join(&entry.path))? == *digest
            }
            (kept, other) => kept == other,
        };
        if !same {
            found.push(format!("path changed after its copy: {}", entry.path));
        }
    }
    found.extend(
        scanned
            .special
            .iter()
            .filter(|special| !archive.lost.contains(&lost_name(special)))
            .map(|special| format!("new path: {}", special.name)),
    );
    found.extend(
        scanned
            .blocked
            .iter()
            .map(|path| format!("path cannot be read: {path}")),
    );
    let manifest = workspace.manifest_file();
    if !workspace.path.eq(&manifest)
        && is_regular(&manifest)
        && archive.manifest.as_ref() != Some(&store::file_digest(&manifest)?)
    {
        found.push(format!("path changed after its copy: {MANIFEST_FILE}"));
    }
    Ok(found)
}

/// Returns the name that an archive records for `special` when it is lost.
pub(super) fn lost_name(special: &Special) -> String {
    format!("root: {}", special.name)
}

/// Reports whether a regular file is at `path`, without following a link.
fn is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Deletes the extra entries of `workspace` that `archive` recorded,
/// children before their parents, and each entry that it could not copy and
/// recorded as lost. A directory that holds something new stays.
pub(super) fn delete_originals(
    archive: &Archive,
    workspace: &Workspace,
    generated: &str,
) -> Result<(), Error> {
    for special in scan(workspace, generated)?.special {
        if archive.lost.contains(&lost_name(&special)) {
            fs::remove_file(&special.path).map_err(|source| Error::Unwritable {
                path: special.path.clone(),
                source,
            })?;
        }
    }
    delete_entries(&archive.root, &workspace.path)
}

/// Deletes each of `entries` below `root`, children before their parents.
pub(super) fn delete_entries(entries: &[RootEntry], root: &Path) -> Result<(), Error> {
    for entry in entries.iter().rev() {
        let path = root.join(&entry.path);
        let removed = match entry.kind {
            RootKind::Directory { .. } => fs::remove_dir(&path),
            RootKind::File { .. } | RootKind::Link { .. } => fs::remove_file(&path),
        };
        match removed {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(source) => return Err(Error::Unwritable { path, source }),
        }
    }
    Ok(())
}

/// Returns the path of the first file of `archive` whose bytes in the
/// archive's directory differ from its recorded digest, or `None` when each
/// one matches.
pub(super) fn first_changed(
    archives: &Archives,
    archive: &Archive,
) -> Result<Option<String>, Error> {
    let dir = archives.dir_of(&archive.name);
    let same = |path: &Path, digest: &str| -> Result<bool, Error> {
        match store::file_digest(path) {
            Ok(found) => Ok(found == digest),
            Err(Error::StateUnreadable { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    };
    if let Some(digest) = &archive.manifest
        && !same(&dir.join(MANIFEST_COPY), digest)?
    {
        return Ok(Some(MANIFEST_FILE.to_owned()));
    }
    for entry in &archive.root {
        if let RootKind::File { digest, .. } = &entry.kind
            && !same(&dir.join(ROOT_DIR).join(&entry.path), digest)?
        {
            return Ok(Some(entry.path.clone()));
        }
    }
    Ok(None)
}

/// Writes the workspace manifest and the extra entries of `archive` back
/// into its workspace's directory `root`, parents before their children.
pub(super) fn write_back(
    archives: &Archives,
    archive: &Archive,
    root: &Path,
    manifest: &Path,
) -> Result<(), Error> {
    let dir = archives.dir_of(&archive.name);
    if archive.manifest.is_some() {
        let mode = archive.manifest_mode.unwrap_or(0o644);
        store::copy_file(&dir.join(MANIFEST_COPY), manifest, mode)?;
    }
    for entry in &archive.root {
        let to = root.join(&entry.path);
        let unwritable = |source| Error::Unwritable {
            path: to.clone(),
            source,
        };
        match &entry.kind {
            RootKind::File { mode, .. } => {
                store::copy_file(&dir.join(ROOT_DIR).join(&entry.path), &to, *mode)?;
            }
            RootKind::Directory { mode } => {
                fs::create_dir(&to).map_err(unwritable)?;
                fs::set_permissions(&to, fs::Permissions::from_mode(*mode)).map_err(unwritable)?;
            }
            RootKind::Link { target } => {
                std::os::unix::fs::symlink(target, &to).map_err(unwritable)?;
            }
        }
    }
    Ok(())
}
