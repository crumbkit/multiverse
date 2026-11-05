//! The rules that [`Plan::new`](super::Plan::new) checks, and the errors it
//! returns when a rule fails.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use super::probe::{
    BadLogPattern, LEAST_PUBLISHED_PROBE_TIMEOUT, LogPattern, ProbeCheck, is_sendable_path,
};
use super::restart::Restart;
use super::{AppPlan, MOST_READY_TIMEOUT, MOST_STOP_GRACE, ProcessKind, ProcessPlan, RunnerKind};
use crate::name::{AppName, PortName, ProcessName};
use crate::source::Source;

/// The reason that a set of apps is not a valid plan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanError {
    /// Two apps have this name.
    #[non_exhaustive]
    DuplicateApp {
        /// The name.
        app: AppName,
    },
    /// Two processes of the app have this name.
    #[non_exhaustive]
    DuplicateProcess {
        /// The app.
        app: AppName,
        /// The name.
        process: ProcessName,
    },
    /// The app depends on an app that the plan lacks.
    #[non_exhaustive]
    UnknownDependency {
        /// The app.
        app: AppName,
        /// The app it depends on.
        depends: AppName,
    },
    /// The apps depend on each other in a cycle, so none of them can start
    /// first.
    #[non_exhaustive]
    DependencyCycle {
        /// The apps of the cycle, in dependency order, ending with the app
        /// it starts with.
        apps: Vec<AppName>,
    },
    /// The processes of the app depend on each other in a cycle, so none of
    /// them can start first.
    #[non_exhaustive]
    ProcessCycle {
        /// The app.
        app: AppName,
        /// The processes of the cycle, in dependency order, ending with the
        /// process it starts with.
        processes: Vec<ProcessName>,
    },
    /// The app's source cannot be used.
    #[non_exhaustive]
    BadSource {
        /// The app.
        app: AppName,
        /// What is wrong with it.
        problem: SourceProblem,
    },
    /// The app's source does not suit its runner: a host app runs from an
    /// image, or a container app runs from a release or a checkout.
    #[non_exhaustive]
    RunnerMismatch {
        /// The app.
        app: AppName,
        /// The app's runner.
        runner: RunnerKind,
    },
    /// The app's permissions let it listen on a port it does not have.
    #[non_exhaustive]
    UnknownPort {
        /// The app.
        app: AppName,
        /// The port.
        port: PortName,
    },
    /// An `up --env` override names a variable that a process cannot
    /// receive.
    #[non_exhaustive]
    BadOverride {
        /// The app.
        app: AppName,
        /// The variable's name.
        name: String,
    },
    /// An `up --env` override names a variable that the app's permissions
    /// name as a secret. An override is never a secret.
    #[non_exhaustive]
    SecretOverride {
        /// The app.
        app: AppName,
        /// The variable's name.
        name: String,
    },
    /// The app's container options cannot be used.
    #[non_exhaustive]
    BadContainer {
        /// The app.
        app: AppName,
        /// What is wrong with them.
        problem: ContainerProblem,
    },
    /// The app's permissions cannot be enforced as written.
    #[non_exhaustive]
    BadPermissions {
        /// The app.
        app: AppName,
        /// What is wrong with them.
        problem: PermissionProblem,
    },
    /// A process of the app cannot run as planned.
    #[non_exhaustive]
    BadProcess {
        /// The app.
        app: AppName,
        /// The process.
        process: ProcessName,
        /// What is wrong with it.
        problem: ProcessProblem,
    },
}

/// What is wrong with an app's permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionProblem {
    /// `connect` names a host that is not an app of the plan. phase reaches
    /// external hosts only through an allowlist proxy, which is not built
    /// yet, so a sandboxed app may connect only to the apps it depends on.
    ExternalHost(String),
    /// `connect` names an app of the plan that the app does not depend on.
    /// A sandboxed app may connect only to the ports of the apps it
    /// depends on.
    NotADependency(AppName),
    /// A process listens on a port that the app's `listen` does not name,
    /// so the sandbox would refuse it.
    #[non_exhaustive]
    ListenNotGranted {
        /// The process.
        process: ProcessName,
        /// The port.
        port: PortName,
    },
    /// An entry of `write` is neither `$STATE`, an absolute path, nor a
    /// path that stays inside the app's checkout, or it holds a quote, a
    /// backslash or a control character, which the sandbox's profile cannot
    /// hold.
    BadWrite(String),
}

impl fmt::Display for PermissionProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExternalHost(host) => write!(
                formatter,
                "it may connect to {host:?}, which is not an app of the plan, and phase reaches external hosts only through its allowlist proxy, which is not built yet"
            ),
            Self::NotADependency(app) => write!(
                formatter,
                "it may connect to {app}, which it does not depend on, and a sandboxed app may connect only to the apps it depends on"
            ),
            Self::ListenNotGranted { process, port } => write!(
                formatter,
                "{process} listens on {port}, which its permissions do not let it listen on"
            ),
            Self::BadWrite(path) => write!(
                formatter,
                "it may write {path:?}, which is neither $STATE, an absolute path, nor a path inside its checkout that a sandbox profile can hold"
            ),
        }
    }
}

/// What is wrong with an app's source.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceProblem {
    /// The checkout's directory is a relative path.
    #[non_exhaustive]
    RelativeCheckout(PathBuf),
    /// The build's output is empty, absolute or leaves the checkout.
    #[non_exhaustive]
    OutputOutsideCheckout(PathBuf),
    /// The release's program is empty, absolute or leaves the asset.
    #[non_exhaustive]
    BinaryOutsideAsset(PathBuf),
    /// The release's asset name is empty, `.` or `..`, or holds a `/`.
    #[non_exhaustive]
    BadAssetName(String),
}

/// What is wrong with an app's container options.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContainerProblem {
    /// A host app has container options.
    OnHostApp,
    /// A bind mount's host directory is a relative path.
    #[non_exhaustive]
    RelativeHostPath(PathBuf),
    /// A bind mount or cache volume is mounted at a relative path inside the
    /// container.
    #[non_exhaustive]
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
    #[non_exhaustive]
    UnknownDependency(ProcessName),
    /// The process receives a variable whose name a process cannot receive.
    #[non_exhaustive]
    BadVariableName(String),
    /// The process marks a variable optional that it does not receive.
    #[non_exhaustive]
    UndeclaredOptional(String),
    /// The ready timeout is zero or longer than
    /// [`MOST_READY_TIMEOUT`].
    ReadyTimeoutOutOfRange,
    /// The stop grace is longer than [`MOST_STOP_GRACE`].
    StopGraceOutOfRange,
    /// A container process's `tcp` probe has a timeout shorter than
    /// [`LEAST_PUBLISHED_PROBE_TIMEOUT`], which leaves no time to tell the
    /// container's listener from Docker's port proxy.
    PublishedProbeTimeoutTooShort,
    /// The process, or its probe, names a port its app does not have.
    #[non_exhaustive]
    UnknownPort(PortName),
    /// A host process publishes a port, mounts a volume, asks for its
    /// secrets in its container's environment or names the owner of its
    /// secret files, which only a container process does.
    ContainerFieldOnHost,
    /// A volume is mounted at a relative path inside the container.
    #[non_exhaustive]
    RelativeContainerPath(PathBuf),
    /// The process's `secret_owner` is not a numeric `uid:gid`.
    #[non_exhaustive]
    BadSecretOwner(String),
    /// The process has an HTTP probe but no port.
    HttpProbeWithoutPort,
    /// A container process's HTTP or TCP probe asks a port that the
    /// process does not publish, so phase cannot reach it from the host.
    #[non_exhaustive]
    ProbePortUnpublished(PortName),
    /// The HTTP probe's path does not start with `/`.
    HttpPathWithoutSlash,
    /// The HTTP probe's path holds a space, a control character, a
    /// character outside ASCII or `#`, so phase cannot send it as written.
    HttpPathUnsendable,
    /// The log probe's pattern is empty.
    EmptyLogPattern,
    /// The log probe's pattern is not a regular expression.
    #[non_exhaustive]
    BadLogPattern {
        /// The regular expression compiler's one-line reason.
        reason: String,
    },
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
            Self::BadPermissions { app, problem } => write!(
                formatter,
                "the permissions of {app} cannot be enforced because {problem}"
            ),
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
            Self::BadAssetName(asset) => {
                write!(formatter, "the asset name {asset:?} does not name a file")
            }
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
            Self::UndeclaredOptional(name) => write!(
                formatter,
                "it marks the variable {name:?} optional, which it does not receive"
            ),
            Self::StopGraceOutOfRange => {
                formatter.write_str("its stop grace is longer than an hour")
            }
            Self::PublishedProbeTimeoutTooShort => formatter.write_str(
                "its container's tcp probe has a timeout shorter than one second",
            ),
            Self::ReadyTimeoutOutOfRange => {
                formatter.write_str("its ready timeout is zero or longer than an hour")
            }
            Self::UnknownPort(port) => write!(formatter, "it names the unknown port {port}"),
            Self::ContainerFieldOnHost => {
                formatter.write_str(
                "it runs on the host but publishes a port, mounts a volume or sets secret_delivery or secret_owner",
            )
            }
            Self::RelativeContainerPath(path) => {
                write!(
                    formatter,
                    "it mounts a volume at the relative path {}",
                    path.display()
                )
            }
            Self::BadSecretOwner(owner) => write!(
                formatter,
                "its secret_owner {owner:?} is not a numeric uid:gid such as 999:999"
            ),
            Self::HttpProbeWithoutPort => formatter.write_str("it has an http probe but no port"),
            Self::ProbePortUnpublished(port) => write!(
                formatter,
                "its probe asks the port {port}, which its container does not publish"
            ),
            Self::HttpPathWithoutSlash => {
                formatter.write_str("its http probe's path does not start with `/`")
            }
            Self::HttpPathUnsendable => formatter.write_str(
                "its http probe's path holds a space, a control character, a character outside ASCII or `#`",
            ),
            Self::EmptyLogPattern => formatter.write_str("its log probe's pattern is empty"),
            Self::BadLogPattern { reason } => write!(
                formatter,
                "its log probe's pattern is not a regular expression: {reason}"
            ),
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

/// Reports whether `owner` is a numeric `uid:gid`, such as `999:999`.
fn is_uid_gid(owner: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    owner
        .split_once(':')
        .is_some_and(|(uid, gid)| digits(uid) && digits(gid))
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
        check_permissions(app, |name| by_name.contains_key(name)).map_err(|problem| {
            PlanError::BadPermissions {
                app: app.name.clone(),
                problem,
            }
        })?;
    }
    Ok(())
}

/// Checks the permissions of the host `app`, whose plan holds the apps for
/// which `planned` is true: each `connect` names an app it depends on, each
/// of its processes listens only on a port it may listen on, and each
/// `write` is one the sandbox can hold. An app without permissions, and a
/// container app, pass.
fn check_permissions(
    app: &AppPlan,
    planned: impl Fn(&AppName) -> bool,
) -> Result<(), PermissionProblem> {
    let Some(permissions) = &app.permissions else {
        return Ok(());
    };
    // phase's sandbox holds only host apps. A container app's network is
    // Docker's, so its permissions are not enforced here yet.
    if app.runner != RunnerKind::Host {
        return Ok(());
    }
    for target in &permissions.connect {
        match target.parse::<AppName>() {
            Ok(name) if planned(&name) => {
                if !app.depends.contains(&name) {
                    return Err(PermissionProblem::NotADependency(name));
                }
            }
            _ => return Err(PermissionProblem::ExternalHost(target.clone())),
        }
    }
    for process in &app.processes {
        if let Some(port) = &process.port
            && !permissions.listen.contains(port)
        {
            return Err(PermissionProblem::ListenNotGranted {
                process: process.name.clone(),
                port: port.clone(),
            });
        }
    }
    if let Some(path) = permissions.write.iter().find(|path| !is_writable(path)) {
        return Err(PermissionProblem::BadWrite(path.clone()));
    }
    Ok(())
}

/// The entry of `write` that stands for the app's own state directory.
pub(crate) const STATE: &str = "$STATE";

/// Reports whether `path`, an entry of `write`, is `$STATE`, an absolute
/// path, or a relative path that stays inside the checkout, and holds no
/// quote, backslash or control character.
fn is_writable(path: &str) -> bool {
    if path == STATE {
        return true;
    }
    if path.is_empty()
        || path.starts_with('$')
        || path
            .chars()
            .any(|character| character == '"' || character == '\\' || character.is_control())
    {
        return false;
    }
    let path = Path::new(path);
    path.is_absolute()
        || path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
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
        .iter()
        .flat_map(|permissions| permissions.listen.iter())
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
    if let Some(name) = app.overrides.iter().find(|name| app.is_secret(name)) {
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
                Some(build) if !names_file_inside(&build.output) => {
                    Err(SourceProblem::OutputOutsideCheckout(build.output.clone()))
                }
                Some(_) | None => Ok(()),
            }
        }
        Source::Release { spec, .. } => {
            if !names_file_inside(&spec.binary) {
                return Err(SourceProblem::BinaryOutsideAsset(spec.binary.clone()));
            }
            let asset = spec.asset.as_str();
            if asset.is_empty() || asset == "." || asset == ".." || asset.contains('/') {
                return Err(SourceProblem::BadAssetName(spec.asset.clone()));
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

/// Reports whether `path` stays within the directory it is relative to and
/// names something below it, which `.` and an empty path do not.
fn names_file_inside(path: &Path) -> bool {
    stays_inside(path)
        && path
            .components()
            .any(|component| matches!(component, Component::Normal(_)))
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
    if let Some(name) = process
        .optional_env
        .iter()
        .find(|name| !process.env.contains(*name))
    {
        return Err(ProcessProblem::UndeclaredOptional(name.clone()));
    }
    if let Some(owner) = process
        .secret_owner
        .as_ref()
        .filter(|owner| !is_uid_gid(owner))
    {
        return Err(ProcessProblem::BadSecretOwner(owner.clone()));
    }
    if process.ready_timeout.is_zero() || process.ready_timeout > MOST_READY_TIMEOUT {
        return Err(ProcessProblem::ReadyTimeoutOutOfRange);
    }
    if process.stop.grace > MOST_STOP_GRACE {
        return Err(ProcessProblem::StopGraceOutOfRange);
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
        if process.publish.is_empty()
            && process.volumes.is_empty()
            && process.secret_delivery.is_file()
            && process.secret_owner.is_none()
        {
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

/// Returns the port that the HTTP or TCP probe `check` of the container
/// process `process` asks and that the process does not publish. phase
/// reaches a container's port only through its published host port.
fn unpublished_probe_port(
    app: &AppPlan,
    process: &ProcessPlan,
    check: &ProbeCheck,
) -> Option<PortName> {
    if app.runner != RunnerKind::Container {
        return None;
    }
    let port = match check {
        ProbeCheck::Http { .. } => process.port.as_ref()?,
        ProbeCheck::Tcp { port } => port,
        ProbeCheck::Command { .. } | ProbeCheck::Log { .. } => return None,
    };
    (!process.publish.contains_key(port)).then(|| port.clone())
}

/// Checks a process's readiness probe.
fn check_probe(app: &AppPlan, process: &ProcessPlan) -> Result<(), ProcessProblem> {
    let Some(probe) = &process.ready else {
        return Ok(());
    };
    if probe.period.is_zero() || probe.timeout.is_zero() {
        return Err(ProcessProblem::ZeroProbeTime);
    }
    check_probe_check(app, process, &probe.check)?;
    if let Some(port) = unpublished_probe_port(app, process, &probe.check) {
        return Err(ProcessProblem::ProbePortUnpublished(port));
    }
    if app.runner == RunnerKind::Container
        && matches!(probe.check, ProbeCheck::Tcp { .. })
        && probe.timeout < LEAST_PUBLISHED_PROBE_TIMEOUT
    {
        return Err(ProcessProblem::PublishedProbeTimeoutTooShort);
    }
    Ok(())
}

/// Checks what one attempt of a process's readiness probe checks.
fn check_probe_check(
    app: &AppPlan,
    process: &ProcessPlan,
    check: &ProbeCheck,
) -> Result<(), ProcessProblem> {
    match check {
        ProbeCheck::Http { .. } if process.port.is_none() => {
            Err(ProcessProblem::HttpProbeWithoutPort)
        }
        ProbeCheck::Http { path } if !path.starts_with('/') => {
            Err(ProcessProblem::HttpPathWithoutSlash)
        }
        ProbeCheck::Http { path } if !is_sendable_path(path) => {
            Err(ProcessProblem::HttpPathUnsendable)
        }
        ProbeCheck::Tcp { port } if !app.ports.contains_key(port) => {
            Err(ProcessProblem::UnknownPort(port.clone()))
        }
        ProbeCheck::Log { pattern } => match LogPattern::new(pattern) {
            Ok(_) => Ok(()),
            Err(BadLogPattern::Empty) => Err(ProcessProblem::EmptyLogPattern),
            Err(BadLogPattern::Invalid(reason)) => Err(ProcessProblem::BadLogPattern { reason }),
        },
        ProbeCheck::Command { run } if run.is_empty() => Err(ProcessProblem::EmptyProbeCommand),
        ProbeCheck::Http { .. } | ProbeCheck::Tcp { .. } | ProbeCheck::Command { .. } => Ok(()),
    }
}
