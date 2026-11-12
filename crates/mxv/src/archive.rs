//! The verbs `archive` and `restore`, the archive form of `list` and
//! `remove`, and the documents they print.
//!
//! The library in [`multiverse`] decides what an archive keeps and carries
//! it out. This module holds what only a command can do: the workspace's
//! lock, the question, stopping the workspace's processes, the check for
//! outside processes, and the docker volumes.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use crumbkit::{Document, DryRun, Listing, Prose, Run, Yes};
use multiverse::health::{self, Broken, Problem, Rebuilt};
use multiverse::phase::host::TableEntry;
use multiverse::{
    ARCHIVE_MAX_FILE, Archive, ArchiveOptions, ArchiveOutcome, ArchivePlan, ArchivePlanned,
    ArchiveRecords, ArchiveState, ArchivedCheckout, Archives, DisposablePath, Git, IgnoredEntry,
    IgnoredKind, KEPT_NAMESPACE, Mode, PutBack, RestoreOutcome, RestorePlanned, Uncommitted,
    Volumes, Workspace, WorkspaceName, Workspaces,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::{ListArgs, RemoveArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::reclaim::{user_reason, users};
use crate::rows::{LifecycleState, RemovalResult, WorkspaceRow};
use crate::run_failures::RunFailure;
use crate::units::Size;
use crate::verbs::{configured, parse_name};

mod failure;
#[cfg(test)]
mod tests;

pub(crate) use failure::ArchiveFailure;
pub(crate) use failure::CODES;
pub use failure::{
    ARCHIVE_CHECKOUT_UNUSABLE, ARCHIVE_IN_USE, ARCHIVE_MANIFEST_DISPOSABLE, ARCHIVE_MIRROR_MISSING,
    ARCHIVE_NAME_TAKEN, ARCHIVE_NOT_FOUND, ARCHIVE_PINS_GONE, ARCHIVE_RECORD_TOO_LARGE,
    ARCHIVE_RENAME_UNFINISHED, ARCHIVE_SOURCE_CHANGED, ARCHIVE_UNFINISHED, ARCHIVE_UNKEEPABLE,
    ARCHIVE_VOLUMES_IN_USE, RESTORE_BRANCH_MOVED, RESTORE_COMMIT_MISSING, RESTORE_IGNORED_DAMAGED,
    RESTORE_IGNORED_TAKEN, RESTORE_NAME_TAKEN, RESTORE_NOT_FOUND, RESTORE_ROOT_FILE_CHANGED,
    RESTORE_UNFINISHED,
};
use failure::{archive_refusal, restore_refusal};

/// The document that `mxv archive` prints.
pub(crate) const ARCHIVE: Document<Prose<ArchiveResult>> = Document::new(
    "mxv",
    "archive",
    2,
    "What the archive kept and where each checkout's commit is pinned, or what it would do on a dry run.",
);

/// The document that `mxv restore` prints.
pub(crate) const RESTORE: Document<Prose<RestoreResult>> = Document::new(
    "mxv",
    "restore",
    1,
    "The workspace that the restore rebuilt and each port that changed, or what it would do on a dry run.",
);

/// The arguments of `mxv archive`.
#[derive(Debug, Args)]
pub(crate) struct ArchiveArgs {
    /// The workspace, which defaults to the one that holds the current directory
    #[arg(short, long, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    /// Lose what the archive cannot keep, such as a paused merge or a file over archive_max_file, and list it in the archive
    #[arg(long)]
    pub(crate) force: bool,

    /// Delete the workspace's docker volumes and the data in them; without this flag the volumes stay for the restore
    #[arg(long)]
    pub(crate) delete_volumes: bool,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Archive the workspace without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// The arguments of `mxv restore`.
#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    /// The name of the archived workspace
    pub(crate) name: String,

    /// Restore without the ignored files whose copy in the archive is missing or damaged, and list them as lost
    #[arg(long)]
    pub(crate) force: bool,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Restore the workspace without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// Where one checkout's commit is kept.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PinRow {
    /// The checkout's folder.
    folder: String,
    /// The branch of the checkout, or null for a detached `HEAD`.
    branch: Option<String>,
    /// The commit that the checkout's `HEAD` named.
    commit: String,
    /// The repository or mirror that holds the reference.
    repository: String,
    /// The reference that keeps the commit.
    reference: String,
}

/// What an archive kept of one checkout besides the commit of its `HEAD`.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct KeptRow {
    /// The checkout's folder.
    folder: String,
    /// The branch of the checkout, or null for a detached `HEAD`.
    branch: Option<String>,
    /// Whether the archive kept uncommitted work, which restore puts back
    /// uncommitted.
    uncommitted: bool,
    /// Each staged or unstaged change of a tracked file, as
    /// `git status --porcelain` prints it, such as ` M src/lib.rs`.
    changes: Vec<String>,
    /// Each untracked path, relative to the checkout. A directory ends
    /// with `/`.
    untracked: Vec<String>,
    /// The number of stash entries.
    stashes: usize,
    /// Each local branch besides the checkout's own.
    branches: Vec<String>,
    /// Each tag that origin does not hold.
    tags: Vec<String>,
    /// Each per-worktree reference, by its name below `refs/worktree/`.
    worktree: Vec<String>,
    /// Each commit that no branch reaches, which restore keeps under
    /// `refs/mxv/kept/`.
    orphans: Vec<String>,
}

impl KeptRow {
    /// Returns what the archive keeps of `checkout`, whose uncommitted
    /// work `uncommitted` lists, or `None` when it keeps nothing besides
    /// `HEAD`.
    fn of(checkout: &ArchivedCheckout, uncommitted: &[Uncommitted]) -> Option<Self> {
        let lines: Vec<&String> = uncommitted
            .iter()
            .filter(|work| work.folder == checkout.folder)
            .flat_map(|work| &work.changes)
            .collect();
        let kept = &checkout.kept;
        let row = Self {
            folder: checkout.folder.clone(),
            branch: (!checkout.detached).then(|| checkout.branch.clone()),
            uncommitted: kept.wip.is_some() || !lines.is_empty(),
            changes: lines
                .iter()
                .filter(|line| !line.starts_with("??"))
                .map(|line| (*line).clone())
                .collect(),
            untracked: lines
                .iter()
                .filter_map(|line| line.strip_prefix("?? "))
                .map(str::to_owned)
                .collect(),
            stashes: kept.stashes.len(),
            branches: kept.branches.keys().cloned().collect(),
            tags: kept.tags.keys().cloned().collect(),
            worktree: kept.worktree.keys().cloned().collect(),
            orphans: kept.orphans.clone(),
        };
        let empty = !row.uncommitted
            && row.stashes == 0
            && row.branches.is_empty()
            && row.tags.is_empty()
            && row.worktree.is_empty()
            && row.orphans.is_empty();
        (!empty).then_some(row)
    }

    /// Writes the sentences of this row, with `keeps` as the verb, such
    /// as `would keep`.
    fn write(&self, formatter: &mut fmt::Formatter<'_>, keeps: &str) -> fmt::Result {
        let folder = &self.folder;
        if !self.changes.is_empty() {
            write!(
                formatter,
                "mxv {keeps} the staged and unstaged changes of {folder}, {}, as uncommitted work that restore brings back.",
                counted(self.changes.len(), "file", "files")
            )?;
            match &self.branch {
                Some(branch) => writeln!(formatter, " The branch {branch} stays where it is.")?,
                None => writeln!(formatter)?,
            }
        }
        for path in &self.untracked {
            let what = if path.ends_with('/') {
                "directory"
            } else {
                "file"
            };
            writeln!(
                formatter,
                "mxv {keeps} the untracked {what} {folder}/{path} with the uncommitted work."
            )?;
        }
        if self.uncommitted && self.changes.is_empty() && self.untracked.is_empty() {
            writeln!(
                formatter,
                "mxv {keeps} the uncommitted work of {folder}, which restore brings back."
            )?;
        }
        if self.stashes > 0 {
            writeln!(
                formatter,
                "mxv {keeps} {} of {folder}.",
                counted(self.stashes, "stash", "stashes")
            )?;
        }
        for branch in &self.branches {
            writeln!(
                formatter,
                "mxv {keeps} the local branch {branch} of {folder}."
            )?;
        }
        for tag in &self.tags {
            writeln!(
                formatter,
                "mxv {keeps} the tag {tag} of {folder}, which origin does not hold."
            )?;
        }
        for name in &self.worktree {
            writeln!(
                formatter,
                "mxv {keeps} the per-worktree reference refs/worktree/{name} of {folder}."
            )?;
        }
        if !self.orphans.is_empty() {
            let references: Vec<String> = self
                .orphans
                .iter()
                .map(|commit| format!("{KEPT_NAMESPACE}{commit}"))
                .collect();
            writeln!(
                formatter,
                "mxv {keeps} {} of {folder} that no branch reaches, {}, as {} after restore.",
                counted(self.orphans.len(), "commit", "commits"),
                self.orphans.join(", "),
                listed(&references)
            )?;
        }
        Ok(())
    }
}

/// The ignored files and links of one checkout that an archive keeps in
/// its private tar.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct IgnoredRow {
    /// The checkout's folder.
    folder: String,
    /// Each file or link, relative to the workspace's directory, such as
    /// `app/.env`.
    paths: Vec<String>,
    /// The bytes of the files, which leaves out the links.
    bytes: u64,
}

impl IgnoredRow {
    /// Returns one row for each folder of `entries`, each a folder, a path
    /// relative to the workspace's directory and a number of bytes, in the
    /// order that the folders first appear.
    fn grouped(entries: impl IntoIterator<Item = (String, String, u64)>) -> Vec<Self> {
        let mut rows: Vec<Self> = Vec::new();
        for (folder, path, bytes) in entries {
            match rows.iter_mut().find(|row| row.folder == folder) {
                Some(row) => {
                    row.paths.push(path);
                    row.bytes += bytes;
                }
                None => rows.push(Self {
                    folder,
                    paths: vec![path],
                    bytes,
                }),
            }
        }
        rows
    }

    /// Returns the rows of what `plan` would keep.
    fn planned(plan: &ArchivePlan) -> Vec<Self> {
        Self::grouped(
            plan.ignored
                .iter()
                .map(|source| (source.folder.clone(), source.archived(), source.bytes)),
        )
    }

    /// Returns the rows of what `archive` keeps.
    fn kept(archive: &Archive) -> Vec<Self> {
        Self::grouped(archive.ignored.iter().map(|entry| {
            let bytes = if matches!(entry.kind, IgnoredKind::File) {
                entry.size
            } else {
                0
            };
            (entry.folder().to_owned(), entry.path.clone(), bytes)
        }))
    }

    /// Writes the sentence of this row, with `keeps` as the verb, such as
    /// `would keep`.
    fn write(&self, formatter: &mut fmt::Formatter<'_>, keeps: &str) -> fmt::Result {
        writeln!(
            formatter,
            "mxv {keeps} {} of {}, {} in all, in a private file that only you can read: {}.",
            counted(self.paths.len(), "ignored file", "ignored files"),
            self.folder,
            Size(self.bytes),
            self.paths.join(", ")
        )
    }
}

/// One disposable directory that an archive deletes, because a build makes
/// it again.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct DisposableRow {
    /// The checkout's folder.
    folder: String,
    /// The directory, relative to the checkout, with `/` after it, such as
    /// `target/`.
    path: String,
    /// The bytes that the directory took on disk.
    bytes: u64,
}

impl DisposableRow {
    /// Returns the rows of `paths`.
    fn of(paths: &[DisposablePath]) -> Vec<Self> {
        paths
            .iter()
            .map(|found| Self {
                folder: found.checkout.clone(),
                path: found.path.clone(),
                bytes: found.bytes,
            })
            .collect()
    }

    /// Writes the one sentence that names every row of `rows`, with
    /// `deletes` as the verb, such as `would delete`.
    fn write_all(rows: &[Self], formatter: &mut fmt::Formatter<'_>, deletes: &str) -> fmt::Result {
        if rows.is_empty() {
            return Ok(());
        }
        let named: Vec<String> = rows
            .iter()
            .map(|row| format!("{}/{} ({})", row.folder, row.path, Size(row.bytes)))
            .collect();
        let noun = if rows.len() == 1 {
            "directory"
        } else {
            "directories"
        };
        writeln!(
            formatter,
            "mxv {deletes} the disposable {noun} {}.",
            listed(&named)
        )
    }
}

/// Returns `count` with the noun that fits it, such as `1 file`.
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Returns `items` joined as a sentence joins them, such as `a, b and c`.
fn listed(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What `mxv archive` did, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ArchiveResult {
    /// Whether the run archived the workspace, would archive it on a dry
    /// run, or had nothing to do because no workspace has the name and its
    /// archive has finished. An unchanged result lists the archive's pins.
    state: LifecycleState,
    /// The workspace's name.
    name: String,
    /// The workspace's directory.
    path: String,
    /// Where each checkout's commit is kept.
    pins: Vec<PinRow>,
    /// Each commit that exists only on this machine, with the checkout's
    /// folder in front. The archive pushes nothing.
    local_only: Vec<String>,
    /// What the archive kept of each checkout besides its commit, or would
    /// keep, for each checkout that keeps something.
    kept: Vec<KeptRow>,
    /// The ignored files and links that the archive kept in a private tar,
    /// or would keep, for each checkout that has some. Restore writes them
    /// back.
    ignored: Vec<IgnoredRow>,
    /// What `--force` lost, or would lose.
    lost: Vec<String>,
    /// The workspace's units that the run stopped, or would stop.
    stopped: Vec<String>,
    /// The disposable directories that the run deleted, or would delete,
    /// because a build makes them again.
    disposable: Vec<DisposableRow>,
    /// Each entry that a process wrote at the top level of a disposable
    /// directory after mxv measured it, with the checkout's folder in
    /// front, such as `app: target/late.o`. The run deleted it with the
    /// directory.
    late: Vec<String>,
    /// The extra entries of the workspace's directory that the archive
    /// holds.
    root_files: Vec<String>,
    /// The docker volumes that `--delete-volumes` deleted.
    volumes: Vec<String>,
    /// What the run could not read or finish, one sentence for each.
    warnings: Vec<String>,
    /// The command that brings the workspace back.
    restore: String,
    /// The directory to change to, when the current directory was inside
    /// the workspace, or null.
    cd: Option<String>,
    /// Whether the result reports a dry run, which changed nothing.
    #[serde(skip)]
    planned: bool,
}

impl fmt::Display for ArchiveResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.state == LifecycleState::Unchanged {
            return writeln!(
                formatter,
                "The workspace {} is already archived, so there is nothing to do. `{}` brings it back.",
                self.name, self.restore
            );
        }
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let (archived, stops, deletes, loses, keeps, is) = if self.planned {
            (
                "would archive",
                "would stop",
                "would delete",
                "would lose",
                "would keep",
                "would stay",
            )
        } else {
            ("archived", "stopped", "deleted", "lost", "kept", "stays")
        };
        for process in &self.stopped {
            writeln!(formatter, "mxv {stops} the unit {process}.")?;
        }
        for row in &self.kept {
            row.write(formatter, keeps)?;
        }
        for row in &self.ignored {
            row.write(formatter, keeps)?;
        }
        for path in &self.lost {
            writeln!(formatter, "mxv {loses} {path}.")?;
        }
        DisposableRow::write_all(&self.disposable, formatter, deletes)?;
        for entry in &self.late {
            writeln!(
                formatter,
                "mxv deleted {entry}, which was written into a disposable directory after mxv measured it."
            )?;
        }
        for volume in &self.volumes {
            writeln!(formatter, "mxv {deletes} the docker volume {volume}.")?;
        }
        for path in &self.root_files {
            writeln!(formatter, "mxv {keeps} the file {path} in the archive.")?;
        }
        for pin in &self.pins {
            writeln!(
                formatter,
                "The commit {} of {} {is} in {} as {}.",
                pin.commit, pin.folder, pin.repository, pin.reference
            )?;
        }
        for commit in &self.local_only {
            writeln!(
                formatter,
                "The commit {commit} exists only on this machine."
            )?;
        }
        writeln!(formatter, "mxv {archived} the workspace {}.", self.name)?;
        writeln!(formatter, "`{}` brings it back.", self.restore)?;
        if let Some(dir) = &self.cd {
            write!(
                formatter,
                "The current directory was inside the workspace. Change to {dir}."
            )?;
        }
        Ok(())
    }
}

/// One port that a restore could not keep.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PortRow {
    /// The port's name in the manifest.
    name: String,
    /// The port that the workspace held.
    from: u16,
    /// The port that it holds now.
    to: u16,
}

/// One checkout that a restore rebuilt.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RestoredCheckout {
    /// The checkout's folder.
    folder: String,
    /// The checkout's directory.
    path: String,
    /// The checkout's branch, or null for a detached `HEAD`.
    branch: Option<String>,
    /// The commit that the checkout is at.
    commit: String,
}

/// What a restore put back into one checkout besides its commit.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PutBackRow {
    /// The checkout's folder.
    folder: String,
    /// The checkout's directory.
    path: String,
    /// Whether the restore put back uncommitted work.
    uncommitted: bool,
    /// The number of files with staged changes, which a dry run does not
    /// count.
    staged: usize,
    /// The number of files with unstaged changes, which a dry run does not
    /// count.
    unstaged: usize,
    /// The number of untracked paths, which a dry run does not count.
    untracked: usize,
    /// The number of stash entries.
    stashes: usize,
    /// Each local branch besides the checkout's own.
    branches: Vec<String>,
    /// Each tag that origin does not hold.
    tags: Vec<String>,
    /// Each per-worktree reference, by its name below `refs/worktree/`.
    worktree: Vec<String>,
    /// Each reference below `refs/mxv/kept/` that keeps a commit no branch
    /// reaches.
    kept: Vec<String>,
    /// Each submodule that comes back uninitialised, which a dry run does
    /// not list.
    submodules: Vec<String>,
    /// Each ignored file or link that the restore wrote back from the
    /// archive's private tar, relative to the workspace's directory, such
    /// as `app/.env`.
    ignored: Vec<String>,
}

impl PutBackRow {
    /// Returns the row of what a restore put back, `put_back`, into the
    /// checkout at `path`, which held uncommitted work when `uncommitted`.
    fn restored(put_back: PutBack, path: String, uncommitted: bool) -> Self {
        Self {
            folder: put_back.folder,
            path,
            uncommitted,
            staged: put_back.staged,
            unstaged: put_back.unstaged,
            untracked: put_back.untracked,
            stashes: put_back.stashes,
            branches: put_back.branches,
            tags: put_back.tags,
            worktree: put_back.worktree,
            kept: put_back.kept,
            submodules: put_back.submodules,
            ignored: put_back.ignored,
        }
    }

    /// Returns the row of what a restore of `checkout` would put back,
    /// with the entries of `ignored` that belong to it.
    fn planned(checkout: &ArchivedCheckout, ignored: &[IgnoredEntry]) -> Self {
        let kept = &checkout.kept;
        Self {
            folder: checkout.folder.clone(),
            path: checkout.path.display().to_string(),
            uncommitted: kept.wip.is_some(),
            staged: 0,
            unstaged: 0,
            untracked: 0,
            stashes: kept.stashes.len(),
            branches: kept.branches.keys().cloned().collect(),
            tags: kept.tags.keys().cloned().collect(),
            worktree: kept.worktree.keys().cloned().collect(),
            kept: kept
                .orphans
                .iter()
                .map(|commit| format!("{KEPT_NAMESPACE}{commit}"))
                .collect(),
            submodules: Vec::new(),
            ignored: ignored
                .iter()
                .filter(|entry| entry.folder() == checkout.folder)
                .map(|entry| entry.path.clone())
                .collect(),
        }
    }

    /// Writes the sentences of this row, with `puts` as the verb, such as
    /// `would put`.
    fn write(&self, formatter: &mut fmt::Formatter<'_>, puts: &str) -> fmt::Result {
        let folder = &self.folder;
        if self.uncommitted {
            let parts: Vec<String> = [
                (self.staged, "staged file", "staged files"),
                (self.unstaged, "unstaged file", "unstaged files"),
                (self.untracked, "untracked file", "untracked files"),
            ]
            .into_iter()
            .filter(|(count, ..)| *count > 0)
            .map(|(count, one, many)| counted(count, one, many))
            .collect();
            if parts.is_empty() {
                writeln!(
                    formatter,
                    "mxv {puts} back the uncommitted work of {folder}."
                )?;
            } else {
                writeln!(
                    formatter,
                    "mxv {puts} back the uncommitted work of {folder}: {}.",
                    listed(&parts)
                )?;
            }
        }
        if self.stashes > 0 {
            writeln!(
                formatter,
                "mxv {puts} back {} of {folder}.",
                counted(self.stashes, "stash", "stashes")
            )?;
        }
        let mut references: Vec<String> = self
            .branches
            .iter()
            .map(|branch| format!("the local branch {branch}"))
            .collect();
        references.extend(self.tags.iter().map(|tag| format!("the tag {tag}")));
        references.extend(
            self.worktree
                .iter()
                .map(|name| format!("the per-worktree reference refs/worktree/{name}")),
        );
        if !references.is_empty() {
            writeln!(
                formatter,
                "mxv {puts} back {} of {folder}.",
                listed(&references)
            )?;
        }
        if !self.kept.is_empty() {
            writeln!(
                formatter,
                "mxv {puts} back {} of {folder} as {}.",
                counted(self.kept.len(), "commit", "commits"),
                listed(&self.kept)
            )?;
        }
        if !self.ignored.is_empty() {
            writeln!(
                formatter,
                "mxv {puts} back {} of {folder}: {}.",
                counted(self.ignored.len(), "ignored file", "ignored files"),
                self.ignored.join(", ")
            )?;
        }
        Ok(())
    }

    /// Writes a warning for each submodule that comes back uninitialised.
    fn write_submodules(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for submodule in &self.submodules {
            writeln!(
                formatter,
                "Warning: the submodule {submodule} of {} comes back uninitialised. `git -C {} submodule update --init` initialises it.",
                self.folder, self.path
            )?;
        }
        Ok(())
    }

    /// Reports whether the row says nothing.
    fn is_empty(&self) -> bool {
        !self.uncommitted
            && self.stashes == 0
            && self.branches.is_empty()
            && self.tags.is_empty()
            && self.worktree.is_empty()
            && self.kept.is_empty()
            && self.submodules.is_empty()
            && self.ignored.is_empty()
    }
}

/// What `mxv restore` did, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RestoreResult {
    /// Whether the run restored the workspace, would restore it on a dry
    /// run, or had nothing to do because a workspace already holds the
    /// name and no archive needs restoring.
    state: LifecycleState,
    /// The workspace's name.
    name: String,
    /// The workspace's directory.
    path: String,
    /// The pins of the archive record that an earlier restore rebuilt and
    /// stopped before it deleted, which an unchanged run deleted with the
    /// record, or would delete on a dry run. Empty otherwise.
    stale_pins: Vec<String>,
    /// Whether an unchanged run found such an archive record, which may
    /// have no pins.
    #[serde(skip)]
    stale: bool,
    /// The checkouts that the restore rebuilt.
    checkouts: Vec<RestoredCheckout>,
    /// What the restore put back into each checkout besides its commit, or
    /// would put back, for each checkout that has something.
    put_back: Vec<PutBackRow>,
    /// Each ignored file or link that `--force` left out, because the
    /// archive's copy of it is missing or differs from its digest, relative
    /// to the workspace's directory. A dry run does not read the copies, so
    /// it lists none.
    lost: Vec<String>,
    /// Each port that the restore gave a new number.
    ports: Vec<PortRow>,
    /// The folder of each clone whose mirror mxv clones again from its
    /// origin.
    missing_mirrors: Vec<String>,
    /// What the run could not finish, one sentence for each.
    warnings: Vec<String>,
    /// Whether the result reports a dry run, which changed nothing.
    #[serde(skip)]
    planned: bool,
}

impl fmt::Display for RestoreResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.state == LifecycleState::Unchanged {
            for warning in &self.warnings {
                writeln!(formatter, "Warning: {warning}.")?;
            }
            // Without a stale archive, the workspace of the name may be one
            // that `mxv new` made after the archive was deleted.
            if !self.stale {
                return writeln!(
                    formatter,
                    "A workspace named {} exists and no archive of it is left, so there is nothing to do.",
                    self.name
                );
            }
            let deleted = if self.planned {
                "would delete"
            } else {
                "deleted"
            };
            writeln!(
                formatter,
                "mxv {deleted} the archive record of {} that an earlier restore left, with its pins {}.",
                self.name,
                listed(&self.stale_pins)
            )?;
            return writeln!(
                formatter,
                "The workspace {} is already restored, so there is nothing to do.",
                self.name
            );
        }
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let (restored, gives, clones, puts) = if self.planned {
            ("would restore", "would give", "would clone", "would put")
        } else {
            ("restored", "gave", "cloned", "put")
        };
        for folder in &self.missing_mirrors {
            writeln!(formatter, "mxv {clones} the mirror of {folder} again.")?;
        }
        for checkout in &self.checkouts {
            let place = checkout.branch.as_ref().map_or_else(
                || format!("detached at {}", checkout.commit),
                |branch| format!("on {branch} at {}", checkout.commit),
            );
            writeln!(formatter, "The checkout {} is {place}.", checkout.path)?;
        }
        for row in &self.put_back {
            row.write(formatter, puts)?;
        }
        for path in &self.lost {
            writeln!(
                formatter,
                "mxv lost the ignored file {path}, because the archive's copy of it is missing or damaged."
            )?;
        }
        for row in &self.put_back {
            row.write_submodules(formatter)?;
        }
        for port in &self.ports {
            writeln!(
                formatter,
                "mxv {gives} {} the port {}, because {} is taken.",
                port.name, port.to, port.from
            )?;
        }
        write!(
            formatter,
            "mxv {restored} the workspace {} at {}.",
            self.name, self.path
        )
    }
}

/// Returns the archives that `environment` names.
pub(crate) fn archives(environment: &Environment) -> Result<Archives, MxvFailure> {
    Ok(Archives::new(
        environment.archives()?,
        environment.archive_data()?,
    ))
}

/// Returns the pids of the ancestors of `own` in `table`, which walks the
/// parents with the stop rules of [`crate::reclaim::owned`]: a parent of 1
/// or 0 belongs to the system, and a walk longer than the table has met a
/// loop.
pub(crate) fn ancestors(table: &[TableEntry], own: u32) -> BTreeSet<u32> {
    let parents: std::collections::BTreeMap<u32, u32> = table
        .iter()
        .map(|entry| (entry.pid, entry.parent))
        .collect();
    let mut found = BTreeSet::new();
    let mut pid = own;
    for _ in 0..=parents.len() {
        match parents.get(&pid) {
            Some(parent) if *parent > 1 && *parent != pid => {
                found.insert(*parent);
                pid = *parent;
            }
            _ => break,
        }
    }
    found
}

/// Returns why a process outside phase keeps `workspace` from being
/// archived, or `None`. The shell or agent that started this command may
/// stand in the workspace, so the ancestors of this process do not count.
/// A background job of that shell is no ancestor, and it counts.
fn outside_user(workspace: &Workspace) -> Result<Option<String>, MxvFailure> {
    let table = multiverse::process_table().map_err(MxvFailure::from)?;
    let own = std::process::id();
    let ancestors = ancestors(&table, own);
    let mut found = users(&table, &workspace.path, &[], own);
    found.retain(|(pid, _)| !ancestors.contains(pid));
    Ok(user_reason(&found))
}

/// Returns the name of the workspace that `args` names, or else of the one
/// that holds the current directory.
fn chosen(environment: &Environment, args: &ArchiveArgs) -> Result<WorkspaceName, crumbkit::Error> {
    if let Some(text) = &args.workspace {
        return Ok(parse_name(text)?);
    }
    let store = multiverse::Store::new(environment.records()?);
    match processes::current(&store, environment)? {
        Some(workspace) => Ok(workspace.name),
        None => Err(RunFailure::NotCurrent.into()),
    }
}

/// Returns the directory to change to after an archive of a workspace at
/// `path`, when the current directory is inside it.
fn cd_target(environment: &Environment, path: &std::path::Path) -> Option<String> {
    let current = environment.current_dir()?;
    let current = current
        .canonicalize()
        .unwrap_or_else(|_| current.to_path_buf());
    let inside = current.starts_with(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    inside.then(|| path.parent().unwrap_or(path).display().to_string())
}

/// What an archive did beyond the record, for its result.
#[derive(Default)]
struct Done {
    /// The units that the run stopped.
    stopped: Vec<String>,
    /// The disposable directories that the run deleted.
    disposable: Vec<DisposableRow>,
    /// The entries written into a disposable directory after mxv measured
    /// it, which the run deleted.
    late: Vec<String>,
    /// The volumes that the run deleted.
    volumes: Vec<String>,
    /// The warnings.
    warnings: Vec<String>,
    /// The directory to change to.
    cd: Option<String>,
}

/// Returns what the archive keeps of each of `checkouts`, whose
/// uncommitted work `uncommitted` lists.
fn kept_rows(checkouts: &[ArchivedCheckout], uncommitted: &[Uncommitted]) -> Vec<KeptRow> {
    checkouts
        .iter()
        .filter_map(|checkout| KeptRow::of(checkout, uncommitted))
        .collect()
}

/// Returns the result of the archive of the checkouts of `archive`, whose
/// uncommitted work `uncommitted` lists.
fn result_of(
    archive: &Archive,
    uncommitted: &[Uncommitted],
    done: Done,
    planned: bool,
) -> ArchiveResult {
    ArchiveResult {
        state: LifecycleState::of(planned),
        name: archive.name.to_string(),
        path: archive.path.display().to_string(),
        pins: pin_rows(&archive.checkouts),
        local_only: local_only(&archive.checkouts),
        kept: kept_rows(&archive.checkouts, uncommitted),
        ignored: IgnoredRow::kept(archive),
        lost: archive.lost.clone(),
        stopped: done.stopped,
        disposable: done.disposable,
        late: done.late,
        root_files: archive
            .root
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
        volumes: done.volumes,
        warnings: done.warnings,
        restore: format!("mxv restore {}", archive.name),
        cd: done.cd,
        planned,
    }
}

/// Returns the result of an archive of a name whose archive `archive` has
/// finished, which has nothing to do. `planned` is set on a dry run, as
/// for every result.
fn unchanged(archive: &Archive, planned: bool) -> ArchiveResult {
    ArchiveResult {
        state: LifecycleState::Unchanged,
        ..result_of(archive, &[], Done::default(), planned)
    }
}

/// Returns where each checkout's commit is kept.
fn pin_rows(checkouts: &[ArchivedCheckout]) -> Vec<PinRow> {
    checkouts
        .iter()
        .map(|checkout| PinRow {
            folder: checkout.folder.clone(),
            branch: (!checkout.detached).then(|| checkout.branch.clone()),
            commit: checkout.commit.clone(),
            repository: checkout.pin_repository().display().to_string(),
            reference: checkout.pin.clone(),
        })
        .collect()
}

/// Returns each commit that exists only on this machine, with the
/// checkout's folder in front.
fn local_only(checkouts: &[ArchivedCheckout]) -> Vec<String> {
    checkouts
        .iter()
        .flat_map(|checkout| {
            checkout
                .local_only
                .iter()
                .map(|commit| format!("{}: {commit}", checkout.folder))
        })
        .collect()
}

/// Returns the result of a dry run of `plan`.
fn dry_result(plan: &ArchivePlan, running: Vec<String>, cd: Option<String>) -> ArchiveResult {
    let workspace = &plan.removal.workspace;
    ArchiveResult {
        state: LifecycleState::Planned,
        name: workspace.name.to_string(),
        path: workspace.path.display().to_string(),
        pins: pin_rows(&plan.checkouts),
        local_only: local_only(&plan.checkouts),
        kept: kept_rows(&plan.checkouts, &plan.uncommitted),
        ignored: IgnoredRow::planned(plan),
        lost: plan.lost.clone(),
        stopped: running,
        disposable: DisposableRow::of(&plan.disposable),
        late: Vec::new(),
        root_files: plan.root.clone(),
        volumes: Vec::new(),
        warnings: plan.warnings.clone(),
        restore: format!("mxv restore {}", workspace.name),
        cd,
        planned: true,
    }
}

/// Returns the question that an archive of `plan` asks.
fn question(plan: &ArchivePlan, running: &[String], delete_volumes: bool) -> String {
    let workspace = &plan.removal.workspace;
    let mut text = format!(
        "Archive the workspace {}? It keeps each checkout's commit in a pin and deletes {}.",
        workspace.name,
        workspace.path.display()
    );
    if !running.is_empty() {
        text.push_str(&format!(" It stops {}.", running.join(", ")));
    }
    if !plan.removal.manifest.is_empty() {
        text.push_str(&format!(
            " It deletes {}, which only the repository's mxv.toml declares disposable.",
            plan.removal.manifest.join(", ")
        ));
    }
    let uncommitted: Vec<&str> = plan
        .uncommitted
        .iter()
        .filter(|work| !work.changes.is_empty())
        .map(|work| work.folder.as_str())
        .collect();
    if !uncommitted.is_empty() {
        text.push_str(&format!(
            " It keeps the uncommitted work of {} on pins, without a commit on any branch, and restore puts it back uncommitted.",
            uncommitted.join(", ")
        ));
    }
    let ignored = IgnoredRow::planned(plan);
    if !ignored.is_empty() {
        let count: usize = ignored.iter().map(|row| row.paths.len()).sum();
        let folders: Vec<String> = ignored.into_iter().map(|row| row.folder).collect();
        text.push_str(&format!(
            " It keeps {} of {} in a private file that only you can read, and restore writes them back.",
            counted(count, "ignored file", "ignored files"),
            listed(&folders)
        ));
    }
    if !plan.lost.is_empty() {
        text.push_str(&format!(" It loses {}.", plan.lost.join(", ")));
    }
    for warning in &plan.warnings {
        text.push_str(&format!(" Warning: {warning}."));
    }
    if delete_volumes {
        text.push_str(" It deletes the workspace's docker volumes and the data in them.");
    }
    text
}

/// Returns the failure when a process outside phase uses `workspace`.
fn refuse_outsider(workspace: &Workspace) -> Result<(), crumbkit::Error> {
    match outside_user(workspace)? {
        Some(reason) => Err(ArchiveFailure::InUse {
            name: workspace.name.to_string(),
            reason,
        }
        .into()),
        None => Ok(()),
    }
}

/// Finishes the archive of `archive`, whose workspace is gone: deletes the
/// run files and the volumes that the run asked to delete, and marks it.
/// It returns the names of the deleted volumes, and adds to `warnings` one
/// sentence for each reason that phase kept its run record. `mxv repair`
/// calls it for an older archive that it renamed.
pub(crate) fn finish(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    records: &Archives,
    archive: &mut Archive,
    volumes: Option<Volumes>,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, crumbkit::Error> {
    warnings.extend(processes::forget(
        environment,
        &archive.name,
        archive.workspace_id.as_ref(),
    )?);
    let mut names = Vec::new();
    if let Some(volumes) = volumes {
        names = volumes.names();
        processes::delete_volumes(environment, volumes)?;
    }
    workspaces
        .finish_archive(records, archive)
        .map_err(MxvFailure::from)?;
    Ok(names)
}

/// Archives the workspace that `args` names.
pub(crate) fn archive(
    output: &mut Run,
    environment: &Environment,
    args: &ArchiveArgs,
) -> Result<Prose<ArchiveResult>, crumbkit::Error> {
    let name = chosen(environment, args)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let runs = environment.runs()?;
    let max_file = environment
        .config()?
        .archive_max_file
        .unwrap_or(ARCHIVE_MAX_FILE);
    let options =
        ArchiveOptions::new(args.force, runs.containers_ran(&name)).with_max_file(max_file);
    if output.is_dry_run() {
        return dry_run(environment, &workspaces, &records, &name, options);
    }
    let lock = processes::lock(environment, &name)?;
    let plan = match workspaces
        .plan_archive(&records, &name, options, Mode::Change)
        .map_err(MxvFailure::from)?
    {
        ArchivePlanned::Ready(plan) => *plan,
        ArchivePlanned::Unchanged(archive) => return Ok(Prose::new(unchanged(&archive, false))),
        ArchivePlanned::Refused(refusal) => return Err(archive_refusal(&name, refusal).into()),
        ArchivePlanned::Finish(mut archive) => {
            let mut done = Done {
                cd: cd_target(environment, &archive.path),
                ..Done::default()
            };
            let volumes = stale_volumes(environment, &archive, args.delete_volumes)?;
            done.volumes = finish(
                environment,
                &workspaces,
                &records,
                &mut archive,
                volumes,
                &mut done.warnings,
            )?;
            return Ok(Prose::new(result_of(&archive, &[], done, false)));
        }
    };
    let workspace = plan.removal.workspace.clone();
    let running = processes::running(environment, &workspace)?;
    if running.is_empty() {
        refuse_outsider(&workspace)?;
    }
    let cd = cd_target(environment, &workspace.path);
    let passed_yes = args.yes != Yes::default();
    if !args.force && !plan.removal.manifest.is_empty() && passed_yes {
        return Err(manifest_disposable(&name, &plan).into());
    }
    output.confirm(args.yes, &question(&plan, &running, args.delete_volumes))?;
    if !running.is_empty() {
        processes::stop_all_locked(environment, &workspace, &lock)?;
    }
    refuse_outsider(&workspace)?;
    let volumes = if args.delete_volumes {
        Some(processes::volumes(environment, &workspace)?)
    } else {
        None
    };
    let uncommitted = plan.uncommitted.clone();
    let mut warnings = plan.warnings.clone();
    let (mut archive, removal, disposable, late) = match workspaces
        .archive(&records, plan)
        .map_err(MxvFailure::from)?
    {
        ArchiveOutcome::Retired {
            archive,
            removal,
            disposable,
            late,
        } => (*archive, *removal, disposable, late),
        ArchiveOutcome::Refused(refusal) => return Err(archive_refusal(&name, refusal).into()),
    };
    warnings.extend(removal.warnings.iter().cloned());
    let volumes = finish(
        environment,
        &workspaces,
        &records,
        &mut archive,
        volumes,
        &mut warnings,
    )?;
    drop(lock);
    let done = Done {
        stopped: running,
        disposable: DisposableRow::of(&disposable),
        late,
        volumes,
        warnings,
        cd,
    };
    Ok(Prose::new(result_of(&archive, &uncommitted, done, false)))
}

/// Returns what an archive of the workspace `name` would do, and changes
/// nothing, so it takes no lock.
fn dry_run(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    records: &Archives,
    name: &WorkspaceName,
    options: ArchiveOptions,
) -> Result<Prose<ArchiveResult>, crumbkit::Error> {
    let plan = match workspaces
        .plan_archive(records, name, options, Mode::DryRun)
        .map_err(MxvFailure::from)?
    {
        ArchivePlanned::Ready(plan) => *plan,
        ArchivePlanned::Unchanged(archive) => return Ok(Prose::new(unchanged(&archive, true))),
        ArchivePlanned::Refused(refusal) => return Err(archive_refusal(name, refusal).into()),
        ArchivePlanned::Finish(archive) => {
            let done = Done {
                cd: cd_target(environment, &archive.path),
                ..Done::default()
            };
            return Ok(Prose::new(result_of(&archive, &[], done, true)));
        }
    };
    let workspace = &plan.removal.workspace;
    let running = processes::running(environment, workspace)?;
    if running.is_empty() {
        refuse_outsider(workspace)?;
    }
    let cd = cd_target(environment, &workspace.path);
    Ok(Prose::new(dry_result(&plan, running, cd)))
}

/// Returns the failure of `--yes` with the paths of `plan` that only a
/// repository's own `mxv.toml` declares disposable.
fn manifest_disposable(name: &WorkspaceName, plan: &ArchivePlan) -> ArchiveFailure {
    ArchiveFailure::ManifestDisposable {
        name: name.to_string(),
        paths: plan.removal.manifest.clone(),
    }
}

/// Returns the volumes of an archive that an earlier run stopped before it
/// deleted them, when the person asks again.
fn stale_volumes(
    environment: &Environment,
    archive: &Archive,
    delete: bool,
) -> Result<Option<Volumes>, crumbkit::Error> {
    match (&archive.workspace_id, delete && archive.containers_ran) {
        (Some(id), true) => Ok(Some(
            environment
                .runs()?
                .archived_volumes(id)
                .map_err(MxvFailure::from)?,
        )),
        _ => Ok(None),
    }
}

/// Returns the result of a restore of `name`, which a workspace already
/// holds. When an earlier restore stopped before it deleted the archive
/// `stale`, which has the workspace's stable id, the run asks and then
/// deletes it with its pins under the workspace's lock, as `mxv repair`
/// does, because the workspace holds every commit again. Under the lock
/// it plans the restore again, and deletes the archive only when the plan
/// still names the same one as stale.
fn restored_already(
    output: &mut Run,
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    records: &Archives,
    name: &WorkspaceName,
    args: &RestoreArgs,
    stale: Option<Box<Archive>>,
) -> Result<Prose<RestoreResult>, crumbkit::Error> {
    let pins = |archive: &Archive| -> Vec<String> {
        archive
            .checkouts
            .iter()
            .map(|checkout| checkout.pin.clone())
            .collect()
    };
    let path = workspaces
        .find(name)
        .map_err(MxvFailure::from)?
        .map_or_else(|| PathBuf::from(name.as_str()), |workspace| workspace.path);
    let mut stale_pins = None;
    let mut warnings = Vec::new();
    if let Some(confirmed) = stale {
        stale_pins = Some(pins(&confirmed));
        if !output.is_dry_run() {
            output.confirm(
                args.yes,
                &format!(
                    "The workspace {name} is already restored. Delete the archive record that an earlier restore left, with its pins?"
                ),
            )?;
            let lock = processes::lock(environment, name)?;
            stale_pins = match workspaces
                .plan_restore(records, name, args.force)
                .map_err(MxvFailure::from)?
            {
                RestorePlanned::Unchanged { stale: Some(again) } if again.id == confirmed.id => {
                    let removed = workspaces
                        .remove_archive(records, *again, Mode::Change)
                        .map_err(MxvFailure::from)?;
                    warnings = removed.warnings;
                    Some(pins(&removed.archive))
                }
                // Another run deleted the archive after the question.
                RestorePlanned::Unchanged { stale: None } => None,
                RestorePlanned::Refused(refusal) => {
                    return Err(restore_refusal(name, refusal).into());
                }
                RestorePlanned::Unchanged { .. } | RestorePlanned::Ready(_) => {
                    return Err(MxvFailure::RestoreChanged {
                        name: name.to_string(),
                    }
                    .into());
                }
            };
            drop(lock);
        }
    }
    Ok(Prose::new(RestoreResult {
        state: LifecycleState::Unchanged,
        name: name.to_string(),
        path: path.display().to_string(),
        stale: stale_pins.is_some(),
        stale_pins: stale_pins.unwrap_or_default(),
        checkouts: Vec::new(),
        put_back: Vec::new(),
        lost: Vec::new(),
        ports: Vec::new(),
        missing_mirrors: Vec::new(),
        warnings,
        planned: output.is_dry_run(),
    }))
}

/// Restores the workspace that `args` names.
pub(crate) fn restore(
    output: &mut Run,
    environment: &Environment,
    args: &RestoreArgs,
) -> Result<Prose<RestoreResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let runs = environment
        .runs()?
        .with_port_range(environment.port_range()?);
    let plan = match workspaces
        .plan_restore(&records, &name, args.force)
        .map_err(MxvFailure::from)?
    {
        RestorePlanned::Ready(plan) => *plan,
        RestorePlanned::Unchanged { stale } => {
            return restored_already(
                output,
                environment,
                &workspaces,
                &records,
                &name,
                args,
                stale,
            );
        }
        RestorePlanned::Refused(refusal) => return Err(restore_refusal(&name, refusal).into()),
    };
    let store = multiverse::Store::new(environment.records()?);
    if output.is_dry_run() {
        let ports = runs
            .planned_ports(&plan.archive.workspace(), &store)
            .map_err(MxvFailure::from)?;
        return Ok(Prose::new(RestoreResult {
            state: LifecycleState::Planned,
            name: name.to_string(),
            path: plan.archive.path.display().to_string(),
            stale_pins: Vec::new(),
            stale: false,
            checkouts: restored_checkouts(&plan.archive),
            put_back: plan
                .archive
                .checkouts
                .iter()
                .map(|checkout| PutBackRow::planned(checkout, &plan.archive.ignored))
                .filter(|row| !row.is_empty())
                .collect(),
            lost: Vec::new(),
            ports: port_rows(ports),
            missing_mirrors: plan.missing_mirrors,
            warnings: Vec::new(),
            planned: true,
        }));
    }
    output.confirm(
        args.yes,
        &format!(
            "Restore the workspace {name} at {}?",
            plan.archive.path.display()
        ),
    )?;
    // The lock comes after the question, so a prompt holds up nobody, and
    // the library reads the archive again under it.
    let lock = processes::lock(environment, &name)?;
    let archive = plan.archive.clone();
    let missing_mirrors = plan.missing_mirrors.clone();
    let restored = match workspaces
        .restore(&records, plan, &runs)
        .map_err(MxvFailure::from)?
    {
        RestoreOutcome::Restored(restored) => *restored,
        RestoreOutcome::Refused(refusal) => return Err(restore_refusal(&name, refusal).into()),
    };
    drop(lock);
    Ok(Prose::new(RestoreResult {
        state: LifecycleState::Done,
        name: name.to_string(),
        path: archive.path.display().to_string(),
        stale_pins: Vec::new(),
        stale: false,
        checkouts: restored_checkouts(&archive),
        put_back: restored
            .put_back
            .into_iter()
            .zip(&archive.checkouts)
            .map(|(put_back, checkout)| {
                PutBackRow::restored(
                    put_back,
                    checkout.path.display().to_string(),
                    checkout.kept.wip.is_some(),
                )
            })
            .filter(|row| !row.is_empty())
            .collect(),
        lost: restored.lost,
        ports: port_rows(restored.ports),
        missing_mirrors,
        warnings: restored.warnings,
        planned: false,
    }))
}

/// Returns the checkouts of `archive` as the rows of a restore.
fn restored_checkouts(archive: &Archive) -> Vec<RestoredCheckout> {
    archive
        .checkouts
        .iter()
        .map(|checkout| RestoredCheckout {
            folder: checkout.folder.clone(),
            path: checkout.path.display().to_string(),
            branch: (!checkout.detached).then(|| checkout.branch.clone()),
            commit: checkout.commit.clone(),
        })
        .collect()
}

/// Returns `changes` as rows.
fn port_rows(changes: Vec<multiverse::PortChange>) -> Vec<PortRow> {
    changes
        .into_iter()
        .map(|change| PortRow {
            name: change.name,
            from: change.from,
            to: change.to,
        })
        .collect()
}

/// Returns the age of `archive`.
fn age(archive: &Archive) -> crumbkit::Age {
    let now = SystemTime::now();
    crumbkit::Age::since(UNIX_EPOCH + Duration::from_secs(archive.time), now)
}

/// Returns the row of `archive`.
pub(crate) fn row(archive: &Archive) -> WorkspaceRow {
    let local = archive
        .checkouts
        .iter()
        .map(|checkout| checkout.local_only.len())
        .sum();
    WorkspaceRow::from(&archive.workspace()).archived(age(archive), local, &archive.ignored)
}

/// Returns the row of `broken` in a listing of the archives: the row of
/// the archive of its name with its problem, or the name alone for a
/// record that mxv cannot read.
fn broken_row(broken: &Broken, found: &[Archive]) -> WorkspaceRow {
    let archive = match broken.problem {
        Problem::Unreadable { .. } => None,
        _ => found
            .iter()
            .find(|archive| archive.name.as_str() == broken.name),
    };
    archive.map_or_else(
        || WorkspaceRow::broken(broken),
        |archive| row(archive).with_broken(broken),
    )
}

/// Returns each archive as a row, the broken ones first with their repair.
pub(crate) fn list(
    environment: &Environment,
    args: &ListArgs,
) -> Result<Listing<WorkspaceRow>, MxvFailure> {
    let records: ArchiveRecords = archives(environment)?.list()?;
    let live = multiverse::Store::new(environment.records()?).list()?;
    let broken = health::classify(&Git, &live, &records).archives;
    let mut rows: Vec<WorkspaceRow> = broken
        .iter()
        .map(|broken| broken_row(broken, &records.archives))
        .collect();
    rows.extend(
        records
            .archives
            .iter()
            .filter(|archive| {
                !broken
                    .iter()
                    .any(|found| found.name == archive.name.as_str())
            })
            .map(row),
    );
    Ok(args.page.first_of(rows).described("archives").next(
        crumbkit::Call::new(&["restore"]).arg("name"),
        "rebuilds an archived workspace.",
    ))
}

/// Deletes the archive that `args` names.
pub(crate) fn remove(
    output: &mut Run,
    environment: &Environment,
    args: &RemoveArgs,
) -> Result<Prose<RemovalResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let Some(archive) = records.read(&name).map_err(MxvFailure::from)? else {
        let path = environment.root()?.path.join(name.as_str());
        return Ok(Prose::new(RemovalResult::unchanged(
            &name,
            &path,
            true,
            output.is_dry_run(),
        )));
    };
    if archive.state == ArchiveState::Retiring {
        return Err(ArchiveFailure::Unfinished {
            name: name.to_string(),
        }
        .into());
    }
    if args.delete_volumes {
        refuse_live_volumes(environment, &archive)?;
    }
    let (volumes, mut warnings) = archive_volumes(environment, &archive, args.delete_volumes)?;
    let names = volumes
        .as_ref()
        .map(|volumes| volumes.names())
        .unwrap_or_default();
    if output.is_dry_run() {
        return Ok(Prose::new(RemovalResult::of_archive(
            &archive, names, warnings, true,
        )));
    }
    let mut text = format!(
        "Delete the archive {name} and its pins? `mxv restore` cannot bring the workspace back afterwards."
    );
    if !names.is_empty() {
        text.push_str(" It deletes the workspace's docker volumes and the data in them.");
    }
    output.confirm(args.yes, &text)?;
    let lock = processes::lock(environment, &name)?;
    if args.delete_volumes {
        refuse_live_volumes(environment, &archive)?;
    }
    // The archive holds the only record of the volumes' stable id, so it
    // stays until they are gone, and a failure can be tried again.
    if let Some(volumes) = volumes {
        processes::delete_volumes(environment, volumes)?;
    }
    let removed = workspaces
        .remove_archive(&records, archive, Mode::Change)
        .map_err(MxvFailure::from)?;
    warnings.extend(removed.warnings);
    drop(lock);
    Ok(Prose::new(RemovalResult::of_archive(
        &removed.archive,
        names,
        warnings,
        false,
    )))
}

/// Returns the failure when a live workspace has the stable id of
/// `archive`, because its docker volumes are then that workspace's. A
/// workspace whose directories are partly gone counts too, so its volumes
/// stay until `mxv remove` removes it.
fn refuse_live_volumes(
    environment: &Environment,
    archive: &Archive,
) -> Result<(), crumbkit::Error> {
    let store = multiverse::Store::new(environment.records()?);
    let live = store.list().map_err(MxvFailure::from)?.workspaces;
    match live
        .iter()
        .find(|workspace| health::rebuilt(archive, workspace) != Rebuilt::Unrelated)
    {
        Some(workspace) => Err(ArchiveFailure::VolumesInUse {
            name: archive.name.to_string(),
            workspace: workspace.name.to_string(),
        }
        .into()),
        None => Ok(()),
    }
}

/// Returns the volumes of `archive` that `delete` asks to delete, and the
/// warning about volumes that stay. A docker that does not answer becomes
/// a warning when no volume is to be deleted, because the removal of the
/// archive does not depend on it, and a failure when the person asked for
/// the deletion.
fn archive_volumes(
    environment: &Environment,
    archive: &Archive,
    delete: bool,
) -> Result<(Option<Volumes>, Vec<String>), crumbkit::Error> {
    let Some(id) = archive
        .workspace_id
        .as_ref()
        .filter(|_| archive.containers_ran)
    else {
        return Ok((None, Vec::new()));
    };
    let found = environment
        .runs()
        .and_then(|runs| runs.archived_volumes(id).map_err(MxvFailure::from));
    match found {
        Ok(volumes) if delete => Ok((Some(volumes), Vec::new())),
        Ok(volumes) if volumes.is_empty() => Ok((None, Vec::new())),
        Ok(volumes) => Ok((
            None,
            vec![format!(
                "the docker volumes {} stay; `--delete-volumes` deletes them",
                volumes.names().join(", ")
            )],
        )),
        Err(failure) if delete => Err(failure.into()),
        Err(failure) => Ok((
            None,
            vec![format!(
                "mxv could not read the workspace's docker volumes, which stay: {failure}"
            )],
        )),
    }
}
