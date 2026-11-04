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
use crate::source::Source;

mod check;
pub(crate) mod container;
pub(crate) mod probe;
pub(crate) mod restart;

use check::check_apps;
pub use check::{ContainerProblem, PlanError, ProcessProblem, SourceProblem};
use container::ContainerOptions;

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
        })
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
    /// What the app's processes may do inside the sandbox.
    #[serde(default)]
    pub permissions: Permissions,
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
            permissions: Permissions::default(),
            overrides: BTreeSet::new(),
            container: ContainerOptions::default(),
        }
    }
}

/// Whether a process keeps running or runs to completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
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
        /// Whether the process runs only on the first `up` of the
        /// workspace. When it is false, the process runs on every `up`.
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
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ProcessPlan {
    /// The process's name, which is unique within the app.
    pub name: ProcessName,
    /// Whether the process keeps running or runs to completion.
    #[serde(default)]
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
    #[serde(default)]
    pub depends: Vec<ProcessName>,
    /// The directory the process starts in, relative to the app's checkout
    /// or release directory, or `None` for that directory itself. It never
    /// leaves that directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The names of the variables the process receives. The plan holds no
    /// value: the [`EnvironmentSource`](crate::EnvironmentSource) supplies
    /// every value, literal or secret. A host process also inherits `PATH`
    /// and `HOME` from the program that calls phase, and no other variable
    /// of its environment.
    #[serde(default)]
    pub env: BTreeSet<String>,
    /// The app port the process listens on, or `None`.
    #[serde(default)]
    pub port: Option<PortName>,
    /// For a container process, the port inside the container that each of
    /// the app's named ports publishes on 127.0.0.1. A host process leaves
    /// it empty.
    #[serde(default)]
    pub publish: BTreeMap<PortName, NonZeroU16>,
    /// For a container process, the workspace's volumes it mounts, each at
    /// an absolute path inside the container, such as a database's data. A
    /// host process leaves it empty.
    #[serde(default)]
    pub volumes: BTreeMap<VolumeName, PathBuf>,
    /// The check that tells phase the process is ready, or `None` when the
    /// process is ready as soon as it starts. A oneshot process has none.
    #[serde(default)]
    pub ready: Option<Probe>,
    /// How long phase waits for the process to become ready, or for a
    /// oneshot to complete, before the start fails. It is more than zero
    /// and at most [`MOST_READY_TIMEOUT`].
    #[serde(
        default = "default_ready_timeout",
        rename = "ready_timeout_ms",
        with = "crate::millis"
    )]
    pub ready_timeout: Duration,
    /// When phase restarts the process.
    #[serde(default)]
    pub restart: Restart,
    /// How phase stops the process.
    #[serde(default)]
    pub stop: Stop,
}

impl ProcessPlan {
    /// Returns a long-running process named `name` that runs `run` in its
    /// app's directory, with no dependencies, variables, ports, volumes or
    /// probe and a ready timeout of [`DEFAULT_READY_TIMEOUT`], which is
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
            port: None,
            publish: BTreeMap::new(),
            volumes: BTreeMap::new(),
            ready: None,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            restart: Restart::No,
            stop: Stop::default(),
        }
    }
}

/// What an app's processes may do inside the sandbox. phase compares these
/// with the permissions granted to the app, and asks before it runs an app
/// whose permissions grew.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Permissions {
    /// The paths the app may write, in which `$STATE` stands for the app's
    /// own state directory.
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
