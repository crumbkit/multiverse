//! The record of a workspace, which the state store keeps as one JSON file
//! for each workspace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::name::WorkspaceName;

/// The version of the record form that this version of multiverse writes
/// for a workspace of clones, and the highest that it reads. A record with
/// a higher version is refused (DUR-3). Version 1 is a workspace of git
/// worktrees, which records without a `kind` are, and its record keeps
/// version 1 so that an older mxv still reads it. Version 2 adds `kind`,
/// so an older mxv refuses a workspace of clones rather than treating
/// its checkouts as worktrees.
pub const RECORD_VERSION: u32 = 2;

/// The version of the record form of a workspace of git worktrees.
pub const WORKTREE_RECORD_VERSION: u32 = 1;

/// How a workspace's checkouts are made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceKind {
    /// Each checkout is a git worktree of the person's repository, sharing
    /// its `.git`, hooks, config and branches. mxv made workspaces this way
    /// before it made clones, and a record without a `kind` is one.
    #[default]
    Worktree,
    /// Each checkout is a `git clone --shared` of a bare mirror that mxv
    /// keeps, with a `.git` of its own, so nothing in the checkout can
    /// change the person's repository or another workspace.
    Clone,
}

impl WorkspaceKind {
    /// Reports whether the kind is [`WorkspaceKind::Worktree`], which a
    /// record leaves out.
    #[must_use]
    pub fn is_worktree(&self) -> bool {
        *self == Self::Worktree
    }
}

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
    /// the name of the checkout's directory, such as `app`.
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
    /// The bare mirror whose objects the checkout borrows, for a checkout
    /// that is a clone, or `None` for a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<PathBuf>,
}

/// The record of one workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Workspace {
    /// The version of the record form, which is [`RECORD_VERSION`].
    pub version: u32,
    /// How the workspace's checkouts are made. A record without a `kind`
    /// reads as [`WorkspaceKind::Worktree`], and a worktree record leaves it
    /// out.
    #[serde(default, skip_serializing_if = "WorkspaceKind::is_worktree")]
    pub kind: WorkspaceKind,
    /// The workspace's name.
    pub name: WorkspaceName,
    /// The workspace's directory, which holds its `AGENTS.md` and one
    /// directory for each checkout.
    pub path: PathBuf,
    /// The workspace's checkouts. A workspace made by `mxv new` has one.
    pub checkouts: Vec<Checkout>,
    /// The host port that the workspace holds for each named port of its
    /// manifest. The first `mxv up` allocates them, and every later one
    /// reuses them. A record without ports leaves the key out, and a
    /// record that an earlier version wrote reads as holding none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
}

impl Workspace {
    /// Returns the record of a workspace of clones named `name` in the
    /// directory `path` with `checkouts`.
    #[must_use]
    pub const fn new(name: WorkspaceName, path: PathBuf, checkouts: Vec<Checkout>) -> Self {
        Self {
            version: RECORD_VERSION,
            kind: WorkspaceKind::Clone,
            name,
            path,
            checkouts,
            ports: BTreeMap::new(),
        }
    }

    /// Returns the record as one of a workspace of git worktrees, with the
    /// version that such a record has.
    #[must_use]
    pub const fn of_worktrees(mut self) -> Self {
        self.version = WORKTREE_RECORD_VERSION;
        self.kind = WorkspaceKind::Worktree;
        self
    }

    /// Returns the path of the workspace's `AGENTS.md`.
    #[must_use]
    pub fn agents_file(&self) -> PathBuf {
        self.path.join(AGENTS_FILE)
    }

    /// Reports whether a checkout's directory is the workspace manifest's
    /// path, [`Workspace::manifest_file`], or holds it, as in an old record
    /// whose checkout is the workspace's directory itself. The repository
    /// then writes that file, so mxv reads no workspace manifest for the
    /// workspace. It compares the paths as the record writes them, and
    /// again once symbolic links are resolved, when the directories exist.
    #[must_use]
    pub fn checkout_holds_manifest_file(&self) -> bool {
        let file = self.manifest_file();
        let resolved = self
            .path
            .canonicalize()
            .ok()
            .map(|dir| dir.join(crate::manifest::MANIFEST_FILE));
        self.checkouts.iter().any(|checkout| {
            file.starts_with(&checkout.path)
                || resolved.as_ref().is_some_and(|file| {
                    checkout
                        .path
                        .canonicalize()
                        .is_ok_and(|path| file.starts_with(path))
                })
        })
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
            mirror: None,
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
        let made = match workspace.kind {
            WorkspaceKind::Worktree => format!("a git worktree of `{}`", checkout.source.display()),
            WorkspaceKind::Clone => format!(
                "a clone of the origin of `{}` with a `.git` of its own",
                checkout.source.display()
            ),
        };
        text.push_str(&format!(
            "- `{}/` is {made} on the branch `{}`, which started from `{}`.\n",
            checkout.name, checkout.branch, checkout.base,
        ));
    }
    let refuses = match workspace.kind {
        WorkspaceKind::Worktree => {
            "It refuses while a checkout has uncommitted\n  \
             changes, untracked files, ignored files such as `.env`, or commits\n  \
             that no branch holds. `--force` discards the files and keeps unmerged\n  \
             branches; `--delete-unmerged` deletes unmerged commits."
        }
        WorkspaceKind::Clone => {
            "It refuses while a checkout has uncommitted\n  \
             changes, untracked files, ignored files such as `.env`, stashes, or\n  \
             commits that `origin` does not hold. `--force` discards the files and\n  \
             stashes; `--delete-unmerged` deletes unpushed commits."
        }
    };
    text.push_str(&format!(
        "\n\
         ## Commands\n\
         \n\
         - `mxv show {name}` prints this workspace's record.\n\
         - `mxv list` lists every workspace.\n\
         - `cd \"$(mxv cd {name})\"` returns to this directory.\n\
         - `mxv up` starts the processes that the checkout's `mxv.toml` declares,\n\
         \x20 `mxv status` reports them, and `mxv down` stops them. Run here, they\n\
         \x20 work on this workspace; elsewhere, `-w {name}` names it.\n\
         - `mxv remove {name}` removes the checkouts, their branches when they are\n\
         \x20 merged, and the record. {refuses}\n\
         \n\
         This file sits above the checkouts, so git does not see it.\n"
    ));
    text
}
