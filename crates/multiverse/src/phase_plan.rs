//! The plan that phase runs a workspace's app from, built from the
//! workspace's manifest, its record's stable id and its ports in the port
//! ledger, and the environment source that gives each process its values.
//!
//! A unit is one entry of the manifest's `[processes.*]` or
//! `[containers.*]` tables, and that is the word mxv shows its users.
//! Each unit is one phase app, named after the unit, and each app has one
//! phase process, named [`MAIN`]. A unit's `depends_on` becomes its app's
//! `depends`. phase names a container app's compose project
//! `phase-<hash>-<unit>`, where `<hash>` is 16 hex digits of a hash of the
//! workspace's stable id, and its container `phase-<hash>-<unit>.main`.
//!
//! phase starts a host process with only `PATH` and `HOME` of the caller's
//! environment, so the plan declares by name every other variable that a
//! process receives, and [`ManifestEnvironment`] resolves each name. A host
//! process receives its manifest `env`, its port variables, and the
//! variables of [`PASSED_THROUGH`] that the caller's environment holds. A
//! container receives only its manifest `env`. The plan declares every
//! variable of [`PASSED_THROUGH`] for each host process, and marks each one
//! that the manifest does not set as optional, so phase starts the process
//! without the ones that the caller's environment lacks. The plan, and its
//! digest, are therefore the same whichever shell runs `mxv up`.
//!
//! A manifest with services adds a container app for each backend, named
//! and published as [`crate::service::Backend::app`], and the host oneshot
//! `mxv-provision`, which depends on the backends. A process that needs a
//! service depends on the service's backend and on `mxv-provision`, and
//! receives the service's variables. No variable of the caller's
//! environment whose name starts with `AWS_` is in [`PASSED_THROUGH`], so
//! such a process receives only the AWS variables that mxv sets.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::num::NonZeroU16;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use phase::{
    AppName, AppPlan, AppliedEnvironment, EnvironmentError, EnvironmentSource, ImageReference,
    Plan, PortName, Probe, ProbeCheck, ProcessName, ProcessPlan, RunnerKind, Source, VolumeName,
    WorkspaceId,
};

use crate::manifest::{Container, Manifest, Process, ReadyCheck, Unit};
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::redacted::Redacted;

mod services;
#[cfg(test)]
mod tests;

pub(crate) use services::{BACKEND_READY_TIMEOUT, PROVISION_TIMEOUT};
use services::{service_apps, service_connects, service_depends};

/// The name of the one process of each unit's app.
pub const MAIN: &str = "main";

/// The variables of the caller's environment that a host process receives
/// when the caller's environment holds them, on top of `PATH` and `HOME`,
/// which phase passes itself. They cover the locale, the terminal, the
/// user, the temporary and XDG directories, and the tool managers that
/// `cargo run`, `pnpm run dev` and `sh -c` rely on. The locale variables
/// are `LANG`, `LANGUAGE` and the standard `LC_` categories; another
/// variable whose name starts with `LC_`, such as `LC_TERMINAL`, does not
/// pass, because the plan names each variable. Credentials, such as cloud
/// keys and tokens, and build overrides, such as `RUSTFLAGS` or
/// `CARGO_TARGET_DIR`, do not pass.
pub const PASSED_THROUGH: &[&str] = &[
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_ADDRESS",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NAME",
    "LC_NUMERIC",
    "LC_PAPER",
    "LC_TELEPHONE",
    "LC_TIME",
    "TZ",
    "TERM",
    "COLORTERM",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "NVM_DIR",
    "PNPM_HOME",
    "COREPACK_HOME",
    "NPM_CONFIG_PREFIX",
    "NODE_EXTRA_CA_CERTS",
    "NODE_USE_SYSTEM_CA",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "MISE_DATA_DIR",
    "MISE_CONFIG_DIR",
    "MISE_CACHE_DIR",
    "SSH_AUTH_SOCK",
    "RUST_LOG",
    "RUST_BACKTRACE",
];

/// The variables that phase gives every host process from the caller's
/// environment itself. A manifest may not set them.
pub const PHASE_SETS: &[&str] = &["PATH", "HOME"];

/// Reports whether `name` is a variable that chooses the docker program,
/// its configuration or its daemon, which phase's `docker exec` receives.
fn chooses_docker(name: &str) -> bool {
    name.starts_with("DOCKER_")
}

/// Reports whether a host process receives the caller's variable `name`.
fn passes(name: &str) -> bool {
    PASSED_THROUGH.contains(&name)
}

/// The environment that mxv's `main` read, which phase's keepers and the
/// processes they start receive part of: `PATH` and `HOME`, and the
/// variables of [`PASSED_THROUGH`]. It also keeps the variables whose names
/// start with `DOCKER_`, which only phase's `docker exec` receives. A
/// variable whose name or value is not UTF-8 is left out.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CallerEnvironment {
    /// The variables, by name.
    vars: BTreeMap<String, Redacted>,
}

impl std::fmt::Debug for CallerEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallerEnvironment")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl CallerEnvironment {
    /// Returns the environment of `vars`, such as `std::env::vars_os()`,
    /// keeping only the variables that phase or a host process receives.
    pub fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        let vars = vars
            .into_iter()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, _)| {
                passes(name) || chooses_docker(name) || PHASE_SETS.contains(&name.as_str())
            })
            .map(|(name, value)| (name, Redacted::new(value)))
            .collect();
        Self { vars }
    }

    /// Returns `PATH` and `HOME`, for phase's inherited variables.
    pub(crate) fn inherited(&self) -> BTreeMap<String, String> {
        PHASE_SETS
            .iter()
            .filter_map(|name| {
                self.vars
                    .get(*name)
                    .map(|value| ((*name).to_owned(), value.reveal().to_owned()))
            })
            .collect()
    }

    /// Returns `PATH`, `HOME` and the variables whose names start with
    /// `DOCKER_`, as the only environment of phase's `docker exec`.
    pub(crate) fn docker_environment(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .filter(|(name, _)| chooses_docker(name) || PHASE_SETS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.reveal().to_owned()))
            .collect()
    }

    /// Returns the variables that a host process receives, by name.
    fn passed(&self) -> impl Iterator<Item = (&String, &Redacted)> {
        self.vars.iter().filter(|(name, _)| passes(name))
    }
}

/// A manifest that phase cannot run, though it parses: what is wrong, in a
/// sentence that names the unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unrunnable {
    /// The process or container, or `None` when the problem is the whole
    /// plan's.
    pub unit: Option<String>,
    /// What is wrong.
    pub message: String,
}

/// The plan of a workspace's app and the values of each app's variables.
pub(crate) struct Built {
    /// The plan.
    pub(crate) plan: Plan,
    /// The values that each app's process receives.
    pub(crate) environment: ManifestEnvironment,
}

/// What [`build`] builds the plan from.
pub(crate) struct Inputs<'a> {
    /// The manifest.
    pub(crate) manifest: &'a Manifest,
    /// The workspace's name.
    pub(crate) workspace: &'a WorkspaceName,
    /// The workspace's stable id.
    pub(crate) id: &'a WorkspaceId,
    /// The checkout that holds the manifest, an absolute path. A process
    /// whose `cwd` is relative runs from it.
    pub(crate) checkout: &'a Path,
    /// The directories that a process whose `cwd` is absolute may run in,
    /// which [`roots`] lists for a workspace: a merged manifest gives each
    /// process the absolute directory of its checkout or of the workspace.
    /// Such a process's source is the root that holds its `cwd`, and its
    /// `cwd` becomes the rest of the path.
    pub(crate) roots: Vec<PathBuf>,
    /// The workspace's own directory, which holds its checkouts, its
    /// manifest and its `AGENTS.md`, or `None` when the plan is built
    /// without a workspace on disk.
    pub(crate) workspace_dir: Option<&'a Path>,
    /// The workspace's host ports, by port name.
    pub(crate) ports: &'a BTreeMap<String, u16>,
    /// How long a process gets after its stop signal.
    pub(crate) grace: Duration,
    /// The caller's environment.
    pub(crate) caller: &'a CallerEnvironment,
    /// The command that provisions the services, such as the mxv program
    /// and `phase provision`, to which the plan adds the resources as one
    /// JSON argument, or `None` when it is unknown. A manifest with
    /// services cannot run without it.
    pub(crate) provisioner: Option<Vec<String>>,
}

/// Returns the command that a manifest's check builds its plan with, which
/// stands in for the mxv program that `up` names.
pub(crate) fn checking_provisioner() -> Vec<String> {
    vec![
        "/mxv".to_owned(),
        "phase".to_owned(),
        "provision".to_owned(),
    ]
}

/// Returns the directories that a process of `workspace`'s merged manifest
/// may run in: each checkout's directory, then the workspace's own, so the
/// checkout that holds a process's directory is found before the
/// workspace that holds every checkout.
pub(crate) fn roots(workspace: &Workspace) -> Vec<PathBuf> {
    workspace
        .checkouts
        .iter()
        .map(|checkout| checkout.path.clone())
        .chain(std::iter::once(workspace.path.clone()))
        .collect()
}

/// Returns the source directory of `process`, named `name`, and the
/// directory it runs in relative to that source, or `None` when it runs at
/// the source's top.
///
/// # Errors
///
/// Returns [`Unrunnable`] when the process's `cwd` is absolute and lies in
/// none of `inputs.roots`.
fn placed(
    inputs: &Inputs<'_>,
    name: &str,
    process: &Process,
) -> Result<(PathBuf, Option<PathBuf>), Unrunnable> {
    let relative =
        |cwd: &Path| (cwd != Path::new(".") && cwd != Path::new("")).then(|| cwd.to_path_buf());
    if !process.cwd.is_absolute() {
        return Ok((inputs.checkout.to_path_buf(), relative(&process.cwd)));
    }
    // `api/../web` in the workspace manifest runs in the checkout `web`, so
    // the path is resolved before its root is chosen.
    let cwd = lexical(&process.cwd);
    inputs
        .roots
        .iter()
        .find_map(|root| {
            cwd.strip_prefix(lexical(root))
                .ok()
                .map(|rest| (root.clone(), relative(rest)))
        })
        .ok_or_else(|| Unrunnable {
            unit: Some(name.to_owned()),
            message: format!(
                "the process `{name}` runs in {}, which is in none of the workspace's checkouts",
                process.cwd.display()
            ),
        })
}

/// Returns `path` with each `.` dropped and each `..` removing the name
/// before it, without reading the file system. A `..` at the root stays at
/// the root.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    resolved
}

/// Returns the plan of every process and container of `inputs.manifest`,
/// and the values of their variables.
///
/// # Errors
///
/// Returns [`Unrunnable`] when a unit's name is not an app name, an image
/// is not one that phase pins, or the apps break one of phase's plan
/// rules.
pub(crate) fn build(inputs: &Inputs<'_>) -> Result<Built, Unrunnable> {
    let mut apps = Vec::new();
    let mut values = BTreeMap::new();
    for (plan, vars) in service_apps(inputs)? {
        values.insert(plan.name.clone(), vars);
        apps.push(plan);
    }
    for name in inputs.manifest.start_order(&[]).unwrap_or_default() {
        let app = app_name(&name)?;
        let (plan, vars) = match inputs.manifest.unit(&name) {
            Some(Unit::Process(process)) => host_app(inputs, &name, &app, process)?,
            Some(Unit::Container(container)) => container_app(inputs, &name, &app, container)?,
            None => continue,
        };
        apps.push(plan);
        values.insert(app, vars);
    }
    let mut plan =
        Plan::new(inputs.workspace.clone(), inputs.id.clone(), apps).map_err(|error| {
            Unrunnable {
                unit: None,
                message: format!("phase cannot run the manifest: {error}"),
            }
        })?;
    // A sandboxed app may use only its own checkout of the workspace, and
    // never the workspace manifest or `AGENTS.md`, which lie in the
    // workspace's own directory.
    if let Some(dir) = inputs.workspace_dir {
        plan = plan.with_workspace_dir(
            dir.to_path_buf(),
            vec![
                dir.join(crate::manifest::MANIFEST_FILE),
                dir.join(crate::record::AGENTS_FILE),
            ],
        );
    }
    Ok(Built {
        plan,
        environment: ManifestEnvironment { values },
    })
}

/// Returns the app name of the unit `name`.
///
/// # Errors
///
/// Returns [`Unrunnable`] when `name` is not an app name, such as one with
/// an uppercase letter.
pub(crate) fn app_name(name: &str) -> Result<AppName, Unrunnable> {
    AppName::try_from(name.to_owned()).map_err(|error| Unrunnable {
        unit: Some(name.to_owned()),
        message: format!(
            "`{name}` is not a name phase can run, which holds 1 to 64 lowercase letters, digits, '-' and '_' and starts with a letter or a digit: {error}"
        ),
    })
}

/// Returns the process name [`MAIN`].
fn main_process() -> Result<ProcessName, Unrunnable> {
    ProcessName::try_from(MAIN.to_owned()).map_err(|error| Unrunnable {
        unit: None,
        message: format!("{MAIN}: {error}"),
    })
}

/// Returns the phase port name of the port `port` of `unit`.
fn port_name(unit: &str, port: &str) -> Result<PortName, Unrunnable> {
    PortName::try_from(port.to_owned()).map_err(|error| Unrunnable {
        unit: Some(unit.to_owned()),
        message: format!("the port `{port}` of `{unit}` is not a port name phase accepts: {error}"),
    })
}

/// Returns the app ports of `unit`: each of `names` with the host port
/// that the workspace holds for it.
fn app_ports<'n>(
    inputs: &Inputs<'_>,
    unit: &str,
    names: impl IntoIterator<Item = &'n str>,
) -> Result<BTreeMap<PortName, NonZeroU16>, Unrunnable> {
    let mut ports = BTreeMap::new();
    for name in names {
        let number = inputs
            .ports
            .get(name)
            .copied()
            .and_then(NonZeroU16::new)
            .ok_or_else(|| Unrunnable {
                unit: Some(unit.to_owned()),
                message: format!(
                    "the workspace holds no host port for the port `{name}` of `{unit}`"
                ),
            })?;
        ports.insert(port_name(unit, name)?, number);
    }
    Ok(ports)
}

/// Returns the apps that the unit `name` depends on.
fn depends(depends_on: &[String]) -> Result<Vec<AppName>, Unrunnable> {
    depends_on.iter().map(|name| app_name(name)).collect()
}

/// Returns the plan of the host process `process`, named `name`, and the
/// values of its variables.
fn host_app(
    inputs: &Inputs<'_>,
    name: &str,
    app: &AppName,
    process: &Process,
) -> Result<(AppPlan, Vec<(String, Redacted)>), Unrunnable> {
    if let Some(variable) = process
        .env
        .keys()
        .chain(process.ports.keys())
        .find(|variable| PHASE_SETS.contains(&variable.as_str()))
    {
        return Err(Unrunnable {
            unit: Some(name.to_owned()),
            message: format!(
                "the process `{name}` sets {variable}, which phase gives every process from mxv's own environment"
            ),
        });
    }
    let mut vars: BTreeMap<String, Redacted> = inputs
        .caller
        .passed()
        .map(|(variable, value)| (variable.clone(), value.clone()))
        .collect();
    for (variable, value) in &process.env {
        vars.insert(variable.clone(), value.render(inputs.ports));
    }
    // No variable of the caller's environment whose name starts with
    // `AWS_` passes, so a process that needs a service receives only the
    // AWS variables that mxv sets here.
    let serviced = inputs.manifest.service_variables(process, inputs.ports);
    let mut set: BTreeSet<&str> = process
        .env
        .keys()
        .chain(process.ports.keys())
        .map(String::as_str)
        .collect();
    set.extend(serviced.keys().map(String::as_str));
    for (variable, value) in &serviced {
        vars.insert(variable.clone(), Redacted::new(value.clone()));
    }
    let mut used: BTreeSet<&str> = process.ports.values().map(String::as_str).collect();
    used.extend(process.env.values().flat_map(|value| value.ports()));
    used.extend(process.listen.as_deref());
    if let Some(ReadyCheck::Port(port)) = process.ready.as_ref().map(|ready| &ready.check) {
        used.insert(port);
    }
    for (variable, port) in &process.ports {
        if let Some(number) = inputs.ports.get(port) {
            vars.insert(variable.clone(), Redacted::new(number.to_string()));
        }
    }
    let (source, cwd) = placed(inputs, name, process)?;
    let mut plan = AppPlan::new(
        app.clone(),
        RunnerKind::Host,
        Source::checkout(source.clone(), None),
    );
    plan.ports = app_ports(inputs, name, used)?;
    plan.depends = depends(&process.depends_on)?;
    plan.depends
        .extend(service_depends(inputs.manifest, process)?);
    let mut main = ProcessPlan::new(main_process()?, process.argv.clone());
    main.cwd = cwd;
    // Each passed-through variable that the manifest does not set is
    // optional, so the plan names the same variables whatever the caller's
    // environment holds, and the source leaves out those it lacks.
    main.optional_env = PASSED_THROUGH
        .iter()
        .filter(|name| !set.contains(**name))
        .map(|name| (*name).to_owned())
        .collect();
    main.env = vars
        .keys()
        .cloned()
        .chain(main.optional_env.iter().cloned())
        .collect();
    main.port = process
        .listen
        .as_deref()
        .map(|port| port_name(name, port))
        .transpose()?;
    if let Some(ready) = &process.ready {
        main.ready = Some(Probe::new(match &ready.check {
            ReadyCheck::Port(port) => ProbeCheck::tcp(port_name(name, port)?),
            ReadyCheck::Log(pattern) => ProbeCheck::log(pattern.as_str().to_owned()),
        }));
        main.ready_timeout = ready.timeout;
    }
    main.stop.grace = inputs.grace;
    plan.processes = vec![main];
    plan.permissions = process
        .permissions
        .as_ref()
        .map(|permissions| app_permissions(name, permissions, &source))
        .transpose()?;
    if let Some(granted) = &mut plan.permissions {
        granted
            .connect
            .extend(service_connects(inputs.manifest, process));
    }
    Ok((plan, vars.into_iter().collect()))
}

/// Returns the permissions of the app of the process `name`, whose source
/// directory is `source`: its write paths, its listen ports as phase's port
/// names and the units it connects to as app names. A process of a merged
/// manifest holds each write path other than `$STATE` as an absolute path,
/// which becomes relative to `source`.
///
/// # Errors
///
/// Returns [`Unrunnable`] when a write path is absolute and lies outside
/// `source`, such as a workspace manifest's path into another checkout
/// than the one the process runs in, or a port or unit name is not one
/// that phase accepts.
fn app_permissions(
    name: &str,
    permissions: &crate::manifest::Permissions,
    source: &Path,
) -> Result<phase::Permissions, Unrunnable> {
    let mut granted = phase::Permissions::default();
    for path in &permissions.write {
        let written = Path::new(path);
        if !written.is_absolute() {
            granted.write.insert(path.clone());
            continue;
        }
        let written = lexical(written);
        let Ok(rest) = written.strip_prefix(lexical(source)) else {
            return Err(Unrunnable {
                unit: Some(name.to_owned()),
                message: format!(
                    "the process `{name}` may write {path}, which is outside {}, the checkout it runs in",
                    source.display()
                ),
            });
        };
        if rest.as_os_str().is_empty() {
            return Err(Unrunnable {
                unit: Some(name.to_owned()),
                message: format!(
                    "the process `{name}` may write {path}, which is the whole checkout it runs in, whose `.git` a sandboxed process may only read"
                ),
            });
        }
        granted.write.insert(rest.display().to_string());
    }
    for port in &permissions.listen {
        granted.listen.insert(port_name(name, port)?);
    }
    for unit in &permissions.connect {
        granted.connect.insert(app_name(unit)?.to_string());
    }
    Ok(granted)
}

/// Returns the plan of the container `container`, named `name`, and the
/// values of its variables.
fn container_app(
    inputs: &Inputs<'_>,
    name: &str,
    app: &AppName,
    container: &Container,
) -> Result<(AppPlan, Vec<(String, Redacted)>), Unrunnable> {
    let image = ImageReference::try_from(container.image.clone()).map_err(|error| Unrunnable {
        unit: Some(name.to_owned()),
        message: format!(
            "the image `{}` of `{name}` is not one phase runs, which is pinned by a tag other than `latest` or by an `@sha256:` digest: {error}",
            container.image
        ),
    })?;
    let mut plan = AppPlan::new(app.clone(), RunnerKind::Container, Source::image(image));
    plan.ports = app_ports(
        inputs,
        name,
        container
            .ports
            .iter()
            .map(|published| published.port.as_str()),
    )?;
    plan.depends = depends(&container.depends_on)?;
    let mut main = ProcessPlan::new(
        main_process()?,
        container.command.clone().unwrap_or_default(),
    );
    main.env = container.env.keys().cloned().collect();
    for published in &container.ports {
        let inside = NonZeroU16::new(published.container).ok_or_else(|| Unrunnable {
            unit: Some(name.to_owned()),
            message: format!("`{name}` publishes the container port 0"),
        })?;
        main.publish
            .insert(port_name(name, &published.port)?, inside);
    }
    for (volume, mount) in &container.volumes {
        let volume = VolumeName::try_from(volume.clone()).map_err(|error| Unrunnable {
            unit: Some(name.to_owned()),
            message: format!(
                "the volume `{volume}` of `{name}` is not a volume name phase accepts: {error}"
            ),
        })?;
        main.volumes.insert(volume, PathBuf::from(mount));
    }
    if let Some(ready) = &container.ready {
        if let ReadyCheck::Port(port) = &ready.check {
            main.ready = Some(Probe::new(ProbeCheck::tcp(port_name(name, port)?)));
        }
        main.ready_timeout = ready.timeout;
    }
    main.stop.grace = inputs.grace;
    plan.processes = vec![main];
    let vars = container
        .env
        .iter()
        .map(|(variable, value)| (variable.clone(), value.clone()))
        .collect();
    Ok((plan, vars))
}

/// The environment source of a workspace's plan, which holds the value of
/// each variable that the plan names for each app. No value is a secret:
/// each comes from the manifest, the port ledger or the caller's
/// environment.
#[derive(Clone, Default)]
pub(crate) struct ManifestEnvironment {
    /// The variables of each app's process, in name order.
    values: BTreeMap<AppName, Vec<(String, Redacted)>>,
}

impl EnvironmentSource for ManifestEnvironment {
    fn resolve(
        &self,
        app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(Applied {
            vars: self.values.get(app).cloned().unwrap_or_default(),
        }))
    }
}

/// The resolved variables of one process.
struct Applied {
    /// The variables, in name order.
    vars: Vec<(String, Redacted)>,
}

impl AppliedEnvironment for Applied {
    fn names(&self) -> BTreeSet<String> {
        self.vars.iter().map(|(name, _)| name.clone()).collect()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        keyed_digest(key, &self.vars)
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        for (name, value) in &self.vars {
            if let Some(reference) = reference(name) {
                command.env(reference, value.reveal());
            }
        }
    }

    fn write_value(&self, name: &str, out: &mut dyn io::Write) -> io::Result<()> {
        let (_, value) = self
            .vars
            .iter()
            .find(|(known, _)| known == name)
            .ok_or(io::ErrorKind::NotFound)?;
        out.write_all(value.reveal().as_bytes())
    }
}

/// Returns a digest of `vars` under `key`: four 64-bit FNV-1a hashes from
/// different starting values over the key and then each name and value,
/// each followed by a zero byte. It tells two environments apart, and is
/// not meant to resist a search for a value, which suits values that are
/// not secrets.
fn keyed_digest(key: &[u8; 32], vars: &[(String, Redacted)]) -> [u8; 32] {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const OFFSETS: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x8422_2325_cbf2_9ce4,
        0x9ce4_8422_2325_cbf2,
        0x2325_cbf2_9ce4_8422,
    ];
    let bytes = vars.iter().flat_map(|(name, value)| {
        name.bytes()
            .chain([0])
            .chain(value.reveal().bytes())
            .chain([0])
    });
    let mut digest = [0; 32];
    for (lane, offset) in OFFSETS.into_iter().enumerate() {
        let hash = key
            .iter()
            .copied()
            .chain(bytes.clone())
            .fold(offset, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(PRIME)
            });
        if let Some(slot) = digest.get_mut(lane * 8..lane * 8 + 8) {
            slot.copy_from_slice(&hash.to_le_bytes());
        }
    }
    digest
}
