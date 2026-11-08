//! Putting a workspace away and rebuilding it: the archive records that
//! `mxv archive` writes, and the operations that write and read them.
//!
//! An archive lives in `<archives>/<name>`, in the state directory, private
//! to the person. `archive.json` holds its [`Archive`] record, `manifest`
//! the bytes of the workspace manifest when the workspace has one, and
//! `root/` the extra files of the workspace's directory. Each checkout's
//! commit stays reachable through a pin, a reference
//! `refs/mxv/archives/<archive id>/<folder>` in the worktree's repository or
//! the clone's mirror. Nothing else reads the pins, and mxv never pushes
//! them.
//!
//! [`Workspaces::plan_archive`] classifies the workspace as `mxv remove`
//! classifies it, with [`Workspaces::plan_removal`], and names everything
//! that an archive could not keep. [`Workspaces::archive`] then pins each
//! `HEAD`, writes the record, copies the root files, and retires each
//! checkout. Retiring moves the checkout aside by one rename, reads its git
//! state and path list again in the new place, and puts it back when
//! anything differs from what the archive admitted. The stages are
//! idempotent, so the same command finishes an archive that stopped partway.
//!
//! Restoring lives in [`restore`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use phase::WorkspaceId;
use phase::random::RANDOM_DEVICE;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, Head};
use crate::error::Error;
use crate::mirror::{directory, redacted};
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::store::{self, MOST_RECORD_BYTES};
use crate::workspaces::{
    Allow, BranchFate, CheckoutRemoval, Entry, Fate, Mode, Removal, RemovalOutcome, RemovalPlan,
    Workspaces, aside, delete_directory,
};

mod restore;
mod root;
#[cfg(test)]
mod tests;

pub use restore::{
    ArchivedRemoval, PortChange, RestoreOutcome, RestorePlan, RestorePlanned, RestoreRefusal,
    Restored,
};
pub use root::{RootEntry, RootKind};

/// The highest version of the archive record that this version of
/// multiverse reads. A record with a higher version is refused (DUR-3).
pub const ARCHIVE_VERSION: u32 = 1;

/// The message of the commit that `mxv archive --wip` makes.
pub const WIP_MESSAGE: &str = "wip: archived by mxv";

/// The name of an archive's record in its directory.
const RECORD_FILE: &str = "archive.json";

/// The name of the copy of the workspace manifest in an archive's directory.
const MANIFEST_COPY: &str = "manifest";

/// The name of the directory that holds an archive's root files.
const ROOT_DIR: &str = "root";

/// The suffix of the name under which `mxv archive` moves a checkout while
/// it compares the checkout with what it admitted, such as
/// `.app.mxv-archiving` beside `app`. A checkout that passes moves on to
/// the name that [`aside`] returns, so what stands under this name is whole.
const QUARANTINE_SUFFIX: &str = ".mxv-archiving";

/// Returns where a test stops or changes a run, which is nothing outside
/// the tests. `step` names the stage that just finished.
#[cfg(test)]
pub(super) fn checkpoint(step: &str) -> Result<(), Error> {
    tests::at_checkpoint(step)
}

/// Returns where a test stops or changes a run, which is nothing outside
/// the tests. `step` names the stage that just finished.
#[cfg(not(test))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the test build of this function can fail, and the callers pass its result on"
)]
const fn checkpoint(_step: &str) -> Result<(), Error> {
    Ok(())
}

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
    /// The URL of the checkout's `origin`, without a user name or password,
    /// or `None` when it has no origin.
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
}

impl ArchivedCheckout {
    /// Returns the repository that holds the checkout's pin: its mirror, or
    /// its source for a worktree.
    #[must_use]
    pub fn pin_repository(&self) -> &Path {
        self.mirror.as_deref().unwrap_or(&self.source)
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

/// The archives that one state directory holds.
#[derive(Clone, Debug)]
pub struct Archives {
    /// The directory that holds one directory for each archive.
    dir: PathBuf,
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
    /// `~/.local/state/mxv/archives`.
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Returns the directory of the archive of `name`.
    #[must_use]
    pub fn dir_of(&self, name: &WorkspaceName) -> PathBuf {
        self.dir.join(name.as_str())
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
            paths.push(entry.map_err(unreadable)?.path().join(RECORD_FILE));
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
    fn write(&self, archive: &Archive) -> Result<(), Error> {
        let dir = self.dir_of(&archive.name);
        store::private_dir(&dir)?;
        let target = dir.join(RECORD_FILE);
        let mut bytes =
            serde_json::to_vec_pretty(archive).map_err(|source| Error::CorruptRecord {
                path: target.clone(),
                source,
            })?;
        bytes.push(b'\n');
        store::replace_file(&target, &bytes)
    }

    /// Deletes the directory of the archive of `name` and everything in it.
    fn delete(&self, name: &WorkspaceName) -> Result<(), Error> {
        delete_directory(&self.dir_of(name))?;
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
    serde_json::from_slice(&bytes).map(Some).map_err(corrupt)
}

/// Returns the name of the pin of the checkout in `folder` of the archive
/// `id`. The folder is encoded as one reference component, because a
/// repository's name can hold bytes that a reference may not.
fn pin_ref(id: &str, folder: &str) -> String {
    format!(
        "refs/mxv/archives/{id}/{}",
        directory(folder).replace("..", ".%2E")
    )
}

/// Returns the path beside `checkout` under which `archive` holds it while
/// it compares it with what it admitted, or `None` for a path without a
/// name.
fn quarantine(checkout: &Path) -> Option<PathBuf> {
    let name = checkout.file_name()?.to_str()?;
    Some(checkout.with_file_name(format!(".{name}{QUARANTINE_SUFFIX}")))
}

/// What the person lets `mxv archive` do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArchiveOptions {
    /// Commit the uncommitted changes and untracked files onto the branch
    /// first.
    pub wip: bool,
    /// Lose what the archive cannot keep, and list it in the record.
    pub force: bool,
    /// Whether any of the workspace's containers has run.
    pub containers_ran: bool,
}

impl ArchiveOptions {
    /// Returns the options with `--wip` and `--force` as given.
    #[must_use]
    pub const fn new(wip: bool, force: bool, containers_ran: bool) -> Self {
        Self {
            wip,
            force,
            containers_ran,
        }
    }
}

/// Everything in a workspace that an archive cannot keep, unless the person
/// says what to do about it. Each item has the checkout's folder in front.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unsaved {
    /// Each uncommitted change or untracked path, as `git status` prints it.
    pub changes: Vec<String>,
    /// Each ignored path outside the disposable set, such as `.env`.
    pub ignored: Vec<String>,
    /// The commit of each stash entry of a clone.
    pub stashes: Vec<String>,
    /// Each commit of a clone that `origin` does not hold and `HEAD` does
    /// not reach, which no pin would keep.
    pub commits: Vec<String>,
    /// Each entry of the workspace's directory that mxv cannot copy, such
    /// as a socket.
    pub root: Vec<String>,
}

impl Unsaved {
    /// Returns every item in one list.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        let mut all = self.changes.clone();
        all.extend(self.ignored.iter().cloned());
        all.extend(self.stashes.iter().cloned());
        all.extend(self.commits.iter().cloned());
        all.extend(self.root.iter().cloned());
        all
    }

    /// Reports whether `--wip` can save everything that is listed.
    fn wip_saves_all(&self) -> bool {
        self.ignored.is_empty()
            && self.stashes.is_empty()
            && self.commits.is_empty()
            && self.root.is_empty()
    }

    /// Reports whether nothing is listed.
    fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.wip_saves_all()
    }
}

/// Why `mxv archive` left a workspace as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArchiveRefusal {
    /// No workspace has the name.
    NotFound,
    /// An archive already holds the name.
    NameTaken(Box<Archive>),
    /// A clone's mirror is gone, so nothing could hold its pin.
    MirrorMissing {
        /// The checkout's folder.
        folder: String,
        /// The mirror that the record names.
        mirror: PathBuf,
    },
    /// A checkout is not in a state that an archive can read, such as one
    /// that is gone or that git has locked.
    Unusable {
        /// The checkout's folder.
        folder: String,
        /// Why, in a sentence.
        reason: String,
    },
    /// The workspace holds work that the archive cannot keep.
    Unsaved(Unsaved),
    /// A checkout differs from what the archive admitted. The checkout is
    /// back in its place and the record stays.
    SourceChanged {
        /// The checkout's folder.
        folder: String,
        /// What differs, one sentence for each difference.
        differences: Vec<String>,
    },
}

/// An archive that [`Workspaces::plan_archive`] found nothing in the way of.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ArchivePlan {
    /// The classification of the workspace by [`Workspaces::plan_removal`].
    pub removal: Removal,
    /// What the person allowed.
    pub options: ArchiveOptions,
    /// The id of the archive, which names its pins.
    pub id: String,
    /// An earlier run's record, when this run finishes that archive.
    pub resuming: Option<Archive>,
    /// Each checkout as the archive would record it.
    pub checkouts: Vec<ArchivedCheckout>,
    /// What the archive would lose, with `--force`.
    pub lost: Vec<String>,
    /// What `--wip` would commit.
    pub wip: Vec<String>,
    /// The extra entries of the workspace's directory that the archive
    /// copies, without digests.
    pub root: Vec<String>,
    /// Why the list of commits on this machine may be incomplete, such as
    /// an origin that could not be fetched.
    pub warnings: Vec<String>,
}

/// The outcome of [`Workspaces::plan_archive`].
#[derive(Debug)]
pub enum ArchivePlanned {
    /// Nothing is in the way.
    Ready(Box<ArchivePlan>),
    /// An earlier run deleted the workspace's record and did not mark the
    /// archive as finished. [`Workspaces::finish_archive`] finishes it.
    Finish(Box<Archive>),
    /// The archive cannot go ahead.
    Refused(ArchiveRefusal),
}

/// The outcome of [`Workspaces::archive`].
#[derive(Debug)]
pub enum ArchiveOutcome {
    /// The workspace is gone, and the record still says
    /// [`ArchiveState::Retiring`] until [`Workspaces::finish_archive`] marks
    /// it.
    Retired {
        /// The record.
        archive: Box<Archive>,
        /// What the removal of the workspace did.
        removal: Box<Removal>,
    },
    /// The archive stopped, and the workspace's remaining checkouts are in
    /// their places.
    Refused(ArchiveRefusal),
}

/// What the records say about an archive of one name.
enum Subject {
    /// The workspace to archive, with the record of an earlier run.
    Found(Box<Workspace>, Option<Box<Archive>>),
    /// The answer that needs no look at the workspace.
    Done(ArchivePlanned),
}

/// A checkout's git state and path list at one moment.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    /// What `HEAD` names.
    head: Head,
    /// Each uncommitted change and untracked path.
    changed: Vec<String>,
    /// Each ignored path.
    ignored: Vec<String>,
    /// The commit of each stash entry.
    stashes: Vec<String>,
    /// The commits that only this checkout holds.
    unpushed: Vec<String>,
    /// Each path below the checkout, outside `.git` and the disposable
    /// directories.
    paths: BTreeSet<String>,
}

impl Snapshot {
    /// Returns what differs between this snapshot and `later`, one sentence
    /// for each difference.
    fn differences(&self, later: &Self) -> Vec<String> {
        let mut found = Vec::new();
        if self.head.commit != later.head.commit || self.head.branch != later.head.branch {
            found.push(format!(
                "HEAD moved from {:?} to {:?}",
                self.head.commit, later.head.commit
            ));
        }
        let lists = [
            ("change", &self.changed, &later.changed),
            ("ignored path", &self.ignored, &later.ignored),
            ("stash", &self.stashes, &later.stashes),
            ("commit", &self.unpushed, &later.unpushed),
        ];
        for (what, before, after) in lists {
            found.extend(
                after
                    .iter()
                    .filter(|item| !before.contains(item))
                    .map(|item| format!("new {what}: {item}")),
            );
            found.extend(
                before
                    .iter()
                    .filter(|item| !after.contains(item))
                    .map(|item| format!("{what} is gone: {item}")),
            );
        }
        found.extend(
            later
                .paths
                .difference(&self.paths)
                .map(|path| format!("new path: {path}")),
        );
        found.extend(
            self.paths
                .difference(&later.paths)
                .map(|path| format!("path is gone: {path}")),
        );
        found
    }
}

/// Adds to `paths` each path below `dir`, relative to `root`, that neither
/// is `.git` at the top nor one of `skip`, without following a symbolic
/// link.
fn collect_paths(
    root: &Path,
    dir: &Path,
    skip: &BTreeSet<&str>,
    paths: &mut BTreeSet<String>,
) -> Result<(), Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if relative == ".git" || skip.contains(relative.as_str()) {
            continue;
        }
        let is_dir = entry.file_type().map_err(unreadable)?.is_dir();
        paths.insert(relative);
        if is_dir {
            collect_paths(root, &path, skip, paths)?;
        }
    }
    Ok(())
}

impl<B: Backend> Workspaces<B> {
    /// Returns what archiving the workspace `name` would do, or why it
    /// cannot be archived, and changes nothing but the remote branches that
    /// a clone holds. In [`Mode::Change`] it first puts back a checkout that
    /// an earlier run left moved aside, so the classification reads it in
    /// its place.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Workspaces::plan_removal`] and of the
    /// backend's commands.
    pub fn plan_archive(
        &self,
        archives: &Archives,
        name: &WorkspaceName,
        options: ArchiveOptions,
        mode: Mode,
    ) -> Result<ArchivePlanned, Error> {
        self.admit(archives, name, options, mode, None, true)
    }

    /// Classifies the workspace `name` for an archive. `id` is the id of
    /// the archive when the caller already chose it. `refresh` says whether
    /// to fetch each clone's `origin` first, which a failure of only warns
    /// about, because the pins keep each commit either way.
    fn admit(
        &self,
        archives: &Archives,
        name: &WorkspaceName,
        options: ArchiveOptions,
        mode: Mode,
        id: Option<String>,
        refresh: bool,
    ) -> Result<ArchivePlanned, Error> {
        let (workspace, resuming) = match self.subject(archives, name)? {
            Subject::Found(workspace, resuming) => (*workspace, resuming.map(|kept| *kept)),
            Subject::Done(planned) => return Ok(planned),
        };
        if mode == Mode::Change {
            self.put_back(&workspace)?;
        }
        let was_kept = |folder: &str| {
            resuming
                .as_ref()
                .and_then(|archive| archive.checkouts.iter().find(|kept| kept.folder == folder))
        };
        if let Some(refused) = check_checkouts(&workspace, resuming.as_ref()) {
            return Ok(refused);
        }
        let warnings = if refresh {
            self.refresh_clones(&workspace)
        } else {
            Vec::new()
        };
        let removal = match self.classify(name)? {
            Ok(removal) => removal,
            Err(refused) => return Ok(refused),
        };
        let id = match id.or_else(|| resuming.as_ref().map(|archive| archive.id.clone())) {
            Some(id) => id,
            None => WorkspaceId::mint()
                .map_err(|source| Error::StateUnreadable {
                    path: PathBuf::from(RANDOM_DEVICE),
                    source,
                })?
                .as_str()
                .to_owned(),
        };
        let mut unsaved = Unsaved {
            changes: removal
                .discarded
                .iter()
                .filter(|item| !removal.ignored.contains(item))
                .cloned()
                .collect(),
            ignored: removal.ignored.clone(),
            ..Unsaved::default()
        };
        let mut checkouts = Vec::new();
        for planned in &removal.checkouts {
            let folder = planned.checkout.name.as_str();
            if planned.directory != Fate::Removed {
                match was_kept(folder) {
                    Some(kept) => checkouts.push(kept.clone()),
                    None => {
                        return Ok(unusable(folder, "its directory is gone"));
                    }
                }
                continue;
            }
            if planned.partial {
                return Ok(unusable(
                    folder,
                    "it has no .git, because an earlier removal stopped partway",
                ));
            }
            let head = self.backend.head(&planned.checkout.path)?;
            let Some(commit) = head.commit.clone() else {
                return Ok(unusable(folder, "its HEAD names no commit"));
            };
            if let Some(kept) = was_kept(folder)
                && kept.commit != commit
            {
                return Ok(ArchivePlanned::Refused(ArchiveRefusal::SourceChanged {
                    folder: folder.to_owned(),
                    differences: vec![format!(
                        "HEAD moved from {} to {commit} after an earlier archive pinned it",
                        kept.commit
                    )],
                }));
            }
            let local_only = self
                .backend
                .local_only_commits(&planned.checkout.path, "HEAD")?;
            unsaved.commits.extend(
                planned
                    .lost
                    .iter()
                    .filter(|commit| !local_only.contains(commit))
                    .map(|commit| format!("{folder}: {commit}")),
            );
            unsaved.stashes.extend(
                planned
                    .stashes
                    .iter()
                    .map(|commit| format!("{folder}: {commit}")),
            );
            checkouts.push(self.archived(planned, head, commit, local_only, &id)?);
        }
        let scanned = root::scan(&workspace)?;
        unsaved
            .root
            .extend(scanned.special.iter().map(|path| format!("root: {path}")));
        if !options.force && !unsaved.is_empty() && !(options.wip && unsaved.wip_saves_all()) {
            return Ok(ArchivePlanned::Refused(ArchiveRefusal::Unsaved(unsaved)));
        }
        let (lost, wip) = if options.force {
            (unsaved.all(), Vec::new())
        } else {
            (Vec::new(), unsaved.changes)
        };
        Ok(ArchivePlanned::Ready(Box::new(ArchivePlan {
            removal,
            options,
            id,
            resuming,
            checkouts,
            lost,
            wip,
            root: scanned.paths(),
            warnings,
        })))
    }

    /// Returns what the record of the workspace `name` and its archive say
    /// about an archive of it.
    fn subject(&self, archives: &Archives, name: &WorkspaceName) -> Result<Subject, Error> {
        let refused = |refusal| Ok(Subject::Done(ArchivePlanned::Refused(refusal)));
        let existing = archives.read(name)?;
        match (self.store.read(name)?, existing) {
            (None, None) => refused(ArchiveRefusal::NotFound),
            (None, Some(archive)) if archive.state == ArchiveState::Retiring => {
                Ok(Subject::Done(ArchivePlanned::Finish(Box::new(archive))))
            }
            (_, Some(archive)) if archive.state == ArchiveState::Archived => {
                refused(ArchiveRefusal::NameTaken(Box::new(archive)))
            }
            (Some(workspace), Some(archive)) if archive.workspace_id != workspace.id => {
                refused(ArchiveRefusal::NameTaken(Box::new(archive)))
            }
            (Some(workspace), resuming) => {
                Ok(Subject::Found(Box::new(workspace), resuming.map(Box::new)))
            }
            (None, Some(_)) => refused(ArchiveRefusal::NotFound),
        }
    }

    /// Fetches each clone's `origin`, so the commits that exist only on
    /// this machine are read against what the origin holds now, and returns
    /// a warning for each fetch that failed. The pins keep each commit
    /// either way.
    fn refresh_clones(&self, workspace: &Workspace) -> Vec<String> {
        if workspace.kind != WorkspaceKind::Clone {
            return Vec::new();
        }
        workspace
            .checkouts
            .iter()
            .filter(|checkout| checkout.path.is_dir())
            .filter_map(|checkout| {
                self.backend.refresh_origin(&checkout.path).err().map(|error| {
                    format!(
                        "mxv could not fetch the origin of {}, so it reads which commits exist only on this machine as the checkout last fetched them: {error}",
                        checkout.name
                    )
                })
            })
            .collect()
    }

    /// Classifies the workspace `name` as `mxv remove` does, allowing
    /// everything, so an archive can list every refusal at once. The error
    /// side holds the refusal of a checkout that it cannot read.
    fn classify(&self, name: &WorkspaceName) -> Result<Result<Removal, ArchivePlanned>, Error> {
        let allow = Allow {
            discard_changes: true,
            delete_unmerged: true,
        };
        Ok(match self.plan_removal(name, allow)? {
            RemovalPlan::Ready(removal) => Ok(removal),
            RemovalPlan::NotFound => Err(ArchivePlanned::Refused(ArchiveRefusal::NotFound)),
            RemovalPlan::Locked {
                checkout, reason, ..
            } => Err(unusable(
                folder_of(&checkout),
                &format!("git has locked it: {reason}"),
            )),
            RemovalPlan::Unowned { checkout, .. } => Err(unusable(
                folder_of(&checkout),
                "it is a symbolic link, or its repository does not list it",
            )),
            other => Err(unusable(
                name.as_str(),
                &format!("the removal plan refused it unexpectedly: {other:?}"),
            )),
        })
    }

    /// Returns the record of the checkout that `planned` retires, at
    /// `commit`, the commit of its `head`, with the pin of the archive `id`.
    fn archived(
        &self,
        planned: &CheckoutRemoval,
        head: Head,
        commit: String,
        local_only: Vec<String>,
        id: &str,
    ) -> Result<ArchivedCheckout, Error> {
        let checkout = &planned.checkout;
        let detached = head.branch.is_none();
        let branch = head.branch.unwrap_or_else(|| checkout.branch.clone());
        let origin = match self.backend.origin_url(&checkout.path) {
            Ok(url) => Some(redacted(&url)),
            Err(Error::NoOrigin { .. }) => None,
            Err(error) => return Err(error),
        };
        Ok(ArchivedCheckout {
            folder: checkout.name.clone(),
            path: checkout.path.clone(),
            source: checkout.source.clone(),
            origin,
            upstream: if detached {
                None
            } else {
                self.backend.upstream(&checkout.path, &branch)?
            },
            branch,
            detached,
            base: checkout.base.clone(),
            commit,
            mirror: checkout.mirror.clone(),
            pin: pin_ref(id, &checkout.name),
            local_only,
        })
    }

    /// Moves back each checkout of `workspace` that an earlier run left
    /// under the name that [`quarantine`] returns, so it is read in its
    /// place.
    fn put_back(&self, workspace: &Workspace) -> Result<(), Error> {
        for checkout in &workspace.checkouts {
            let Some(held) = quarantine(&checkout.path) else {
                continue;
            };
            if Entry::at(&held)? == Entry::Present && Entry::at(&checkout.path)? == Entry::Absent {
                self.move_back(workspace, checkout, &held)?;
            }
        }
        Ok(())
    }

    /// Renames `held` to the place of `checkout`, and repairs a worktree's
    /// entry in its repository, which git may have pointed at `held` when
    /// it read the checkout there.
    fn move_back(
        &self,
        workspace: &Workspace,
        checkout: &Checkout,
        held: &Path,
    ) -> Result<(), Error> {
        store::move_aside(held, &checkout.path)?;
        if workspace.kind == WorkspaceKind::Worktree && checkout.source.is_dir() {
            self.backend
                .repair_checkout(&checkout.source, &checkout.path)?;
        }
        Ok(())
    }

    /// Carries out `confirmed`, which [`Workspaces::plan_archive`] returned
    /// and the caller confirmed. The caller holds the workspace's lock and
    /// has stopped its processes.
    ///
    /// # Errors
    ///
    /// Returns the failures of the backend's commands and of the writes.
    /// The record stays, so the same command finishes the archive.
    pub fn archive(
        &self,
        archives: &Archives,
        confirmed: ArchivePlan,
    ) -> Result<ArchiveOutcome, Error> {
        let name = confirmed.removal.workspace.name.clone();
        let mut options = confirmed.options;
        if options.wip {
            for planned in &confirmed.removal.checkouts {
                let path = &planned.checkout.path;
                if planned.directory == Fate::Removed
                    && !self.backend.changes(path)?.changed.is_empty()
                {
                    self.backend.commit_all(path, WIP_MESSAGE)?;
                }
            }
            checkpoint("wip")?;
            options.wip = false;
        }
        // The workspace may have changed while the caller confirmed, and
        // the processes that the caller stopped may have written files, so
        // what counts is what is there now.
        let plan = match self.admit(
            archives,
            &name,
            options,
            Mode::Change,
            Some(confirmed.id.clone()),
            false,
        )? {
            ArchivePlanned::Ready(plan) => *plan,
            ArchivePlanned::Refused(refusal) => return Ok(ArchiveOutcome::Refused(refusal)),
            ArchivePlanned::Finish(_) => {
                return Ok(ArchiveOutcome::Refused(ArchiveRefusal::NotFound));
            }
        };
        if let Some(new) = plan.lost.iter().find(|item| !confirmed.lost.contains(item)) {
            return Ok(ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
                folder: name.to_string(),
                differences: vec![format!("--force would lose more than mxv showed: {new}")],
            }));
        }
        for (planned, archived) in plan.removal.checkouts.iter().zip(&plan.checkouts) {
            if planned.directory == Fate::Removed {
                self.pin(planned, archived)?;
            }
        }
        checkpoint("pins")?;
        let mut archive = plan.record(&name);
        archives.write(&archive)?;
        checkpoint("record")?;
        let mut admitted = Vec::new();
        for planned in &plan.removal.checkouts {
            admitted.push(if planned.directory == Fate::Removed {
                Some(self.snapshot(plan.removal.workspace.kind, &planned.checkout.path, planned)?)
            } else {
                None
            });
        }
        root::copy_in(archives, &mut archive, &plan.removal.workspace)?;
        archives.write(&archive)?;
        checkpoint("root")?;
        for (planned, admitted) in plan.removal.checkouts.iter().zip(admitted) {
            if let Some(admitted) = admitted
                && let Some(refusal) = self.retire(&plan.removal, planned, &admitted)?
            {
                return Ok(ArchiveOutcome::Refused(refusal));
            }
        }
        root::delete_originals(&archive, &plan.removal.workspace)?;
        checkpoint("delete")?;
        let mut removal = plan.removal.clone();
        for planned in &mut removal.checkouts {
            // A worktree's branch stays in its repository, and the pin
            // keeps a commit that a later change of the branch abandons.
            planned.branch = BranchFate::KeptUnmerged { commits: None };
            planned.tip = None;
        }
        match self.remove(removal)? {
            RemovalOutcome::Done(removal) => {
                checkpoint("removed")?;
                Ok(ArchiveOutcome::Retired {
                    archive: Box::new(archive),
                    removal: Box::new(removal),
                })
            }
            RemovalOutcome::Changed(fresh) => {
                Ok(ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
                    folder: name.to_string(),
                    differences: vec![format!(
                        "the workspace changed while mxv removed it: {fresh:?}"
                    )],
                }))
            }
        }
    }

    /// Marks `archive`, whose workspace is gone, as finished.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unwritable`] when the record cannot be written.
    pub fn finish_archive(&self, archives: &Archives, archive: &mut Archive) -> Result<(), Error> {
        archive.state = ArchiveState::Archived;
        archives.write(archive)
    }

    /// Writes the pin of `archived`, the checkout that `planned` retires,
    /// and checks that it names the checkout's commit. The commit of a
    /// clone may exist only in the clone, so it is fetched into the mirror
    /// first, while the mirror's lock is held.
    fn pin(&self, planned: &CheckoutRemoval, archived: &ArchivedCheckout) -> Result<(), Error> {
        let Some(mirror) = &archived.mirror else {
            return self
                .backend
                .write_ref(&archived.source, &archived.pin, &archived.commit);
        };
        let _lock = self.lock_mirror(mirror)?;
        let pinned =
            self.backend
                .fetch_head_into_mirror(mirror, &planned.checkout.path, &archived.pin)?;
        if pinned == archived.commit {
            return Ok(());
        }
        Err(Error::CommandFailed {
            operation: "fetch into mirror",
            dir: mirror.clone(),
            code: None,
            message: format!(
                "the pin {} names {pinned}, and the checkout's HEAD was {}",
                archived.pin, archived.commit
            ),
        })
    }

    /// Reads the git state and the path list of a checkout at `dir`, which
    /// `planned` describes.
    fn snapshot(
        &self,
        kind: WorkspaceKind,
        dir: &Path,
        planned: &CheckoutRemoval,
    ) -> Result<Snapshot, Error> {
        let mut head = self.backend.head(dir)?;
        // The top level is the directory itself, which moves.
        head.toplevel = None;
        let changes = self.backend.changes(dir)?;
        let (stashes, unpushed) = if kind == WorkspaceKind::Clone {
            (
                self.backend.stashes(dir)?,
                self.backend.unpushed_commits(dir)?,
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let trimmed: Vec<&str> = planned
            .disposable
            .iter()
            .map(|path| path.trim_end_matches('/'))
            .collect();
        let skip: BTreeSet<&str> = trimmed.into_iter().collect();
        let mut paths = BTreeSet::new();
        collect_paths(dir, dir, &skip, &mut paths)?;
        Ok(Snapshot {
            head,
            changed: changes.changed,
            ignored: changes.ignored,
            stashes,
            unpushed,
            paths,
        })
    }

    /// Moves the checkout that `planned` describes aside, compares it with
    /// `admitted`, and deletes it. It puts a checkout that differs back in
    /// its place and returns the refusal.
    fn retire(
        &self,
        removal: &Removal,
        planned: &CheckoutRemoval,
        admitted: &Snapshot,
    ) -> Result<Option<ArchiveRefusal>, Error> {
        let checkout = &planned.checkout;
        let invalid = || Error::Unwritable {
            path: checkout.path.clone(),
            source: io::ErrorKind::InvalidInput.into(),
        };
        let held = quarantine(&checkout.path).ok_or_else(invalid)?;
        let doomed = aside(&checkout.path).ok_or_else(invalid)?;
        delete_directory(&doomed)?;
        if planned.unlock {
            self.backend
                .unlock_checkout(&checkout.source, &checkout.path)?;
        }
        checkpoint(&format!("before-move:{}", checkout.name))?;
        store::move_aside(&checkout.path, &held)?;
        checkpoint(&format!("moved:{}", checkout.name))?;
        let moved = self.snapshot(removal.workspace.kind, &held, planned)?;
        let differences = admitted.differences(&moved);
        if !differences.is_empty() {
            self.move_back(&removal.workspace, checkout, &held)?;
            return Ok(Some(ArchiveRefusal::SourceChanged {
                folder: checkout.name.clone(),
                differences,
            }));
        }
        store::move_aside(&held, &doomed)?;
        checkpoint(&format!("cleared:{}", checkout.name))?;
        delete_directory(&doomed)?;
        if removal.workspace.kind == WorkspaceKind::Worktree && checkout.source.is_dir() {
            self.backend.prune_checkouts(&checkout.source)?;
        }
        checkpoint(&format!("retired:{}", checkout.name))?;
        Ok(None)
    }
}

impl ArchivePlan {
    /// Returns the record of the archive of the workspace `name`, in the
    /// state [`ArchiveState::Retiring`].
    fn record(&self, name: &WorkspaceName) -> Archive {
        let workspace = &self.removal.workspace;
        let mut lost = self
            .resuming
            .as_ref()
            .map(|archive| archive.lost.clone())
            .unwrap_or_default();
        for item in &self.lost {
            if !lost.contains(item) {
                lost.push(item.clone());
            }
        }
        let earlier = self.resuming.as_ref();
        Archive {
            version: ARCHIVE_VERSION,
            id: self.id.clone(),
            state: ArchiveState::Retiring,
            time: earlier.map_or_else(now, |archive| archive.time),
            name: name.clone(),
            workspace_id: workspace.id.clone(),
            kind: workspace.kind,
            path: workspace.path.clone(),
            ports: workspace.ports.clone(),
            pinned: workspace.pinned,
            containers_ran: self.options.containers_ran
                || earlier.is_some_and(|archive| archive.containers_ran),
            checkouts: self.checkouts.clone(),
            lost,
            root: earlier
                .map(|archive| archive.root.clone())
                .unwrap_or_default(),
            manifest: earlier.and_then(|archive| archive.manifest.clone()),
        }
    }
}

/// Returns the seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Returns the refusal of the first checkout of `workspace` that an archive
/// cannot start on: one that `mxv add` left unfinished, or a clone whose
/// mirror is gone, unless an earlier run of this archive `resuming` already
/// retired the clone.
fn check_checkouts(workspace: &Workspace, resuming: Option<&Archive>) -> Option<ArchivePlanned> {
    for checkout in &workspace.checkouts {
        if workspace.is_unfinished(checkout) {
            return Some(unusable(
                &checkout.name,
                "an earlier `mxv add` did not finish it",
            ));
        }
        let retired = resuming.is_some_and(|archive| {
            archive
                .checkouts
                .iter()
                .any(|kept| kept.folder == checkout.name)
        }) && !checkout.path.exists();
        if let Some(mirror) = &checkout.mirror
            && workspace.kind == WorkspaceKind::Clone
            && !mirror.is_dir()
            && !retired
        {
            return Some(ArchivePlanned::Refused(ArchiveRefusal::MirrorMissing {
                folder: checkout.name.clone(),
                mirror: mirror.clone(),
            }));
        }
    }
    None
}

/// Returns the refusal of a checkout in `folder` that an archive cannot
/// read, for `reason`.
fn unusable(folder: &str, reason: &str) -> ArchivePlanned {
    ArchivePlanned::Refused(ArchiveRefusal::Unusable {
        folder: folder.to_owned(),
        reason: reason.to_owned(),
    })
}

/// Returns the last component of `path`, which names a checkout's folder.
fn folder_of(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
}
