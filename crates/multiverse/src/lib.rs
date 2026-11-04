//! multiverse creates task workspaces that span several git repositories.
//!
//! A workspace holds one checkout for each repository, branched from the
//! freshly fetched default branch of its `origin`, an `AGENTS.md` that
//! tells an agent how to work there, and a record in a [`Store`]. The `mxv`
//! command is built on this library.
//!
//! [`Workspaces`] holds the operations. It makes checkouts through a
//! [`Backend`], and [`Git`] is the one backend, which runs `git worktree`.
//! A workspace named `reading-list` with a checkout of the repository
//! `app` lives at `<root>/reading-list/app`, with its `AGENTS.md` at
//! `<root>/reading-list/AGENTS.md`, outside the checkout.
//!
//! The library reads no environment variable. Its caller passes the root
//! directory of the workspaces and the directory of the store.

mod backend;
mod clock;
mod docker;
mod error;
mod git;
mod manifest;
mod name;
mod ports;
mod process;
mod record;
mod runs;
mod store;
mod workspaces;

pub use crate::backend::{Backend, Changes, INITIALIZING, Listed, Repository};
pub use crate::clock::FOLLOW_INTERVAL;
pub use crate::docker::{
    CONTAINER_LABEL, DOCKER_DEADLINE, PULL_DEADLINE, STORE_LABEL, Scope, VOLUME_LABEL,
    WORKSPACE_LABEL,
};
pub use crate::error::Error;
pub use crate::git::{CHECKOUT_DEADLINE, FETCH_DEADLINE, Git, LOCAL_DEADLINE, MOST_OUTPUT_BYTES};
pub use crate::manifest::{
    Container, DEFAULT_READY_TIMEOUT, EnvValue, LogPattern, MANIFEST_FILE, MANIFEST_VERSION,
    MOST_MANIFEST_BYTES, MOST_READY_SECONDS, Manifest, ManifestError, ManifestProblem, Process,
    Published, Ready, ReadyCheck, Unit,
};
pub use crate::name::{BadName, MOST_NAME_BYTES, WorkspaceName};
pub use crate::ports::DEFAULT_PORT_RANGE;
pub use crate::record::{
    AGENTS_FILE, BackendKind, Checkout, RECORD_VERSION, Workspace, agents_text,
};
pub use crate::runs::{
    DownOutcome, Follower, KILL_WAIT, Kind, MOST_FOLLOW_BYTES, MOST_LOG_BYTES, MOST_TAIL_BYTES,
    ProcessState, ProcessStatus, RUN_VERSION, Runs, START_GRACE, STOP_GRACE, TAIL_LINES,
    Unstoppable, UpOutcome, Waited, tail,
};
pub use crate::store::{Created, LOCK_WAIT, Lock, MOST_RECORD_BYTES, Records, Store};
pub use crate::workspaces::{
    Allow, BranchFate, CheckoutRemoval, DISPOSABLE, Fate, Mode, NewOutcome, Removal,
    RemovalOutcome, RemovalPlan, Workspaces,
};
#[cfg(test)]
mod testing;
