//! The run plan: plain data describing every app, process, source, port,
//! probe, restart policy, dependency and permission of one workspace.
//!
//! multiverse builds the plan from the workspace record and each app's
//! manifest. A plan holds no environment value, so it can be printed,
//! stored and reviewed. [`Plan::new`] is the one way to build a plan, and
//! reading one from JSON goes through it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};

use crate::name::{AppName, PortName, ProcessName, WorkspaceId, WorkspaceName};
use crate::source::Source;

pub(crate) mod probe;
pub(crate) mod restart;

#[cfg(test)]
mod tests;

use probe::{Probe, ProbeCheck};
use restart::{Restart, Stop};

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
    /// app's source does not suit its runner, a port is not one of its
    /// app's ports, or a process's command or probe is unusable.
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
        }
    }
}

/// Whether a process keeps running or runs to completion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ProcessKind {
    /// The process runs until phase stops it, and its probe says when it is
    /// ready.
    #[default]
    LongRunning,
    /// The process runs to completion, such as a migration. It is ready when
    /// it exits with status 0, and the processes that depend on it wait for
    /// that.
    Oneshot {
        /// Whether the process runs only on the first `up` of the
        /// workspace. When it is false, the process runs on every `up`.
        once: bool,
    },
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
    /// process it is the program and its arguments, and `{program}` stands
    /// for the app's program. It is not empty. For a container process it
    /// replaces the image's command, and an empty command keeps the image's.
    pub run: Vec<String>,
    /// The app port the process listens on, or `None`.
    #[serde(default)]
    pub port: Option<PortName>,
    /// The check that tells phase the process is ready, or `None` when the
    /// process is ready as soon as it starts. A oneshot process has none.
    #[serde(default)]
    pub ready: Option<Probe>,
    /// When phase restarts the process.
    #[serde(default)]
    pub restart: Restart,
    /// How phase stops the process.
    #[serde(default)]
    pub stop: Stop,
}

impl ProcessPlan {
    /// Returns a long-running process named `name` that runs `run`, with no
    /// port and no probe, which is never restarted and stops with the
    /// default [`Stop`].
    #[must_use]
    pub fn new(name: ProcessName, run: Vec<String>) -> Self {
        Self {
            name,
            kind: ProcessKind::LongRunning,
            run,
            port: None,
            ready: None,
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

/// The reason that a set of apps is not a valid plan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanError {
    /// Two apps have this name.
    DuplicateApp {
        /// The name.
        app: AppName,
    },
    /// Two processes of the app have this name.
    DuplicateProcess {
        /// The app.
        app: AppName,
        /// The name.
        process: ProcessName,
    },
    /// The app depends on an app that the plan lacks.
    UnknownDependency {
        /// The app.
        app: AppName,
        /// The app it depends on.
        depends: AppName,
    },
    /// The app's source does not suit its runner: a host app runs from an
    /// image, or a container app runs from a release or a checkout.
    RunnerMismatch {
        /// The app.
        app: AppName,
        /// The app's runner.
        runner: RunnerKind,
    },
    /// The app's permissions let it listen on a port it does not have.
    UnknownPort {
        /// The app.
        app: AppName,
        /// The port.
        port: PortName,
    },
    /// A process of the app cannot run as planned.
    BadProcess {
        /// The app.
        app: AppName,
        /// The process.
        process: ProcessName,
        /// What is wrong with it.
        problem: ProcessProblem,
    },
}

/// What is wrong with a process in a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessProblem {
    /// A host process's command is empty.
    EmptyCommand,
    /// The process, or its probe, names a port its app does not have.
    UnknownPort(PortName),
    /// The process has an HTTP probe but no port.
    HttpProbeWithoutPort,
    /// The HTTP probe's path does not start with `/`.
    HttpPathWithoutSlash,
    /// The command probe's command is empty.
    EmptyProbeCommand,
    /// The probe's period or timeout is zero.
    ZeroProbeTime,
    /// A oneshot process has a readiness probe, but it is ready when it
    /// exits with status 0.
    OneshotWithProbe,
    /// A oneshot process restarts always, so it would never complete.
    OneshotRestartsAlways,
}

impl fmt::Display for ProcessProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand => formatter.write_str("its command is empty"),
            Self::UnknownPort(port) => write!(formatter, "it names the unknown port {port}"),
            Self::HttpProbeWithoutPort => formatter.write_str("it has an http probe but no port"),
            Self::HttpPathWithoutSlash => {
                formatter.write_str("its http probe's path does not start with `/`")
            }
            Self::EmptyProbeCommand => formatter.write_str("its probe's command is empty"),
            Self::ZeroProbeTime => formatter.write_str("its probe's period or timeout is zero"),
            Self::OneshotWithProbe => formatter.write_str("it is a oneshot with a probe"),
            Self::OneshotRestartsAlways => {
                formatter.write_str("it is a oneshot that restarts always")
            }
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateApp { app } => write!(formatter, "two apps are named {app}"),
            Self::DuplicateProcess { app, process } => {
                write!(formatter, "two processes of {app} are named {process}")
            }
            Self::UnknownDependency { app, depends } => {
                write!(
                    formatter,
                    "{app} depends on {depends}, which the plan lacks"
                )
            }
            Self::RunnerMismatch { app, runner } => {
                let runner = match runner {
                    RunnerKind::Host => "host",
                    RunnerKind::Container => "container",
                };
                write!(
                    formatter,
                    "{app} is a {runner} app with a source that does not suit it"
                )
            }
            Self::UnknownPort { app, port } => {
                write!(
                    formatter,
                    "{app} may listen on {port}, which is not one of its ports"
                )
            }
            Self::BadProcess {
                app,
                process,
                problem,
            } => write!(formatter, "{app}/{process} cannot run because {problem}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Checks the rules of [`Plan::new`].
fn check_apps(apps: &[AppPlan]) -> Result<(), PlanError> {
    let mut names = BTreeSet::new();
    for app in apps {
        if !names.insert(&app.name) {
            return Err(PlanError::DuplicateApp {
                app: app.name.clone(),
            });
        }
    }
    for app in apps {
        if let Some(depends) = app.depends.iter().find(|depends| !names.contains(depends)) {
            return Err(PlanError::UnknownDependency {
                app: app.name.clone(),
                depends: depends.clone(),
            });
        }
        check_app(app)?;
    }
    Ok(())
}

/// Checks one app's source, ports and processes.
fn check_app(app: &AppPlan) -> Result<(), PlanError> {
    let suits = match (&app.runner, &app.source) {
        (RunnerKind::Host, Source::Release { .. } | Source::Checkout { .. })
        | (RunnerKind::Container, Source::Image { .. }) => true,
        (RunnerKind::Host, Source::Image { .. })
        | (RunnerKind::Container, Source::Release { .. } | Source::Checkout { .. }) => false,
    };
    if !suits {
        return Err(PlanError::RunnerMismatch {
            app: app.name.clone(),
            runner: app.runner,
        });
    }
    if let Some(port) = app
        .permissions
        .listen
        .iter()
        .find(|port| !app.ports.contains_key(*port))
    {
        return Err(PlanError::UnknownPort {
            app: app.name.clone(),
            port: port.clone(),
        });
    }
    let mut processes = BTreeSet::new();
    for process in &app.processes {
        if !processes.insert(&process.name) {
            return Err(PlanError::DuplicateProcess {
                app: app.name.clone(),
                process: process.name.clone(),
            });
        }
        check_process(app, process).map_err(|problem| PlanError::BadProcess {
            app: app.name.clone(),
            process: process.name.clone(),
            problem,
        })?;
    }
    Ok(())
}

/// Checks one process against its app.
fn check_process(app: &AppPlan, process: &ProcessPlan) -> Result<(), ProcessProblem> {
    if app.runner == RunnerKind::Host && process.run.is_empty() {
        return Err(ProcessProblem::EmptyCommand);
    }
    if let Some(port) = &process.port
        && !app.ports.contains_key(port)
    {
        return Err(ProcessProblem::UnknownPort(port.clone()));
    }
    if let ProcessKind::Oneshot { .. } = process.kind {
        if process.ready.is_some() {
            return Err(ProcessProblem::OneshotWithProbe);
        }
        if let Restart::Always(_) = process.restart {
            return Err(ProcessProblem::OneshotRestartsAlways);
        }
    }
    let Some(probe) = &process.ready else {
        return Ok(());
    };
    if probe.period.is_zero() || probe.timeout.is_zero() {
        return Err(ProcessProblem::ZeroProbeTime);
    }
    match &probe.check {
        ProbeCheck::Http { .. } if process.port.is_none() => {
            Err(ProcessProblem::HttpProbeWithoutPort)
        }
        ProbeCheck::Http { path } if !path.starts_with('/') => {
            Err(ProcessProblem::HttpPathWithoutSlash)
        }
        ProbeCheck::Tcp { port } if !app.ports.contains_key(port) => {
            Err(ProcessProblem::UnknownPort(port.clone()))
        }
        ProbeCheck::Command { run } if run.is_empty() => Err(ProcessProblem::EmptyProbeCommand),
        ProbeCheck::Http { .. } | ProbeCheck::Tcp { .. } | ProbeCheck::Command { .. } => Ok(()),
    }
}
