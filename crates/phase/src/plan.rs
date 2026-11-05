//! The run plan: plain data describing every app, process, source, port,
//! probe, restart policy, dependency and permission of one workspace.
//!
//! multiverse builds the plan from the workspace record and each app's
//! manifest. A plan holds no environment value, so it can be printed,
//! stored and reviewed. [`Plan::new`] is the one way to build a plan, and
//! reading one from JSON goes through it.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, PortName, ProcessName, VolumeName, WorkspaceId, WorkspaceName};
use crate::sandbox::WorkspacePaths;
use crate::source::Source;

mod check;
pub(crate) mod container;
pub(crate) mod probe;
mod process_file;
pub(crate) mod restart;

pub(crate) use check::STATE;
use check::check_apps;
pub use check::{ContainerProblem, PermissionProblem, PlanError, ProcessProblem, SourceProblem};
use container::ContainerOptions;
pub use process_file::OnceWithoutOneshot;
use process_file::ProcessFile;

#[cfg(test)]
mod tests;

use probe::Probe;
use restart::{Restart, Stop};

/// How long phase waits for a process to become ready, unless the plan
/// states another time.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest time a plan may give a process to become ready. A longer
/// time is refused with [`ProcessProblem::ReadyTimeoutOutOfRange`].
pub const MOST_READY_TIMEOUT: Duration = Duration::from_secs(3600);

/// The longest stop grace a plan may give a process. A longer grace is
/// refused with [`ProcessProblem::StopGraceOutOfRange`].
pub const MOST_STOP_GRACE: Duration = Duration::from_secs(3600);

/// Returns [`DEFAULT_READY_TIMEOUT`], for a plan read from JSON that omits
/// it.
const fn default_ready_timeout() -> Duration {
    DEFAULT_READY_TIMEOUT
}

/// The run plan of one workspace. App names are unique within it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PlanFile", into = "PlanFile")]
pub struct Plan {
    /// The workspace's name.
    workspace: WorkspaceName,
    /// The workspace's stable id, which keys its run record.
    id: WorkspaceId,
    /// The apps, in the order the plan lists them.
    apps: Vec<AppPlan>,
    /// The workspace's own directory and the paths of the workspace that
    /// a sandboxed app may never use, which the host program gives with
    /// [`Plan::with_workspace_dir`]. The plan's JSON form and digest leave
    /// them out.
    workspace_paths: WorkspacePaths,
}

impl Plan {
    /// Returns the plan of the workspace named `workspace`, whose stable id
    /// is `id`, running `apps`.
    ///
    /// # Errors
    ///
    /// Returns a [`PlanError`] when two apps share a name, two processes of
    /// an app share a name, an app depends on an app the plan lacks, an
    /// app's source does not suit its runner, the dependencies form a
    /// cycle, a port is not one of its app's ports, a secret has a value in
    /// the plan, or a process's command, environment, probe or container
    /// fields are unusable, or the processes of an app depend on each other
    /// in a cycle.
    pub fn new(
        workspace: WorkspaceName,
        id: WorkspaceId,
        apps: Vec<AppPlan>,
    ) -> Result<Self, PlanError> {
        check_apps(&apps)?;
        Ok(Self {
            workspace,
            id,
            apps,
            workspace_paths: WorkspacePaths::default(),
        })
    }

    /// Returns this plan with `dir` as the workspace's own directory, which
    /// holds its checkouts, and `protected` as the workspace's paths that a
    /// sandboxed app may never read or write, such as the workspace
    /// manifest. phase refuses to sandbox an app whose checkout is `dir`,
    /// and denies a sandboxed app the rest of `dir` outside its checkout.
    /// Without `dir`, phase takes the parent of each app's checkout as the
    /// workspace's directory.
    #[must_use]
    pub fn with_workspace_dir(mut self, dir: PathBuf, protected: Vec<PathBuf>) -> Self {
        self.workspace_paths = WorkspacePaths {
            dir: Some(dir),
            protected,
        };
        self
    }

    /// Returns the workspace's own directory and protected paths.
    pub(crate) const fn workspace_paths(&self) -> &WorkspacePaths {
        &self.workspace_paths
    }

    /// Returns the workspace's name.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceName {
        &self.workspace
    }

    /// Returns the workspace's stable id.
    #[must_use]
    pub const fn id(&self) -> &WorkspaceId {
        &self.id
    }

    /// Returns the apps, in the order the plan lists them.
    #[must_use]
    pub fn apps(&self) -> &[AppPlan] {
        &self.apps
    }

    /// Returns the app named `name`, or `None` when the plan has no such app.
    #[must_use]
    pub fn app(&self, name: &AppName) -> Option<&AppPlan> {
        self.apps.iter().find(|app| &app.name == name)
    }

    /// Returns a 32-byte digest of the plan's JSON form, which the run
    /// record keeps so a later `up` can tell that the plan changed. Equal
    /// plans give equal digests. It is four 64-bit FNV-1a hashes from
    /// different starting values, which detect a change and are not meant
    /// to resist a forged plan.
    pub(crate) fn digest(&self) -> [u8; 32] {
        // A plan always serializes, since its fields are plain data.
        digest(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// Returns the 32-byte digest of `bytes` that [`Plan::digest`] describes.
pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const OFFSETS: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x8422_2325_cbf2_9ce4,
        0x9ce4_8422_2325_cbf2,
        0x2325_cbf2_9ce4_8422,
    ];
    let mut digest = [0; 32];
    for (lane, offset) in OFFSETS.into_iter().enumerate() {
        let hash = bytes.iter().fold(offset, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
        });
        if let Some(slot) = digest.get_mut(lane * 8..lane * 8 + 8) {
            slot.copy_from_slice(&hash.to_be_bytes());
        }
    }
    digest
}

/// The version of the plan's JSON form.
#[derive(Clone, Copy, Serialize, Deserialize)]
enum PlanSchema {
    /// The first version.
    #[serde(rename = "phase/plan/v1")]
    V1,
}

/// The JSON form of a [`Plan`]. A reader refuses any other `schema`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanFile {
    /// The version of the form.
    schema: PlanSchema,
    /// The workspace's name.
    workspace: WorkspaceName,
    /// The workspace's stable id.
    id: WorkspaceId,
    /// The apps.
    apps: Vec<AppPlan>,
}

impl TryFrom<PlanFile> for Plan {
    type Error = PlanError;

    fn try_from(file: PlanFile) -> Result<Self, PlanError> {
        Self::new(file.workspace, file.id, file.apps)
    }
}

impl From<Plan> for PlanFile {
    fn from(plan: Plan) -> Self {
        Self {
            schema: PlanSchema::V1,
            workspace: plan.workspace,
            id: plan.id,
            apps: plan.apps,
        }
    }
}

/// How an app's processes run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunnerKind {
    /// Each process runs on the machine, under a keeper and the sandbox.
    Host,
    /// The app is one Docker Compose project, and each process is one of
    /// its services.
    Container,
}

/// One app of a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AppPlan {
    /// The app's name, which is unique within the workspace.
    pub name: AppName,
    /// How the app's processes run.
    pub runner: RunnerKind,
    /// Where the app's program comes from. A host app uses a release or a
    /// checkout, and a container app uses an image.
    pub source: Source,
    /// The app's named ports and the host port that multiverse's ledger
    /// gives each one. phase never allocates a port.
    #[serde(default)]
    pub ports: BTreeMap<PortName, NonZeroU16>,
    /// The app's processes. Process names are unique within the app.
    pub processes: Vec<ProcessPlan>,
    /// The apps that must be ready before this one starts.
    #[serde(default)]
    pub depends: Vec<AppName>,
    /// What the app's processes may do inside the sandbox, or `None` when
    /// the app declares no `[permissions]`. Only a host app that declares
    /// them runs under the sandbox, so `None` runs its processes
    /// unsandboxed. In JSON an app without permissions writes
    /// `"permissions": null`, so the plan's hash, which each process's
    /// run record entry keeps, changes when an app turns the sandbox on or
    /// off.
    #[serde(default)]
    pub permissions: Option<Permissions>,
    /// The names of the variables that `up --env KEY=VALUE` sets for this
    /// run. Such a value is not a secret, and it reaches the app through
    /// the [`EnvironmentSource`](crate::EnvironmentSource), so the plan
    /// records only its name.
    #[serde(default)]
    pub overrides: BTreeSet<String>,
    /// The bind mounts, cache volumes and run-as user of a container app. A
    /// host app leaves them empty.
    #[serde(default)]
    pub container: ContainerOptions,
}

impl AppPlan {
    /// Returns the plan of an app named `name` that runs with `runner` from
    /// `source`, with no ports, processes, dependencies or permissions.
    #[must_use]
    pub fn new(name: AppName, runner: RunnerKind, source: Source) -> Self {
        Self {
            name,
            runner,
            source,
            ports: BTreeMap::new(),
            processes: Vec::new(),
            depends: Vec::new(),
            permissions: None,
            overrides: BTreeSet::new(),
            container: ContainerOptions::default(),
        }
    }

    /// Returns the 32-byte digest of what a run of this app's process
    /// `process` depends on in the plan: the app's runner, source, ports,
    /// permissions and override names, and the process's own plan. The
    /// other processes and apps of the plan leave it unchanged, and so do
    /// environment values, which the plan does not hold. A oneshot that
    /// declares `once` runs again when its digest changes.
    pub(crate) fn process_digest(&self, process: &ProcessPlan) -> [u8; 32] {
        /// The parts of the plan that the digest covers.
        #[derive(Serialize)]
        struct ProcessPart<'a> {
            /// The app's runner.
            runner: RunnerKind,
            /// The app's source.
            source: &'a Source,
            /// The app's ports.
            ports: &'a BTreeMap<PortName, NonZeroU16>,
            /// The app's permissions.
            permissions: Option<&'a Permissions>,
            /// The names that `up --env` overrides.
            overrides: &'a BTreeSet<String>,
            /// The process.
            process: &'a ProcessPlan,
        }
        let part = ProcessPart {
            runner: self.runner,
            source: &self.source,
            ports: &self.ports,
            permissions: self.permissions.as_ref(),
            overrides: &self.overrides,
            process,
        };
        // The parts are plain data, so they always serialize.
        digest(&serde_json::to_vec(&part).unwrap_or_default())
    }

    /// Reports whether its permissions name `name` as a secret.
    pub(crate) fn is_secret(&self, name: &str) -> bool {
        self.permissions
            .as_ref()
            .is_some_and(|permissions| permissions.secrets.contains(name))
    }
}

/// Whether a process keeps running or runs to completion.
///
/// In JSON a process writes its kind as `"kind": "long-running"` or
/// `"kind": "oneshot"`, and a oneshot writes `"once"` beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessKind {
    /// The process runs until phase stops it, and its probe says when it is
    /// ready.
    #[default]
    LongRunning,
    /// The process runs to completion, such as a migration. It is ready when
    /// it exits with status 0, and the processes that depend on it wait for
    /// that.
    #[non_exhaustive]
    Oneshot {
        /// Whether the process runs only until it has completed once.
        /// With `once`, a later `up` leaves alone a oneshot whose run
        /// record holds its completion from the same part of the plan.
        /// Without it, an `up` runs the oneshot again unless the oneshot's
        /// completion is from the same part of the plan, every other
        /// process of its app is already running, and every app that its
        /// app depends on was already running. `down` forgets the
        /// completion, and `restart` runs the oneshot again either way.
        once: bool,
    },
}

impl ProcessKind {
    /// Returns the kind of a process that runs to completion, only on the
    /// first `up` when `once` is true.
    #[must_use]
    pub const fn oneshot(once: bool) -> Self {
        Self::Oneshot { once }
    }
}

/// One process of an app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProcessFile", into = "ProcessFile")]
#[non_exhaustive]
pub struct ProcessPlan {
    /// The process's name, which is unique within the app.
    pub name: ProcessName,
    /// Whether the process keeps running or runs to completion.
    pub kind: ProcessKind,
    /// The command, as separate values that no shell reads. For a host
    /// process it is the program and its arguments, such as `npm run dev`,
    /// and `{program}` stands for the app's program, which a release or a
    /// checkout with a build supplies. It is not empty. For a container
    /// process it replaces the image's command, and an empty command keeps
    /// the image's.
    pub run: Vec<String>,
    /// The processes of the same app that must be ready, or for a oneshot
    /// must have completed, before this one starts.
    pub depends: Vec<ProcessName>,
    /// The directory the process starts in, relative to the app's checkout
    /// or release directory, or `None` for that directory itself. It never
    /// leaves that directory.
    pub cwd: Option<PathBuf>,
    /// The names of the variables the process receives. The plan holds no
    /// value: the [`EnvironmentSource`](crate::EnvironmentSource) supplies
    /// every value, literal or secret. A host process also inherits `PATH`
    /// and `HOME` from the program that calls phase, and no other variable
    /// of its environment, so the source supplies any other inherited
    /// variable the app relies on, such as `LANG`.
    pub env: BTreeSet<String>,
    /// The names in `env` that the process may receive without a value,
    /// such as `LANG` when the caller's shell may not set it. When the
    /// [`EnvironmentSource`](crate::EnvironmentSource) resolves such a
    /// variable to absent, by leaving it out of
    /// [`AppliedEnvironment::names`](crate::AppliedEnvironment::names), a
    /// host process starts with the variable unset, and a container
    /// process's service leaves the variable out, so the container does
    /// not receive an empty string. An optional secret that is absent gets
    /// no file, and phase's wrapper does not wait for one. A variable that
    /// this set does not name is required, and the source fails for it
    /// with an [`EnvironmentError`](crate::EnvironmentError) when it has no
    /// value, and phase refuses an environment that lacks it in the same
    /// way. Each name is also in `env`, and the plan holds no value. In
    /// JSON it is `"optional_env"`, left out when it is empty.
    pub optional_env: BTreeSet<String>,
    /// The app port the process listens on, or `None`.
    pub port: Option<PortName>,
    /// For a container process, the port inside the container that each of
    /// the app's named ports publishes on 127.0.0.1. A host process leaves
    /// it empty.
    pub publish: BTreeMap<PortName, NonZeroU16>,
    /// For a container process, the workspace's volumes it mounts, each at
    /// an absolute path inside the container, such as a database's data. A
    /// host process leaves it empty.
    pub volumes: BTreeMap<VolumeName, PathBuf>,
    /// The check that tells phase the process is ready, or `None` when the
    /// process is ready as soon as it starts. A oneshot process has none.
    pub ready: Option<Probe>,
    /// How long phase waits for the process to become ready, or for a
    /// oneshot to complete, before the start fails. It is more than zero
    /// and at most [`MOST_READY_TIMEOUT`].
    pub ready_timeout: Duration,
    /// When phase restarts the process.
    pub restart: Restart,
    /// How phase stops the process.
    pub stop: Stop,
    /// How a container process receives the variables that its app's
    /// permissions name as secrets. A host process keeps the default,
    /// [`SecretDelivery::File`], and receives its secrets in its keeper's
    /// environment.
    pub secret_delivery: SecretDelivery,
    /// For a container process that receives its secrets as files, the
    /// owner of the files as a numeric `uid:gid`, such as `999:999`. It is
    /// the user that the app reads its secrets as, which an image that
    /// starts as root and then switches to another user needs. `None` makes
    /// the files belong to the app's run-as user, or else to the image's
    /// own user, and phase refuses a process whose files would then belong
    /// to root. A host process leaves it `None`.
    pub secret_owner: Option<String>,
}

/// How a container process receives its secrets.
///
/// In JSON it is `"secret_delivery": "file"` or `"secret_delivery":
/// "env"`, and a process that omits it receives its secrets as files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SecretDelivery {
    /// phase writes each secret to `/run/secrets/<NAME>`, a file in a
    /// memory-only directory of the container, and starts the process
    /// through phase's entrypoint wrapper, which waits for the files. No
    /// value reaches the container's environment, `docker inspect` or the
    /// compose file.
    #[default]
    File,
    /// The process receives each secret as an environment variable, through
    /// the same private reference on its `docker compose up` child as a
    /// variable that is not secret. It is for an image that can read a
    /// secret only from its environment. `docker inspect` and every process
    /// in the container can then read the value, a new value means a new
    /// container, and Compose hashes the value, unsalted, into the
    /// container's `com.docker.compose.config-hash` label, so anyone who
    /// can read the container's labels can test a guess of a low-entropy
    /// value against that label.
    Env,
}

impl SecretDelivery {
    /// Reports whether this is the default, [`SecretDelivery::File`].
    #[must_use]
    pub const fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }
}

impl ProcessPlan {
    /// Returns the names of the variables that the process receives and
    /// does not mark optional, in name order.
    pub(crate) fn required_env(&self) -> impl Iterator<Item = &String> {
        self.env
            .iter()
            .filter(|name| !self.optional_env.contains(*name))
    }

    /// Returns a long-running process named `name` that runs `run` in its
    /// app's directory, with no dependencies, variables, ports, volumes or
    /// probe, no optional variables, and a ready timeout of
    /// [`DEFAULT_READY_TIMEOUT`], which is
    /// never restarted and stops with the default [`Stop`].
    #[must_use]
    pub fn new(name: ProcessName, run: Vec<String>) -> Self {
        Self {
            name,
            kind: ProcessKind::LongRunning,
            run,
            depends: Vec::new(),
            cwd: None,
            env: BTreeSet::new(),
            optional_env: BTreeSet::new(),
            port: None,
            publish: BTreeMap::new(),
            volumes: BTreeMap::new(),
            ready: None,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            restart: Restart::No,
            stop: Stop::default(),
            secret_delivery: SecretDelivery::File,
            secret_owner: None,
        }
    }
}

/// What an app's processes may do inside the sandbox. phase compares these
/// with the permissions granted to the app, and asks before it runs an app
/// whose permissions grew.
///
/// Until phase runs the allowlist proxy that reaches external hosts, an app
/// may connect only to the apps it depends on, over loopback, and a plan
/// whose `connect` names anything else is refused.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Permissions {
    /// The paths the app may write besides its own state and temporary
    /// directories, which it may always write. `$STATE` stands for the
    /// app's own state directory. Any other entry is an absolute path, or a
    /// path relative to the app's checkout that stays inside it, such as
    /// `target`.
    #[serde(default)]
    pub write: BTreeSet<String>,
    /// The app ports the app may listen on.
    #[serde(default)]
    pub listen: BTreeSet<PortName>,
    /// The apps, by name, and the external hosts the app may connect to.
    #[serde(default)]
    pub connect: BTreeSet<String>,
    /// The names of the app's environment variables that are secrets.
    #[serde(default)]
    pub secrets: BTreeSet<String>,
}
