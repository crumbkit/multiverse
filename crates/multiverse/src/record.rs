//! The record of a workspace, which the state store keeps as one JSON file
//! for each workspace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use phase::WorkspaceId;
use phase::random::RANDOM_DEVICE;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::name::WorkspaceName;

/// The highest version of the record form that this version of multiverse
/// reads. A record with a higher version is refused (DUR-3). Version 1 is
/// a workspace of git worktrees, which records without a `kind` are, and
/// its record keeps version 1 so that an older mxv still reads it.
/// Version 2 adds `kind`, so an older mxv refuses a workspace of clones
/// rather than treating its checkouts as worktrees. Version 3 adds the
/// stable `id`, and every record that holds one has at least version 3,
/// whatever its kind, so an older mxv refuses the record rather than
/// dropping the id when it rewrites it. Version 4 is a workspace with more
/// than one checkout, whose workspace manifest's paths are relative to the
/// workspace's directory, so an older mxv refuses it rather than reading
/// those paths relative to each checkout.
pub const RECORD_VERSION: u32 = MULTI_CHECKOUT_RECORD_VERSION;

/// The version of the record form of a workspace with more than one
/// checkout.
pub const MULTI_CHECKOUT_RECORD_VERSION: u32 = 4;

/// The version of the record form of a workspace with one checkout and a
/// stable id.
pub const ID_RECORD_VERSION: u32 = 3;

/// The version of the record form of a workspace of clones without a
/// stable id.
pub const CLONE_RECORD_VERSION: u32 = 2;

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
    /// The name of the checkout's directory, its folder, such as `app`. It
    /// is the repository's name, the last component of `source`, unless
    /// the person gave the checkout another folder with `--as`.
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
    /// The workspace's stable id: 32 lowercase hex digits, which mxv mints
    /// when it creates the workspace and never changes, even when the
    /// workspace moves. It labels the workspace's docker objects and names
    /// its run record. A record that an earlier version wrote has none, and
    /// gets one the first time mxv writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<WorkspaceId>,
    /// The workspace's name.
    pub name: WorkspaceName,
    /// The workspace's directory, which holds its `AGENTS.md` and one
    /// directory for each checkout.
    pub path: PathBuf,
    /// The workspace's checkouts, one for each repository, in the order
    /// that `mxv new` and `mxv add` made them.
    pub checkouts: Vec<Checkout>,
    /// The host port that the workspace holds for each named port of its
    /// manifest. The first `mxv up` allocates them, and every later one
    /// reuses them. A record without ports leaves the key out, and a
    /// record that an earlier version wrote reads as holding none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,
    /// Whether `mxv pin` keeps `mxv reclaim` from stopping the workspace.
    /// An unpinned record leaves the key out. An older mxv that rewrites
    /// the record drops the key, which unpins the workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// What `mxv reclaim` stopped, until the next `mxv up` or `mxv down`,
    /// or `None`. A record without the mark leaves the key out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclaimed: Option<Reclaimed>,
}

/// The mark that `mxv reclaim` leaves on a workspace that it stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Reclaimed {
    /// The processes and containers that ran when reclaim stopped them,
    /// which `mxv up` without names starts again.
    pub processes: Vec<String>,
}

impl Reclaimed {
    /// Returns the mark of a reclaim that stopped `processes`.
    #[must_use]
    pub const fn new(processes: Vec<String>) -> Self {
        Self { processes }
    }

    /// Returns the mark that is left once `mxv up`, `restart` or `down`
    /// has started or stopped the processes `names`, or every process when
    /// `names` is empty: the processes that are still to start, or `None`
    /// when none is.
    #[must_use]
    pub fn without(&self, names: &[String]) -> Option<Self> {
        if names.is_empty() {
            return None;
        }
        let processes: Vec<String> = self
            .processes
            .iter()
            .filter(|process| !names.contains(process))
            .cloned()
            .collect();
        (!processes.is_empty()).then_some(Self { processes })
    }
}

impl Workspace {
    /// Returns the record of a workspace of clones named `name` in the
    /// directory `path` with `checkouts`.
    #[must_use]
    pub const fn new(name: WorkspaceName, path: PathBuf, checkouts: Vec<Checkout>) -> Self {
        Self {
            version: CLONE_RECORD_VERSION,
            kind: WorkspaceKind::Clone,
            id: None,
            name,
            path,
            checkouts,
            ports: BTreeMap::new(),
            pinned: false,
            reclaimed: None,
        }
    }

    /// Returns the record as one of a workspace of git worktrees, with the
    /// version that such a record has: [`WORKTREE_RECORD_VERSION`], or
    /// [`ID_RECORD_VERSION`] once it holds a stable id.
    #[must_use]
    pub const fn of_worktrees(mut self) -> Self {
        self.version = if self.id.is_some() {
            ID_RECORD_VERSION
        } else {
            WORKTREE_RECORD_VERSION
        };
        self.kind = WorkspaceKind::Worktree;
        self
    }

    /// Gives the workspace a new stable id when its record has none, and
    /// reports whether it did. An id that the record holds is never
    /// changed. A record that holds an id has at least
    /// [`ID_RECORD_VERSION`], and a record with more than one checkout has
    /// [`MULTI_CHECKOUT_RECORD_VERSION`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the operating system's
    /// random number generator, [`RANDOM_DEVICE`], cannot be read.
    pub fn ensure_id(&mut self) -> Result<bool, Error> {
        let minted = if self.id.is_some() {
            false
        } else {
            let id = WorkspaceId::mint().map_err(|source| Error::StateUnreadable {
                path: PathBuf::from(RANDOM_DEVICE),
                source,
            })?;
            self.id = Some(id);
            true
        };
        self.version = self.version.max(ID_RECORD_VERSION);
        if self.checkouts.len() > 1 {
            self.version = self.version.max(MULTI_CHECKOUT_RECORD_VERSION);
        }
        Ok(minted)
    }

    /// Returns the path of the workspace's `AGENTS.md`.
    #[must_use]
    pub fn agents_file(&self) -> PathBuf {
        self.path.join(AGENTS_FILE)
    }

    /// Returns the first checkout of a workspace of clones that has no
    /// commit, which an `add` or a `new` that stopped partway left
    /// unfinished, or `None`. While one exists, the record names a
    /// checkout that may be missing or partial, so the commands that act
    /// on the manifest refuse until the `add` that left it resumes.
    #[must_use]
    pub fn unfinished_checkout(&self) -> Option<&Checkout> {
        self.checkouts
            .iter()
            .find(|checkout| self.is_unfinished(checkout))
    }

    /// Reports whether `checkout`, one of the workspace's, is unfinished,
    /// as [`Workspace::unfinished_checkout`] describes.
    #[must_use]
    pub fn is_unfinished(&self, checkout: &Checkout) -> bool {
        self.kind == WorkspaceKind::Clone && checkout.commit.is_none()
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
/// where it is, which repository, origin and branch each checkout holds,
/// and how to list, resume or remove the workspace with `mxv`. `origins`
/// holds the origin URL of each checkout, in the record's order, without a
/// user name or password.
#[must_use]
pub fn agents_text(workspace: &Workspace, origins: &[String]) -> String {
    let name = workspace.name.as_str();
    let several = workspace.checkouts.len() > 1;
    let work = if several {
        "Work inside the checkouts below, and commit and push\n\
         from each of them."
    } else {
        "Work inside the checkout below, and commit and push\n\
         from there."
    };
    let mut text = format!(
        "# Workspace {name}\n\
         \n\
         This directory is the multiverse workspace `{name}`. mxv created it and\n\
         keeps its record. {work}\n\
         \n\
         ## Checkouts\n\
         \n"
    );
    for (index, checkout) in workspace.checkouts.iter().enumerate() {
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
        text.push_str(&format!("  Its path is `{}`", checkout.path.display()));
        match origins.get(index) {
            Some(origin) => text.push_str(&format!(" and its origin is `{origin}`.\n")),
            None => text.push_str(".\n"),
        }
    }
    if several {
        text.push_str(
            "\nmxv merges the workspace's `mxv.toml` with each checkout's own `mxv.toml`.\n\
             Each process, container and port of a checkout's file is named\n\
             `<folder>-<name>`, such as `api-server` for the process `server` of `api/`.\n",
        );
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
