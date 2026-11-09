//! Deciding whether a workspace can be archived, and what the archive would
//! keep and lose.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use phase::WorkspaceId;
use phase::random::RANDOM_DEVICE;

use crate::archive::record::now;
use crate::archive::root::{self, RootEntry, RootKind, Scanned};
use crate::archive::{
    ARCHIVE_VERSION, Archive, ArchiveState, ArchivedCheckout, Archives, DIGEST_LENGTH, RECORD_FILE,
    pin_ref,
};
use crate::backend::{Backend, Concealed, Head};
use crate::disposable::DisposableSet;
use crate::error::Error;
use crate::mirror::recorded_origin;
use crate::name::WorkspaceName;
use crate::record::{Workspace, WorkspaceKind};
use crate::store::MOST_RECORD_BYTES;
use crate::workspaces::{Allow, CheckoutRemoval, Fate, Mode, Removal, RemovalPlan, Workspaces};

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
    /// Each repository inside a checkout that git does not track.
    pub nested: Vec<String>,
    /// Each file marked skip-worktree or assume-unchanged that has local
    /// edits, which `git status` does not show.
    pub hidden: Vec<String>,
    /// The changes, ignored files, stashes and unpushed commits of each
    /// submodule, with the submodule's path after the folder.
    pub submodules: Vec<String>,
    /// Each file below a checkout that no listing of git accounts for, such
    /// as a file in a submodule directory that is not initialised.
    pub unlisted: Vec<String>,
    /// Each git operation that stands paused, such as a merge, whose state
    /// the checkout's git directory holds.
    pub paused: Vec<String>,
}

impl Unsaved {
    /// Adds what `found` says about the checkout at `checkout`, in `folder`.
    /// A nested repository that `git status` also lists is named once.
    fn add_concealed(&mut self, folder: &str, checkout: &Path, found: &Concealed) {
        let named = |item: &String| format!("{folder}: {item}");
        for path in &found.nested {
            let listed = format!("{folder}: ?? {path}");
            self.changes.retain(|item| *item != listed);
        }
        self.nested.extend(found.nested.iter().map(named));
        self.hidden.extend(found.hidden.iter().map(named));
        self.submodules
            .extend(submodule_items(checkout, found).iter().map(named));
        self.unlisted.extend(found.unlisted.iter().map(named));
        self.paused.extend(found.paused.iter().map(named));
        self.commits.extend(found.referenced.iter().map(named));
    }

    /// Returns every item in one list.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        let mut all = self.changes.clone();
        all.extend(self.ignored.iter().cloned());
        all.extend(self.stashes.iter().cloned());
        all.extend(self.commits.iter().cloned());
        all.extend(self.root.iter().cloned());
        all.extend(self.nested.iter().cloned());
        all.extend(self.hidden.iter().cloned());
        all.extend(self.submodules.iter().cloned());
        all.extend(self.unlisted.iter().cloned());
        all.extend(self.paused.iter().cloned());
        all
    }

    /// Reports whether `--wip` can save everything that is listed. A commit
    /// would store a nested repository or a submodule as a commit id without
    /// its files, it leaves a hidden edit and a file that git does not list
    /// out, and it does not keep a paused git operation.
    #[must_use]
    pub fn wip_saves_all(&self) -> bool {
        self.ignored.is_empty()
            && self.stashes.is_empty()
            && self.commits.is_empty()
            && self.root.is_empty()
            && self.nested.is_empty()
            && self.hidden.is_empty()
            && self.submodules.is_empty()
            && self.unlisted.is_empty()
            && self.paused.is_empty()
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
    Unsaved(Box<Unsaved>),
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

/// What the records say about an archive of one name.
enum Subject {
    /// The workspace to archive, with the record of an earlier run.
    Found(Box<Workspace>, Option<Box<Archive>>),
    /// The answer that needs no look at the workspace.
    Done(ArchivePlanned),
}

/// Returns the work of each submodule in `found`, of the checkout at
/// `checkout`, one item for each, with the submodule's path in front. An
/// ignored path that the built-in disposable set names is a build's output
/// and does not count, unless it holds what a build does not make, such as
/// a repository.
pub(super) fn submodule_items(checkout: &Path, found: &Concealed) -> Vec<String> {
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
                .map(|item| format!("{}: {item}", work.path)),
        );
    }
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
            let head = self.backend.head(&planned.checkout.path)?;
            let Some(commit) = head.commit.clone() else {
                return Ok(unusable(folder, "its HEAD names no commit"));
            };
            let concealed = self.backend.concealed(&planned.checkout.path)?;
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
            unsaved.add_concealed(folder, &planned.checkout.path, &concealed);
            if refresh {
                warnings.extend(self.unkept_names(planned, head.branch.as_deref())?);
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
        let generated = self.generated_agents(&workspace)?;
        let scanned = root::scan(&workspace, &generated)?;
        if let Some(refused) = refuse_root(&workspace, &scanned) {
            return Ok(refused);
        }
        unsaved
            .root
            .extend(scanned.special.iter().map(root::lost_name));
        if !options.force && !unsaved.is_empty() && !(options.wip && unsaved.wip_saves_all()) {
            return Ok(ArchivePlanned::Refused(ArchiveRefusal::Unsaved(Box::new(
                unsaved,
            ))));
        }
        let (lost, wip) = if options.force {
            let mut lost = unsaved.all();
            lost.extend(removal.manifest.iter().cloned());
            (lost, Vec::new())
        } else {
            (Vec::new(), unsaved.changes)
        };
        let plan = ArchivePlan {
            removal,
            options,
            id,
            resuming,
            checkouts,
            lost,
            wip,
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

    /// Returns a warning for each kind of name in the clone that `planned`
    /// retires which its mirror does not hold, so a rebuilt clone lacks it.
    /// `current` is the branch that the record keeps.
    fn unkept_names(
        &self,
        planned: &CheckoutRemoval,
        current: Option<&str>,
    ) -> Result<Vec<String>, Error> {
        let checkout = &planned.checkout;
        let Some(mirror) = &checkout.mirror else {
            return Ok(Vec::new());
        };
        let names = self
            .backend
            .local_names(&checkout.path, mirror, &checkout.source)?;
        let branches: Vec<String> = names
            .branches
            .into_iter()
            .filter(|branch| Some(branch.as_str()) != current)
            .collect();
        Ok([
            ("branches", branches),
            ("tags", names.tags),
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
        .collect())
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
    /// read yet, counted at their full length.
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
