//! multiverse creates task workspaces that span several git repositories.
//!
//! A workspace holds one checkout for each repository, branched from the
//! freshly fetched default branch of its `origin`, an `AGENTS.md` that
//! tells an agent how to work there, and a record in a [`Store`]. The `mxv`
//! command is built on this library.
//!
//! [`Workspaces`] holds the operations. It makes checkouts through a
//! [`Backend`], and [`Git`] is the one backend. With
//! [`Workspaces::with_mirrors`], each checkout is a `git clone --shared` of
//! a bare mirror of the repository's origin, with a `.git` of its own, so
//! nothing in a workspace can change the person's repository or another
//! workspace. A workspace that an earlier version made holds git worktrees
//! of the person's repository, and its record's [`WorkspaceKind`] says so.
//! A workspace named `reading-list` with a checkout of the repository
//! `app` lives at `<root>/reading-list/app`, with its `AGENTS.md` at
//! `<root>/reading-list/AGENTS.md`, outside the checkout.
//!
//! The library reads no environment variable. Its caller passes the root
//! directory of the workspaces and the directory of the store.

mod backend;
mod clock;
mod clone;
mod disk;
mod disposable;
mod docker;
mod error;
mod git;
mod lookup;
mod manifest;
mod mirror;
mod name;
mod ports;
mod process;
mod record;
mod redacted;
mod run_files;
mod runs;
mod store;
mod template;
mod workspaces;

pub use crate::backend::{Backend, Changes, INITIALIZING, Listed, Repository};
pub use crate::clock::FOLLOW_INTERVAL;
pub use crate::clone::{Cloned, UNSUPPORTED};
pub use crate::disk::{Cleaned, DiskUsage, DisposablePath, FailedPath, KeptPath};
pub use crate::disposable::{
    BadGlob, CLONE_NEVER, DEFAULT_CLONE_ON_NEW, DisposableSet, DisposableSource, PathGlob,
};
pub use crate::error::Error;
pub use crate::git::{CHECKOUT_DEADLINE, FETCH_DEADLINE, Git, LOCAL_DEADLINE, MOST_OUTPUT_BYTES};
pub use crate::lookup::{ManifestInputs, ManifestSource};
pub use crate::manifest::{
    Container, DEFAULT_READY_TIMEOUT, EnvValue, LogPattern, MANIFEST_FILE, MANIFEST_VERSION,
    MOST_MANIFEST_BYTES, MOST_READY_SECONDS, Manifest, ManifestError, ManifestProblem, Process,
    Published, Ready, ReadyCheck, Unit,
};
pub use crate::name::{BadName, MOST_NAME_BYTES, WorkspaceName};
pub use crate::ports::DEFAULT_PORT_RANGE;
pub use crate::record::{
    AGENTS_FILE, BackendKind, Checkout, RECORD_VERSION, WORKTREE_RECORD_VERSION, Workspace,
    WorkspaceKind, agents_text,
};
pub use crate::redacted::Redacted;
pub use crate::run_files::RUN_VERSION;
pub use crate::runs::{
    DownOutcome, ExitReason, Follower, KILL_WAIT, Kind, MOST_FOLLOW_BYTES, MOST_LOG_BYTES,
    MOST_TAIL_BYTES, ProcessState, ProcessStatus, Runs, START_GRACE, STOP_GRACE, TAIL_LINES,
    Unstoppable, UpOutcome, Volumes, Waited, tail,
};
pub use crate::store::{Created, LOCK_WAIT, Lock, MOST_RECORD_BYTES, Records, Store};
pub use crate::template::{
    Found as FoundTemplate, TEMPLATE_TABLE, Template, find as find_template,
};
pub use crate::workspaces::{
    Allow, BranchFate, CheckoutRemoval, DISPOSABLE, Fate, MIRROR_WAIT, Mode, NewOutcome, Removal,
    RemovalOutcome, RemovalPlan, Workspaces,
};
#[cfg(test)]
mod testing;
