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
/// The code of a removal that ignored files outside the disposable set,
/// such as `.env`, stop.
pub const WORKSPACE_IGNORED_FILES: Code = Code::new("mxv", "workspace", "ignored_files");
/// The code of a removal that commits on no branch, such as commits on a
/// detached `HEAD`, stop.
pub const CHECKOUT_UNREACHABLE_COMMITS: Code = Code::new("mxv", "checkout", "unreachable_commits");
/// The code of a removal that a checkout locked for a reason other than
/// `initializing` stops.
pub const CHECKOUT_LOCKED: Code = Code::new("mxv", "checkout", "locked");
/// The code of a path that mxv cannot inspect, such as one below a
/// directory it cannot search.
pub const PATH_INACCESSIBLE: Code = Code::new("mxv", "path", "inaccessible");
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
/// The code of a workspace record that holds another name than its file.
pub const RECORD_MISMATCHED: Code = Code::new("mxv", "record", "mismatched");
/// The code of a file or directory that cannot be written or removed.
pub const FILE_UNWRITABLE: Code = Code::new("mxv", "file", "unwritable");
/// The code of an environment in which mxv finds no home directory, so it
/// has no directory for the records, the config file or the default root.
pub const ENVIRONMENT_NO_DIRECTORY: Code = Code::new("mxv", "environment", "no_directory");
/// The code of an `MXV_ROOT` that is a relative path.
pub const ENVIRONMENT_RELATIVE_ROOT: Code = Code::new("mxv", "environment", "relative_root");
/// The code of a config file that mxv cannot parse or that sets something
/// it does not accept.
pub const CONFIG_INVALID: Code = Code::new("mxv", "config", "invalid");
/// The code of a config file that is there but cannot be read.
pub const CONFIG_UNREADABLE: Code = Code::new("mxv", "config", "unreadable");

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
    /// A checkout holds ignored files outside the disposable set.
    IgnoredFiles {
        /// The workspace's name.
        name: String,
        /// Each ignored path, with its checkout's name in front.
        paths: Vec<String>,
    },
    /// A checkout's `HEAD` holds commits that no branch holds.
    StrayCommits {
        /// The workspace's name.
        name: String,
        /// The checkout's directory.
        path: PathBuf,
        /// The number of commits that removing the checkout would lose.
        commits: u32,
    },
    /// A checkout is locked for a reason other than `initializing`.
    Locked {
        /// The workspace's name.
        name: String,
        /// The checkout's repository.
        repository: PathBuf,
        /// The checkout's directory.
        path: PathBuf,
        /// The lock's reason.
        reason: String,
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
    /// mxv finds no home directory.
    NoHome,
    /// `MXV_ROOT` is a relative path.
    RelativeRoot {
        /// The value of `MXV_ROOT`.
        value: PathBuf,
    },
    /// The config file cannot be parsed, or sets something mxv does not
    /// accept.
    BadConfig {
        /// The config file.
        path: PathBuf,
        /// The line at fault, counted from 1, when the parser names one.
        line: Option<usize>,
        /// What is wrong.
        reason: String,
    },
    /// The config file is there but cannot be read.
    ConfigUnreadable {
        /// The config file.
        path: PathBuf,
        /// The error of the read.
        source: std::io::Error,
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
    &WORKSPACE_IGNORED_FILES,
    &CHECKOUT_UNREACHABLE_COMMITS,
    &CHECKOUT_LOCKED,
    &PATH_INACCESSIBLE,
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
    &RECORD_MISMATCHED,
    &FILE_UNWRITABLE,
    &ENVIRONMENT_NO_DIRECTORY,
    &ENVIRONMENT_RELATIVE_ROOT,
    &CONFIG_INVALID,
    &CONFIG_UNREADABLE,
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
            Self::IgnoredFiles { .. } => &WORKSPACE_IGNORED_FILES,
            Self::StrayCommits { .. } => &CHECKOUT_UNREACHABLE_COMMITS,
            Self::Locked { .. } => &CHECKOUT_LOCKED,
            Self::DirectoryTaken { .. } => &DIRECTORY_TAKEN,
            Self::Unowned { .. } => &CHECKOUT_UNOWNED,
            Self::BranchExists { .. } => &BRANCH_EXISTS,
            Self::NoHome => &ENVIRONMENT_NO_DIRECTORY,
            Self::RelativeRoot { .. } => &ENVIRONMENT_RELATIVE_ROOT,
            Self::BadConfig { .. } => &CONFIG_INVALID,
            Self::ConfigUnreadable { .. } => &CONFIG_UNREADABLE,
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
                Error::MismatchedRecord { .. } => &RECORD_MISMATCHED,
                Error::Inaccessible { .. } => &PATH_INACCESSIBLE,
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
                "commit, stash or delete the changes, or run `mxv remove {name} --force` to discard them with the workspace; `--force` keeps a branch with unmerged commits"
            )),
            Self::IgnoredFiles { name, .. } => hint(format!(
                "move the ignored files you want to keep out of the checkout, or run `mxv remove {name} --force` to delete them with the workspace"
            )),
            Self::Locked {
                name,
                repository,
                path,
                ..
            } => hint(format!(
                "when nothing else uses the checkout, run `git -C {} worktree unlock {}`, and then `mxv remove {name}` again",
                shown(repository),
                shown(path)
            )),
            Self::StrayCommits { name, path, .. } => hint(format!(
                "keep the commits on a branch with `git -C {} switch -c <BRANCH>`, or run `mxv remove {name} --delete-unmerged` to delete them",
                shown(path)
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
            Self::NoHome => hint("set HOME to your home directory"),
            Self::RelativeRoot { .. } => hint(
                "set MXV_ROOT to an absolute path, or unset it to use the config file or the default root",
            ),
            Self::BadConfig { path, line, .. } => hint(match line {
                Some(line) => format!(
                    "fix line {line} of {}, which may set only `root = \"<ABSOLUTE PATH>\"`",
                    shown(path)
                ),
                None => format!(
                    "fix {}, which may set only `root = \"<ABSOLUTE PATH>\"`",
                    shown(path)
                ),
            }),
            Self::ConfigUnreadable { path, .. } => hint(format!(
                "make {} a readable file, or remove it",
                shown(path)
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
            Self::IgnoredFiles { name, paths } => vec![
                Field::new("name", name),
                Field::new("paths", FieldValue::list(paths.iter())),
            ],
            Self::Locked {
                name,
                repository,
                path,
                reason,
            } => vec![
                Field::new("name", name),
                Field::new("repository", shown(repository)),
                Field::new("path", shown(path)),
                Field::new("reason", reason),
            ],
            Self::StrayCommits {
                name,
                path,
                commits,
            } => vec![
                Field::new("name", name),
                Field::new("path", shown(path)),
                Field::new("commits", *commits),
            ],
            Self::DirectoryTaken { path } => vec![Field::new("path", shown(path))],
            Self::Unowned { name, path } => {
                vec![Field::new("name", name), Field::new("path", shown(path))]
            }
            Self::BranchExists { repository, branch } => vec![
                Field::new("repository", shown(repository)),
                Field::new("branch", branch),
            ],
            Self::NoHome => Vec::new(),
            Self::RelativeRoot { value } => vec![Field::new("value", shown(value))],
            Self::BadConfig { path, line, .. } => {
                let mut fields = vec![Field::new("path", shown(path))];
                if let Some(line) = line {
                    fields.push(Field::new("line", *line));
                }
                fields
            }
            Self::ConfigUnreadable { path, .. } => vec![Field::new("path", shown(path))],
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
        Error::TimedOut { operation, dir, .. } => hint(format!(
            "check that git {operation} can finish in {}, such as that its origin answers and no other git command holds a lock there, and run the command again; when `mxv list` still lists the workspace, `mxv remove <NAME> --force` removes what is left",
            shown(dir)
        )),
        Error::StateUnreadable { path, .. } => hint(format!("make {} readable", shown(path))),
        Error::CorruptRecord { path, .. } => hint(format!(
            "repair {} or delete it, which makes mxv forget that workspace",
            shown(path)
        )),
        Error::NewerRecord { .. } => {
            hint("upgrade mxv to the version that wrote the record, or a later one")
        }
        Error::Inaccessible { path, .. } => hint(format!(
            "make the directories above {} searchable, and run the command again",
            shown(path)
        )),
        Error::MismatchedRecord { path, .. } => hint(format!(
            "rename {} to the name that it holds, or delete it, which makes mxv forget that workspace",
            shown(path)
        )),
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
        Error::TimedOut {
            operation,
            dir,
            deadline,
        } => vec![
            Field::new("operation", *operation),
            Field::new("dir", shown(dir)),
            Field::new("seconds", deadline.as_secs()),
        ],
        Error::Inaccessible { path, .. } => vec![Field::new("path", shown(path))],
        Error::MismatchedRecord { path, name } => {
            vec![Field::new("path", shown(path)), Field::new("name", name)]
        }
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
            Self::IgnoredFiles { name, paths } => write!(
                formatter,
                "the workspace {name} has {} ignored {} that mxv does not treat as disposable",
                paths.len(),
                if paths.len() == 1 { "path" } else { "paths" }
            ),
            Self::Locked {
                name, path, reason, ..
            } => write!(
                formatter,
                "the checkout {} of the workspace {name} is locked: {}",
                shown(path),
                if reason.is_empty() { "git gives no reason" } else { reason }
            ),
            Self::StrayCommits {
                name,
                path,
                commits,
            } => write!(
                formatter,
                "the checkout {} of the workspace {name} holds {commits} {} that no branch holds",
                shown(path),
                if *commits == 1 { "commit" } else { "commits" }
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
            Self::NoHome => formatter.write_str(
                "mxv cannot find your home directory, which holds its state, config and data directories",
            ),
            Self::RelativeRoot { value } => write!(
                formatter,
                "MXV_ROOT is the relative path {}, and mxv needs an absolute path",
                shown(value)
            ),
            Self::BadConfig { path, line, reason } => match line {
                Some(line) => write!(
                    formatter,
                    "the config file {} is invalid at line {line}: {reason}",
                    shown(path)
                ),
                None => write!(formatter, "the config file {} is invalid: {reason}", shown(path)),
            },
            Self::ConfigUnreadable { path, source } => write!(
                formatter,
                "mxv cannot read the config file {}: {source}",
                shown(path)
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
            Self::ConfigUnreadable { source, .. } => Some(source),
            Self::NotFound { .. }
            | Self::Exists { .. }
            | Self::Dirty { .. }
            | Self::DirectoryTaken { .. }
            | Self::Unowned { .. }
            | Self::BranchExists { .. }
            | Self::IgnoredFiles { .. }
            | Self::StrayCommits { .. }
            | Self::Locked { .. }
            | Self::NoHome
            | Self::RelativeRoot { .. }
            | Self::BadConfig { .. } => None,
        }
    }
}
