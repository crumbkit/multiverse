//! The record of a workspace, which the state store keeps as one JSON file
//! for each workspace.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::name::WorkspaceName;

/// The version of the record form that this version of multiverse writes
/// and reads. A record with a higher version is refused (DUR-3).
pub const RECORD_VERSION: u32 = 1;

/// The version-control system that made a checkout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BackendKind {
    /// git, with the checkout as a git worktree.
    Git,
}

/// One repository's checkout inside a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Checkout {
    /// The repository's name, which is the last component of `source` and
    /// the name of the checkout's directory, such as `inbox`.
    pub name: String,
    /// The version-control system that made the checkout.
    pub backend: BackendKind,
    /// The top-level directory of the repository that the checkout belongs
    /// to.
    pub source: PathBuf,
    /// The checkout's directory.
    pub path: PathBuf,
    /// The branch that the checkout was created on.
    pub branch: String,
    /// The remote branch that the branch started from, such as
    /// `refs/remotes/origin/main`.
    pub base: String,
    /// The commit that the branch started at, or `None` for a planned
    /// checkout that a dry run reports.
    pub commit: Option<String>,
}

/// The record of one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Workspace {
    /// The version of the record form, which is [`RECORD_VERSION`].
    pub version: u32,
    /// The workspace's name.
    pub name: WorkspaceName,
    /// The workspace's directory, which holds its `AGENTS.md` and one
    /// directory for each checkout.
    pub path: PathBuf,
    /// The workspace's checkouts. A workspace made by `mxv new` has one.
    pub checkouts: Vec<Checkout>,
}

impl Workspace {
    /// Returns the record of a workspace named `name` in the directory
    /// `path` with `checkouts`.
    #[must_use]
    pub const fn new(name: WorkspaceName, path: PathBuf, checkouts: Vec<Checkout>) -> Self {
        Self {
            version: RECORD_VERSION,
            name,
            path,
            checkouts,
        }
    }

    /// Returns the path of the workspace's `AGENTS.md`.
    #[must_use]
    pub fn agents_file(&self) -> PathBuf {
        self.path.join(AGENTS_FILE)
    }
}

impl Checkout {
    /// Returns the checkout of the repository `name` at `source`, in the
    /// directory `path`, on `branch` from `base`.
    #[must_use]
    pub fn new(name: String, source: &Path, path: PathBuf, branch: String, base: String) -> Self {
        Self {
            name,
            backend: BackendKind::Git,
            source: source.to_path_buf(),
            path,
            branch,
            base,
            commit: None,
        }
    }
}

/// The name of the file in a workspace's directory that tells an agent how
/// to work there.
pub const AGENTS_FILE: &str = "AGENTS.md";

/// Returns the text of a workspace's `AGENTS.md`, which tells an agent
/// where it is, which repository and branch each checkout holds, and how to
/// list, resume or remove the workspace with `mxv`.
#[must_use]
pub fn agents_text(workspace: &Workspace) -> String {
    let name = workspace.name.as_str();
    let mut text = format!(
        "# Workspace {name}\n\
         \n\
         This directory is the multiverse workspace `{name}`. mxv created it and\n\
         keeps its record. Work inside the checkout below, and commit and push\n\
         from there.\n\
         \n\
         ## Checkouts\n\
         \n"
    );
    for checkout in &workspace.checkouts {
        text.push_str(&format!(
            "- `{}/` is a git worktree of `{}` on the branch `{}`, which started from `{}`.\n",
            checkout.name,
            checkout.source.display(),
            checkout.branch,
            checkout.base,
        ));
    }
    text.push_str(&format!(
        "\n\
         ## Commands\n\
         \n\
         - `mxv show {name}` prints this workspace's record.\n\
         - `mxv list` lists every workspace.\n\
         - `cd \"$(mxv cd {name})\"` returns to this directory.\n\
         - `mxv remove {name}` removes the checkouts, their branches when they are\n\
         \x20 merged, and the record. It refuses while a checkout has uncommitted\n\
         \x20 changes, untracked files, ignored files such as `.env`, or commits\n\
         \x20 that no branch holds. `--force` discards the files and keeps unmerged\n\
         \x20 branches; `--delete-unmerged` deletes unmerged commits.\n\
         \n\
         This file sits above the checkouts, so git does not see it.\n"
    ));
    text
}
