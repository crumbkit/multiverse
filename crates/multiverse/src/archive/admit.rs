//! Deciding whether a workspace can be archived, what the archive keeps,
//! and what it cannot represent.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use phase::WorkspaceId;
use phase::random::RANDOM_DEVICE;

use crate::archive::record::now;
use crate::archive::root::{self, RootEntry, RootKind, Scanned};
use crate::archive::{
    ARCHIVE_MAX_FILE, ARCHIVE_VERSION, Archive, ArchiveState, ArchivedCheckout, Archives,
    DIGEST_LENGTH, Kept, KeptStash, RECORD_FILE, pin_ref,
};
use crate::backend::{Backend, Concealed, Head, Operation, RefHolder};
use crate::disposable::DisposableSet;
use crate::error::Error;
use crate::mirror::recorded_origin;
use crate::name::WorkspaceName;
use crate::record::{Workspace, WorkspaceKind};
use crate::store::MOST_RECORD_BYTES;
use crate::workspaces::{Allow, CheckoutRemoval, Fate, Mode, Removal, RemovalPlan, Workspaces};

/// What the person lets `mxv archive` do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArchiveOptions {
    /// Lose what the archive cannot keep, and list it in the record.
    pub force: bool,
    /// Whether any of the workspace's containers has run.
    pub containers_ran: bool,
    /// The largest file of a checkout's uncommitted work that the archive
    /// keeps, in bytes. A larger file cannot be kept.
    pub max_file: u64,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self::new(false, false)
    }
}

impl ArchiveOptions {
    /// Returns the options with `--force` as given, and the largest file
    /// [`ARCHIVE_MAX_FILE`].
    #[must_use]
    pub const fn new(force: bool, containers_ran: bool) -> Self {
        Self {
            force,
            containers_ran,
            max_file: ARCHIVE_MAX_FILE,
        }
    }

    /// Returns the options with the largest file that the archive keeps
    /// set to `bytes`.
    #[must_use]
    pub const fn with_max_file(mut self, bytes: u64) -> Self {
        self.max_file = bytes;
        self
    }
}

/// What a checkout or the workspace's directory holds that an archive
/// cannot represent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnkeptKind {
    /// A git operation stands paused, in the checkout or in a submodule.
    Paused {
        /// The submodule's path with `/` after it, or empty for the
        /// checkout itself.
        at: String,
        /// The operation.
        operation: Operation,
    },
    /// A repository inside the checkout that git does not track.
    Nested {
        /// Its path, relative to the checkout, with `/` after it.
        path: String,
    },
    /// Work of an initialised submodule, or of the git directory of a
    /// submodule whose files are gone: a change, an ignored path, a stash
    /// or an unpushed commit.
    Submodule {
        /// The submodule's path, relative to the checkout.
        path: String,
        /// The work, as `git status` prints a change, or a commit.
        work: String,
    },
    /// A file of the uncommitted work that is larger than the limit.
    TooLarge {
        /// The file, relative to the checkout.
        path: String,
        /// Its size in bytes.
        bytes: u64,
        /// The limit in bytes.
        most: u64,
        /// Whether git does not track the file.
        untracked: bool,
    },
    /// A file marked skip-worktree or assume-unchanged with local edits.
    Hidden {
        /// The file, relative to the checkout.
        path: String,
        /// Whether it is marked skip-worktree, or else assume-unchanged.
        skip_worktree: bool,
    },
    /// A file below the checkout that no listing of git accounts for, such
    /// as a file in a submodule directory that is not initialised.
    Unlisted {
        /// The file, relative to the checkout.
        path: String,
    },
    /// An ignored path outside the disposable set, such as `.env`.
    Ignored {
        /// The path, relative to the checkout.
        path: String,
    },
    /// An entry of the workspace's directory that mxv cannot copy, such as
    /// a socket.
    Root {
        /// The entry, relative to the workspace's directory.
        entry: String,
    },
}

/// One thing that an archive cannot keep.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unkept {
    /// The checkout's folder, or `root` for the workspace's directory.
    pub folder: String,
    /// The checkout's directory, or the workspace's directory for `root`.
    pub dir: PathBuf,
    /// What it is.
    pub kind: UnkeptKind,
}

impl fmt::Display for Unkept {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: ", self.folder)?;
        match &self.kind {
            UnkeptKind::Paused { at, operation } => {
                let what = match operation {
                    Operation::Merge => "a merge",
                    Operation::Rebase => "a rebase",
                    Operation::CherryPick => "a cherry-pick",
                    Operation::Revert => "a revert",
                    Operation::Sequence => "a sequence of cherry-picks or reverts",
                    Operation::Bisect => "a bisect",
                };
                match at.trim_end_matches('/') {
                    "" => write!(formatter, "{what} is paused"),
                    submodule => {
                        write!(formatter, "{what} is paused in the submodule {submodule}")
                    }
                }
            }
            UnkeptKind::Nested { path } => {
                write!(formatter, "the repository {path}, which git does not track")
            }
            UnkeptKind::Submodule { path, work } => {
                write!(formatter, "the submodule {path} holds {work}")
            }
            UnkeptKind::TooLarge {
                path, bytes, most, ..
            } => write!(
                formatter,
                "the file {path} is {bytes} bytes, over the limit of {most} bytes"
            ),
            UnkeptKind::Hidden {
                path,
                skip_worktree,
            } => {
                let flag = if *skip_worktree {
                    "skip-worktree"
                } else {
                    "assume-unchanged"
                };
                write!(
                    formatter,
                    "the file {path} is marked {flag} and has local edits"
                )
            }
            UnkeptKind::Unlisted { path } => {
                write!(formatter, "git does not list the file {path}")
            }
            UnkeptKind::Ignored { path } => write!(formatter, "the ignored path {path}"),
            UnkeptKind::Root { entry } => write!(formatter, "{entry}"),
        }
    }
}

/// Everything in a workspace that an archive cannot represent, which
/// refuses the archive unless the person passes `--force` to lose it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Unkeepable {
    /// Each thing, in the order of the checkouts.
    pub items: Vec<Unkept>,
}

impl Unkeepable {
    /// Adds `kind` of the checkout in `folder` at `dir`.
    fn push(&mut self, folder: &str, dir: &Path, kind: UnkeptKind) {
        self.items.push(Unkept {
            folder: folder.to_owned(),
            dir: dir.to_path_buf(),
            kind,
        });
    }

    /// Adds what `found` says about the checkout at `checkout`, in
    /// `folder`.
    fn add_concealed(&mut self, folder: &str, checkout: &Path, found: &Concealed) {
        for path in &found.nested {
            self.push(folder, checkout, UnkeptKind::Nested { path: path.clone() });
        }
        for hidden in &found.hidden {
            self.push(
                folder,
                checkout,
                UnkeptKind::Hidden {
                    path: hidden.path.clone(),
                    skip_worktree: hidden.skip_worktree,
                },
            );
        }
        for (path, work) in submodule_items(checkout, found) {
            self.push(folder, checkout, UnkeptKind::Submodule { path, work });
        }
        for path in &found.unlisted {
            self.push(
                folder,
                checkout,
                UnkeptKind::Unlisted { path: path.clone() },
            );
        }
        for paused in &found.paused {
            self.push(
                folder,
                checkout,
                UnkeptKind::Paused {
                    at: paused.at.clone(),
                    operation: paused.operation,
                },
            );
        }
    }

    /// Reports whether nothing is listed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The uncommitted work of one checkout, which an archive keeps as the
/// commits `wip-index` and `wip`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Uncommitted {
    /// The checkout's folder.
    pub folder: String,
    /// Each change and untracked path, as `git status --porcelain` prints
    /// it, such as `?? notes.md`.
    pub changes: Vec<String>,
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
    /// The workspace holds work that the archive cannot represent.
    Unkeepable(Box<Unkeepable>),
    /// A checkout differs from what the archive admitted. The checkout is
    /// back in its place and the record stays.
    SourceChanged {
        /// The checkout's folder.
        folder: String,
        /// What differs, one sentence for each difference.
        differences: Vec<String>,
    },
    /// The archive's record would be larger than a record may be.
    RecordTooLarge {
        /// The size that the record would have, in bytes.
        bytes: u64,
        /// The largest record that mxv writes and reads, in bytes.
        most: u64,
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
    /// Each checkout as the archive would record it. The commits of the
    /// uncommitted work are written later, so a checkout that the archive
    /// still retires has none yet.
    pub checkouts: Vec<ArchivedCheckout>,
    /// What the archive would lose, with `--force`.
    pub lost: Vec<String>,
    /// The uncommitted work of each checkout that the archive retires and
    /// keeps, in the order of the checkouts.
    pub uncommitted: Vec<Uncommitted>,
    /// The extra entries of the workspace's directory that the archive
    /// copies, without digests.
    pub root: Vec<String>,
    /// Why the list of commits on this machine may be incomplete, such as
    /// an origin that could not be fetched, and what the archive loses
    /// without refusing, such as a clone's hooks.
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

/// What the records say about an archive of one name.
enum Subject {
    /// The workspace to archive, with the record of an earlier run.
    Found(Box<Workspace>, Option<Box<Archive>>),
    /// The answer that needs no look at the workspace.
    Done(ArchivePlanned),
}

/// Returns the work of each submodule in `found`, of the checkout at
/// `checkout`, as the submodule's path and the work, one item for each.
/// An ignored path that the built-in disposable set names is a build's
/// output and does not count, unless it holds what a build does not make,
/// such as a repository. A commit that only another reference of a
/// submodule's git directory keeps counts as work of `.git/modules`.
pub(super) fn submodule_items(checkout: &Path, found: &Concealed) -> Vec<(String, String)> {
    let disposable = DisposableSet::built_in();
    let mut items = Vec::new();
    for work in found.submodules.iter().chain(&found.orphaned) {
        let lists = [&work.changes.changed, &work.stashes, &work.commits];
        let root = checkout.join(&work.path);
        let ignored = work.changes.ignored.iter().filter(|path| {
            disposable.source(path).is_none() || crate::disk::keep_reason(&root, path).is_some()
        });
        items.extend(
            lists
                .into_iter()
                .flatten()
                .chain(ignored)
                .map(|item| (work.path.clone(), item.clone())),
        );
    }
    items.extend(
        found
            .module_referenced
            .iter()
            .map(|item| (".git/modules".to_owned(), item.clone())),
    );
    items
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

/// Returns the items of `listed`, each `<folder>: <item>`, whose folder is
/// `folder`, without the folder.
fn of_folder<'a>(listed: &'a [String], folder: &'a str) -> impl Iterator<Item = &'a str> {
    listed.iter().filter_map(move |item| {
        item.strip_prefix(folder)
            .and_then(|rest| rest.strip_prefix(": "))
    })
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
    pub(super) fn admit(
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
        if let Some(refused) = check_checkouts(&workspace, resuming.as_ref()) {
            return Ok(refused);
        }
        if let Some(refused) = self.check_heads(&workspace) {
            return Ok(refused);
        }
        let mut warnings = if refresh {
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
        let mut unkeepable = Unkeepable::default();
        let mut uncommitted = Vec::new();
        let mut checkouts = Vec::new();
        for planned in &removal.checkouts {
            let folder = planned.checkout.name.as_str();
            let path = &planned.checkout.path;
            let earlier = resuming
                .as_ref()
                .and_then(|archive| archive.checkouts.iter().find(|kept| kept.folder == folder));
            if planned.directory != Fate::Removed {
                match earlier {
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
            let head = self.backend.head(path)?;
            let Some(commit) = head.commit.clone() else {
                return Ok(unusable(folder, "its HEAD names no commit"));
            };
            let concealed = self.backend.concealed(path)?;
            if workspace.kind == WorkspaceKind::Worktree
                && let Some(submodule) = concealed.submodules.first()
            {
                return Ok(unusable(
                    folder,
                    &format!(
                        "the submodule {} is initialised, and a worktree's submodules stop working when mxv moves the worktree aside",
                        submodule.path
                    ),
                ));
            }
            uncommitted.push(self.scan_checkout(
                &removal,
                planned,
                &concealed,
                options.max_file,
                &mut unkeepable,
            )?);
            if refresh {
                warnings.extend(self.unkept_names(planned)?);
            }
            let local_only = self.backend.local_only_commits(path, "HEAD")?;
            let kept = self.kept(workspace.kind, planned, head.branch.as_deref())?;
            checkouts.push(self.archived(planned, head, commit, local_only, kept, &id)?);
        }
        let generated = self.generated_agents(&workspace)?;
        let scanned = root::scan(&workspace, &generated)?;
        if let Some(refused) = refuse_root(&workspace, &scanned) {
            return Ok(refused);
        }
        for special in &scanned.special {
            unkeepable.push(
                "root",
                &workspace.path,
                UnkeptKind::Root {
                    entry: special.name.clone(),
                },
            );
        }
        if !options.force && !unkeepable.is_empty() {
            return Ok(ArchivePlanned::Refused(ArchiveRefusal::Unkeepable(
                Box::new(unkeepable),
            )));
        }
        let lost = if options.force {
            let mut lost: Vec<String> = unkeepable.items.iter().map(Unkept::to_string).collect();
            lost.extend(removal.manifest.iter().cloned());
            lost
        } else {
            Vec::new()
        };
        let plan = ArchivePlan {
            removal,
            options,
            id,
            resuming,
            checkouts,
            lost,
            uncommitted,
            root: scanned.paths(),
            warnings,
        };
        let bytes = plan.predicted_bytes(name, scanned)?;
        if bytes > MOST_RECORD_BYTES {
            return Ok(ArchivePlanned::Refused(ArchiveRefusal::RecordTooLarge {
                bytes,
                most: MOST_RECORD_BYTES,
            }));
        }
        Ok(ArchivePlanned::Ready(Box::new(plan)))
    }

    /// Adds to `unkeepable` what the checkout that `planned` retires holds
    /// and an archive cannot keep: its ignored files, what `concealed`
    /// found, and each file over `max_file` bytes. Returns the checkout's
    /// uncommitted changes that the archive keeps, which leave out the
    /// ignored files and the nested repositories.
    fn scan_checkout(
        &self,
        removal: &Removal,
        planned: &CheckoutRemoval,
        concealed: &Concealed,
        max_file: u64,
        unkeepable: &mut Unkeepable,
    ) -> Result<Uncommitted, Error> {
        let folder = planned.checkout.name.as_str();
        let path = &planned.checkout.path;
        for ignored in of_folder(&removal.ignored, folder) {
            unkeepable.push(
                folder,
                path,
                UnkeptKind::Ignored {
                    path: ignored.to_owned(),
                },
            );
        }
        unkeepable.add_concealed(folder, path, concealed);
        for large in self.backend.large_files(path, max_file)? {
            unkeepable.push(
                folder,
                path,
                UnkeptKind::TooLarge {
                    path: large.path,
                    bytes: large.bytes,
                    most: max_file,
                    untracked: large.untracked,
                },
            );
        }
        let ignored: Vec<&str> = of_folder(&removal.ignored, folder).collect();
        let nested: Vec<String> = concealed
            .nested
            .iter()
            .map(|path| format!("?? {path}"))
            .collect();
        Ok(Uncommitted {
            folder: folder.to_owned(),
            changes: of_folder(&removal.discarded, folder)
                .filter(|item| !ignored.contains(item) && !nested.iter().any(|n| n == item))
                .map(str::to_owned)
                .collect(),
        })
    }

    /// Returns what the archive keeps of the checkout that `planned`
    /// retires besides `HEAD`, whose branch is `current`, without its
    /// uncommitted work, which the archive commits later. A worktree's
    /// stashes, branches and tags stay in its repository, so only its
    /// per-worktree references and the commits that only the reflog of its
    /// detached `HEAD` holds are kept.
    fn kept(
        &self,
        kind: WorkspaceKind,
        planned: &CheckoutRemoval,
        current: Option<&str>,
    ) -> Result<Kept, Error> {
        let path = &planned.checkout.path;
        let own = RefHolder::Repository(path);
        let mut kept = Kept {
            worktree: self
                .backend
                .refs_under(own, "refs/worktree/")?
                .into_iter()
                .collect(),
            ..Kept::default()
        };
        if kind == WorkspaceKind::Clone {
            kept.stashes = self
                .backend
                .stash_entries(path)?
                .into_iter()
                .rev()
                .map(|entry| KeptStash {
                    commit: entry.commit,
                    message: entry.message,
                })
                .collect();
            // A branch that names what origin's branch of the same name
            // names, such as the default branch that the clone started
            // with, comes back with a rebuilt clone, so only the others are
            // kept.
            let mut held_heads = self.backend.refs_under(own, "refs/remotes/origin/")?;
            let mut held: Vec<(String, String)> =
                self.backend.refs_under(own, "refs/mxv/origin/tags/")?;
            if let Some(mirror) = planned
                .checkout
                .mirror
                .as_deref()
                .filter(|dir| dir.is_dir())
            {
                let mirror = RefHolder::Mirror(mirror);
                held_heads.extend(self.backend.refs_under(mirror, "refs/heads/")?);
                held.extend(self.backend.refs_under(mirror, "refs/tags/")?);
            }
            kept.branches = self
                .backend
                .refs_under(own, "refs/heads/")?
                .into_iter()
                .filter(|branch| Some(branch.0.as_str()) != current && !held_heads.contains(branch))
                .collect();
            kept.tags = self
                .backend
                .refs_under(own, "refs/tags/")?
                .into_iter()
                .filter(|tag| !held.contains(tag))
                .collect();
        }
        kept.orphans =
            self.backend
                .orphans(path, &kept.references(), kind == WorkspaceKind::Worktree)?;
        Ok(kept)
    }

    /// Returns the refusal of a checkout of `workspace` whose `HEAD` names
    /// no commit, which the classification could not read.
    fn check_heads(&self, workspace: &Workspace) -> Option<ArchivePlanned> {
        workspace
            .checkouts
            .iter()
            .filter(|checkout| checkout.path.is_dir())
            .find(|checkout| {
                self.backend
                    .head(&checkout.path)
                    .is_ok_and(|head| head.commit.is_none())
            })
            .map(|checkout| unusable(&checkout.name, "its HEAD names no commit"))
    }

    /// Returns a warning for each kind of thing in the clone that `planned`
    /// retires which neither the archive keeps nor a rebuilt clone has.
    fn unkept_names(&self, planned: &CheckoutRemoval) -> Result<Vec<String>, Error> {
        let checkout = &planned.checkout;
        if checkout.mirror.is_none() {
            return Ok(Vec::new());
        }
        let names = self.backend.local_names(&checkout.path, &checkout.source)?;
        let mut warnings: Vec<String> = [
            ("remotes", names.remotes),
            ("hooks", names.hooks),
            ("configuration keys", names.config),
            ("exclude rules", names.excludes),
        ]
        .into_iter()
        .filter(|(_, listed)| !listed.is_empty())
        .map(|(what, listed)| {
            format!(
                "the restored clone {} will not have the {what} {}, which exist only in this clone",
                checkout.name,
                listed.join(", ")
            )
        })
        .collect();
        warnings.extend(names.worktrees.iter().map(|worktree| {
            format!(
                "the worktree {worktree} was made from the clone {}, and it stops working when mxv deletes the clone",
                checkout.name
            )
        }));
        Ok(warnings)
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
    /// `commit`, the commit of its `head`, with the pin of the archive `id`
    /// and what the archive `kept` besides.
    fn archived(
        &self,
        planned: &CheckoutRemoval,
        head: Head,
        commit: String,
        local_only: Vec<String>,
        kept: Kept,
        id: &str,
    ) -> Result<ArchivedCheckout, Error> {
        let checkout = &planned.checkout;
        let detached = head.branch.is_none();
        let branch = head.branch.unwrap_or_else(|| checkout.branch.clone());
        let origin = match self.backend.origin_url(&checkout.path) {
            Ok(url) => Some(recorded_origin(&url)),
            Err(Error::NoOrigin { .. }) => None,
            Err(error) => return Err(error),
        };
        let upstream_remote = if detached {
            None
        } else {
            self.backend.upstream_remote(&checkout.path, &branch)?
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
            upstream_remote,
            branch,
            detached,
            base: checkout.base.clone(),
            commit,
            mirror: checkout.mirror.clone(),
            pin: pin_ref(id, &checkout.name),
            local_only,
            kept,
        })
    }
}

impl ArchivePlan {
    /// Returns the record of the archive of the workspace `name`, in the
    /// state [`ArchiveState::Retiring`].
    pub(super) fn record(&self, name: &WorkspaceName) -> Archive {
        let workspace = &self.removal.workspace;
        // An earlier run's item stays only when this run cannot read it
        // again: the checkout that held it is gone. While any checkout
        // stands, the workspace's directory is still whole too, so its items
        // are read again.
        let stands = |planned: &&CheckoutRemoval| planned.directory == Fate::Removed;
        let all_gone = !self
            .removal
            .checkouts
            .iter()
            .any(|planned| stands(&planned));
        let already_lost = |item: &String| {
            all_gone
                || self.removal.checkouts.iter().any(|planned| {
                    !stands(&planned) && item.starts_with(&format!("{}: ", planned.checkout.name))
                })
        };
        let mut lost: Vec<String> = self
            .resuming
            .iter()
            .flat_map(|archive| archive.lost.iter().filter(|item| already_lost(item)))
            .cloned()
            .collect();
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
            manifest_mode: earlier.and_then(|archive| archive.manifest_mode),
        }
    }

    /// Returns the size that the record of this plan would have, in bytes,
    /// with the digests of the files that `scanned` found, which nothing has
    /// read yet, and the commits of the uncommitted work, which nothing has
    /// written yet, counted at their full length.
    fn predicted_bytes(&self, name: &WorkspaceName, scanned: Scanned) -> Result<u64, Error> {
        let mut archive = self.record(name);
        let mut merged: BTreeMap<String, RootEntry> = archive
            .root
            .drain(..)
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        for mut entry in scanned.entries {
            if let RootKind::File { digest, .. } = &mut entry.kind {
                *digest = "0".repeat(DIGEST_LENGTH);
            }
            merged.insert(entry.path.clone(), entry);
        }
        archive.root = merged.into_values().collect();
        archive.manifest = Some("0".repeat(DIGEST_LENGTH));
        archive.manifest_mode = Some(0o7777);
        for checkout in &mut archive.checkouts {
            if checkout.kept.wip.is_none() {
                checkout.kept.wip = Some(crate::archive::KeptWip {
                    index: "0".repeat(DIGEST_LENGTH),
                    worktree: "0".repeat(DIGEST_LENGTH),
                });
            }
        }
        let bytes = serde_json::to_vec_pretty(&archive).map_err(|source| Error::CorruptRecord {
            path: PathBuf::from(RECORD_FILE),
            source,
        })?;
        Ok(bytes.len() as u64 + 1)
    }
}

/// Returns the refusal of a directory of `workspace` that an archive could
/// not finish on: an entry that `scanned` could not read or empty, or a
/// workspace manifest that has another hard link. A restored manifest would
/// be a file of its own, which mxv trusts as the person's.
fn refuse_root(workspace: &Workspace, scanned: &Scanned) -> Option<ArchivePlanned> {
    if !scanned.blocked.is_empty() {
        return Some(unusable(
            "root",
            &format!(
                "mxv cannot read or empty {}; give yourself read and write access, then archive again",
                scanned.blocked.join(", ")
            ),
        ));
    }
    let manifest = workspace.manifest_file();
    let hard_linked = fs::symlink_metadata(&manifest)
        .is_ok_and(|metadata| metadata.is_file() && crate::disk::links(&metadata) > 1);
    hard_linked.then(|| {
        unusable(
            "root",
            &format!(
                "the workspace manifest {} has another hard link, and a restored copy would be a file of its own",
                manifest.display()
            ),
        )
    })
}
