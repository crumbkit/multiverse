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
mod braces;
mod clock;
mod clone;
mod disk;
mod disposable;
mod error;
mod git;
pub mod health;
mod host;
mod lifecycle;
mod lookup;
mod manifest;
mod marker;
mod merged;
mod mirror;
mod name;
mod phase_plan;
mod ports;
mod preflight;
mod process;
mod provision;
mod proxy;
mod record;
mod redacted;
mod runs;
mod service;
mod store;
mod template;
mod workspaces;

pub use crate::archive::{
    ARCHIVE_MAX_FILE, ARCHIVE_VERSION, Archive, ArchiveHook, ArchiveId, ArchiveOptions,
    ArchiveOutcome, ArchivePlan, ArchivePlanned, ArchiveRecords, ArchiveRefusal, ArchiveState,
    ArchivedCheckout, ArchivedRemoval, Archives, FileStatus, HOST_ARCHIVE_VERSION, HookError,
    HostFile, IgnoredEntry, IgnoredKind, IgnoredSource, KEPT_NAMESPACE, Kept, KeptStash, KeptWip,
    LostPin, MOST_HOST_FILE_BYTES, MOST_HOST_FILES, MirrorLoss, NO_HOST_ARCHIVE_VERSION,
    OLDER_ARCHIVE_VERSION, PortChange, PutBack, RestoreMark, RestoreOptions, RestoreOutcome,
    RestorePlan, RestorePlanned, RestoreRefusal, Restored, RootEntry, RootKind, Uncommitted,
    Unkeepable, Unkept, UnkeptKind, WIP_MESSAGE,
};
pub use crate::backend::{
    Backend, Changes, Concealed, Head, Hidden, INITIALIZING, LargeFile, Listed, LocalNames,
    Operation, Paused, RefHolder, Repository, StashEntry, SubmoduleWork, WipCommits, WipTrees,
};
pub use crate::clone::{Cloned, UNSUPPORTED};
pub use crate::disk::{Cleaned, DiskUsage, DisposablePath, FailedPath, KeptPath};
pub use crate::disposable::{
    BadGlob, CLONE_NEVER, DEFAULT_CLONE_ON_NEW, DisposableSet, DisposableSource, PathGlob,
};
pub use crate::error::{Error, SshRefusal};
pub use crate::git::{CHECKOUT_DEADLINE, FETCH_DEADLINE, Git, LOCAL_DEADLINE, MOST_OUTPUT_BYTES};
pub use crate::host::{BadHost, Host, HostName, MOST_HOST_NAME_BYTES, MOST_MANIFEST_FILE_BYTES};
pub use crate::lifecycle::{
    ArchiveDeleted, Archived, Finish, FinishDone, Finished, InUse, Lifecycle, RemovalDone,
    RemoveAsked, Removed, RepairDone, Repairing, Restoration, RetireDone, VolumesAsked,
};
pub use crate::lookup::{ManifestInputs, ManifestSource, TaskHome, checkout_prefix, prefixed};
pub use crate::manifest::{
    Command, Container, DEFAULT_READY_TIMEOUT, EnvValue, Joined, LogPattern, MANIFEST_FILE,
    MANIFEST_VERSION, MOST_MANIFEST_BYTES, MOST_READY_SECONDS, Manifest, ManifestError,
    ManifestProblem, Permissions, Process, Published, Ready, ReadyCheck, Secret, StarmapFound,
    Task, TaskName, Unit,
};
pub use crate::marker::{MARKER_FILE, Marker, ensure as ensure_marker};
pub use crate::merged::{GitHub, HELPER_DEADLINE, PullRequests, Verdict, update_board_mount};
pub use crate::name::{BadName, MOST_NAME_BYTES, WorkspaceName};
pub use crate::phase_plan::{
    CallerEnvironment, MAIN, PASSED_THROUGH, PHASE_SETS, ProvidedBind, RefusedBind, RefusedBinds,
    Unrunnable, UnrunnableKind,
};
pub use crate::ports::DEFAULT_PORT_RANGE;
pub use crate::preflight::{Approvals, CHECK_TIMEOUT, Checked, Outcome, Preflight};
pub use crate::process::process_table;
pub use crate::provision::{ProvisionError, Spec as ProvisionSpec, provision};
pub use crate::proxy::{
    Authority, Check as ProxyCheck, CheckRow as ProxyCheckRow, CheckState as ProxyCheckState,
    DOMAIN as ROUTE_DOMAIN, Declared, Derived, Ensured, Proxy, ProxyError, ProxyProblem,
    Resolution, Resolver, Route, Settings as ProxySettings, Shadowed, ShadowedHost,
    Status as ProxyStatus, SyncMode, Synced, SystemResolver, derive as derive_routes,
    line_for as hosts_line, missing as missing_hosts,
};
pub use crate::record::{
    AGENTS_FILE, BackendKind, BoardMount, CLONE_RECORD_VERSION, Checkout, HOST_RECORD_VERSION,
    HostData, HostKey, ID_RECORD_VERSION, MOST_HOST_DATA_BYTES, MOST_HOST_KEY_BYTES,
    MOST_HOST_VALUES, MULTI_CHECKOUT_RECORD_VERSION, Made, RECORD_VERSION, Reclaimed,
    WORKTREE_RECORD_VERSION, Workspace, WorkspaceKind, WorkspaceState, agents_text,
};
pub use crate::redacted::Redacted;
pub use crate::runs::{
    Admission, Asked, BuildEnd, BuildStop, BuildsPassed, DownOutcome, DownResult, Exec, ExecEnd,
    ExecTarget, ExitReason, Footprint, Granting, Kind, LogOutput, MiseListing, MiseTaskName,
    Needed, PermissionAsk, Place, PreparedTask, ProcessNote, ProcessState, ProcessStatus,
    RenamedVolume, Runs, STOP_GRACE, StillNeeded, TaskEnd, TaskServices, TaskStart, Toucher,
    Unstoppable, UpOutcome, Volumes, Waited, WorkspaceLock,
};
pub use crate::service::{
    ACCOUNT, AWS_PREFIX, AttributeType, Backend as ServiceBackend, COMMON_VARIABLES, HOST, Index,
    KeyAttribute, LOCALSTACK_IMAGE, MOST_VISIBILITY_SECONDS, PROVISION_APP, Placeholder, Queue,
    REGION, RESERVED_PREFIX, SERVICES_APP, SERVICES_READY, Service, ServiceKind, ServiceValue,
    Table,
};
pub use crate::store::{Created, LOCK_WAIT, MOST_RECORD_BYTES, Records, Store};
pub use crate::template::{
    Found as FoundTemplate, TEMPLATE_TABLE, Template, find as find_template,
};
pub use crate::workspaces::{
    AddOutcome, Allow, BranchFate, CheckoutRemoval, CreateOptions, DISPOSABLE, Fate, MIRROR_WAIT,
    Mode, NewOutcome, Removal, RemovalKind, RemovalOutcome, RemovalPlan, Repo, Workspaces,
    folder_name,
};
pub use phase;
#[cfg(test)]
mod docs_tests;
#[cfg(test)]
mod testing;
