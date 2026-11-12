//! The rows and results that the workspace verbs print.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use crumbkit::{Age, Children, Row};
use multiverse::health::{Broken, Problem, Repair};
use multiverse::{
    Allow, Archive, Backend, BranchFate, Checkout, CheckoutRemoval, Cloned, DiskUsage,
    DisposableSource, Fate, IgnoredEntry, Kind, MANIFEST_FILE, Manifest, ManifestSource,
    ProcessNote, ProcessState, ProcessStatus, Removal, Workspace, WorkspaceKind,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::environment::{Root, RootSource};
use crate::failures::remove_line;
use crate::run_failures::RunFailure;
use crate::units::Size;

#[cfg(test)]
mod tests;

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
    /// Which manifest the workspace's commands read: `workspace` for the
    /// workspace manifest above the checkout, `repository` for the
    /// checkout's own `mxv.toml`, `merged` for the files of a workspace
    /// with several checkouts, or `none` for a bare workspace. Null in a
    /// row whose record mxv cannot read.
    manifest: Option<ManifestRow>,
    /// The path of the manifest that the workspace's commands read, or
    /// null when they read none. For a merged manifest, it is the
    /// workspace manifest when it exists, or else the first checkout's
    /// `mxv.toml`.
    manifest_path: Option<String>,
    /// Every file that the workspace's commands read: the one manifest of
    /// a workspace with one checkout, or the workspace manifest and each
    /// checkout's `mxv.toml` that exist in a workspace with several.
    manifest_paths: Vec<String>,
    /// The workspace's checkouts, one for each repository.
    checkouts: Vec<CheckoutRow>,
    /// What is wrong with the workspace or its archive, or empty when
    /// nothing is: a record that mxv cannot read, such as one that is not
    /// JSON or that a newer mxv wrote, an archive that stopped before it
    /// finished, one that stopped and whose pins are gone, an older archive
    /// beside a newer workspace of the same name, which only the archive
    /// listing shows, a restore that stopped before it deleted the archive,
    /// a workspace whose checkout directories are all gone, or a size that
    /// mxv cannot measure. A row of a record that mxv cannot read
    /// names the record in `name`, a workspace record without `.json` and
    /// an archive record by its directory, and leaves the other fields
    /// empty.
    problem: String,
    /// The command that repairs the problem, such as
    /// `mxv archive -w old-api` or `mxv repair old-api`, what the person
    /// does by hand when no command can, or empty when no repair is known.
    repair: String,
    /// The host port that the workspace holds for each named port of its
    /// manifest, which the first `mxv up` allocates.
    ports: BTreeMap<String, u16>,
    /// The bytes that the workspace's directory takes on disk, with each
    /// file that has several hard links counted once and no symbolic link
    /// followed, or null when `--sizes` was not given.
    size: Option<Size>,
    /// How long ago the workspace was archived, or null in a row of a
    /// workspace that is not archived.
    archived: Option<Age>,
    /// The number of commits of an archived workspace that exist only on
    /// this machine, or null in a row of a workspace that is not archived.
    local_only: Option<u32>,
    /// The number of ignored files and links, such as `.env`, that the
    /// archive keeps in its private tar, or null in a row of a workspace
    /// that is not archived.
    ignored: Option<u32>,
    /// The bytes that the ignored files and links in the archive's private
    /// tar take, or null in a row of a workspace that is not archived.
    ignored_bytes: Option<Size>,
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
    /// Which manifest the workspace's commands read: `workspace` for the
    /// workspace manifest above the checkout, `repository` for the
    /// checkout's own `mxv.toml`, `merged` for the files of a workspace
    /// with several checkouts, or `none` for a bare workspace.
    manifest: ManifestRow,
    /// The path of the manifest that the workspace's commands read, or
    /// null when they read none. For a merged manifest, it is the
    /// workspace manifest when it exists, or else the first checkout's
    /// `mxv.toml`.
    manifest_path: Option<String>,
    /// Every file that the workspace's commands read: the one manifest of
    /// a workspace with one checkout, or the workspace manifest and each
    /// checkout's `mxv.toml` that exist in a workspace with several.
    manifest_paths: Vec<String>,
    /// The workspace's checkouts, one for each repository.
    checkouts: Vec<CheckoutRow>,
    /// The host port that the workspace holds for each named port of its
    /// manifest, which the first `mxv up` allocates.
    ports: BTreeMap<String, u16>,
}

/// Which manifest a workspace's commands read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ManifestRow {
    /// The workspace manifest, `<workspace>/mxv.toml`, above the checkout.
    Workspace,
    /// The checkout's own `mxv.toml`.
    Repository,
    /// The workspace manifest and each checkout's own `mxv.toml`, merged,
    /// in a workspace with several checkouts.
    Merged,
    /// Neither file, so the workspace is bare.
    None,
}

impl From<ManifestSource> for ManifestRow {
    fn from(source: ManifestSource) -> Self {
        match source {
            ManifestSource::Workspace => Self::Workspace,
            ManifestSource::Repository => Self::Repository,
            ManifestSource::Merged => Self::Merged,
            ManifestSource::None => Self::None,
        }
    }
}

impl fmt::Display for ManifestRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Workspace => "workspace",
            Self::Repository => "repository",
            Self::Merged => "merged",
            Self::None => "none",
        })
    }
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
    const LONG_COLUMNS: &'static [&'static str] =
        &["kind", "manifest", "manifest_path", "repositories"];
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
            manifest: row.manifest.unwrap_or(ManifestRow::None),
            manifest_path: row.manifest_path,
            manifest_paths: row.manifest_paths,
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
        "archived",
        "local_only",
        "ignored",
        "ignored_bytes",
        "ports",
        "problem",
        "repair",
    ];
    const PROBLEM: Option<&'static str> = Some("problem");
    const LONG_COLUMNS: &'static [&'static str] =
        &["kind", "manifest", "manifest_path", "repositories"];
    // A person copies the name and the repair into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name", "repair"];
}

/// Returns the text of the problem of `broken`.
fn problem_text(broken: &Broken) -> String {
    match &broken.problem {
        Problem::Unreadable { reason } => reason.clone(),
        Problem::ArchiveUnfinished => "the archive stopped before it finished".to_owned(),
        Problem::ArchivePinsGone => {
            "the archive stopped before it finished, and its pins are gone".to_owned()
        }
        Problem::ArchiveBesideNewerWorkspace => {
            format!("an older archive beside a newer workspace {}", broken.name)
        }
        Problem::RenameUnfinished => {
            "a repair stopped while it moved the archive to its dated name".to_owned()
        }
        Problem::RestoreUnfinished => {
            "the restore stopped before it deleted the archive".to_owned()
        }
        Problem::CheckoutsGone => "every checkout directory is gone".to_owned(),
    }
}

/// Returns the command of `repair`, or what the person does by hand.
pub(crate) fn repair_text(repair: &Repair) -> String {
    match repair {
        Repair::Archive(name) => format!("mxv archive -w {name}"),
        Repair::Remove(name) => format!("mxv remove {name}"),
        Repair::Repair(name) => format!("mxv repair {name}"),
        Repair::MoveAside(path) => format!(
            "move {} aside by hand; mxv never deletes a record it cannot read",
            path.display()
        ),
    }
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
        let inputs = workspace.manifest_inputs();
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
            manifest: Some(inputs.source().into()),
            manifest_path: inputs.chosen().map(|path| path.display().to_string()),
            manifest_paths: inputs
                .files()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            checkouts: workspace.checkouts.iter().map(CheckoutRow::from).collect(),
            problem: String::new(),
            repair: String::new(),
            ports: workspace.ports.clone(),
            size: None,
            archived: None,
            local_only: None,
            ignored: None,
            ignored_bytes: None,
            disposable: None,
        }
    }
}

impl WorkspaceRow {
    /// Returns the row of `broken` alone, such as a record that mxv cannot
    /// read, with every field but the name, the problem and the repair
    /// empty.
    pub(crate) fn broken(broken: &Broken) -> Self {
        Self {
            name: broken.name.clone(),
            branch: String::new(),
            path: String::new(),
            repositories: Vec::new(),
            kind: None,
            manifest: None,
            manifest_path: None,
            manifest_paths: Vec::new(),
            checkouts: Vec::new(),
            problem: String::new(),
            repair: String::new(),
            ports: BTreeMap::new(),
            size: None,
            archived: None,
            local_only: None,
            ignored: None,
            ignored_bytes: None,
            disposable: None,
        }
        .with_broken(broken)
    }

    /// Returns the row with the problem and the repair of `broken`.
    pub(crate) fn with_broken(mut self, broken: &Broken) -> Self {
        self.problem = problem_text(broken);
        self.repair = broken.repair.as_ref().map(repair_text).unwrap_or_default();
        self
    }

    /// Returns the row with the sizes of `usage`.
    pub(crate) const fn with_usage(mut self, usage: &DiskUsage) -> Self {
        self.size = Some(Size(usage.bytes));
        self.disposable = Some(Size(usage.disposable_bytes));
        self
    }

    /// Returns the row of an archived workspace, archived `age` ago, with
    /// `local_only` commits that exist only on this machine and the
    /// `ignored` files and links in the archive's private tar.
    pub(crate) fn archived(
        mut self,
        age: Age,
        local_only: usize,
        ignored: &[IgnoredEntry],
    ) -> Self {
        self.archived = Some(age);
        self.local_only = Some(u32::try_from(local_only).unwrap_or(u32::MAX));
        self.ignored = Some(u32::try_from(ignored.len()).unwrap_or(u32::MAX));
        self.ignored_bytes = Some(Size(ignored.iter().map(|entry| entry.size).sum()));
        self
    }

    /// Returns whether the row has a problem.
    pub(crate) const fn has_problem(&self) -> bool {
        !self.problem.is_empty()
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
    /// `mxv.toml` declares, such as an invalid manifest, or why its paths
    /// wait for a confirmation, and each table of the manifest that mxv
    /// ignores although its name is close to one of mxv's own, or null.
    warnings: Option<String>,
    /// Each local AWS service of the workspace's manifest, by name, with
    /// the variables that a process that needs it receives, such as
    /// `AWS_ENDPOINT_URL_DYNAMODB` and the service's own `env`. A value
    /// holds `<port>` until the first `mxv up` gives the service's backend
    /// a host port. No value is a secret: the access keys are the literal
    /// `test`.
    services: BTreeMap<String, BTreeMap<String, String>>,
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
    /// A glob of the workspace manifest, which the person wrote for the
    /// workspace.
    Workspace,
    /// A glob of the repository's own `mxv.toml`, and nothing else.
    Manifest,
}

impl From<DisposableSource> for SourceRow {
    fn from(source: DisposableSource) -> Self {
        match source {
            DisposableSource::BuiltIn => Self::BuiltIn,
            DisposableSource::Config => Self::Config,
            DisposableSource::Workspace => Self::Workspace,
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
    const LONG_COLUMNS: &'static [&'static str] = &[
        "kind",
        "manifest",
        "manifest_path",
        "repositories",
        "disposable_paths",
        "kept_paths",
        "services",
    ];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

impl ShowRow {
    /// Returns the row with the variables of each service of `manifest`,
    /// rendered with the workspace's host ports.
    pub(crate) fn with_services(mut self, workspace: &Workspace, manifest: &Manifest) -> Self {
        self.services = manifest.service_listing(&workspace.ports);
        self
    }

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
            services: BTreeMap::new(),
        }
    }

    /// Returns the row with `warnings` after the warnings it holds.
    pub(crate) fn with_warnings(mut self, warnings: &[String]) -> Self {
        let mut all: Vec<String> = self.warnings.into_iter().collect();
        all.extend_from_slice(warnings);
        self.warnings = joined(&all);
        self
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
    /// The file that `new` copied into the workspace manifest: the
    /// `--manifest` file, or the template whose `[template]` table lists
    /// the repository's origin, or null when it copied none.
    manifest_from: Option<String>,
    /// The sentences, joined with `; `, for each template that `new`
    /// skipped because it cannot use it, and for each table of the copied
    /// manifest that mxv ignores although its name is close to one of
    /// mxv's own, or null.
    warnings: Option<String>,
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
        "manifest",
        "manifest_from",
        "warnings",
    ];
    const LONG_COLUMNS: &'static [&'static str] = &["kind", "manifest_path", "repositories"];
    // A person copies the name into the next command.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];
}

impl NewRow {
    /// Returns the row of `workspace` with `root`, the root that the run
    /// used, and the directories that `cloned` names.
    ///
    /// `copied` is the file that `new` copied into the workspace manifest,
    /// if any. A dry run writes no file, so the row then names the manifest
    /// that the workspace would read: the workspace manifest when `new`
    /// would copy one, or else the repository's `mxv.toml` when the
    /// repository has one.
    pub(crate) fn new(
        workspace: &Workspace,
        root: &Root,
        cloned: Cloned,
        copied: Option<&Path>,
    ) -> Self {
        let mut placed = PlacedRow::new(workspace, root);
        if workspace.has_several_checkouts() {
            // A dry run writes no file, so the row names the files that
            // the merged manifest would read.
            let mut paths: Vec<std::path::PathBuf> = Vec::new();
            if copied.is_some() {
                paths.push(workspace.manifest_file());
            }
            for checkout in &workspace.checkouts {
                if checkout.source.join(MANIFEST_FILE).is_file() {
                    paths.push(checkout.path.join(MANIFEST_FILE));
                }
            }
            placed.manifest = if paths.is_empty() {
                ManifestRow::None
            } else {
                ManifestRow::Merged
            };
            placed.manifest_path = paths.first().map(|path| path.display().to_string());
            placed.manifest_paths = paths
                .iter()
                .map(|path| path.display().to_string())
                .collect();
        } else if copied.is_some() {
            placed.manifest = ManifestRow::Workspace;
            placed.manifest_path = Some(workspace.manifest_file().display().to_string());
            placed.manifest_paths = placed.manifest_path.iter().cloned().collect();
        } else if placed.manifest == ManifestRow::None
            && let Some(checkout) = workspace.manifest_checkout()
            && checkout.source.join(MANIFEST_FILE).is_file()
        {
            placed.manifest = ManifestRow::Repository;
            placed.manifest_path = Some(checkout.path.join(MANIFEST_FILE).display().to_string());
            placed.manifest_paths = placed.manifest_path.iter().cloned().collect();
        }
        Self {
            placed,
            cloned: cloned.paths,
            clone_warning: cloned.warning,
            manifest_from: copied.map(|path| path.display().to_string()),
            warnings: None,
        }
    }

    /// Returns the row with `warnings`.
    pub(crate) fn with_warnings(mut self, warnings: &[String]) -> Self {
        self.warnings = joined(warnings);
        self
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
    /// `--delete-volumes`, which are its named volumes.
    volumes: Vec<String>,
    /// The paths of `disposable` that only the repository's own `mxv.toml`
    /// declares disposable, which mxv deletes only after the person
    /// confirmed them at the prompt or passed `--force`.
    manifest_paths: Vec<String>,
    /// Why a checkout's disposable set holds fewer globs than its
    /// `mxv.toml` declares, such as an invalid manifest, or why its paths
    /// wait for a confirmation.
    warnings: Vec<String>,
    /// The workspace manifest that the removal deletes, which a person may
    /// have edited by hand, or null when the workspace has none.
    manifest_deleted: Option<String>,
    /// The references that `mxv remove --archived` deleted, or empty for
    /// the removal of a workspace.
    pins: Vec<String>,
    /// Whether the result reports the deletion of an archive.
    #[serde(skip)]
    of_archive: bool,
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
    /// The number of stash entries of a clone, which `--force` allowed
    /// the removal to delete, or 0 for a worktree.
    stashes: u32,
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
            stashes: u32::try_from(removal.stashes.len()).unwrap_or(u32::MAX),
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
            manifest_deleted: removal
                .workspace_manifest
                .as_ref()
                .map(|path| path.display().to_string()),
            pins: Vec::new(),
            of_archive: false,
            planned,
        }
    }

    /// Returns the result of deleting `archive` and `volumes`, which a dry
    /// run only planned when `planned` is set.
    pub(crate) fn of_archive(
        archive: &Archive,
        volumes: Vec<String>,
        warnings: Vec<String>,
        planned: bool,
    ) -> Self {
        Self {
            name: archive.name.to_string(),
            path: archive.path.display().to_string(),
            directory: DirectoryFate::Missing,
            checkouts: Vec::new(),
            force: false,
            delete_unmerged: false,
            disposable: Vec::new(),
            discarded: Vec::new(),
            containers: Vec::new(),
            volumes,
            manifest_paths: Vec::new(),
            warnings,
            manifest_deleted: None,
            pins: archive
                .checkouts
                .iter()
                .map(|checkout| checkout.pin.clone())
                .collect(),
            of_archive: true,
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

    /// Returns this result with `warnings` added after its own.
    pub(crate) fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings.extend(warnings);
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
        if self.of_archive {
            for pin in &self.pins {
                writeln!(formatter, "mxv {deletes} the pin {pin}.")?;
            }
            for volume in &self.volumes {
                writeln!(
                    formatter,
                    "mxv {deletes} the docker volume {volume} and the data in it."
                )?;
            }
            return write!(formatter, "mxv {deletes} the archive {}.", self.name);
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
        if let Some(path) = &self.manifest_deleted {
            writeln!(formatter, "mxv {deletes} the workspace manifest {path}.")?;
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
            if checkout.stashes > 0 {
                writeln!(
                    formatter,
                    "mxv {deletes} {} from the checkout {}.",
                    stashes(checkout.stashes),
                    checkout.path
                )?;
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
/// Returns `count` stash entries in words, such as `1 stash entry`.
pub(crate) fn stashes(count: u32) -> String {
    if count == 1 {
        "1 stash entry".to_owned()
    } else {
        format!("{count} stash entries")
    }
}

fn commits(count: u32) -> String {
    if count == 1 {
        "1 commit".to_owned()
    } else {
        format!("{count} commits")
    }
}

/// One unit of a workspace, as `up`, `down`, `restart`
/// and `status` print it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ProcessRow {
    /// The unit's name in the manifest.
    name: String,
    /// `process` or `container`.
    kind: KindName,
    /// `running`, `exited` when it ended on its own or something other
    /// than `mxv down` stopped it, `created` for a container that docker
    /// created and did not start, `stopped`, `starting` while it is not
    /// ready yet, `stopping`, `restarting` while docker restarts a
    /// container, or `unknown` when mxv cannot tell.
    state: StateName,
    /// Whether the unit has been ready since it last started: true
    /// once its readiness check passed, or once a unit without one
    /// outlasted its start grace, and false while it runs and has not,
    /// such as after its keeper or docker restarted it. It is null when it
    /// does not run.
    ready: Option<bool>,
    /// The process id, which is also its process group's id, while a
    /// process runs, and null for a container.
    pid: Option<u32>,
    /// The exit code of an exited unit, or null when a signal ended it
    /// or it has not exited. A container's code is the one docker reports,
    /// such as 137 for a container that SIGKILL ended.
    code: Option<i32>,
    /// The number of the signal that ended an exited unit, such as 15
    /// for SIGTERM, or null.
    signal: Option<i32>,
    /// Why mxv cannot tell the state of an `unknown` unit, or why it
    /// does not know the exit code of an exited unit, or null when it
    /// knows.
    reason: Option<String>,
    /// The ports that a process receives, as `VARIABLE=port`, or that a
    /// container publishes, as `port-name=port`.
    ports: Vec<String>,
    /// The unit's log file, which `mxv logs` prints, or null for a
    /// container, whose output docker keeps and `mxv logs` also prints.
    log: Option<String>,
    /// Whether phase runs a host process under its sandbox, as phase
    /// describes it: `seatbelt (partial: <what it leaves open>)` on macOS,
    /// or `unsandboxed (<reason>)`, such as `unsandboxed (its app declares
    /// no permissions)` for a process whose manifest has no `permissions`
    /// table. It is null for a container and for a process that phase
    /// does not run.
    sandbox: Option<String>,
    /// What `up` and `restart` found about the unit beside its state:
    /// a warning when the unit's running container still has its
    /// earlier secret files, because `up` could not rewrite them, an info
    /// line when `up` rewrote them, and an info line when `up` restarted a
    /// running host process because its part of the plan changed. Every
    /// other command prints none.
    notes: Vec<NoteRow>,
}

/// Something that `up` or `restart` found about a unit, as its row
/// prints it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct NoteRow {
    /// `warning` when something did not happen as asked, or `info`.
    level: NoteLevel,
    /// phase's sentence, which names the unit and holds no value.
    message: String,
}

/// How much a note matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum NoteLevel {
    /// Something did not happen as asked, such as a rewrite of secret
    /// files that failed.
    Warning,
    /// Something happened that the person may want to know, such as a
    /// rewrite of secret files.
    Info,
}

impl From<&ProcessNote> for NoteRow {
    fn from(note: &ProcessNote) -> Self {
        Self {
            level: if note.is_warning() {
                NoteLevel::Warning
            } else {
                NoteLevel::Info
            },
            message: note.message().to_owned(),
        }
    }
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

/// The state of a unit, as a row prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StateName {
    /// The unit runs.
    Running,
    /// The unit ended.
    Exited,
    /// docker created the container and did not start it.
    Created,
    /// The unit is not started.
    Stopped,
    /// The unit started and is not ready yet.
    Starting,
    /// The unit is stopping.
    Stopping,
    /// docker is restarting the container.
    Restarting,
    /// mxv cannot tell the unit's state.
    Unknown,
}

impl Row for ProcessRow {
    const COLUMNS: &'static [&'static str] = &[
        "name", "kind", "state", "ready", "pid", "code", "signal", "reason", "ports", "sandbox",
    ];
    const LONG_COLUMNS: &'static [&'static str] = &["log"];
    const CHILDREN: Option<Children> = Some(Children::field("notes").help(
        "one indented line under a unit for each note of `up` or `restart`, which starts with `warning:` or `info:`.",
    ));
    // A person copies the name into `mxv logs`.
    const UNCUT_COLUMNS: &'static [&'static str] = &["name"];

    /// An exited unit reports a problem unless it exited with code 0.
    fn is_problem(&self) -> bool {
        self.state == StateName::Exited && self.code != Some(0)
    }

    fn child_lines(&self) -> Vec<String> {
        self.notes
            .iter()
            .map(|note| match note.level {
                NoteLevel::Warning => format!("warning: {}", note.message),
                NoteLevel::Info => format!("info: {}", note.message),
            })
            .collect()
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
            } => (
                StateName::Exited,
                None,
                code,
                signal,
                reason.map(|reason| reason.describe()),
            ),
            ProcessState::Created => (StateName::Created, None, None, None, None),
            ProcessState::Stopped => (StateName::Stopped, None, None, None, None),
            ProcessState::Starting { pid } => (StateName::Starting, pid, None, None, None),
            ProcessState::Stopping { pid } => (StateName::Stopping, pid, None, None, None),
            ProcessState::Restarting => (StateName::Restarting, None, None, None, None),
            ProcessState::Unknown { reason } => {
                (StateName::Unknown, None, None, None, Some(reason))
            }
        };
        Self {
            name: status.name.clone(),
            kind: match status.kind {
                Kind::Process => KindName::Process,
                Kind::Container => KindName::Container,
            },
            state,
            ready: status.ready,
            pid,
            code,
            signal,
            reason: reason.map(str::to_owned),
            ports: status
                .ports
                .iter()
                .map(|(variable, number)| format!("{variable}={number}"))
                .collect(),
            log: status.log.as_ref().map(|log| log.display().to_string()),
            sandbox: status.sandbox.clone(),
            notes: status.notes.iter().map(NoteRow::from).collect(),
        }
    }
}

/// One line of a unit's log, as `mxv logs` prints it.
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

/// What `mxv status` says above its rows: which manifest the workspace
/// reads, whether it could be read, and its warnings.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct StatusSummary {
    /// Which manifest the workspace's commands read: `workspace` for the
    /// workspace manifest above the checkout, `repository` for the
    /// checkout's own `mxv.toml`, `merged` for the files of a workspace
    /// with several checkouts, or `none` for a bare workspace.
    manifest: ManifestRow,
    /// The path of the manifest that the workspace's commands read, or
    /// null when they read none. For a merged manifest, it is the
    /// workspace manifest when it exists, or else the first checkout's
    /// `mxv.toml`.
    manifest_path: Option<String>,
    /// Every file that the workspace's commands read.
    manifest_paths: Vec<String>,
    /// The manifest's error when it cannot be read or does not parse, in
    /// which case the rows come from the run records alone, or null.
    manifest_error: Option<String>,
    /// The code of `manifest_error`, such as `mxv::manifest::unreadable`
    /// for a symbolic link that leads nowhere, which `mxv up` fails with,
    /// or null.
    manifest_error_code: Option<String>,
    /// One sentence for each table of the manifest that mxv ignores
    /// although its name is close to one of mxv's own, such as
    /// `[proceses.web]`.
    warnings: Vec<String>,
    /// Each checkout of the workspace, with the branch and commit it has
    /// checked out and how many of its entries are uncommitted or
    /// untracked.
    checkouts: Vec<CheckoutStateRow>,
    /// Whether `mxv pin` keeps `mxv reclaim` from stopping the workspace.
    pinned: bool,
    /// What `mxv reclaim` stopped, which `mxv up` starts again, or null
    /// when reclaim has not stopped the workspace since the last `up` or
    /// `down`.
    reclaimed: Option<ReclaimedRow>,
}

/// What `mxv reclaim` stopped in a workspace, as `mxv status` reports it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ReclaimedRow {
    /// The units that ran when reclaim stopped them.
    processes: Vec<String>,
}

/// One checkout of a workspace, as `mxv status` reports it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CheckoutStateRow {
    /// The checkout's folder, which `mxv cd <NAME> <CHECKOUT>` takes.
    name: String,
    /// The checkout's directory.
    path: String,
    /// The branch that the checkout has checked out, or null for a
    /// detached `HEAD` or a checkout that git cannot read.
    branch: Option<String>,
    /// The commit that the checkout's `HEAD` names, or null.
    commit: Option<String>,
    /// How many entries of the checkout are uncommitted changes or
    /// untracked paths, as `git status --porcelain` counts them, or null
    /// when git cannot read the checkout.
    changes: Option<usize>,
    /// Why git cannot read the checkout, such as a directory that is
    /// gone, or null.
    problem: Option<String>,
}

impl CheckoutStateRow {
    /// Returns the state of `checkout` that `backend` reads.
    pub(crate) fn read(backend: &impl Backend, checkout: &Checkout) -> Self {
        let mut row = Self {
            name: checkout.name.clone(),
            path: checkout.path.display().to_string(),
            branch: None,
            commit: None,
            changes: None,
            problem: None,
        };
        if !checkout.path.is_dir() {
            row.problem = Some("its directory is gone".to_owned());
            return row;
        }
        let head = match backend.head(&checkout.path) {
            Ok(head) => head,
            Err(error) => {
                row.problem = Some(error.to_string());
                return row;
            }
        };
        let own = head.toplevel.as_deref().is_some_and(|toplevel| {
            toplevel.canonicalize().ok() == checkout.path.canonicalize().ok()
        });
        if !own {
            row.problem = Some(match &head.toplevel {
                Some(toplevel) => format!(
                    "it has no .git of its own, so git reads the repository at {}",
                    toplevel.display()
                ),
                None => "it is not a git checkout".to_owned(),
            });
            return row;
        }
        match backend.changes(&checkout.path) {
            Ok(changes) => {
                row.branch = head.branch;
                row.commit = head.commit;
                row.changes = Some(changes.changed.len());
            }
            Err(error) => row.problem = Some(error.to_string()),
        }
        row
    }
}

impl fmt::Display for CheckoutStateRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(problem) = &self.problem {
            return write!(
                formatter,
                "The checkout {} at {} cannot be read: {problem}.",
                self.name, self.path
            );
        }
        let commit = self
            .commit
            .as_deref()
            .map_or("no commit", |commit| commit.get(..12).unwrap_or(commit));
        let changes = self.changes.unwrap_or_default();
        match &self.branch {
            Some(branch) => write!(
                formatter,
                "The checkout {} at {} is on {branch} at {commit}, with {changes} uncommitted or untracked entries.",
                self.name, self.path
            ),
            None => write!(
                formatter,
                "The checkout {} at {} has a detached HEAD at {commit}, with {changes} uncommitted or untracked entries.",
                self.name, self.path
            ),
        }
    }
}

impl StatusSummary {
    /// Returns the summary of a status of `workspace`, whose manifest is
    /// `manifest`, or failed with `error`, and whose checkouts are in the
    /// states `checkouts`.
    pub(crate) fn new(
        workspace: &Workspace,
        manifest: Option<&Manifest>,
        error: Option<&RunFailure>,
        checkouts: Vec<CheckoutStateRow>,
    ) -> Self {
        let inputs = workspace.manifest_inputs();
        Self {
            manifest: inputs.source().into(),
            manifest_path: inputs.chosen().map(|path| path.display().to_string()),
            manifest_paths: inputs
                .files()
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            manifest_error: error.map(ToString::to_string),
            manifest_error_code: error
                .map(|error| crumbkit::Spelling::COLONS.spell(crumbkit::Failure::code(error))),
            warnings: manifest
                .map(|manifest| manifest.warnings.clone())
                .unwrap_or_default(),
            checkouts,
            pinned: workspace.pinned,
            reclaimed: workspace.reclaimed.as_ref().map(|reclaimed| ReclaimedRow {
                processes: reclaimed.processes.clone(),
            }),
        }
    }
}

/// Prints which manifest the workspace reads, one more line when the
/// manifest cannot be read or does not parse, a line for each warning, and
/// a line for each checkout.
impl fmt::Display for StatusSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.manifest, &self.manifest_path) {
            (ManifestRow::Workspace, Some(path)) => {
                write!(formatter, "The manifest is the workspace manifest {path}.")?;
            }
            (ManifestRow::Repository, Some(path)) => {
                write!(formatter, "The manifest is the repository's {path}.")?;
            }
            (ManifestRow::Merged, Some(_)) => {
                write!(
                    formatter,
                    "The manifest merges {}.",
                    self.manifest_paths.join(", ")
                )?;
            }
            _ => formatter.write_str("The workspace has no manifest.")?,
        }
        if let Some(error) = &self.manifest_error {
            write!(
                formatter,
                "\nThe manifest cannot be used, so the units come from the run records: {error}"
            )?;
        }
        for warning in &self.warnings {
            write!(formatter, "\nWarning: {warning}.")?;
        }
        if let Some(reclaimed) = &self.reclaimed {
            write!(
                formatter,
                "\nmxv reclaim stopped {} to free memory, and mxv up starts them again.",
                reclaimed.processes.join(", ")
            )?;
        }
        if self.pinned {
            formatter.write_str("\nThe workspace is pinned, so mxv reclaim never stops it.")?;
        }
        for checkout in &self.checkouts {
            write!(formatter, "\n{checkout}")?;
        }
        Ok(())
    }
}

/// Returns `texts` joined with `; `, or `None` when there are none, so an
/// empty list prints as an empty cell.
pub(crate) fn joined(texts: &[String]) -> Option<String> {
    (!texts.is_empty()).then(|| texts.join("; "))
}
