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
mod error;
mod git;
mod name;
mod record;
mod store;
mod workspaces;

pub use crate::backend::{Backend, Changes, INITIALIZING, Listed, Repository};
pub use crate::error::Error;
pub use crate::git::{CHECKOUT_DEADLINE, FETCH_DEADLINE, Git, LOCAL_DEADLINE, MOST_OUTPUT_BYTES};
pub use crate::name::{BadName, MOST_NAME_BYTES, WorkspaceName};
pub use crate::record::{
    AGENTS_FILE, BackendKind, Checkout, RECORD_VERSION, Workspace, agents_text,
};
pub use crate::store::{Created, MOST_RECORD_BYTES, Records, Store};
pub use crate::workspaces::{
    Allow, BranchFate, CheckoutRemoval, DISPOSABLE, Fate, Mode, NewOutcome, Removal, RemovalPlan,
    Workspaces,
};
#[cfg(test)]
mod testing;
