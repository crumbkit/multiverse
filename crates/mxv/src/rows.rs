//! The rows and results that the workspace verbs print.

use std::collections::BTreeMap;
use std::fmt;

use crumbkit::Row;
use multiverse::Error;
use multiverse::{
    Allow, BranchFate, Checkout, CheckoutRemoval, Cloned, DiskUsage, DisposableSource, Fate, Kind,
    ProcessState, ProcessStatus, Removal, Workspace, WorkspaceKind,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::environment::{Root, RootSource};
use crate::failures::remove_line;
use crate::units::Size;

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
    /// How the workspace's checkouts are made: `clone` for a clone with a
    /// `.git` of its own, or `worktree` for a git worktree of the
    /// repository, which mxv made before clones. Null in a row whose record
    /// mxv cannot read.
    kind: Option<KindRow>,
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
    /// The bytes that the workspace's directory takes on disk, with each
    /// file that has several hard links counted once and no symbolic link
    /// followed, or null when `--sizes` was not given.
    size: Option<Size>,
    /// The bytes that the workspace's disposable directories take on disk,
    /// such as `target/` and `node_modules/`, which `mxv clean` deletes, or
    /// null when `--sizes` was not given.
    disposable: Option<Size>,
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
    /// How the workspace's checkouts are made: `clone` for a clone with a
    /// `.git` of its own, or `worktree` for a git worktree of the
    /// repository, which mxv made before clones.
    kind: KindRow,
    /// The workspace's checkouts, one for each repository.
    checkouts: Vec<CheckoutRow>,
    /// The host port that the workspace holds for each named port of its
    /// manifest, which the first `mxv up` allocates.
    ports: BTreeMap<String, u16>,
}

/// How a workspace's checkouts are made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KindRow {
    /// A git worktree of the repository, which shares its `.git`.
    Worktree,
    /// A clone of a mirror of the repository's origin, with a `.git` of
    /// its own.
    Clone,
}

impl From<WorkspaceKind> for KindRow {
    fn from(kind: WorkspaceKind) -> Self {
        match kind {
            WorkspaceKind::Worktree => Self::Worktree,
            WorkspaceKind::Clone => Self::Clone,
        }
    }
}

impl Row for PlacedRow {
    const COLUMNS: &'static [&'static str] =
        &["name", "branch", "path", "root", "root_source", "ports"];
    const LONG_COLUMNS: &'static [&'static str] = &["kind", "repositories"];
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
            kind: workspace.kind.into(),
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
    /// The bare mirror whose objects a clone borrows, or null for a
    /// worktree.
    mirror: Option<String>,
}

impl Row for WorkspaceRow {
    const COLUMNS: &'static [&'static str] = &[
        "name",
        "branch",
        "path",
        "size",
        "disposable",
        "ports",
        "problem",
    ];
    const PROBLEM: Option<&'static str> = Some("problem");
    const LONG_COLUMNS: &'static [&'static str] = &["kind", "repositories"];
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
            mirror: checkout
                .mirror
                .as_ref()
                .map(|mirror| mirror.display().to_string()),
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
            kind: Some(workspace.kind.into()),
            checkouts: workspace.checkouts.iter().map(CheckoutRow::from).collect(),
            problem: String::new(),
            ports: workspace.ports.clone(),
            size: None,
            disposable: None,
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
            kind: None,
            checkouts: Vec::new(),
            problem: error.to_string(),
            ports: BTreeMap::new(),
            size: None,
            disposable: None,
        }
    }

    /// Returns the row with the sizes of `usage`.
    pub(crate) const fn with_usage(mut self, usage: &DiskUsage) -> Self {
        self.size = Some(Size(usage.bytes));
        self.disposable = Some(Size(usage.disposable_bytes));
        self
    }

    /// Returns the row with `problem`, such as a measurement that failed.
    pub(crate) fn with_problem(mut self, problem: String) -> Self {
        self.problem = problem;
        self
    }
}

/// One workspace, the workspace root that the run used and its sizes, as
/// `show` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ShowRow {
    /// The workspace and the root.
    #[serde(flatten)]
    placed: PlacedRow,
    /// The bytes that the workspace's directory takes on disk, with each
    /// file that has several hard links counted once and no symbolic link
    /// followed.
    size: Size,
    /// The bytes that the workspace's disposable directories take on disk,
    /// such as `target/` and `node_modules/`, which `mxv clean` deletes.
    disposable: Size,
    /// Each disposable path with its checkout and the bytes it takes.
    disposable_paths: Vec<DisposablePathRow>,
    /// Each path that the disposable set matches and that mxv keeps, with
    /// the reason, such as a directory that holds a `.git` entry.
    kept_paths: Vec<String>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest, or null.
    warnings: Option<String>,
}

/// A disposable path of a checkout and the bytes it takes, as `show
/// --full` prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct DisposablePathRow {
    /// The checkout's name, such as `app`.
    checkout: String,
    /// The path relative to the checkout, such as `target/`.
    path: String,
    /// The bytes that the path takes on disk.
    bytes: Size,
    /// What made the path disposable: `built_in` for a name such as
    /// `target`, `config` for a glob of the config file, or `manifest` for
    /// a glob of the repository's own `mxv.toml` and nothing else.
    source: SourceRow,
}

/// What made a path disposable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceRow {
    /// One of mxv's built-in directory names, such as `target`.
    BuiltIn,
    /// A glob of the person's config file.
    Config,
    /// A glob of the repository's own `mxv.toml`, and nothing else.
    Manifest,
}

impl From<DisposableSource> for SourceRow {
    fn from(source: DisposableSource) -> Self {
        match source {
            DisposableSource::BuiltIn => Self::BuiltIn,
            DisposableSource::Config => Self::Config,
            // A source that a later library adds counts as the least
            // trusted one.
            _ => Self::Manifest,
        }
    }
}

impl Row for ShowRow {
    const COLUMNS: &'static [&'static str] = &[
        "name",
        "branch",
        "path",
        "root",
        "root_source",
        "size",
        "disposable",
        "ports",
        "warnings",
    ];
    const LONG_COLUMNS: &'static [&'static str] =
        &["kind", "repositories", "disposable_paths", "kept_paths"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

impl ShowRow {
    /// Returns the row of `workspace` with `root`, the root that the run
    /// used, and the sizes of `usage`.
    pub(crate) fn new(workspace: &Workspace, root: &Root, usage: &DiskUsage) -> Self {
        Self {
            placed: PlacedRow::new(workspace, root),
            size: Size(usage.bytes),
            disposable: Size(usage.disposable_bytes),
            disposable_paths: usage
                .disposable
                .iter()
                .map(|found| DisposablePathRow {
                    checkout: found.checkout.clone(),
                    path: found.path.clone(),
                    bytes: Size(found.bytes),
                    source: found.source.into(),
                })
                .collect(),
            kept_paths: usage
                .kept
                .iter()
                .map(|kept| format!("{}: {}, because {}", kept.checkout, kept.path, kept.reason))
                .collect(),
            warnings: joined(&usage.warnings),
        }
    }
}

/// The workspace that `new` created, or would create on a dry run, and the
/// ignored directories that it cloned into the checkout.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct NewRow {
    /// The workspace and the root.
    #[serde(flatten)]
    placed: PlacedRow,
    /// Each ignored directory of the repository that `new` cloned into the
    /// checkout, or would clone on a dry run, with the checkout's name in
    /// front, such as `app: web/node_modules/`. A clone shares its disk
    /// with the repository's copy until a file in either changes.
    cloned: Vec<String>,
    /// Why `new` cloned fewer directories than the globs name, such as a
    /// file system that cannot clone, or null. `new` never copies the
    /// directories in full.
    clone_warning: Option<String>,
}

impl Row for NewRow {
    const COLUMNS: &'static [&'static str] = &[
        "name",
        "branch",
        "path",
        "root",
        "root_source",
        "ports",
        "cloned",
        "clone_warning",
    ];
    const LONG_COLUMNS: &'static [&'static str] = &["kind", "repositories"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

impl NewRow {
    /// Returns the row of `workspace` with `root`, the root that the run
    /// used, and the directories that `cloned` names.
    pub(crate) fn new(workspace: &Workspace, root: &Root, cloned: Cloned) -> Self {
        Self {
            placed: PlacedRow::new(workspace, root),
            cloned: cloned.paths,
            clone_warning: cloned.warning,
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
    /// The workspace's docker containers, running or stopped, that the
    /// removal removes.
    containers: Vec<String>,
    /// The docker volumes that the removal deletes because of
    /// `--delete-volumes`, named and anonymous.
    volumes: Vec<String>,
    /// The paths of `disposable` that only the repository's own `mxv.toml`
    /// declares disposable, which mxv deletes only after the person
    /// confirmed them at the prompt or passed `--force`.
    manifest_paths: Vec<String>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest.
    warnings: Vec<String>,
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
            containers: Vec::new(),
            volumes: Vec::new(),
            manifest_paths: removal.manifest.clone(),
            warnings: removal.warnings.clone(),
            planned,
        }
    }

    /// Returns this result with the docker containers that the removal
    /// removes and the volumes that it deletes.
    pub(crate) fn with_docker(mut self, containers: Vec<String>, volumes: Vec<String>) -> Self {
        self.containers = containers;
        self.volumes = volumes;
        self
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
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        for path in &self.discarded {
            writeln!(formatter, "mxv {discards} {path}.")?;
        }
        for path in &self.disposable {
            if self.manifest_paths.contains(path) {
                writeln!(
                    formatter,
                    "mxv {deletes} the ignored directory {path}, which only the repository's mxv.toml declares disposable."
                )?;
            } else {
                writeln!(
                    formatter,
                    "mxv {deletes} the ignored build directory {path}."
                )?;
            }
        }
        for container in &self.containers {
            writeln!(formatter, "mxv {removes} the docker container {container}.")?;
        }
        for volume in &self.volumes {
            writeln!(
                formatter,
                "mxv {deletes} the docker volume {volume} and the data in it."
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

/// One process or container of a workspace, as `up`, `down`, `restart`
/// and `status` print it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ProcessRow {
    /// The process's or container's name in the manifest.
    name: String,
    /// `process` or `container`.
    kind: KindName,
    /// `running`, `exited` when it ended on its own or something other
    /// than `mxv down` stopped it, `created` for a container that docker
    /// created and did not start, or `stopped`.
    state: StateName,
    /// The process id, which is also its process group's id, while a
    /// process runs, and null for a container.
    pid: Option<u32>,
    /// The exit code of an exited process, or null when a signal ended it
    /// or it has not exited. A container's code is the one docker reports,
    /// such as 137 for a container that SIGKILL ended.
    code: Option<i32>,
    /// The number of the signal that ended an exited process, such as 15
    /// for SIGTERM, or null.
    signal: Option<i32>,
    /// Why mxv does not know the exit code of an exited process, or null
    /// when it knows the code or the signal. A process group that ended
    /// before its shell wrote the exit file, such as one that SIGKILL
    /// ended, has a reason.
    reason: Option<String>,
    /// Whether the process group of an exited process still has a member,
    /// such as a child it started in the background, which `mxv down`
    /// stops. It is null while the process runs.
    group_alive: Option<bool>,
    /// The ports that a process receives, as `VARIABLE=port`, or that a
    /// container publishes, as `port-name=port`.
    ports: Vec<String>,
    /// The process's log file, which `mxv logs` prints.
    log: String,
}

/// Whether a row is a process or a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum KindName {
    /// A process that mxv starts on the host.
    Process,
    /// A container that mxv runs with docker.
    Container,
}

/// The state of a process, as a row prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StateName {
    /// The process runs.
    Running,
    /// The process ended.
    Exited,
    /// docker created the container and did not start it.
    Created,
    /// The process is not started.
    Stopped,
}

impl Row for ProcessRow {
    const COLUMNS: &'static [&'static str] = &[
        "name",
        "kind",
        "state",
        "pid",
        "code",
        "signal",
        "reason",
        "group_alive",
        "ports",
    ];
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
        let (state, pid, code, signal, reason) = match status.state {
            ProcessState::Running { pid } => (StateName::Running, pid, None, None, None),
            ProcessState::Exited {
                code,
                signal,
                reason,
            } => (StateName::Exited, None, code, signal, reason),
            ProcessState::Created => (StateName::Created, None, None, None, None),
            ProcessState::Stopped => (StateName::Stopped, None, None, None, None),
        };
        Self {
            name: status.name.clone(),
            kind: match status.kind {
                Kind::Process => KindName::Process,
                Kind::Container => KindName::Container,
            },
            state,
            pid,
            code,
            signal,
            reason: reason.map(|reason| reason.describe().to_owned()),
            group_alive: status.group_alive,
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

/// What `mxv status` says above its rows: whether the manifest parsed.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StatusSummary {
    /// The manifest's error when it does not parse, in which case the rows
    /// come from the run records alone, or null.
    manifest_error: Option<String>,
}

impl StatusSummary {
    /// Returns the summary of a status whose manifest failed with `error`,
    /// or parsed or is missing when `error` is `None`.
    pub(crate) const fn new(manifest_error: Option<String>) -> Self {
        Self { manifest_error }
    }
}

/// Prints one line when the manifest does not parse, and nothing
/// otherwise.
impl fmt::Display for StatusSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.manifest_error {
            Some(error) => write!(
                formatter,
                "The manifest does not parse, so the processes come from the run records: {error}"
            ),
            None => Ok(()),
        }
    }
}

/// Returns `texts` joined with `; `, or `None` when there are none, so an
/// empty list prints as an empty cell.
pub(crate) fn joined(texts: &[String]) -> Option<String> {
    (!texts.is_empty()).then(|| texts.join("; "))
}
