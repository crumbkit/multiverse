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

mod archive;
mod backend;
mod clock;
mod clone;
mod disk;
mod disposable;
mod error;
mod git;
mod lookup;
mod manifest;
mod mirror;
mod name;
mod phase_plan;
mod ports;
mod process;
mod provision;
mod record;
mod redacted;
mod runs;
mod service;
mod store;
mod template;
mod workspaces;

pub use crate::archive::{
    ARCHIVE_VERSION, Archive, ArchiveOptions, ArchiveOutcome, ArchivePlan, ArchivePlanned,
    ArchiveRecords, ArchiveRefusal, ArchiveState, ArchivedCheckout, ArchivedRemoval, Archives,
    PortChange, RestoreOutcome, RestorePlan, RestorePlanned, RestoreRefusal, Restored, RootEntry,
    RootKind, Unsaved, WIP_MESSAGE,
};
pub use crate::backend::{
    Backend, Changes, Concealed, Head, INITIALIZING, Listed, LocalNames, Repository, SubmoduleWork,
};
pub use crate::clone::{Cloned, UNSUPPORTED};
pub use crate::disk::{Cleaned, DiskUsage, DisposablePath, FailedPath, KeptPath};
pub use crate::disposable::{
    BadGlob, CLONE_NEVER, DEFAULT_CLONE_ON_NEW, DisposableSet, DisposableSource, PathGlob,
};
pub use crate::error::Error;
pub use crate::git::{CHECKOUT_DEADLINE, FETCH_DEADLINE, Git, LOCAL_DEADLINE, MOST_OUTPUT_BYTES};
pub use crate::lookup::{ManifestInputs, ManifestSource, TaskHome, checkout_prefix, prefixed};
pub use crate::manifest::{
    Command, Container, DEFAULT_READY_TIMEOUT, EnvValue, Joined, LogPattern, MANIFEST_FILE,
    MANIFEST_VERSION, MOST_MANIFEST_BYTES, MOST_READY_SECONDS, Manifest, ManifestError,
    ManifestProblem, Permissions, Process, Published, Ready, ReadyCheck, Task, TaskName, Unit,
};
pub use crate::name::{BadName, MOST_NAME_BYTES, WorkspaceName};
pub use crate::phase_plan::{CallerEnvironment, MAIN, PASSED_THROUGH, PHASE_SETS, Unrunnable};
pub use crate::ports::DEFAULT_PORT_RANGE;
pub use crate::process::process_table;
pub use crate::provision::{ProvisionError, Spec as ProvisionSpec, provision};
pub use crate::record::{
    AGENTS_FILE, BackendKind, CLONE_RECORD_VERSION, Checkout, ID_RECORD_VERSION,
    MULTI_CHECKOUT_RECORD_VERSION, RECORD_VERSION, Reclaimed, WORKTREE_RECORD_VERSION, Workspace,
    WorkspaceKind, agents_text,
};
pub use crate::redacted::Redacted;
pub use crate::runs::{
    Admission, Asked, BuildEnd, BuildStop, BuildsPassed, DownOutcome, Exec, ExecEnd, ExecTarget,
    ExitReason, Footprint, Kind, LogOutput, MiseListing, MiseTaskName, Place, PreparedTask,
    ProcessNote, ProcessState, ProcessStatus, RenamedVolume, Runs, STOP_GRACE, TaskEnd,
    TaskServices, TaskStart, Toucher, Unstoppable, UpOutcome, Volumes, Waited,
};
pub use crate::service::{
    ACCOUNT, AWS_PREFIX, AttributeType, Backend as ServiceBackend, COMMON_VARIABLES,
    DYNAMODB_IMAGE, HOST, Index, KeyAttribute, LOCALSTACK_IMAGE, MOST_VISIBILITY_SECONDS,
    PROVISION_APP, Placeholder, Queue, REGION, RESERVED_PREFIX, Service, ServiceKind, ServiceValue,
    Table,
};
pub use crate::store::{Created, LOCK_WAIT, Lock, MOST_RECORD_BYTES, Records, Store};
pub use crate::template::{
    Found as FoundTemplate, TEMPLATE_TABLE, Template, find as find_template,
};
pub use crate::workspaces::{
    AddOutcome, Allow, BranchFate, CheckoutRemoval, DISPOSABLE, Fate, MIRROR_WAIT, Mode,
    NewOutcome, Removal, RemovalOutcome, RemovalPlan, Repo, Workspaces, folder_name,
};
pub use phase;
#[cfg(test)]
mod testing;
