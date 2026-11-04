//! The rows and results that the workspace verbs print.

use std::fmt;

use crumbkit::Row;
use multiverse::{BranchFate, Checkout, CheckoutRemoval, Fate, Removal, Workspace};
use schemars::JsonSchema;
use serde::Serialize;

/// One workspace, as `new`, `list` and `show` print it.
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
    /// The remote branch that the branch started from, such as `origin/main`.
    base: String,
    /// The commit that the branch started at, or null in a dry run, which
    /// does not fetch.
    commit: Option<String>,
}

impl Row for WorkspaceRow {
    const COLUMNS: &'static [&'static str] = &["name", "branch", "path"];
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
    /// Whether the removal was forced with `--force`.
    forced: bool,
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
    /// What happens to the branch: `deleted`, `kept` when it holds commits
    /// that are not on its base, or `missing` when it was already gone.
    branch_fate: BranchState,
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
#[serde(rename_all = "lowercase")]
pub(crate) enum BranchState {
    /// The branch is deleted.
    Deleted,
    /// The branch stays, because it holds commits that are not on its base.
    Kept,
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

impl From<BranchFate> for BranchState {
    fn from(fate: BranchFate) -> Self {
        match fate {
            BranchFate::Deleted => Self::Deleted,
            BranchFate::KeptUnmerged => Self::Kept,
            BranchFate::Missing => Self::Missing,
        }
    }
}

impl From<&CheckoutRemoval> for CheckoutRemovalRow {
    fn from(removal: &CheckoutRemoval) -> Self {
        Self {
            name: removal.checkout.name.clone(),
            path: removal.checkout.path.display().to_string(),
            checkout: removal.directory.into(),
            branch: removal.checkout.branch.clone(),
            branch_fate: removal.branch.into(),
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
            forced: removal.forced,
            planned,
        }
    }
}

/// Prints one sentence for the workspace, one for each checkout, and one
/// for the record, in the future tense for a dry run.
impl fmt::Display for RemovalResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (removes, deletes, keeps) = if self.planned {
            ("would remove", "would delete", "would keep")
        } else {
            ("removed", "deleted", "kept")
        };
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
            match checkout.branch_fate {
                BranchState::Deleted => {
                    writeln!(formatter, "mxv {deletes} the branch {}.", checkout.branch)?
                }
                BranchState::Kept => writeln!(
                    formatter,
                    "mxv {keeps} the branch {}, because it holds commits that are not on its base. `--force` deletes it.",
                    checkout.branch
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
