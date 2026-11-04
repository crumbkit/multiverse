//! The rules that [`Plan::new`](super::Plan::new) checks, and the errors it
//! returns when a rule fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use super::probe::ProbeCheck;
use super::restart::Restart;
use super::{AppPlan, ProcessKind, ProcessPlan, RunnerKind};
use crate::name::{AppName, PortName, ProcessName};
use crate::source::Source;

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
    /// The apps depend on each other in a cycle, so none of them can start
    /// first.
    DependencyCycle {
        /// The apps of the cycle, in dependency order, ending with the app
        /// it starts with.
        apps: Vec<AppName>,
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
    /// An `up --env` override names a variable that a process cannot
    /// receive.
    BadOverride {
        /// The app.
        app: AppName,
        /// The variable's name.
        name: String,
    },
    /// The app's container options cannot be used.
    BadContainer {
        /// The app.
        app: AppName,
        /// What is wrong with them.
        problem: ContainerProblem,
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

/// What is wrong with an app's container options.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerProblem {
    /// A host app has container options.
    OnHostApp,
    /// A bind mount's host directory is a relative path.
    RelativeHostPath(PathBuf),
    /// A bind mount or cache volume is mounted at a relative path inside the
    /// container.
    RelativeContainerPath(PathBuf),
    /// The run-as user is empty.
    EmptyUser,
}

/// What is wrong with a process in a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessProblem {
    /// A host process's command is empty.
    EmptyCommand,
    /// The process's working directory is absolute or leaves its app's
    /// directory.
    CwdOutsideApp,
    /// A literal value names a variable that a process cannot receive.
    BadVariableName(String),
    /// A literal value belongs to a variable that the app's permissions name
    /// as a secret.
    SecretInPlan(String),
    /// The process, or its probe, names a port its app does not have.
    UnknownPort(PortName),
    /// A host process publishes a port or mounts a volume, which only a
    /// container process does.
    ContainerFieldOnHost,
    /// A volume is mounted at a relative path inside the container.
    RelativeContainerPath(PathBuf),
    /// The process has an HTTP probe but no port.
    HttpProbeWithoutPort,
    /// The HTTP probe's path does not start with `/`.
    HttpPathWithoutSlash,
    /// The log probe's pattern is empty.
    EmptyLogPattern,
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
            Self::DependencyCycle { apps } => {
                formatter.write_str("the apps depend on each other in a cycle: ")?;
                for (index, app) in apps.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(" -> ")?;
                    }
                    write!(formatter, "{app}")?;
                }
                Ok(())
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
            Self::UnknownPort { app, port } => write!(
                formatter,
                "{app} may listen on {port}, which is not one of its ports"
            ),
            Self::BadOverride { app, name } => write!(
                formatter,
                "an override of {app} names the variable {name:?}, which a process cannot receive"
            ),
            Self::BadContainer { app, problem } => {
                write!(
                    formatter,
                    "the container options of {app} cannot be used because {problem}"
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

impl fmt::Display for ContainerProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OnHostApp => formatter.write_str("the app runs on the host"),
            Self::RelativeHostPath(path) => {
                write!(formatter, "the host path {} is relative", path.display())
            }
            Self::RelativeContainerPath(path) => {
                write!(
                    formatter,
                    "the container path {} is relative",
                    path.display()
                )
            }
            Self::EmptyUser => formatter.write_str("the run-as user is empty"),
        }
    }
}

impl fmt::Display for ProcessProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand => formatter.write_str("its command is empty"),
            Self::CwdOutsideApp => {
                formatter.write_str("its working directory is outside its app's directory")
            }
            Self::BadVariableName(name) => {
                write!(
                    formatter,
                    "it sets the variable {name:?}, which a process cannot receive"
                )
            }
            Self::SecretInPlan(name) => {
                write!(formatter, "the plan holds a value of the secret {name}")
            }
            Self::UnknownPort(port) => write!(formatter, "it names the unknown port {port}"),
            Self::ContainerFieldOnHost => {
                formatter.write_str("it runs on the host but publishes a port or mounts a volume")
            }
            Self::RelativeContainerPath(path) => {
                write!(
                    formatter,
                    "it mounts a volume at the relative path {}",
                    path.display()
                )
            }
            Self::HttpProbeWithoutPort => formatter.write_str("it has an http probe but no port"),
            Self::HttpPathWithoutSlash => {
                formatter.write_str("its http probe's path does not start with `/`")
            }
            Self::EmptyLogPattern => formatter.write_str("its log probe's pattern is empty"),
            Self::EmptyProbeCommand => formatter.write_str("its probe's command is empty"),
            Self::ZeroProbeTime => formatter.write_str("its probe's period or timeout is zero"),
            Self::OneshotWithProbe => formatter.write_str("it is a oneshot with a probe"),
            Self::OneshotRestartsAlways => {
                formatter.write_str("it is a oneshot that restarts always")
            }
        }
    }
}

/// Reports whether a process can receive a variable named `name`: 1 or more
/// ASCII letters, digits and `_`, starting with a letter or `_`.
fn is_variable_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Checks the rules of [`Plan::new`](super::Plan::new).
pub(super) fn check_apps(apps: &[AppPlan]) -> Result<(), PlanError> {
    let mut by_name = BTreeMap::new();
    for app in apps {
        if by_name.insert(&app.name, app).is_some() {
            return Err(PlanError::DuplicateApp {
                app: app.name.clone(),
            });
        }
    }
    for app in apps {
        if let Some(depends) = app
            .depends
            .iter()
            .find(|depends| !by_name.contains_key(depends))
        {
            return Err(PlanError::UnknownDependency {
                app: app.name.clone(),
                depends: depends.clone(),
            });
        }
    }
    let mut done = BTreeSet::new();
    for app in apps {
        let mut path = Vec::new();
        if let Some(apps) = find_cycle(&app.name, &by_name, &mut path, &mut done) {
            return Err(PlanError::DependencyCycle { apps });
        }
    }
    for app in apps {
        check_app(app)?;
    }
    Ok(())
}

/// Follows the dependencies of `name` depth first, and returns the first
/// cycle it finds. `path` holds the apps from the walk's start to `name`,
/// and `done` holds the apps whose dependencies hold no cycle. The walk's
/// depth is at most the number of apps.
fn find_cycle<'a>(
    name: &'a AppName,
    by_name: &BTreeMap<&'a AppName, &'a AppPlan>,
    path: &mut Vec<&'a AppName>,
    done: &mut BTreeSet<&'a AppName>,
) -> Option<Vec<AppName>> {
    if done.contains(name) {
        return None;
    }
    if let Some(start) = path.iter().position(|seen| *seen == name) {
        let mut cycle: Vec<AppName> = path
            .iter()
            .skip(start)
            .map(|seen| (*seen).clone())
            .collect();
        cycle.push(name.clone());
        return Some(cycle);
    }
    path.push(name);
    if let Some(app) = by_name.get(name) {
        for depends in &app.depends {
            if let Some(cycle) = find_cycle(depends, by_name, path, done) {
                return Some(cycle);
            }
        }
    }
    path.pop();
    done.insert(name);
    None
}

/// Checks one app's source, ports, overrides, container options and
/// processes.
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
    if let Some(name) = app.overrides.iter().find(|name| !is_variable_name(name)) {
        return Err(PlanError::BadOverride {
            app: app.name.clone(),
            name: name.clone(),
        });
    }
    check_container(app).map_err(|problem| PlanError::BadContainer {
        app: app.name.clone(),
        problem,
    })?;
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

/// Checks an app's container options.
fn check_container(app: &AppPlan) -> Result<(), ContainerProblem> {
    let options = &app.container;
    if app.runner == RunnerKind::Host {
        let empty =
            options.mounts.is_empty() && options.caches.is_empty() && options.user.is_none();
        return if empty {
            Ok(())
        } else {
            Err(ContainerProblem::OnHostApp)
        };
    }
    for mount in &options.mounts {
        if !mount.source.is_absolute() {
            return Err(ContainerProblem::RelativeHostPath(mount.source.clone()));
        }
    }
    let targets = options
        .mounts
        .iter()
        .map(|mount| &mount.target)
        .chain(options.caches.values());
    for target in targets {
        if !target.is_absolute() {
            return Err(ContainerProblem::RelativeContainerPath(target.clone()));
        }
    }
    if options.user.as_ref().is_some_and(String::is_empty) {
        return Err(ContainerProblem::EmptyUser);
    }
    Ok(())
}

/// Reports whether `path` is relative and stays within the directory it is
/// relative to.
fn stays_inside(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

/// Checks one process against its app.
fn check_process(app: &AppPlan, process: &ProcessPlan) -> Result<(), ProcessProblem> {
    if app.runner == RunnerKind::Host && process.run.is_empty() {
        return Err(ProcessProblem::EmptyCommand);
    }
    if process.cwd.as_deref().is_some_and(|cwd| !stays_inside(cwd)) {
        return Err(ProcessProblem::CwdOutsideApp);
    }
    for name in process.env.keys() {
        if !is_variable_name(name) {
            return Err(ProcessProblem::BadVariableName(name.clone()));
        }
        if app.permissions.secrets.contains(name) {
            return Err(ProcessProblem::SecretInPlan(name.clone()));
        }
    }
    check_ports(app, process)?;
    if let ProcessKind::Oneshot { .. } = process.kind {
        if process.ready.is_some() {
            return Err(ProcessProblem::OneshotWithProbe);
        }
        if let Restart::Always(_) = process.restart {
            return Err(ProcessProblem::OneshotRestartsAlways);
        }
    }
    check_probe(app, process)
}

/// Checks a process's listen port, published ports and volumes.
fn check_ports(app: &AppPlan, process: &ProcessPlan) -> Result<(), ProcessProblem> {
    let named = process.port.iter().chain(process.publish.keys());
    if let Some(port) = named
        .into_iter()
        .find(|port| !app.ports.contains_key(*port))
    {
        return Err(ProcessProblem::UnknownPort(port.clone()));
    }
    if app.runner == RunnerKind::Host {
        if process.publish.is_empty() && process.volumes.is_empty() {
            return Ok(());
        }
        return Err(ProcessProblem::ContainerFieldOnHost);
    }
    if let Some(target) = process
        .volumes
        .values()
        .find(|target| !target.is_absolute())
    {
        return Err(ProcessProblem::RelativeContainerPath(target.clone()));
    }
    Ok(())
}

/// Checks a process's readiness probe.
fn check_probe(app: &AppPlan, process: &ProcessPlan) -> Result<(), ProcessProblem> {
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
        ProbeCheck::Log { pattern } if pattern.is_empty() => Err(ProcessProblem::EmptyLogPattern),
        ProbeCheck::Command { run } if run.is_empty() => Err(ProcessProblem::EmptyProbeCommand),
        ProbeCheck::Http { .. }
        | ProbeCheck::Tcp { .. }
        | ProbeCheck::Log { .. }
        | ProbeCheck::Command { .. } => Ok(()),
    }
}
