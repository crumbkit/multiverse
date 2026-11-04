//! The failures of the workspace verbs, each with a stable code and a next
//! step (CLI-3).

use std::fmt;
use std::path::{Path, PathBuf};

use crumbkit::{Call, Code, Failure, Field, FieldValue, Recovery};
use multiverse::{BadName, Error};

/// The code of a name that no workspace has.
pub const WORKSPACE_NOT_FOUND: Code = Code::new("mxv", "workspace", "not_found");
/// The code of a new workspace whose name another workspace has.
pub const WORKSPACE_EXISTS: Code = Code::new("mxv", "workspace", "exists");
/// The code of a name that breaks the naming rules.
pub const WORKSPACE_BAD_NAME: Code = Code::new("mxv", "workspace", "bad_name");
/// The code of a removal that a checkout's uncommitted or untracked changes
/// stop.
pub const WORKSPACE_DIRTY: Code = Code::new("mxv", "workspace", "dirty");
/// The code of a new workspace whose directory already exists.
pub const DIRECTORY_TAKEN: Code = Code::new("mxv", "directory", "taken");
/// The code of a checkout directory that its repository does not list.
pub const CHECKOUT_UNOWNED: Code = Code::new("mxv", "checkout", "unowned");
/// The code of a new workspace whose branch already exists.
pub const BRANCH_EXISTS: Code = Code::new("mxv", "branch", "exists");
/// The code of a `--repo` that is not a git repository.
pub const REPOSITORY_NOT_FOUND: Code = Code::new("mxv", "repository", "not_found");
/// The code of a repository without a remote named `origin`.
pub const REPOSITORY_NO_ORIGIN: Code = Code::new("mxv", "repository", "no_origin");
/// The code of a repository whose `origin` has no commits yet.
pub const REPOSITORY_EMPTY_ORIGIN: Code = Code::new("mxv", "repository", "empty_origin");
/// The code of a repository that knows no default branch of `origin`.
pub const REPOSITORY_NO_DEFAULT_BRANCH: Code = Code::new("mxv", "repository", "no_default_branch");
/// The code of a `git` program that cannot start.
pub const GIT_NOT_FOUND: Code = Code::new("mxv", "git", "not_found");
/// The code of a git command that failed.
pub const GIT_FAILED: Code = Code::new("mxv", "git", "failed");
/// The code of a git command that missed its deadline.
pub const GIT_TIMED_OUT: Code = Code::new("mxv", "git", "timed_out");
/// The code of a workspace record or record directory that cannot be read.
pub const RECORD_UNREADABLE: Code = Code::new("mxv", "record", "unreadable");
/// The code of a workspace record that is not a record.
pub const RECORD_CORRUPT: Code = Code::new("mxv", "record", "corrupt");
/// The code of a workspace record that a newer mxv wrote.
pub const RECORD_NEWER: Code = Code::new("mxv", "record", "newer");
/// The code of a file or directory that cannot be written or removed.
pub const FILE_UNWRITABLE: Code = Code::new("mxv", "file", "unwritable");
/// The code of an environment that names no directory for the workspaces or
/// their records.
pub const ENVIRONMENT_NO_DIRECTORY: Code = Code::new("mxv", "environment", "no_directory");

/// A failure of a workspace verb.
#[derive(Debug)]
pub(crate) enum MxvFailure {
    /// No workspace has the name.
    NotFound {
        /// The name.
        name: String,
    },
    /// Another workspace has the name.
    Exists {
        /// The name.
        name: String,
    },
    /// The name breaks the naming rules.
    BadName {
        /// The name as the caller typed it.
        name: String,
        /// The rule it breaks.
        reason: BadName,
    },
    /// A checkout has uncommitted or untracked changes.
    Dirty {
        /// The workspace's name.
        name: String,
        /// Each change, with its checkout's name in front.
        changes: Vec<String>,
    },
    /// The workspace's directory already exists.
    DirectoryTaken {
        /// The directory.
        path: PathBuf,
    },
    /// A checkout directory exists, and its repository does not list it.
    Unowned {
        /// The workspace's name.
        name: String,
        /// The directory.
        path: PathBuf,
    },
    /// The repository already has a branch with the workspace's name.
    BranchExists {
        /// The repository's top-level directory.
        repository: PathBuf,
        /// The branch.
        branch: String,
    },
    /// The environment names no directory for the workspaces or their
    /// records.
    NoDirectory {
        /// The variable that would name the directory.
        variable: &'static str,
    },
    /// A technical failure of the multiverse library.
    Library(Error),
}

impl From<Error> for MxvFailure {
    fn from(error: Error) -> Self {
        Self::Library(error)
    }
}

/// Every code that an [`MxvFailure`] returns.
pub(crate) const CODES: &[&Code] = &[
    &WORKSPACE_NOT_FOUND,
    &WORKSPACE_EXISTS,
    &WORKSPACE_BAD_NAME,
    &WORKSPACE_DIRTY,
    &DIRECTORY_TAKEN,
    &CHECKOUT_UNOWNED,
    &BRANCH_EXISTS,
    &REPOSITORY_NOT_FOUND,
    &REPOSITORY_NO_ORIGIN,
    &REPOSITORY_EMPTY_ORIGIN,
    &REPOSITORY_NO_DEFAULT_BRANCH,
    &GIT_NOT_FOUND,
    &GIT_FAILED,
    &GIT_TIMED_OUT,
    &RECORD_UNREADABLE,
    &RECORD_CORRUPT,
    &RECORD_NEWER,
    &FILE_UNWRITABLE,
    &ENVIRONMENT_NO_DIRECTORY,
];

/// Returns `path` as text for a field or a hint.
fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Returns the hint `text` as a recovery.
fn hint(text: impl Into<String>) -> Recovery {
    Recovery::Hint(text.into())
}

impl Failure for MxvFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotFound { .. } => &WORKSPACE_NOT_FOUND,
            Self::Exists { .. } => &WORKSPACE_EXISTS,
            Self::BadName { .. } => &WORKSPACE_BAD_NAME,
            Self::Dirty { .. } => &WORKSPACE_DIRTY,
            Self::DirectoryTaken { .. } => &DIRECTORY_TAKEN,
            Self::Unowned { .. } => &CHECKOUT_UNOWNED,
            Self::BranchExists { .. } => &BRANCH_EXISTS,
            Self::NoDirectory { .. } => &ENVIRONMENT_NO_DIRECTORY,
            Self::Library(error) => match error {
                Error::RepositoryNotFound { .. } => &REPOSITORY_NOT_FOUND,
                Error::NoOrigin { .. } => &REPOSITORY_NO_ORIGIN,
                Error::EmptyOrigin { .. } => &REPOSITORY_EMPTY_ORIGIN,
                Error::NoDefaultBranch { .. } => &REPOSITORY_NO_DEFAULT_BRANCH,
                Error::ProgramMissing { .. } => &GIT_NOT_FOUND,
                Error::CommandFailed { .. } => &GIT_FAILED,
                Error::TimedOut { .. } => &GIT_TIMED_OUT,
                Error::StateUnreadable { .. } => &RECORD_UNREADABLE,
                Error::CorruptRecord { .. } => &RECORD_CORRUPT,
                Error::NewerRecord { .. } => &RECORD_NEWER,
                // The library marks its error non-exhaustive, so a variant
                // that a later version adds reads as a failed write until
                // mxv gives it a code of its own.
                _ => &FILE_UNWRITABLE,
            },
        }
    }

    fn recovery(&self) -> Recovery {
        match self {
            Self::NotFound { .. } => Recovery::Run(Call::new(&["list"])),
            Self::Exists { name } => Recovery::Run(Call::new(&["show"]).value("names", name)),
            Self::BadName { .. } => hint(
                "use 1 to 64 letters, digits, '-', '_' and '.', starting with a letter or a digit",
            ),
            Self::Dirty { name, .. } => hint(format!(
                "commit, stash or delete the changes, or run `mxv remove {name} --force` to delete them with the workspace"
            )),
            Self::DirectoryTaken { path } => hint(format!(
                "choose another name, or move {} out of the way",
                shown(path)
            )),
            Self::Unowned { path, .. } => hint(format!(
                "move {} out of the way, because git does not list it as a checkout, and run the removal again",
                shown(path)
            )),
            Self::BranchExists { repository, branch } => hint(format!(
                "choose another name, or delete the branch {branch} in {}",
                shown(repository)
            )),
            Self::NoDirectory { variable } => hint(format!(
                "set HOME to your home directory, or set {variable} to an absolute path"
            )),
            Self::Library(error) => library_recovery(error),
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::NotFound { name } | Self::Exists { name } | Self::BadName { name, .. } => {
                vec![Field::new("name", name)]
            }
            Self::Dirty { name, changes } => vec![
                Field::new("name", name),
                Field::new("changes", FieldValue::list(changes.iter())),
            ],
            Self::DirectoryTaken { path } => vec![Field::new("path", shown(path))],
            Self::Unowned { name, path } => {
                vec![Field::new("name", name), Field::new("path", shown(path))]
            }
            Self::BranchExists { repository, branch } => vec![
                Field::new("repository", shown(repository)),
                Field::new("branch", branch),
            ],
            Self::NoDirectory { variable } => vec![Field::new("variable", *variable)],
            Self::Library(error) => library_fields(error),
        }
    }
}

/// Returns the next step after a technical failure of the library.
fn library_recovery(error: &Error) -> Recovery {
    match error {
        Error::RepositoryNotFound { .. } => {
            hint("pass --repo the path of a git repository that has a working tree")
        }
        Error::NoOrigin { repository } => hint(format!(
            "add a remote named origin to {}",
            shown(repository)
        )),
        Error::EmptyOrigin { repository } => hint(format!(
            "push an initial commit to the default branch of origin, such as with `git commit --allow-empty -m 'Start' && git push -u origin HEAD:main` in {}, and run the command again",
            shown(repository)
        )),
        Error::NoDefaultBranch { repository } => hint(format!(
            "push an initial commit to the default branch of origin, or run `git remote set-head origin --auto` in {} when origin already has one",
            shown(repository)
        )),
        Error::ProgramMissing { .. } => hint("install git and put it on PATH"),
        Error::CommandFailed { .. } => {
            hint("fix the cause that git's message names, and run the command again")
        }
        Error::TimedOut { .. } => Recovery::Retry,
        Error::StateUnreadable { path, .. } => hint(format!("make {} readable", shown(path))),
        Error::CorruptRecord { path, .. } => hint(format!(
            "repair {} or delete it, which makes mxv forget that workspace",
            shown(path)
        )),
        Error::NewerRecord { .. } => {
            hint("upgrade mxv to the version that wrote the record, or a later one")
        }
        Error::Unwritable { path, .. } => hint(format!(
            "make {} writable, and run the command again",
            shown(path)
        )),
        _ => hint("run the command again"),
    }
}

/// Returns the values at fault in a technical failure of the library.
fn library_fields(error: &Error) -> Vec<Field<'_>> {
    match error {
        Error::RepositoryNotFound { path } => vec![Field::new("path", shown(path))],
        Error::NoOrigin { repository }
        | Error::EmptyOrigin { repository }
        | Error::NoDefaultBranch { repository } => {
            vec![Field::new("repository", shown(repository))]
        }
        Error::CommandFailed {
            operation,
            dir,
            message,
            ..
        } => vec![
            Field::new("operation", *operation),
            Field::new("dir", shown(dir)),
            Field::new("message", message),
        ],
        Error::TimedOut { operation, .. } => vec![Field::new("operation", *operation)],
        Error::StateUnreadable { path, .. }
        | Error::CorruptRecord { path, .. }
        | Error::Unwritable { path, .. } => vec![Field::new("path", shown(path))],
        Error::NewerRecord { path, version } => vec![
            Field::new("path", shown(path)),
            Field::new("version", *version),
        ],
        _ => Vec::new(),
    }
}

impl fmt::Display for MxvFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(formatter, "no workspace is named {name}"),
            Self::Exists { name } => write!(formatter, "a workspace named {name} already exists"),
            Self::BadName { name, .. } => write!(formatter, "{name:?} is not a workspace name"),
            Self::Dirty { name, changes } => write!(
                formatter,
                "the workspace {name} has {} uncommitted or untracked {}",
                changes.len(),
                if changes.len() == 1 {
                    "change"
                } else {
                    "changes"
                }
            ),
            Self::DirectoryTaken { path } => {
                write!(formatter, "the directory {} already exists", shown(path))
            }
            Self::Unowned { name, path } => write!(
                formatter,
                "the checkout {} of the workspace {name} is not a checkout that git lists",
                shown(path)
            ),
            Self::BranchExists { repository, branch } => write!(
                formatter,
                "the repository {} already has a branch named {branch}",
                shown(repository)
            ),
            Self::NoDirectory { variable } => write!(
                formatter,
                "mxv cannot find a directory for workspaces, because HOME and {variable} name no absolute path"
            ),
            Self::Library(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MxvFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadName { reason, .. } => Some(reason),
            Self::Library(error) => error.source(),
            Self::NotFound { .. }
            | Self::Exists { .. }
            | Self::Dirty { .. }
            | Self::DirectoryTaken { .. }
            | Self::Unowned { .. }
            | Self::BranchExists { .. }
            | Self::NoDirectory { .. } => None,
        }
    }
}
