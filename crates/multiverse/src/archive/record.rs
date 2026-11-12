//! The archive record and the directory of archives that holds it.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use phase::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::archive::ignored::{IGNORED_TAR, IgnoredEntry};
use crate::archive::root::RootEntry;
use crate::archive::{ARCHIVE_VERSION, HEAD_PIN, PIN_NAMESPACE, RECORD_FILE, WIP_INDEX};
use crate::backend::RefHolder;
use crate::error::Error;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::store::{self, MOST_RECORD_BYTES};
use crate::workspaces::delete_directory;

/// Whether an archive has finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveState {
    /// The record is written and the workspace is still being retired. The
    /// same `mxv archive` finishes it.
    Retiring,
    /// The workspace is gone, and `mxv restore` can rebuild it.
    Archived,
}

/// One checkout of an archived workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ArchivedCheckout {
    /// The checkout's folder in the workspace's directory.
    pub folder: String,
    /// The checkout's directory.
    pub path: PathBuf,
    /// The top-level directory of the person's repository.
    pub source: PathBuf,
    /// The URL of the checkout's `origin`, without a password, and without
    /// a user name unless it is an SSH URL, or `None` when it has no origin.
    pub origin: Option<String>,
    /// The branch that the checkout had checked out, or the branch that it
    /// was created on when `detached` is set.
    pub branch: String,
    /// Whether the checkout's `HEAD` was detached from every branch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// The remote branch that the workspace's branch started from, such as
    /// `refs/remotes/origin/main`.
    pub base: String,
    /// The branch on the remote that the checkout's branch tracked, such as
    /// `refs/heads/triage`, or `None` when it tracked none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// The remote that `upstream` lies on, or `None` when the branch tracked
    /// none or the record is older than this field, which means `origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_remote: Option<String>,
    /// The commit that the checkout's `HEAD` named.
    pub commit: String,
    /// The bare mirror that holds a clone's pin, or `None` for a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<PathBuf>,
    /// The reference that keeps the commit.
    pub pin: String,
    /// The commits of `HEAD` that no remote branch of `origin` held when the
    /// workspace was archived, which exist only on this machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_only: Vec<String>,
    /// The git state besides `HEAD` that the archive keeps on pins below
    /// the checkout's pin directory. A record of version 1 has none.
    #[serde(default, skip_serializing_if = "Kept::is_empty")]
    pub kept: Kept,
}

/// The uncommitted work of a checkout, as two commits that only the
/// archive's pins name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeptWip {
    /// The commit of the index, pinned as `wip-index`, whose parent is
    /// `HEAD`.
    pub index: String,
    /// The commit of the working files, pinned as `wip`, whose parents are
    /// `HEAD` and `index`.
    pub worktree: String,
}

/// A stash entry that an archive keeps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct KeptStash {
    /// The stash's commit.
    pub commit: String,
    /// The message of its entry, which the restore gives it again.
    pub message: String,
}

/// The git state of a checkout that an archive keeps besides `HEAD`, each
/// piece on a pin of its own below the checkout's pin directory.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Kept {
    /// The uncommitted work, or `None` when the checkout had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wip: Option<KeptWip>,
    /// Each stash entry, oldest first, pinned as `stash/<n>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stashes: Vec<KeptStash>,
    /// Each local branch besides the checkout's own, by name, with its
    /// commit, pinned as `branches/<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, String>,
    /// Each tag that `origin` does not hold, by name, with the object it
    /// names, pinned as `tags/<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    /// Each per-worktree reference, by its name below `refs/worktree/`,
    /// with the object it names, pinned as `worktree/<name>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub worktree: BTreeMap<String, String>,
    /// Each commit that `origin` does not hold and no other kept reference
    /// reaches, pinned as `orphans/<commit>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<String>,
}

impl Kept {
    /// Reports whether the archive keeps nothing besides `HEAD`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Returns each pin of what is kept, below `dir`, the checkout's pin
    /// directory, as its reference and the object that it names.
    #[must_use]
    pub fn pins(&self, dir: &str) -> Vec<(String, String)> {
        let mut pins = Vec::new();
        if let Some(wip) = &self.wip {
            pins.push((format!("{dir}/wip-index"), wip.index.clone()));
            pins.push((format!("{dir}/wip"), wip.worktree.clone()));
        }
        for (place, stash) in self.stashes.iter().enumerate() {
            pins.push((format!("{dir}/stash/{place}"), stash.commit.clone()));
        }
        let named = [
            ("branches", &self.branches),
            ("tags", &self.tags),
            ("worktree", &self.worktree),
        ];
        for (kind, names) in named {
            pins.extend(
                names
                    .iter()
                    .map(|(name, object)| (format!("{dir}/{kind}/{name}"), object.clone())),
            );
        }
        pins.extend(
            self.orphans
                .iter()
                .map(|commit| (format!("{dir}/orphans/{commit}"), commit.clone())),
        );
        pins
    }

    /// Returns every object that a pin of what is kept names, besides the
    /// uncommitted work.
    #[must_use]
    pub fn references(&self) -> Vec<String> {
        let mut objects: Vec<String> = self
            .stashes
            .iter()
            .map(|stash| stash.commit.clone())
            .collect();
        for names in [&self.branches, &self.tags, &self.worktree] {
            objects.extend(names.values().cloned());
        }
        objects
    }

    /// Returns every commit that a restore needs: the commits of the
    /// uncommitted work, of each stash, branch and per-worktree reference,
    /// and each orphan. A tag can name an object other than a commit, so
    /// tags are left out.
    #[must_use]
    pub fn commits(&self) -> Vec<String> {
        let mut commits: Vec<String> = self
            .wip
            .iter()
            .flat_map(|wip| [wip.index.clone(), wip.worktree.clone()])
            .collect();
        commits.extend(self.stashes.iter().map(|stash| stash.commit.clone()));
        for names in [&self.branches, &self.worktree] {
            commits.extend(names.values().cloned());
        }
        commits.extend(self.orphans.iter().cloned());
        commits
    }

    /// Returns the first name of a branch, a tag or a per-worktree
    /// reference that would not stay below the reference it is joined to.
    fn escaping_name(&self) -> Option<&str> {
        [&self.branches, &self.tags, &self.worktree]
            .into_iter()
            .flat_map(BTreeMap::keys)
            .map(String::as_str)
            .find(|name| !stays_inside(name))
    }
}

impl ArchivedCheckout {
    /// Returns the directory below which the pins of what the archive keeps
    /// lie. A record of version 2 names the pin of `HEAD` `<dir>/head`.
    #[must_use]
    pub fn pin_dir(&self) -> &str {
        self.pin.strip_suffix(HEAD_PIN).unwrap_or(&self.pin)
    }

    /// Returns every pin of the checkout, the pin of `HEAD` first, each
    /// with the object that it names.
    #[must_use]
    pub fn pins(&self) -> Vec<(String, String)> {
        let mut pins = vec![(self.pin.clone(), self.commit.clone())];
        pins.extend(self.kept.pins(self.pin_dir()));
        pins
    }

    /// Returns the repository that holds the checkout's pin: its mirror, or
    /// its source for a worktree.
    #[must_use]
    pub fn pin_repository(&self) -> &Path {
        self.pin_holder().path()
    }

    /// Returns the repository that holds the checkout's pin, and says
    /// whether it is a mirror.
    #[must_use]
    pub fn pin_holder(&self) -> RefHolder<'_> {
        self.mirror
            .as_deref()
            .map_or(RefHolder::Repository(&self.source), RefHolder::Mirror)
    }
}

/// The record of one archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Archive {
    /// The version of the record form, which is [`ARCHIVE_VERSION`].
    pub version: u32,
    /// The archive's own id, which names its pins: 32 lowercase hex digits.
    pub id: String,
    /// Whether the archive has finished.
    pub state: ArchiveState,
    /// When the archive started, in seconds since the Unix epoch.
    pub time: u64,
    /// The workspace's name.
    pub name: WorkspaceName,
    /// The workspace's stable id, which its docker volumes are labelled
    /// with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// How the workspace's checkouts were made.
    pub kind: WorkspaceKind,
    /// The workspace's directory.
    pub path: PathBuf,
    /// The host port that the workspace held for each named port.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// Whether `mxv pin` kept `mxv reclaim` from stopping the workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Whether any of the workspace's containers has run, so docker may
    /// hold volumes labelled with its stable id.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub containers_ran: bool,
    /// The workspace's checkouts, in the record's order.
    pub checkouts: Vec<ArchivedCheckout>,
    /// Each thing that `--force` lost, with the checkout's folder in front.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost: Vec<String>,
    /// The extra entries of the workspace's directory, parents before their
    /// children.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root: Vec<RootEntry>,
    /// The SHA-256 digest of the workspace manifest's bytes, or `None` when
    /// the workspace has no workspace manifest of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// The permission bits of the workspace manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_mode: Option<u32>,
    /// Each ignored file or link that the archive keeps in its private tar,
    /// in the order it was written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored: Vec<IgnoredEntry>,
}

impl Archive {
    /// Returns the archive's checkouts as the checkouts of a workspace, each
    /// on its recorded branch, with no commit yet.
    pub(crate) fn workspace_checkouts(&self) -> Vec<Checkout> {
        self.checkouts
            .iter()
            .map(|archived| {
                let mut checkout = Checkout::new(
                    archived.folder.clone(),
                    &archived.source,
                    archived.path.clone(),
                    archived.branch.clone(),
                    archived.base.clone(),
                );
                checkout.mirror.clone_from(&archived.mirror);
                checkout
            })
            .collect()
    }

    /// Returns the record of the workspace that the archive holds, with its
    /// stable id, ports and checkouts.
    #[must_use]
    pub fn workspace(&self) -> Workspace {
        let mut workspace = Workspace::new(
            self.name.clone(),
            self.path.clone(),
            self.workspace_checkouts(),
        );
        workspace.id.clone_from(&self.workspace_id);
        if self.kind == WorkspaceKind::Worktree {
            workspace = workspace.of_worktrees();
        }
        workspace.ports.clone_from(&self.ports);
        workspace.pinned = self.pinned;
        workspace
    }

    /// Reports whether a checkout's commit exists on no remote.
    #[must_use]
    pub fn has_local_only_commits(&self) -> bool {
        self.checkouts
            .iter()
            .any(|checkout| !checkout.local_only.is_empty())
    }
}

/// The part of a record that is read first, so a record of a newer version
/// is refused before its other fields are read.
#[derive(Deserialize)]
struct Versioned {
    /// The record's version.
    version: u32,
}

/// The archives that one state directory holds, with the data directory
/// that holds what their records only name.
#[derive(Clone, Debug)]
pub struct Archives {
    /// The directory that holds one directory for each archive, by name.
    dir: PathBuf,
    /// The directory that holds each archive's private tar of ignored
    /// files, in a directory named by the archive's id.
    data: PathBuf,
}

/// The archives that [`Archives::list`] found.
#[derive(Debug)]
#[non_exhaustive]
pub struct ArchiveRecords {
    /// Each archive that could be read, in name order.
    pub archives: Vec<Archive>,
    /// The error of each record that could not be read, in name order.
    pub problems: Vec<Error>,
}

impl Archives {
    /// Returns the archives in `dir`, such as
    /// `~/.local/state/mxv/archives`, whose tars of ignored files lie in
    /// `data`, such as `~/.local/share/mxv/archives`.
    #[must_use]
    pub const fn new(dir: PathBuf, data: PathBuf) -> Self {
        Self { dir, data }
    }

    /// Returns the directory of the archive of `name`.
    #[must_use]
    pub fn dir_of(&self, name: &WorkspaceName) -> PathBuf {
        self.dir.join(name.as_str())
    }

    /// Returns the data directory of the archive `id`, which holds its
    /// private tar of ignored files.
    #[must_use]
    pub fn data_of(&self, id: &str) -> PathBuf {
        self.data.join(id)
    }

    /// Returns the record of the archive of `name`, or `None` when no
    /// archive has the name.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the record cannot be read,
    /// [`Error::NewerRecord`] when a newer multiverse wrote it,
    /// [`Error::CorruptRecord`] when it is not a record, and
    /// [`Error::MismatchedRecord`] when it holds another name.
    pub fn read(&self, name: &WorkspaceName) -> Result<Option<Archive>, Error> {
        let path = self.dir_of(name).join(RECORD_FILE);
        let found = read_record(&path)?;
        match found {
            Some(archive) if archive.name != *name => Err(Error::MismatchedRecord {
                path,
                name: archive.name.to_string(),
            }),
            found => Ok(found),
        }
    }

    /// Returns every archive in name order, and the error of each record
    /// that cannot be read, so one bad record does not hide the others.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the directory cannot be read.
    pub fn list(&self) -> Result<ArchiveRecords, Error> {
        let unreadable = |source| Error::StateUnreadable {
            path: self.dir.clone(),
            source,
        };
        let mut records = ArchiveRecords {
            archives: Vec::new(),
            problems: Vec::new(),
        };
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(records),
            Err(error) => return Err(unreadable(error)),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(unreadable)?;
            // A name never starts with `.`, so such a directory is one that
            // a repair moved aside, which no listing shows.
            if entry.file_name().as_encoded_bytes().starts_with(b".") {
                continue;
            }
            paths.push(entry.path().join(RECORD_FILE));
        }
        paths.sort();
        for path in paths {
            match read_record(&path) {
                Ok(Some(archive)) => records.archives.push(archive),
                Ok(None) => {}
                Err(error) => records.problems.push(error),
            }
        }
        Ok(records)
    }

    /// Writes the record of `archive`, and syncs it and its directory before
    /// it returns. The archive's directory is private to the person.
    pub(super) fn write(&self, archive: &Archive) -> Result<(), Error> {
        let dir = self.dir_of(&archive.name);
        store::private_dir(&dir)?;
        let target = dir.join(RECORD_FILE);
        let mut bytes =
            serde_json::to_vec_pretty(archive).map_err(|source| Error::CorruptRecord {
                path: target.clone(),
                source,
            })?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MOST_RECORD_BYTES {
            return Err(Error::Unwritable {
                path: target,
                source: io::ErrorKind::FileTooLarge.into(),
            });
        }
        store::replace_file(&target, &bytes)
    }

    /// Returns the path of the temporary index in the directory of the
    /// archive of `name`, through which the archive reads and commits a
    /// checkout's uncommitted work, and makes the directory when it is
    /// missing. The directory is private to the person.
    pub(super) fn wip_index(&self, name: &WorkspaceName) -> Result<PathBuf, Error> {
        let dir = self.dir_of(name);
        store::private_dir(&dir)?;
        Ok(dir.join(WIP_INDEX))
    }

    /// Returns the directory that a repair moves the directory of an
    /// archive whose pins are gone to, as
    /// [`Workspaces::repair`](crate::Workspaces::repair) says: `.<name>.<id>` beside it, which no
    /// listing shows, because a name never starts with `.`.
    #[must_use]
    pub fn aside_of(&self, archive: &Archive) -> PathBuf {
        self.dir.join(format!(".{}.{}", archive.name, archive.id))
    }

    /// Returns the private tar of the ignored files of `archive`, when it
    /// exists.
    #[must_use]
    pub fn ignored_tar(&self, archive: &Archive) -> Option<PathBuf> {
        let tar = self.data_of(&archive.id).join(IGNORED_TAR);
        tar.exists().then_some(tar)
    }

    /// Moves the directory of `archive`, with its record, its root files
    /// and everything else in it, to [`Archives::aside_of`] in one rename,
    /// so the archive leaves every listing and nothing in it is deleted.
    /// The private tar of its ignored files stays where it is.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the rename fails, such as when the
    /// target already exists.
    pub(crate) fn move_aside(&self, archive: &Archive) -> Result<PathBuf, Error> {
        let from = self.dir_of(&archive.name);
        let to = self.aside_of(archive);
        store::move_aside(&from, &to)?;
        Ok(to)
    }

    /// Gives `archive` the name `to`, with the workspace's directory and
    /// each checkout's directory moved to the directory of that name
    /// beside the old one, and returns it. The pins and the private tar
    /// are named by the archive's id, so they stay. It writes the renamed
    /// record first, then moves each other entry of the old directory,
    /// such as the copies of the root files and of the workspace manifest,
    /// into the new one with one rename each, then deletes the old
    /// directory. A run that stops partway leaves a record of the same id
    /// under the new name, and the next run moves the entries that are
    /// left.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when a record, a move or a deletion
    /// fails, and [`Error::StateUnreadable`] when the old directory cannot
    /// be read.
    pub(crate) fn rename(&self, archive: &Archive, to: &WorkspaceName) -> Result<Archive, Error> {
        let mut renamed = archive.clone();
        renamed.name = to.clone();
        if let Some(parent) = archive.path.parent() {
            renamed.path = parent.join(to.as_str());
        }
        for checkout in &mut renamed.checkouts {
            checkout.path = renamed.path.join(&checkout.folder);
        }
        self.write(&renamed)?;
        let old = self.dir_of(&archive.name);
        let new = self.dir_of(to);
        let unreadable = |source| Error::StateUnreadable {
            path: old.clone(),
            source,
        };
        for entry in fs::read_dir(&old).map_err(unreadable)? {
            let name = entry.map_err(unreadable)?.file_name();
            // The record was written new, and the temporary index of the
            // uncommitted work is only read while a checkout retires.
            if name == RECORD_FILE || name == WIP_INDEX || new.join(&name).exists() {
                continue;
            }
            store::move_aside(&old.join(&name), &new.join(&name))?;
        }
        delete_directory(&old)?;
        store::sync_dir(&self.dir);
        Ok(renamed)
    }

    /// Deletes the data directory of `archive`, which holds its ignored
    /// files, then the directory of its record, and everything in them.
    pub(super) fn delete(&self, archive: &Archive) -> Result<(), Error> {
        delete_directory(&self.data_of(&archive.id))?;
        store::sync_dir(&self.data);
        delete_directory(&self.dir_of(&archive.name))?;
        store::sync_dir(&self.dir);
        Ok(())
    }
}

/// Returns the archive record at `path`, or `None` when no file is there.
fn read_record(path: &Path) -> Result<Option<Archive>, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: path.to_path_buf(),
        source,
    };
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error)),
    };
    let mut bytes = Vec::new();
    file.take(MOST_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let corrupt = |source| Error::CorruptRecord {
        path: path.to_path_buf(),
        source,
    };
    let versioned: Versioned = serde_json::from_slice(&bytes).map_err(corrupt)?;
    if versioned.version > ARCHIVE_VERSION {
        return Err(Error::NewerRecord {
            path: path.to_path_buf(),
            version: versioned.version,
        });
    }
    let archive: Archive = serde_json::from_slice(&bytes).map_err(corrupt)?;
    if !is_archive_id(&archive.id) {
        return Err(corrupt(serde::de::Error::custom(format!(
            "the archive id {:?} is not 32 lowercase hex digits",
            archive.id
        ))));
    }
    if let Some(entry) = archive.root.iter().find(|entry| !stays_inside(&entry.path)) {
        return Err(corrupt(serde::de::Error::custom(format!(
            "the root entry {} leaves the workspace's directory",
            entry.path
        ))));
    }
    if let Some(entry) = archive
        .ignored
        .iter()
        .find(|entry| !stays_inside(&entry.path))
    {
        return Err(corrupt(serde::de::Error::custom(format!(
            "the ignored entry {} leaves the workspace's directory",
            entry.path
        ))));
    }
    for checkout in &archive.checkouts {
        if !inside_workspace(&archive.path, &checkout.path) {
            return Err(corrupt(serde::de::Error::custom(format!(
                "the checkout {} leaves the workspace's directory",
                checkout.folder
            ))));
        }
        if !is_pin(&checkout.pin) {
            return Err(corrupt(serde::de::Error::custom(format!(
                "the pin {} leaves {PIN_NAMESPACE}",
                checkout.pin
            ))));
        }
        if let Some(name) = checkout.kept.escaping_name() {
            return Err(corrupt(serde::de::Error::custom(format!(
                "the kept reference {name} of the checkout {} leaves its namespace",
                checkout.folder
            ))));
        }
    }
    Ok(Some(archive))
}

/// Reports whether `path`, a checkout's path from a record, is the
/// workspace's directory `workspace` or lies below it. An old workspace's
/// checkout can be the directory itself.
fn inside_workspace(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace).is_ok_and(|below| {
        below
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    })
}

/// Reports whether `id`, an archive id from a record, is 32 lowercase hex
/// digits, so that the data directory it names lies below the archives'
/// data directory.
fn is_archive_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reports whether `pin`, a reference from a record, lies below the
/// namespace of the archives' pins.
fn is_pin(pin: &str) -> bool {
    pin.strip_prefix(PIN_NAMESPACE).is_some_and(stays_inside)
}

/// Reports whether `path`, a path from a record, names something below the
/// directory that it is joined to.
fn stays_inside(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Returns the seconds since the Unix epoch.
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
