//! The rules that [`Plan::new`](super::Plan::new) checks, and the errors it
//! returns when a rule fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use super::probe::ProbeCheck;
use super::restart::Restart;
use super::{AppPlan, MOST_READY_TIMEOUT, ProcessKind, ProcessPlan, RunnerKind};
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
    /// The processes of the app depend on each other in a cycle, so none of
    /// them can start first.
    ProcessCycle {
        /// The app.
        app: AppName,
        /// The processes of the cycle, in dependency order, ending with the
        /// process it starts with.
        processes: Vec<ProcessName>,
    },
    /// The app's source cannot be used.
    BadSource {
        /// The app.
        app: AppName,
        /// What is wrong with it.
        problem: SourceProblem,
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
    /// An `up --env` override names a variable that the app's permissions
    /// name as a secret. An override is never a secret.
    SecretOverride {
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

/// What is wrong with an app's source.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceProblem {
    /// The checkout's directory is a relative path.
    RelativeCheckout(PathBuf),
    /// The build's output is absolute or leaves the checkout.
    OutputOutsideCheckout(PathBuf),
    /// The release's program is absolute or leaves the asset.
    BinaryOutsideAsset(PathBuf),
    /// The release's asset name holds a `/`.
    AssetWithSlash(String),
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
    /// The command names `{program}`, but the app runs from a checkout
    /// without a build, so it has no program.
    ProgramWithoutBuild,
    /// The process depends on a process its app does not have.
    UnknownDependency(ProcessName),
    /// The process receives a variable whose name a process cannot receive.
    BadVariableName(String),
    /// The ready timeout is zero or longer than
    /// [`MOST_READY_TIMEOUT`](super::MOST_READY_TIMEOUT).
    ReadyTimeoutOutOfRange,
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
                write_chain(formatter, apps)
            }
            Self::ProcessCycle { app, processes } => {
                write!(
                    formatter,
                    "the processes of {app} depend on each other in a cycle: "
                )?;
                write_chain(formatter, processes)
            }
            Self::BadSource { app, problem } => {
                write!(
                    formatter,
                    "the source of {app} cannot be used because {problem}"
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
            Self::UnknownPort { app, port } => write!(
                formatter,
                "{app} may listen on {port}, which is not one of its ports"
            ),
            Self::BadOverride { app, name } => write!(
                formatter,
                "an override of {app} names the variable {name:?}, which a process cannot receive"
            ),
            Self::SecretOverride { app, name } => write!(
                formatter,
                "an override of {app} sets the secret {name}, and an override is never a secret"
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

/// Writes `names` joined by ` -> `.
fn write_chain<N: fmt::Display>(formatter: &mut fmt::Formatter<'_>, names: &[N]) -> fmt::Result {
    for (index, name) in names.iter().enumerate() {
        if index > 0 {
            formatter.write_str(" -> ")?;
        }
        write!(formatter, "{name}")?;
    }
    Ok(())
}

impl fmt::Display for SourceProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeCheckout(dir) => {
                write!(
                    formatter,
                    "the checkout {} is a relative path",
                    dir.display()
                )
            }
            Self::OutputOutsideCheckout(path) => {
                write!(
                    formatter,
                    "the build output {} is outside the checkout",
                    path.display()
                )
            }
            Self::BinaryOutsideAsset(path) => {
                write!(
                    formatter,
                    "the program {} is outside the asset",
                    path.display()
                )
            }
            Self::AssetWithSlash(asset) => write!(formatter, "the asset {asset:?} holds a `/`"),
        }
    }
}

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
            Self::ProgramWithoutBuild => {
                formatter.write_str("it names `{program}` but its checkout has no build")
            }
            Self::UnknownDependency(process) => {
                write!(formatter, "it depends on {process}, which its app lacks")
            }
            Self::BadVariableName(name) => write!(
                formatter,
                "it receives the variable {name:?}, which a process cannot receive"
            ),
            Self::ReadyTimeoutOutOfRange => {
                formatter.write_str("its ready timeout is zero or longer than an hour")
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
    let graph = apps
        .iter()
        .map(|app| (&app.name, app.depends.as_slice()))
        .collect();
    if let Some(apps) = find_cycle(&graph) {
        return Err(PlanError::DependencyCycle { apps });
    }
    for app in apps {
        check_app(app)?;
    }
    Ok(())
}

/// Returns the first cycle in `graph`, which maps each name to the names it
/// depends on, as the names of the cycle in dependency order, ending with
/// the name it starts with. Every name that `graph` depends on is one of its
/// keys. The walk is depth first, with an explicit stack in place of
/// recursion, so a long chain of dependencies cannot overflow the call
/// stack.
fn find_cycle<'a, N: Ord + Clone>(graph: &BTreeMap<&'a N, &'a [N]>) -> Option<Vec<N>> {
    // `done` holds the names whose dependencies hold no cycle.
    let mut done: BTreeSet<&N> = BTreeSet::new();
    for &start in graph.keys() {
        if done.contains(start) {
            continue;
        }
        // Each entry is a name on the current path and the index of its next
        // dependency to visit.
        let mut stack: Vec<(&N, usize)> = vec![(start, 0)];
        while let Some(&(name, next)) = stack.last() {
            let depends = graph.get(name).copied().unwrap_or_default();
            let Some(depend) = depends.get(next) else {
                done.insert(name);
                stack.pop();
                continue;
            };
            if let Some(top) = stack.last_mut() {
                top.1 = next + 1;
            }
            if done.contains(depend) {
                continue;
            }
            if let Some(position) = stack.iter().position(|(seen, _)| *seen == depend) {
                let mut cycle: Vec<N> = stack
                    .iter()
                    .skip(position)
                    .map(|(seen, _)| (*seen).clone())
                    .collect();
                cycle.push(depend.clone());
                return Some(cycle);
            }
            stack.push((depend, 0));
        }
    }
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
    check_source(&app.source).map_err(|problem| PlanError::BadSource {
        app: app.name.clone(),
        problem,
    })?;
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
    if let Some(name) = app
        .overrides
        .iter()
        .find(|name| app.permissions.secrets.contains(*name))
    {
        return Err(PlanError::SecretOverride {
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
    let graph = app
        .processes
        .iter()
        .map(|process| (&process.name, process.depends.as_slice()))
        .collect();
    if let Some(processes) = find_cycle(&graph) {
        return Err(PlanError::ProcessCycle {
            app: app.name.clone(),
            processes,
        });
    }
    Ok(())
}

/// Checks an app's source's paths and asset name.
fn check_source(source: &Source) -> Result<(), SourceProblem> {
    match source {
        Source::Checkout { dir, build } => {
            if !dir.is_absolute() {
                return Err(SourceProblem::RelativeCheckout(dir.clone()));
            }
            match build {
                Some(build) if !stays_inside(&build.output) => {
                    Err(SourceProblem::OutputOutsideCheckout(build.output.clone()))
                }
                Some(_) | None => Ok(()),
            }
        }
        Source::Release { spec, .. } => {
            if !stays_inside(&spec.binary) {
                return Err(SourceProblem::BinaryOutsideAsset(spec.binary.clone()));
            }
            if spec.asset.contains('/') {
                return Err(SourceProblem::AssetWithSlash(spec.asset.clone()));
            }
            Ok(())
        }
        Source::Image { .. } => Ok(()),
    }
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
    let no_program = matches!(app.source, Source::Checkout { build: None, .. });
    if no_program && process.run.iter().any(|part| part.contains("{program}")) {
        return Err(ProcessProblem::ProgramWithoutBuild);
    }
    if let Some(depend) = process
        .depends
        .iter()
        .find(|depend| !app.processes.iter().any(|other| &other.name == *depend))
    {
        return Err(ProcessProblem::UnknownDependency(depend.clone()));
    }
    if process.cwd.as_deref().is_some_and(|cwd| !stays_inside(cwd)) {
        return Err(ProcessProblem::CwdOutsideApp);
    }
    if let Some(name) = process.env.iter().find(|name| !is_variable_name(name)) {
        return Err(ProcessProblem::BadVariableName(name.clone()));
    }
    if process.ready_timeout.is_zero() || process.ready_timeout > MOST_READY_TIMEOUT {
        return Err(ProcessProblem::ReadyTimeoutOutOfRange);
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
