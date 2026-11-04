//! The failures of the workspace verbs, each with a stable code and a next
//! step (CLI-3).

use std::fmt;
use std::path::{Path, PathBuf};

use crumbkit::{Call, Code, Failure, Field, FieldValue, Recovery};
use multiverse::{Allow, BadName, Error};

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
/// The code of a removal that stopped without removing anything, because
/// the workspace changed while mxv waited for the confirmation, so the
/// removal would lose more than mxv showed.
pub const WORKSPACE_CHANGED: Code = Code::new("mxv", "workspace", "changed");
/// The code of a removal that commits on no branch, such as commits on a
/// detached `HEAD`, stop.
pub const CHECKOUT_UNREACHABLE_COMMITS: Code = Code::new("mxv", "checkout", "unreachable_commits");
/// The code of a removal that a clone's commits stop, because no remote
/// branch of its `origin` holds them.
pub const CHECKOUT_UNPUSHED_COMMITS: Code = Code::new("mxv", "checkout", "unpushed_commits");
/// The code of a removal that a clone's stash entries stop.
pub const CHECKOUT_STASHES: Code = Code::new("mxv", "checkout", "stashes");
/// The code of a removal that cannot fetch a clone's `origin`, such as on
/// a machine that is offline, so mxv cannot tell which commits the origin
/// holds.
pub const CHECKOUT_ORIGIN_UNREACHABLE: Code = Code::new("mxv", "checkout", "origin_unreachable");
/// The code of a mirror whose lock another mxv command holds for longer
/// than mxv waits, such as a `mxv new` that still fetches it.
pub const MIRROR_LOCKED: Code = Code::new("mxv", "mirror", "locked");
/// The code of a mirror that mxv cannot fetch from its origin.
pub const MIRROR_FETCH_FAILED: Code = Code::new("mxv", "mirror", "fetch_failed");
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
/// The code of an app's process that cannot start, such as when its `cwd`
/// does not exist.
pub const PROCESS_UNSTARTABLE: Code = Code::new("mxv", "process", "unstartable");
/// The code of a process table that `ps` cannot read.
pub const PROCESS_TABLE_UNREADABLE: Code = Code::new("mxv", "process", "table_unreadable");
/// The code of an operating system whose process-table structs differ
/// from the ones mxv reads.
pub const PROCESS_UNSUPPORTED_OS: Code = Code::new("mxv", "process", "unsupported_os");
/// The code of a port range whose every port is held by a workspace or taken.
pub const PORT_EXHAUSTED: Code = Code::new("mxv", "port", "exhausted");
/// The code of a workspace whose lock another mxv command holds, such as an
/// `mxv up` that still waits for a process to be ready.
pub const WORKSPACE_BUSY: Code = Code::new("mxv", "workspace", "busy");
/// The code of a manifest that declares containers on a machine without a
/// `docker` program on `PATH`.
pub const DOCKER_NOT_FOUND: Code = Code::new("mxv", "docker", "not_found");
/// The code of a docker without its `compose` plugin, which mxv needs to
/// start containers.
pub const DOCKER_COMPOSE_MISSING: Code = Code::new("mxv", "docker", "compose_missing");
/// The code of a docker daemon with no free address pool for a network,
/// such as one that holds hundreds of networks.
pub const DOCKER_ADDRESS_POOLS_EXHAUSTED: Code =
    Code::new("mxv", "docker", "address_pools_exhausted");
/// The code of a docker daemon that does not answer.
pub const DOCKER_UNAVAILABLE: Code = Code::new("mxv", "docker", "unavailable");
/// The code of a docker command that failed or missed its deadline, such as
/// a `docker compose up` whose image cannot be pulled.
pub const DOCKER_FAILED: Code = Code::new("mxv", "docker", "failed");
/// The code of a docker container or volume that has a name of mxv's and
/// lacks the labels of this state directory and workspace, which mxv
/// leaves alone.
pub const DOCKER_FOREIGN_OBJECT: Code = Code::new("mxv", "docker", "foreign_object");
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
/// The code of `mxv clean --over-limit` when the config file sets no
/// `max_disposable`.
pub const CONFIG_NO_LIMIT: Code = Code::new("mxv", "config", "no_limit");

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
        /// What the removal's flags allowed.
        allow: Allow,
    },
    /// A checkout holds ignored files outside the disposable set.
    IgnoredFiles {
        /// The workspace's name.
        name: String,
        /// Each ignored path, with its checkout's name in front.
        paths: Vec<String>,
        /// Why a checkout's disposable set holds fewer globs than its
        /// `mxv.toml` declares, such as an invalid manifest.
        warnings: Vec<String>,
        /// What the removal's flags allowed.
        allow: Allow,
    },
    /// The removal would delete paths that only the repository's own
    /// `mxv.toml` declares disposable, and `--yes` without `--force` gave
    /// no person a chance to confirm them.
    ManifestDisposable {
        /// The workspace's name.
        name: String,
        /// Each such path, with its checkout's name in front.
        paths: Vec<String>,
        /// What the removal's flags allowed.
        allow: Allow,
    },
    /// The workspace changed while mxv waited for the confirmation, so the
    /// removal would lose more than mxv showed, and mxv removed nothing.
    Changed {
        /// The workspace's name.
        name: String,
        /// What the removal's flags allowed.
        allow: Allow,
    },
    /// A checkout's `HEAD` holds commits that no branch holds.
    StrayCommits {
        /// The workspace's name.
        name: String,
        /// The checkout's directory.
        path: PathBuf,
        /// The number of commits that removing the checkout would lose.
        commits: u32,
        /// What the removal's flags allowed.
        allow: Allow,
    },
    /// A clone holds commits that no remote branch of its `origin` holds.
    Unpushed {
        /// The workspace's name.
        name: String,
        /// The clone's directory.
        path: PathBuf,
        /// The number of commits that deleting the clone would lose.
        commits: u32,
        /// What the removal's flags allowed.
        allow: Allow,
    },
    /// A clone holds stash entries.
    Stashed {
        /// The workspace's name.
        name: String,
        /// The clone's directory.
        path: PathBuf,
        /// The number of stash entries.
        stashes: u32,
        /// What the removal's flags allowed.
        allow: Allow,
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
        /// What the removal's flags allowed.
        allow: Allow,
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
    /// `mxv clean --over-limit` ran, and the config file sets no
    /// `max_disposable`.
    NoLimit {
        /// The config file.
        path: PathBuf,
    },
    /// `mxv clean` could not delete every disposable path of a workspace.
    /// It deleted the others and reports them.
    PartlyCleaned {
        /// The workspace's name.
        name: String,
        /// The first file or directory that could not be deleted.
        path: PathBuf,
        /// Each path that the run deleted, with its checkout's name in
        /// front.
        deleted: Vec<String>,
        /// Each path whose deletion failed, with the entry that failed and
        /// why.
        failed: Vec<String>,
        /// The bytes that the run freed in the workspace.
        freed: u64,
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
    &WORKSPACE_CHANGED,
    &CHECKOUT_UNREACHABLE_COMMITS,
    &CHECKOUT_UNPUSHED_COMMITS,
    &CHECKOUT_STASHES,
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
    &MIRROR_LOCKED,
    &MIRROR_FETCH_FAILED,
    &CHECKOUT_ORIGIN_UNREACHABLE,
    &RECORD_UNREADABLE,
    &RECORD_CORRUPT,
    &RECORD_NEWER,
    &RECORD_MISMATCHED,
    &FILE_UNWRITABLE,
    &ENVIRONMENT_NO_DIRECTORY,
    &ENVIRONMENT_RELATIVE_ROOT,
    &CONFIG_INVALID,
    &CONFIG_UNREADABLE,
    &CONFIG_NO_LIMIT,
    &PROCESS_UNSTARTABLE,
    &PROCESS_TABLE_UNREADABLE,
    &PROCESS_UNSUPPORTED_OS,
    &PORT_EXHAUSTED,
    &WORKSPACE_BUSY,
    &DOCKER_NOT_FOUND,
    &DOCKER_COMPOSE_MISSING,
    &DOCKER_ADDRESS_POOLS_EXHAUSTED,
    &DOCKER_UNAVAILABLE,
    &DOCKER_FAILED,
    &DOCKER_FOREIGN_OBJECT,
];

/// Returns `path` as text for a field or a hint.
fn shown(path: &Path) -> String {
    path.display().to_string()
}

/// Returns the keys that the config file may set, as a hint names them.
fn settings() -> String {
    let (last, rest) = crate::config::SETTINGS
        .split_last()
        .map_or(("", &[][..]), |(last, rest)| (*last, rest));
    format!("{} and {last}", rest.join(", "))
}

/// Returns the hint `text` as a recovery.
fn hint(text: impl Into<String>) -> Recovery {
    Recovery::Hint(text.into())
}

/// Returns the words after `mxv` of a removal of the workspace `name` with
/// the flags that `allow` stands for, followed by `last`, such as
/// `["remove", "notes", "--force", "--yes"]`.
fn remove_words(name: &str, allow: Allow, last: &str) -> Vec<String> {
    let mut words = vec!["remove".to_owned(), name.to_owned()];
    if allow.discard_changes {
        words.push("--force".to_owned());
    }
    if allow.delete_unmerged {
        words.push("--delete-unmerged".to_owned());
    }
    words.push(last.to_owned());
    words
}

/// Returns the command line of a removal of the workspace `name` that
/// allows what `allow` allows and runs without asking, such as
/// `mxv remove notes --force --yes`, for the text of a hint. A caller
/// without a terminal cannot answer the question, so the line has `--yes`.
pub(crate) fn remove_line(name: &str, allow: Allow) -> String {
    format!("mxv {}", remove_words(name, allow, "--yes").join(" "))
}

/// Returns the command line of a removal of the workspace `name` that
/// allows what `allow` allows, deletes the workspace's docker volumes and
/// runs without asking, such as `mxv remove notes --force
/// --delete-volumes --yes`, for the text of a hint.
pub(crate) fn remove_volumes_line(name: &str, allow: Allow) -> String {
    let mut words = remove_words(name, allow, "--delete-volumes");
    words.push("--yes".to_owned());
    format!("mxv {}", words.join(" "))
}

/// Returns the recovery that runs the removal of the workspace `name` with
/// the flags of `allow` and `--yes`.
fn run_removal(name: &str, allow: Allow) -> Recovery {
    Recovery::Run(Call::typed(remove_words(name, allow, "--yes")))
}

/// Returns `allow` with discarding changes allowed as well.
const fn forced(allow: Allow) -> Allow {
    Allow {
        discard_changes: true,
        ..allow
    }
}

/// Returns `allow` with deleting unmerged commits allowed as well.
const fn unmerged(allow: Allow) -> Allow {
    Allow {
        delete_unmerged: true,
        ..allow
    }
}

impl Failure for MxvFailure {
    fn code(&self) -> &'static Code {
        match self {
            Self::NotFound { .. } => &WORKSPACE_NOT_FOUND,
            Self::Exists { .. } => &WORKSPACE_EXISTS,
            Self::BadName { .. } => &WORKSPACE_BAD_NAME,
            Self::Dirty { .. } => &WORKSPACE_DIRTY,
            Self::IgnoredFiles { .. } | Self::ManifestDisposable { .. } => &WORKSPACE_IGNORED_FILES,
            Self::Changed { .. } => &WORKSPACE_CHANGED,
            Self::StrayCommits { .. } => &CHECKOUT_UNREACHABLE_COMMITS,
            Self::Unpushed { .. } => &CHECKOUT_UNPUSHED_COMMITS,
            Self::Stashed { .. } => &CHECKOUT_STASHES,
            Self::Locked { .. } => &CHECKOUT_LOCKED,
            Self::DirectoryTaken { .. } => &DIRECTORY_TAKEN,
            Self::Unowned { .. } => &CHECKOUT_UNOWNED,
            Self::BranchExists { .. } => &BRANCH_EXISTS,
            Self::NoHome => &ENVIRONMENT_NO_DIRECTORY,
            Self::RelativeRoot { .. } => &ENVIRONMENT_RELATIVE_ROOT,
            Self::BadConfig { .. } => &CONFIG_INVALID,
            Self::ConfigUnreadable { .. } => &CONFIG_UNREADABLE,
            Self::NoLimit { .. } => &CONFIG_NO_LIMIT,
            Self::PartlyCleaned { .. } => &FILE_UNWRITABLE,
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
                Error::ProcessUnstartable { .. } => &PROCESS_UNSTARTABLE,
                Error::ProcessTable { .. } => &PROCESS_TABLE_UNREADABLE,
                Error::UnsupportedOs { .. } => &PROCESS_UNSUPPORTED_OS,
                Error::NoFreePort { .. } => &PORT_EXHAUSTED,
                Error::Busy { .. } => &WORKSPACE_BUSY,
                Error::MirrorBusy { .. } => &MIRROR_LOCKED,
                Error::MirrorFetchFailed { .. } => &MIRROR_FETCH_FAILED,
                Error::OriginFetchFailed { .. } => &CHECKOUT_ORIGIN_UNREACHABLE,
                Error::Inaccessible { .. } => &PATH_INACCESSIBLE,
                Error::DockerMissing { .. } => &DOCKER_NOT_FOUND,
                Error::DockerComposeMissing { .. } => &DOCKER_COMPOSE_MISSING,
                Error::DockerNoAddressPool { .. } => &DOCKER_ADDRESS_POOLS_EXHAUSTED,
                Error::DockerUnavailable { .. } => &DOCKER_UNAVAILABLE,
                Error::DockerFailed { .. } => &DOCKER_FAILED,
                Error::DockerForeign { .. } => &DOCKER_FOREIGN_OBJECT,
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
            Self::Dirty { name, allow, .. } => hint(format!(
                "commit, stash or delete the changes and run `{}` again; `--force` discards them with the workspace and keeps a branch with unmerged commits",
                remove_line(name, *allow)
            )),
            Self::IgnoredFiles { name, allow, .. } => hint(format!(
                "move the ignored files you want to keep out of the checkout and run `{}` again; `--force` deletes them with the workspace",
                remove_line(name, *allow)
            )),
            Self::ManifestDisposable { name, .. } => hint(format!(
                "check that the paths hold nothing you want to keep, then answer the prompt of `mxv remove {name}` at a terminal, or pass `--force`, which deletes them with the workspace"
            )),
            Self::Changed { name, allow } => {
                Recovery::Run(Call::typed(remove_words(name, *allow, "--dry-run")))
            }
            Self::Locked {
                name,
                repository,
                path,
                allow,
                ..
            } => hint(format!(
                "when nothing else uses the checkout, run `git -C {} worktree unlock {}`, and then `{}` again",
                shown(repository),
                shown(path),
                remove_line(name, *allow)
            )),
            Self::StrayCommits {
                name, path, allow, ..
            } => hint(format!(
                "keep the commits on a branch with `git -C {} switch -c <BRANCH>` and run `{}` again; `--delete-unmerged` deletes them",
                shown(path),
                remove_line(name, *allow)
            )),
            Self::Unpushed {
                name, path, allow, ..
            } => hint(format!(
                "push the commits with `git -C {} push origin HEAD` and run `{}` again; `--delete-unmerged` deletes them with the clone",
                shown(path),
                remove_line(name, *allow)
            )),
            Self::Stashed {
                name, path, allow, ..
            } => hint(format!(
                "apply or drop the stash entries with `git -C {} stash pop` or `git -C {} stash drop` and run `{}` again; `--force` deletes them with the clone",
                shown(path),
                shown(path),
                remove_line(name, *allow)
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
                    "fix line {line} of {}, which may set only {}",
                    shown(path),
                    settings()
                ),
                None => format!("fix {}, which may set only {}", shown(path), settings()),
            }),
            // Deleting an entry needs write permission on the directory
            // that holds it, so the hint names that directory.
            Self::PartlyCleaned { name, path, .. } => hint(format!(
                "give the directory {} write permission, or delete {} yourself, and run `mxv clean {name}` again",
                shown(path.parent().unwrap_or(path)),
                shown(path)
            )),
            Self::NoLimit { path } => hint(format!(
                "set `max_disposable = \"50GiB\"` in {}, or name the workspaces to clean, or pass `--all`",
                shown(path)
            )),
            Self::ConfigUnreadable { path, .. } => hint(format!(
                "make {} a readable file, or remove it",
                shown(path)
            )),
            Self::Library(error) => library_recovery(error),
        }
    }

    fn alternatives(&self) -> Vec<Recovery> {
        match self {
            Self::Dirty { name, allow, .. }
            | Self::IgnoredFiles { name, allow, .. }
            | Self::ManifestDisposable { name, allow, .. } => {
                vec![run_removal(name, forced(*allow))]
            }
            Self::StrayCommits { name, allow, .. } | Self::Unpushed { name, allow, .. } => {
                vec![run_removal(name, unmerged(*allow))]
            }
            Self::Stashed { name, allow, .. } => vec![run_removal(name, forced(*allow))],
            _ => Vec::new(),
        }
    }

    fn fields(&self) -> Vec<Field<'_>> {
        match self {
            Self::NotFound { name }
            | Self::Exists { name }
            | Self::BadName { name, .. }
            | Self::Changed { name, .. } => {
                vec![Field::new("name", name)]
            }
            Self::Dirty { name, changes, .. } => vec![
                Field::new("name", name),
                Field::new("changes", FieldValue::list(changes.iter())),
            ],
            Self::ManifestDisposable { name, paths, .. } => vec![
                Field::new("name", name),
                Field::new("paths", FieldValue::list(paths.iter())),
            ],
            Self::IgnoredFiles {
                name,
                paths,
                warnings,
                ..
            } => {
                let mut fields = vec![
                    Field::new("name", name),
                    Field::new("paths", FieldValue::list(paths.iter())),
                ];
                if !warnings.is_empty() {
                    fields.push(Field::new("warnings", FieldValue::list(warnings.iter())));
                }
                fields
            }
            Self::Locked {
                name,
                repository,
                path,
                reason,
                ..
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
                ..
            } => vec![
                Field::new("name", name),
                Field::new("path", shown(path)),
                Field::new("commits", *commits),
            ],
            Self::Unpushed {
                name,
                path,
                commits,
                ..
            } => vec![
                Field::new("name", name),
                Field::new("path", shown(path)),
                Field::new("commits", *commits),
            ],
            Self::Stashed {
                name,
                path,
                stashes,
                ..
            } => vec![
                Field::new("name", name),
                Field::new("path", shown(path)),
                Field::new("stashes", *stashes),
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
            Self::ConfigUnreadable { path, .. } | Self::NoLimit { path } => {
                vec![Field::new("path", shown(path))]
            }
            Self::PartlyCleaned {
                name,
                path,
                deleted,
                failed,
                freed,
            } => vec![
                Field::new("name", name),
                Field::new("path", shown(path)),
                Field::new("deleted", FieldValue::list(deleted.iter())),
                Field::new("failed", FieldValue::list(failed.iter())),
                Field::new("freed", *freed),
            ],
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
        Error::ProcessUnstartable { process, .. } => hint(format!(
            "check that the cwd of {process} in mxv.toml exists in the checkout, and run the command again"
        )),
        Error::ProcessTable { .. } => hint("put ps on PATH, and run the command again"),
        Error::UnsupportedOs { .. } => hint(
            "run mxv on a version of the operating system whose process table it reads, and report the version and the message",
        ),
        Error::NoFreePort { .. } => hint(
            "widen `ports_range` in the config file, or remove workspaces that hold ports you no longer need",
        ),
        Error::MirrorBusy { .. } => hint(
            "wait for the other mxv command that fetches the mirror to finish, such as another `mxv new` of the same repository, and run the command again",
        ),
        Error::MirrorFetchFailed { origin, .. } => hint(format!(
            "check that the machine is online and that `git ls-remote {origin}` works with your credentials, and run the command again"
        )),
        Error::OriginFetchFailed { checkout, .. } => hint(format!(
            "check that the machine is online and that `git -C {} fetch origin` works with your credentials, and run the command again; `--delete-unmerged` removes the workspace without the fetch, and deletes the commits that origin does not hold",
            shown(checkout)
        )),
        Error::Busy { .. } => hint(
            "wait for the other mxv command on this workspace to finish, such as an `mxv up` that waits for a process to be ready, and run the command again",
        ),
        Error::DockerMissing { .. } => hint(
            "install docker and put it on PATH, and run the command again; a workspace that has run containers needs docker until `mxv remove` removes it",
        ),
        Error::DockerNoAddressPool { .. } => hint(
            "list docker's networks with `docker network ls`, remove the ones nothing uses with `docker network prune`, or add address pools under `default-address-pools` in the docker daemon's settings, and run the command again",
        ),
        Error::DockerComposeMissing { .. } => hint(
            "install the docker compose plugin, such as with Docker Desktop or your package manager's docker-compose-plugin, and run the command again",
        ),
        Error::DockerUnavailable { .. } => hint(
            "start the docker daemon, such as by opening Docker Desktop, and run the command again",
        ),
        Error::DockerFailed { operation, .. } => hint(format!(
            "read docker's message, fix what `docker {operation}` reports, such as an image name that cannot be pulled, and run the command again"
        )),
        Error::DockerForeign { kind, name } => hint(format!(
            "find out who made the docker {kind} {name}, rename or remove it with docker if nobody needs it, and run the command again"
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
        | Error::Unwritable { path, .. }
        | Error::Busy { path } => vec![Field::new("path", shown(path))],
        Error::MirrorBusy { mirror } => vec![Field::new("mirror", shown(mirror))],
        Error::OriginFetchFailed { checkout, message } => vec![
            Field::new("checkout", shown(checkout)),
            Field::new("message", message),
        ],
        Error::MirrorFetchFailed {
            origin,
            mirror,
            message,
        } => vec![
            Field::new("origin", origin),
            Field::new("mirror", shown(mirror)),
            Field::new("message", message),
        ],
        Error::NewerRecord { path, version } => vec![
            Field::new("path", shown(path)),
            Field::new("version", *version),
        ],
        Error::ProcessUnstartable { process, .. } => vec![Field::new("process", process)],
        Error::DockerUnavailable { message }
        | Error::DockerComposeMissing { message }
        | Error::DockerNoAddressPool { message } => {
            vec![Field::new("message", message)]
        }
        Error::DockerFailed {
            operation,
            container,
            message,
        } => {
            let mut fields = vec![Field::new("operation", *operation)];
            if let Some(container) = container {
                fields.push(Field::new("container", container));
            }
            fields.push(Field::new("message", message));
            fields
        }
        Error::DockerForeign { kind, name } => {
            vec![Field::new("kind", *kind), Field::new("name", name)]
        }
        _ => Vec::new(),
    }
}

impl fmt::Display for MxvFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { name } => write!(formatter, "no workspace is named {name}"),
            Self::Exists { name } => write!(formatter, "a workspace named {name} already exists"),
            Self::BadName { name, .. } => write!(formatter, "{name:?} is not a workspace name"),
            Self::Dirty { name, changes, .. } => write!(
                formatter,
                "the workspace {name} has {} uncommitted or untracked {}",
                changes.len(),
                if changes.len() == 1 {
                    "change"
                } else {
                    "changes"
                }
            ),
            Self::IgnoredFiles { name, paths, .. } => write!(
                formatter,
                "the workspace {name} has {} ignored {} that mxv does not treat as disposable",
                paths.len(),
                if paths.len() == 1 { "path" } else { "paths" }
            ),
            Self::ManifestDisposable { name, paths, .. } => write!(
                formatter,
                "the removal of the workspace {name} would delete {}, which only the repository's mxv.toml declares disposable, and --yes alone does not delete such paths",
                paths.join(", ")
            ),
            Self::Changed { name, .. } => write!(
                formatter,
                "the workspace {name} changed while mxv waited for the confirmation, so the removal would lose more than mxv showed, and mxv removed nothing"
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
                ..
            } => write!(
                formatter,
                "the checkout {} of the workspace {name} holds {commits} {} that no branch holds",
                shown(path),
                if *commits == 1 { "commit" } else { "commits" }
            ),
            Self::Unpushed {
                name,
                path,
                commits,
                ..
            } => write!(
                formatter,
                "the clone {} of the workspace {name} holds {commits} {} that origin does not hold",
                shown(path),
                if *commits == 1 { "commit" } else { "commits" }
            ),
            Self::Stashed {
                name,
                path,
                stashes,
                ..
            } => write!(
                formatter,
                "the clone {} of the workspace {name} holds {stashes} stash {}",
                shown(path),
                if *stashes == 1 { "entry" } else { "entries" }
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
            Self::NoLimit { path } => write!(
                formatter,
                "--over-limit needs a limit, and the config file {} sets no `max_disposable`",
                shown(path)
            ),
            Self::PartlyCleaned { name, path, .. } => write!(
                formatter,
                "mxv could not delete {} in the workspace {name}, and it deleted the workspace's other disposable paths",
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
            | Self::ManifestDisposable { .. }
            | Self::Changed { .. }
            | Self::StrayCommits { .. }
            | Self::Unpushed { .. }
            | Self::Stashed { .. }
            | Self::Locked { .. }
            | Self::NoHome
            | Self::RelativeRoot { .. }
            | Self::NoLimit { .. }
            | Self::PartlyCleaned { .. }
            | Self::BadConfig { .. } => None,
        }
    }
}
