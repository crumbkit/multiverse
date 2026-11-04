//! The rows and results that the workspace verbs print.

use std::collections::BTreeMap;
use std::fmt;

use crumbkit::Row;
use multiverse::Error;
use multiverse::{
    Allow, BranchFate, Checkout, CheckoutRemoval, Fate, ProcessState, ProcessStatus, Removal,
    Workspace,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::environment::{Root, RootSource};
use crate::failures::remove_line;

/// One workspace, as `list` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct WorkspaceRow {
    /// The workspace's name, which is also the name of its directory and of
    /// its checkouts' branch.
    name: String,
    /// The branch that the workspace's checkouts were created on.
    branch: String,
    /// The workspace's directory, which `mxv cd` prints.
    path: String,
    /// The top-level directory of each repository that the workspace checks
    /// out.
    repositories: Vec<String>,
    /// The workspace's checkouts, one for each repository.
    checkouts: Vec<CheckoutRow>,
    /// Why mxv cannot read the workspace's record, such as a record that
    /// is not JSON or that a newer mxv wrote, or empty when it can. A row
    /// with a problem names its record, without `.json`, in `name`, and
    /// leaves the other fields empty.
    problem: String,
    /// The host port that the workspace holds for each named port of its
    /// manifest, which the first `mxv up` allocates.
    ports: BTreeMap<String, u16>,
}

/// One workspace and the workspace root that the run used, as `new` and
/// `show` print it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PlacedRow {
    /// The workspace's name, which is also the name of its directory and of
    /// its checkouts' branch.
    name: String,
    /// The branch that the workspace's checkouts were created on.
    branch: String,
    /// The workspace's directory, which `mxv cd` prints. It comes from the
    /// workspace's record, so it stays where the workspace was created when
    /// the root changes later.
    path: String,
    /// The workspace root that the run used, in which `new` creates a
    /// workspace's directory.
    root: String,
    /// Where the root came from: `env` for `MXV_ROOT`, `config` for the
    /// config file's `root`, or `default` for
    /// `$XDG_DATA_HOME/mxv/workspaces`.
    root_source: RootSource,
    /// The top-level directory of each repository that the workspace checks
    /// out.
    repositories: Vec<String>,
    /// The workspace's checkouts, one for each repository.
    checkouts: Vec<CheckoutRow>,
    /// The host port that the workspace holds for each named port of its
    /// manifest, which the first `mxv up` allocates.
    ports: BTreeMap<String, u16>,
}

impl Row for PlacedRow {
    const COLUMNS: &'static [&'static str] =
        &["name", "branch", "path", "root", "root_source", "ports"];
    const LONG_COLUMNS: &'static [&'static str] = &["repositories"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

impl PlacedRow {
    /// Returns the row of `workspace` with `root`, the root that the run
    /// used.
    pub(crate) fn new(workspace: &Workspace, root: &Root) -> Self {
        let row = WorkspaceRow::from(workspace);
        Self {
            name: row.name,
            branch: row.branch,
            path: row.path,
            root: root.path.display().to_string(),
            root_source: root.source,
            repositories: row.repositories,
            checkouts: row.checkouts,
            ports: row.ports,
        }
    }
}

/// One checkout of a workspace.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CheckoutRow {
    /// The repository's name, which names the checkout's directory.
    name: String,
    /// The version-control system that made the checkout, which is `git`.
    backend: String,
    /// The repository's top-level directory.
    source: String,
    /// The checkout's directory.
    path: String,
    /// The branch that the checkout was created on.
    branch: String,
    /// The remote branch that the branch started from, such as
    /// `refs/remotes/origin/main`.
    base: String,
    /// The commit that the branch started at, or null in a dry run, which
    /// does not fetch.
    commit: Option<String>,
}

impl Row for WorkspaceRow {
    const COLUMNS: &'static [&'static str] = &["name", "branch", "path", "ports", "problem"];
    const PROBLEM: Option<&'static str> = Some("problem");
    const LONG_COLUMNS: &'static [&'static str] = &["repositories"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

impl From<&Checkout> for CheckoutRow {
    fn from(checkout: &Checkout) -> Self {
        Self {
            name: checkout.name.clone(),
            backend: "git".to_owned(),
            source: checkout.source.display().to_string(),
            path: checkout.path.display().to_string(),
            branch: checkout.branch.clone(),
            base: checkout.base.clone(),
            commit: checkout.commit.clone(),
        }
    }
}

impl From<&Workspace> for WorkspaceRow {
    fn from(workspace: &Workspace) -> Self {
        Self {
            name: workspace.name.to_string(),
            branch: workspace.checkouts.first().map_or_else(
                || workspace.name.to_string(),
                |checkout| checkout.branch.clone(),
            ),
            path: workspace.path.display().to_string(),
            repositories: workspace
                .checkouts
                .iter()
                .map(|checkout| checkout.source.display().to_string())
                .collect(),
            checkouts: workspace.checkouts.iter().map(CheckoutRow::from).collect(),
            problem: String::new(),
            ports: workspace.ports.clone(),
        }
    }
}

impl WorkspaceRow {
    /// Returns the row of a record that mxv cannot read, with `error`, a
    /// record error that names the record's path, as its problem.
    pub(crate) fn unreadable(error: &Error) -> Self {
        let name = match error {
            Error::CorruptRecord { path, .. }
            | Error::NewerRecord { path, .. }
            | Error::MismatchedRecord { path, .. }
            | Error::StateUnreadable { path, .. } => path
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => String::new(),
        };
        Self {
            name,
            branch: String::new(),
            path: String::new(),
            repositories: Vec::new(),
            checkouts: Vec::new(),
            problem: error.to_string(),
            ports: BTreeMap::new(),
        }
    }
}

/// What `mxv remove` did to a workspace, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RemovalResult {
    /// The workspace's name.
    name: String,
    /// The workspace's directory.
    path: String,
    /// What happens to the workspace's directory: `removed`, `missing` when
    /// it was already gone, or `kept` when it still holds other files.
    directory: DirectoryFate,
    /// What happens to each checkout and its branch.
    checkouts: Vec<CheckoutRemovalRow>,
    /// Whether `--force` allowed discarding uncommitted changes, untracked
    /// files and ignored files.
    force: bool,
    /// Whether `--delete-unmerged` allowed deleting commits that no other
    /// branch holds.
    delete_unmerged: bool,
    /// The ignored build directories, such as `app: target/`, that the
    /// removal deletes without asking, because a build makes them again.
    disposable: Vec<String>,
    /// The changes and other ignored paths, such as `app: .env`, that
    /// the removal discards because of `--force`.
    discarded: Vec<String>,
    /// Whether the result reports a dry run, which changed nothing.
    #[serde(skip)]
    planned: bool,
}

/// What `mxv remove` does to one checkout and its branch.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CheckoutRemovalRow {
    /// The repository's name.
    name: String,
    /// The checkout's directory.
    path: String,
    /// What happens to the checkout: `removed`, or `missing` when it was
    /// already gone.
    checkout: DirectoryFate,
    /// The checkout's branch.
    branch: String,
    /// The repository that holds the branch, for the text that says how to
    /// delete a branch that the removal kept.
    #[serde(skip)]
    repository: String,
    /// What happens to the branch: `deleted` when its commits are all on
    /// its base, `kept_unmerged` when it holds other commits, which
    /// `--delete-unmerged` would delete, `deleted_unmerged` when
    /// `--delete-unmerged` deletes such commits, `kept_moved` when it
    /// pointed at another commit when the removal came to delete it than
    /// when mxv planned the removal, or `missing` when it was already gone.
    branch_fate: BranchState,
    /// The number of commits on the branch that are not on its base, or
    /// null when the base that the record names is gone, so the number is
    /// unknown and the branch counts as unmerged, or when the branch moved
    /// after mxv planned the removal.
    unmerged_commits: Option<u32>,
    /// The number of commits that the checkout's `HEAD` holds and no branch
    /// holds, such as commits on a detached `HEAD`, which
    /// `--delete-unmerged` allowed the removal to delete.
    stray_commits: u32,
}

/// What `mxv remove` does to a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DirectoryFate {
    /// The directory is removed.
    Removed,
    /// The directory was already gone.
    Missing,
    /// The directory stays, because it still holds other files.
    Kept,
}

/// What `mxv remove` does to a branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BranchState {
    /// The branch is deleted, because its commits are all on its base.
    Deleted,
    /// The branch stays, because it holds commits that are not on its base.
    KeptUnmerged,
    /// The branch is deleted with commits that are not on its base.
    DeletedUnmerged,
    /// The branch stays, because it moved after mxv planned the removal,
    /// so it may hold work that mxv did not show.
    KeptMoved,
    /// The branch was already gone.
    Missing,
}

impl From<Fate> for DirectoryFate {
    fn from(fate: Fate) -> Self {
        match fate {
            Fate::Removed => Self::Removed,
            Fate::Missing => Self::Missing,
            Fate::Kept => Self::Kept,
        }
    }
}

/// Returns the state of a branch and its number of unmerged commits.
fn branch_state(fate: BranchFate) -> (BranchState, Option<u32>) {
    match fate {
        BranchFate::Deleted => (BranchState::Deleted, Some(0)),
        BranchFate::KeptUnmerged { commits } => (BranchState::KeptUnmerged, commits),
        BranchFate::DeletedUnmerged { commits } => (BranchState::DeletedUnmerged, commits),
        BranchFate::KeptMoved => (BranchState::KeptMoved, None),
        BranchFate::Missing => (BranchState::Missing, Some(0)),
    }
}

impl From<&CheckoutRemoval> for CheckoutRemovalRow {
    fn from(removal: &CheckoutRemoval) -> Self {
        let (branch_fate, unmerged_commits) = branch_state(removal.branch);
        Self {
            name: removal.checkout.name.clone(),
            path: removal.checkout.path.display().to_string(),
            checkout: removal.directory.into(),
            branch: removal.checkout.branch.clone(),
            repository: removal.checkout.source.display().to_string(),
            branch_fate,
            unmerged_commits,
            stray_commits: removal.stray_commits,
        }
    }
}

impl RemovalResult {
    /// Returns the result of `removal`, which a dry run only planned when
    /// `planned` is set.
    pub(crate) fn new(removal: &Removal, planned: bool) -> Self {
        Self {
            name: removal.workspace.name.to_string(),
            path: removal.workspace.path.display().to_string(),
            directory: removal.directory.into(),
            checkouts: removal
                .checkouts
                .iter()
                .map(CheckoutRemovalRow::from)
                .collect(),
            force: removal.allow.discard_changes,
            delete_unmerged: removal.allow.delete_unmerged,
            disposable: removal.disposable.clone(),
            discarded: removal.discarded.clone(),
            planned,
        }
    }
}

/// Prints one sentence for the workspace, one for each checkout, and one
/// for the record, in the future tense for a dry run.
impl fmt::Display for RemovalResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (removes, deletes, keeps, discards) = if self.planned {
            (
                "would remove",
                "would delete",
                "would keep",
                "would discard",
            )
        } else {
            ("removed", "deleted", "kept", "discarded")
        };
        for path in &self.discarded {
            writeln!(formatter, "mxv {discards} {path}.")?;
        }
        for path in &self.disposable {
            writeln!(
                formatter,
                "mxv {deletes} the ignored build directory {path}."
            )?;
        }
        for checkout in &self.checkouts {
            match checkout.checkout {
                DirectoryFate::Removed | DirectoryFate::Kept => {
                    writeln!(formatter, "mxv {removes} the checkout {}.", checkout.path)?;
                }
                DirectoryFate::Missing => writeln!(
                    formatter,
                    "The checkout {} was already gone.",
                    checkout.path
                )?,
            }
            if checkout.stray_commits > 0 {
                writeln!(
                    formatter,
                    "mxv {deletes} {} that no branch holds from the checkout {}.",
                    commits(checkout.stray_commits),
                    checkout.path
                )?;
            }
            match checkout.branch_fate {
                BranchState::Deleted => {
                    writeln!(formatter, "mxv {deletes} the branch {}.", checkout.branch)?
                }
                BranchState::KeptUnmerged => writeln!(
                    formatter,
                    "mxv {keeps} the branch {}, because {}. `{}` deletes it.",
                    checkout.branch,
                    unmerged(checkout.unmerged_commits),
                    self.deletion(checkout)
                )?,
                BranchState::KeptMoved => writeln!(
                    formatter,
                    "mxv {keeps} the branch {}, because it moved after mxv planned the removal, so it may hold work that mxv did not show. `git -C {} log {}` shows its commits.",
                    checkout.branch, checkout.repository, checkout.branch
                )?,
                BranchState::DeletedUnmerged => writeln!(
                    formatter,
                    "mxv {deletes} the branch {}, although {}.",
                    checkout.branch,
                    unmerged(checkout.unmerged_commits)
                )?,
                BranchState::Missing => writeln!(
                    formatter,
                    "The branch {} was already gone.",
                    checkout.branch
                )?,
            }
        }
        match self.directory {
            DirectoryFate::Removed => {
                writeln!(formatter, "mxv {removes} the directory {}.", self.path)?
            }
            DirectoryFate::Kept => writeln!(
                formatter,
                "mxv {keeps} the directory {}, because it holds other files.",
                self.path
            )?,
            DirectoryFate::Missing => {
                writeln!(formatter, "The directory {} was already gone.", self.path)?
            }
        }
        write!(
            formatter,
            "mxv {deletes} the record of the workspace {}.",
            self.name
        )
    }
}

impl RemovalResult {
    /// Returns the command line that deletes the branch of `checkout`,
    /// which the removal keeps because it holds unmerged commits. Before the
    /// removal, it is the same removal with `--delete-unmerged`. After it,
    /// the record is gone, so it is the git command that deletes the
    /// branch.
    fn deletion(&self, checkout: &CheckoutRemovalRow) -> String {
        if self.planned {
            remove_line(
                &self.name,
                Allow {
                    discard_changes: self.force,
                    delete_unmerged: true,
                },
            )
        } else {
            format!(
                "git -C {} branch -D {}",
                checkout.repository, checkout.branch
            )
        }
    }
}

/// Returns `count` commits as words, such as `1 commit` or `3 commits`.
fn commits(count: u32) -> String {
    if count == 1 {
        "1 commit".to_owned()
    } else {
        format!("{count} commits")
    }
}

/// One process of a workspace, as `up`, `down`, `restart` and `status`
/// print it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ProcessRow {
    /// The process's name in the manifest.
    name: String,
    /// `running`, `exited` when it ended on its own or something other
    /// than `mxv down` stopped it, or `stopped`.
    state: StateName,
    /// The process id, which is also its process group's id, while it
    /// runs.
    pid: Option<u32>,
    /// The exit code of an exited process, or null when a signal ended it
    /// or it has not exited.
    code: Option<i32>,
    /// The number of the signal that ended an exited process, such as 15
    /// for SIGTERM, or null.
    signal: Option<i32>,
    /// The ports that the process receives, as `VARIABLE=port`.
    ports: Vec<String>,
    /// The process's log file, which `mxv logs` prints.
    log: String,
}

/// The state of a process, as a row prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StateName {
    /// The process runs.
    Running,
    /// The process ended.
    Exited,
    /// The process is not started.
    Stopped,
}

impl Row for ProcessRow {
    const COLUMNS: &'static [&'static str] = &["name", "state", "pid", "code", "signal", "ports"];
    const LONG_COLUMNS: &'static [&'static str] = &["log"];
    // A person copies the name into `mxv logs`.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];

    /// An exited process reports a problem unless it exited with code 0.
    fn is_problem(&self) -> bool {
        self.state == StateName::Exited && self.code != Some(0)
    }
}

impl From<&ProcessStatus> for ProcessRow {
    fn from(status: &ProcessStatus) -> Self {
        let (state, pid, code, signal) = match status.state {
            ProcessState::Running { pid } => (StateName::Running, Some(pid), None, None),
            ProcessState::Exited { code, signal } => (StateName::Exited, None, code, signal),
            ProcessState::Stopped => (StateName::Stopped, None, None, None),
        };
        Self {
            name: status.name.clone(),
            state,
            pid,
            code,
            signal,
            ports: status
                .ports
                .iter()
                .map(|(variable, number)| format!("{variable}={number}"))
                .collect(),
            log: status.log.display().to_string(),
        }
    }
}

/// One line of a process's log, as `mxv logs` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct LogLine {
    /// The line's text, without its line feed.
    line: String,
}

impl LogLine {
    /// Returns the log line `line`.
    pub(crate) const fn new(line: String) -> Self {
        Self { line }
    }
}

impl Row for LogLine {
    const COLUMNS: &'static [&'static str] = &["line"];
    const LINE: Option<&'static str> = Some("<LINE>");

    fn text_line(&self) -> Option<String> {
        Some(self.line.clone())
    }
}

/// One named port of a workspace and the host port that it holds, as
/// `mxv ports reset` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PortRow {
    /// The port's name in the manifest.
    name: String,
    /// The host port that the workspace holds for it.
    port: u16,
}

impl PortRow {
    /// Returns the row of the port `name` and its host port `port`.
    pub(crate) const fn new(name: String, port: u16) -> Self {
        Self { name, port }
    }
}

impl Row for PortRow {
    const COLUMNS: &'static [&'static str] = &["name", "port"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

/// Returns why a branch counts as unmerged: the number of its commits that
/// are not on its base, or that its base is gone.
fn unmerged(count: Option<u32>) -> String {
    match count {
        Some(1) => "it holds 1 commit that is not on its base".to_owned(),
        Some(count) => format!("it holds {count} commits that are not on its base"),
        None => "its base is gone, so mxv cannot tell whether its commits are merged".to_owned(),
    }
}
