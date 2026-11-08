//! The failures of `mxv archive`, `mxv restore` and `mxv remove --archived`,
//! each with a stable code and a next step (CLI-3).

use std::fmt;

use crumbkit::{Call, Code, Failure, Field, FieldValue, Recovery};
use multiverse::{ArchiveRefusal, RestoreRefusal, Unsaved, WorkspaceName};

/// The code of a name that no workspace or archive has.
pub const ARCHIVE_NOT_FOUND: Code = Code::new("mxv", "archive", "not_found");
/// The code of an archive whose name another archive holds.
pub const ARCHIVE_NAME_TAKEN: Code = Code::new("mxv", "archive", "name_taken");
/// The code of a checkout that differs from what the archive admitted.
pub const ARCHIVE_SOURCE_CHANGED: Code = Code::new("mxv", "archive", "source_changed");
/// The code of a clone whose mirror is gone, so nothing could hold its pin.
pub const ARCHIVE_MIRROR_MISSING: Code = Code::new("mxv", "archive", "mirror_missing");
/// The code of a workspace with work that an archive cannot keep.
pub const ARCHIVE_UNSAVED_WORK: Code = Code::new("mxv", "archive", "unsaved_work");
/// The code of a workspace that a process outside phase works in.
pub const ARCHIVE_IN_USE: Code = Code::new("mxv", "archive", "in_use");
/// The code of a checkout that an archive cannot read.
pub const ARCHIVE_CHECKOUT_UNUSABLE: Code = Code::new("mxv", "archive", "checkout_unusable");
/// The code of an archive that has not finished, for a command that needs
/// a finished one.
pub const ARCHIVE_UNFINISHED: Code = Code::new("mxv", "archive", "unfinished");
/// The code of `--yes` with paths that only a repository's own `mxv.toml`
/// declares disposable.
pub const ARCHIVE_MANIFEST_DISPOSABLE: Code = Code::new("mxv", "archive", "manifest_disposable");
/// The code of a restore of a name that no archive has.
pub const RESTORE_NOT_FOUND: Code = Code::new("mxv", "restore", "not_found");
/// The code of a restore whose name or directory something else holds.
pub const RESTORE_NAME_TAKEN: Code = Code::new("mxv", "restore", "name_taken");
/// The code of a worktree's branch that moved after the archive.
pub const RESTORE_BRANCH_MOVED: Code = Code::new("mxv", "restore", "branch_moved");
/// The code of an archived commit that nothing holds any more.
pub const RESTORE_COMMIT_MISSING: Code = Code::new("mxv", "restore", "commit_missing");
/// The code of a restore of an archive that has not finished.
pub const RESTORE_UNFINISHED: Code = Code::new("mxv", "restore", "unfinished");
/// The code of a file of an archive that differs from its digest.
pub const RESTORE_ROOT_FILE_CHANGED: Code = Code::new("mxv", "restore", "root_file_changed");

/// Every failure code of this module.
pub(crate) const CODES: &[&Code] = &[
    &ARCHIVE_NOT_FOUND,
    &ARCHIVE_NAME_TAKEN,
    &ARCHIVE_SOURCE_CHANGED,
    &ARCHIVE_MIRROR_MISSING,
    &ARCHIVE_UNSAVED_WORK,
    &ARCHIVE_IN_USE,
    &ARCHIVE_CHECKOUT_UNUSABLE,
    &ARCHIVE_UNFINISHED,
    &ARCHIVE_MANIFEST_DISPOSABLE,
    &RESTORE_NOT_FOUND,
    &RESTORE_NAME_TAKEN,
    &RESTORE_BRANCH_MOVED,
    &RESTORE_COMMIT_MISSING,
    &RESTORE_UNFINISHED,
    &RESTORE_ROOT_FILE_CHANGED,
];

/// Why an archive or a restore stopped.
#[derive(Debug)]
pub(crate) enum ArchiveFailure {
    /// No workspace or archive has the name.
    NotFound {
        /// The name.
        name: String,
    },
    /// An archive holds the name.
    NameTaken {
        /// The name.
        name: String,
    },
    /// A checkout differs from what the archive admitted.
    SourceChanged {
        /// The workspace's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// What differs.
        differences: Vec<String>,
    },
    /// A clone's mirror is gone.
    MirrorMissing {
        /// The checkout's folder.
        folder: String,
        /// The mirror.
        mirror: String,
    },
    /// The workspace holds work that the archive cannot keep.
    UnsavedWork {
        /// The workspace's name.
        name: String,
        /// The work.
        unsaved: Unsaved,
    },
    /// A process outside phase works in the workspace.
    InUse {
        /// The workspace's name.
        name: String,
        /// Which process, in a clause.
        reason: String,
    },
    /// A checkout cannot be read.
    Unusable {
        /// The checkout's folder.
        folder: String,
        /// Why, in a clause.
        reason: String,
    },
    /// The archive has not finished.
    Unfinished {
        /// The name.
        name: String,
    },
    /// `--yes` with paths that only a repository's `mxv.toml` declares
    /// disposable.
    ManifestDisposable {
        /// The workspace's name.
        name: String,
        /// The paths.
        paths: Vec<String>,
    },
    /// No archive has the name.
    RestoreNotFound {
        /// The name.
        name: String,
    },
    /// The archive has not finished.
    RestoreUnfinished {
        /// The name.
        name: String,
    },
    /// Something holds the name or the directory.
    RestoreNameTaken {
        /// The name.
        name: String,
        /// The directory.
        path: String,
        /// Whether a workspace record holds the name.
        record: bool,
    },
    /// A worktree's branch moved.
    BranchMoved {
        /// The archive's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// The branch.
        branch: String,
        /// The pinned commit.
        pinned: String,
        /// The branch's commit now.
        tip: String,
    },
    /// The archived commit is gone.
    CommitMissing {
        /// The archive's name.
        name: String,
        /// The checkout's folder.
        folder: String,
        /// The commit.
        commit: String,
    },
    /// A file of the archive changed.
    RootFileChanged {
        /// The archive's name.
        name: String,
        /// The file.
        path: String,
    },
}

/// Returns a next step that a person reads.
fn hint(text: impl Into<String>) -> Recovery {
    Recovery::Hint(text.into())
}

/// Returns the failure of `refusal` for the workspace `name`.
pub(super) fn archive_refusal(name: &WorkspaceName, refusal: ArchiveRefusal) -> ArchiveFailure {
    let name = name.to_string();
    match refusal {
        ArchiveRefusal::NameTaken(_) => ArchiveFailure::NameTaken { name },
        ArchiveRefusal::MirrorMissing { folder, mirror } => ArchiveFailure::MirrorMissing {
            folder,
            mirror: mirror.display().to_string(),
        },
        ArchiveRefusal::Unusable { folder, reason } => ArchiveFailure::Unusable { folder, reason },
        ArchiveRefusal::Unsaved(unsaved) => ArchiveFailure::UnsavedWork { name, unsaved },
        ArchiveRefusal::SourceChanged {
            folder,
            differences,
        } => ArchiveFailure::SourceChanged {
            name,
            folder,
            differences,
        },
        _ => ArchiveFailure::NotFound { name },
    }
}

/// Returns the failure of `refusal` for the archive `name`.
pub(super) fn restore_refusal(name: &WorkspaceName, refusal: RestoreRefusal) -> ArchiveFailure {
    let name = name.to_string();
    match refusal {
        RestoreRefusal::Unfinished(_) => ArchiveFailure::RestoreUnfinished { name },
        RestoreRefusal::NameTaken { path, record } => ArchiveFailure::RestoreNameTaken {
            name,
            path: path.display().to_string(),
            record,
        },
        RestoreRefusal::BranchMoved {
            folder,
            branch,
            pinned,
            tip,
        } => ArchiveFailure::BranchMoved {
            name,
            folder,
            branch,
            pinned,
            tip,
        },
        RestoreRefusal::CommitMissing { folder, commit } => ArchiveFailure::CommitMissing {
            name,
            folder,
            commit,
        },
        RestoreRefusal::RootFileChanged { path } => ArchiveFailure::RootFileChanged { name, path },
        _ => ArchiveFailure::RestoreNotFound { name },
    }
}

impl Failure for ArchiveFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotFound { .. } => &ARCHIVE_NOT_FOUND,
            Self::NameTaken { .. } => &ARCHIVE_NAME_TAKEN,
            Self::SourceChanged { .. } => &ARCHIVE_SOURCE_CHANGED,
            Self::MirrorMissing { .. } => &ARCHIVE_MIRROR_MISSING,
            Self::UnsavedWork { .. } => &ARCHIVE_UNSAVED_WORK,
            Self::InUse { .. } => &ARCHIVE_IN_USE,
            Self::Unusable { .. } => &ARCHIVE_CHECKOUT_UNUSABLE,
            Self::Unfinished { .. } => &ARCHIVE_UNFINISHED,
            Self::ManifestDisposable { .. } => &ARCHIVE_MANIFEST_DISPOSABLE,
            Self::RestoreNotFound { .. } => &RESTORE_NOT_FOUND,
            Self::RestoreUnfinished { .. } => &RESTORE_UNFINISHED,
            Self::RestoreNameTaken { .. } => &RESTORE_NAME_TAKEN,
            Self::BranchMoved { .. } => &RESTORE_BRANCH_MOVED,
            Self::CommitMissing { .. } => &RESTORE_COMMIT_MISSING,
            Self::RootFileChanged { .. } => &RESTORE_ROOT_FILE_CHANGED,
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::NotFound { .. } | Self::RestoreNotFound { .. } => {
                Recovery::Run(Call::new(&["list"]))
            }
            Self::NameTaken { name } => hint(format!(
                "run `mxv restore {name}` to bring the archive back, or `mxv remove --archived {name}` to delete it"
            )),
            Self::SourceChanged { name, .. } => hint(format!(
                "look at what changed, then run `mxv archive -w {name}` again to finish the archive"
            )),
            Self::MirrorMissing { .. } => hint(
                "run `mxv new` for the same repository, which clones the mirror again, then archive the workspace again",
            ),
            Self::UnsavedWork { name, unsaved } => {
                let wip_saves_all = unsaved.ignored.is_empty()
                    && unsaved.stashes.is_empty()
                    && unsaved.commits.is_empty()
                    && unsaved.root.is_empty();
                if wip_saves_all {
                    hint(format!(
                        "commit the work, or run `mxv archive -w {name} --wip` to commit it onto the branch, or `--force` to lose it"
                    ))
                } else {
                    hint(format!(
                        "save or delete the paths and commits that the failure lists, and archive again, or run `mxv archive -w {name} --force` to lose them; `--wip` does not touch ignored files, stashes or other commits"
                    ))
                }
            }
            Self::InUse { name, .. } => hint(format!(
                "stop that process or move it out of the workspace, then run `mxv archive -w {name}` again"
            )),
            Self::Unusable { .. } => {
                hint("repair the checkout with git or `mxv add`, then archive the workspace again")
            }
            Self::Unfinished { name } | Self::RestoreUnfinished { name } => hint(format!(
                "run `mxv archive -w {name}`, which finishes the archive"
            )),
            Self::ManifestDisposable { name, .. } => hint(format!(
                "run `mxv archive -w {name}` without --yes and answer the prompt, or pass --force"
            )),
            Self::RestoreNameTaken { name, path, record } => {
                if *record {
                    hint(format!(
                        "remove the workspace {name} first, or run `mxv remove --archived {name}` to delete the archive"
                    ))
                } else {
                    hint(format!("move or delete {path}, then restore again"))
                }
            }
            Self::BranchMoved { branch, pinned, .. } => hint(format!(
                "point the branch back with `git branch --force {branch} {pinned}`, or rename it, then restore again"
            )),
            Self::CommitMissing { name, .. } => hint(format!(
                "`mxv remove --archived {name}` deletes the archive, because nothing can rebuild this checkout"
            )),
            Self::RootFileChanged { name, .. } => hint(format!(
                "put the file back, or run `mxv remove --archived {name}` to delete the archive"
            )),
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::NotFound { name }
            | Self::NameTaken { name }
            | Self::Unfinished { name }
            | Self::RestoreNotFound { name }
            | Self::RestoreUnfinished { name } => vec![Field::new("name", name)],
            Self::SourceChanged {
                name,
                folder,
                differences,
            } => vec![
                Field::new("name", name),
                Field::new("folder", folder),
                Field::new("differences", FieldValue::list(differences.iter())),
            ],
            Self::MirrorMissing { folder, mirror } => {
                vec![Field::new("folder", folder), Field::new("mirror", mirror)]
            }
            Self::UnsavedWork { name, unsaved } => vec![
                Field::new("name", name),
                Field::new("changes", FieldValue::list(unsaved.changes.iter())),
                Field::new("ignored", FieldValue::list(unsaved.ignored.iter())),
                Field::new("stashes", FieldValue::list(unsaved.stashes.iter())),
                Field::new("commits", FieldValue::list(unsaved.commits.iter())),
                Field::new("root", FieldValue::list(unsaved.root.iter())),
            ],
            Self::InUse { name, reason } => {
                vec![Field::new("name", name), Field::new("reason", reason)]
            }
            Self::Unusable { folder, reason } => {
                vec![Field::new("folder", folder), Field::new("reason", reason)]
            }
            Self::ManifestDisposable { name, paths } => vec![
                Field::new("name", name),
                Field::new("paths", FieldValue::list(paths.iter())),
            ],
            Self::RestoreNameTaken { name, path, .. } => {
                vec![Field::new("name", name), Field::new("path", path)]
            }
            Self::BranchMoved {
                name,
                folder,
                branch,
                pinned,
                tip,
            } => vec![
                Field::new("name", name),
                Field::new("folder", folder),
                Field::new("branch", branch),
                Field::new("pinned", pinned),
                Field::new("tip", tip),
            ],
            Self::CommitMissing {
                name,
                folder,
                commit,
            } => vec![
                Field::new("name", name),
                Field::new("folder", folder),
                Field::new("commit", commit),
            ],
            Self::RootFileChanged { name, path } => {
                vec![Field::new("name", name), Field::new("path", path)]
            }
        }
    }
}

impl fmt::Display for ArchiveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(formatter, "no workspace or archive is named {name}"),
            Self::NameTaken { name } => {
                write!(formatter, "an archive already holds the name {name}")
            }
            Self::SourceChanged {
                folder,
                differences,
                ..
            } => write!(
                formatter,
                "the checkout {folder} changed after mxv admitted it, and mxv put it back: {}",
                differences.join("; ")
            ),
            Self::MirrorMissing { folder, mirror } => write!(
                formatter,
                "the mirror {mirror} of the checkout {folder} is gone, so nothing could keep its commit"
            ),
            Self::UnsavedWork { name, unsaved } => write!(
                formatter,
                "the workspace {name} holds work that an archive cannot keep: {}",
                unsaved.all().join(", ")
            ),
            Self::InUse { name, reason } => {
                write!(formatter, "the workspace {name} is in use: {reason}")
            }
            Self::Unusable { folder, reason } => {
                write!(
                    formatter,
                    "mxv cannot archive the checkout {folder}: {reason}"
                )
            }
            Self::Unfinished { name } | Self::RestoreUnfinished { name } => {
                write!(formatter, "the archive of {name} stopped partway")
            }
            Self::ManifestDisposable { name, paths } => write!(
                formatter,
                "archiving {name} would delete {}, which only the repository's mxv.toml declares disposable, and --yes alone does not delete such paths",
                paths.join(", ")
            ),
            Self::RestoreNotFound { name } => write!(formatter, "no archive is named {name}"),
            Self::RestoreNameTaken { name, path, record } => {
                if *record {
                    write!(formatter, "a workspace already holds the name {name}")
                } else {
                    write!(formatter, "the directory {path} already exists")
                }
            }
            Self::BranchMoved {
                folder,
                branch,
                pinned,
                tip,
                ..
            } => write!(
                formatter,
                "the branch {branch} of {folder} points at {tip}, and the archive pinned {pinned}"
            ),
            Self::CommitMissing { folder, commit, .. } => write!(
                formatter,
                "the commit {commit} of {folder} is gone from every repository that mxv can read"
            ),
            Self::RootFileChanged { path, .. } => write!(
                formatter,
                "the archive's copy of {path} differs from its recorded digest"
            ),
        }
    }
}

impl std::error::Error for ArchiveFailure {}
