//! A workspace's compose project: its compose file, the environment of the
//! `docker compose` process that holds the values of the containers'
//! variables, and `docker compose up`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::cancel::Cancel;
use crate::engine::container::ContainerError as Error;
use crate::engine::container::command::{
    ChildEnv, Finished, PULL_DEADLINE, check, compose_docker_line, message, run, run_input,
};
use crate::engine::container::names::{
    CONTAINER_LABEL, Scope, ServiceName, VOLUME_LABEL, VolumeKey,
};
use crate::engine::container::objects::inspect_named;

/// The value of a container's variable, which may be a secret. Its
/// `Debug` prints `<redacted>`, it implements neither `Display` nor
/// `Serialize`, so it reaches no log, error or file, and it has no
/// accessor. Only the environment of the `docker compose` process reads
/// it.
#[derive(Clone, PartialEq, Eq)]
pub struct ComposeValue(String);

impl ComposeValue {
    /// Returns the value `value` of a container's variable.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for ComposeValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// A container port that a [`ContainerSpec`] publishes as a named port.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublishedPort {
    /// The TCP port inside the container.
    pub container: u16,
    /// The name of the port whose host port it is published on.
    pub port: String,
}

impl PublishedPort {
    /// Returns the container port `container`, published on the host port
    /// of the port named `port`.
    #[must_use]
    pub const fn new(container: u16, port: String) -> Self {
        Self { container, port }
    }
}

/// What the compose file declares for one service: the container engine's
/// own copy of a manifest container, which the caller builds.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ContainerSpec {
    /// The image.
    pub image: String,
    /// The arguments that replace the image's command, or `None` to keep
    /// it.
    pub command: Option<Vec<String>>,
    /// The variables that the container receives, by name.
    pub env: BTreeMap<String, ComposeValue>,
    /// The container ports that are published on the loopback address.
    pub ports: Vec<PublishedPort>,
    /// The named volumes that the container mounts, with the path that
    /// each mounts at.
    pub volumes: BTreeMap<VolumeKey, String>,
}

impl ContainerSpec {
    /// Returns the service that runs `image` with its own command, no
    /// variables, no published ports and no volumes.
    #[must_use]
    pub const fn new(image: String) -> Self {
        Self {
            image,
            command: None,
            env: BTreeMap::new(),
            ports: Vec::new(),
            volumes: BTreeMap::new(),
        }
    }
}

/// The services of a workspace's compose project, in name order.
pub type ContainerSpecs = BTreeMap<ServiceName, ContainerSpec>;

/// The variables of a `docker compose` process: the values of one
/// container's variables, under the names that [`compose_file`] refers
/// to. Like [`ComposeValue`], it implements neither `Debug`, `Display`
/// nor `Serialize`.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ComposeEnv(Vec<(String, ComposeValue)>);

impl ComposeEnv {
    /// Returns the variables as names and values, for the environment of
    /// the docker process, and consumes them.
    pub(super) fn into_vars(self) -> impl Iterator<Item = (String, String)> {
        self.0.into_iter().map(|(name, value)| (name, value.0))
    }

    /// Returns the names and values, for a test that compares them.
    #[cfg(test)]
    pub(super) fn exposed(&self) -> Vec<(&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.0.as_str()))
            .collect()
    }
}

/// Writes `$` as `$$`, which Compose reads back as `$`, so it interpolates
/// nothing in `text`.
pub(super) fn literal(text: &str) -> String {
    text.replace('$', "$$")
}

/// Returns the variable of the `docker compose` process that holds the
/// value of the `variable`th variable of the `service`th service, in the
/// order of the manifest's containers and of their variables.
pub(super) fn variable_of(service: usize, variable: usize) -> String {
    format!("MXV_ENV_{service}_{variable}")
}

/// Returns the compose file of `scope` for `containers`, the services of
/// the project, whose ports `ports` holds by port name. A port that `ports`
/// lacks is not published. The file holds no value of a container's
/// variables: each variable refers to a variable of the `docker compose`
/// process, which [`compose_env`] returns.
pub(crate) fn compose_file(
    scope: &Scope,
    containers: &ContainerSpecs,
    ports: &BTreeMap<String, u16>,
) -> Value {
    let mut services = Map::new();
    let mut volumes = Map::new();
    for (service, (name, container)) in containers.iter().enumerate() {
        let environment: Map<String, Value> = container
            .env
            .keys()
            .enumerate()
            .map(|(variable, key)| {
                (
                    key.clone(),
                    Value::String(format!("${{{}:-}}", variable_of(service, variable))),
                )
            })
            .collect();
        let published: Vec<Value> = container
            .ports
            .iter()
            .filter_map(|published| {
                ports.get(&published.port).map(|host| {
                    Value::String(format!("127.0.0.1:{host}:{}/tcp", published.container))
                })
            })
            .collect();
        let mounts: Vec<Value> = container
            .volumes
            .iter()
            .map(|(volume, path)| {
                json!({"type": "volume", "source": volume.as_str(), "target": literal(path)})
            })
            .collect();
        for volume in container.volumes.keys() {
            volumes.insert(
                volume.as_str().to_owned(),
                json!({
                    "name": scope.volume_name(volume).as_str(),
                    "labels": scope.labels(VOLUME_LABEL, volume.as_str()),
                }),
            );
        }
        let mut definition = Map::new();
        definition.insert("image".to_owned(), Value::String(literal(&container.image)));
        definition.insert(
            "container_name".to_owned(),
            Value::String(scope.container_name(name).as_str().to_owned()),
        );
        definition.insert(
            "network_mode".to_owned(),
            Value::String("bridge".to_owned()),
        );
        definition.insert(
            "labels".to_owned(),
            scope.labels(CONTAINER_LABEL, name.as_str()),
        );
        if let Some(command) = &container.command {
            definition.insert(
                "command".to_owned(),
                command.iter().map(|argument| literal(argument)).collect(),
            );
        }
        definition.insert("environment".to_owned(), Value::Object(environment));
        definition.insert("ports".to_owned(), Value::Array(published));
        definition.insert("volumes".to_owned(), Value::Array(mounts));
        services.insert(name.as_str().to_owned(), Value::Object(definition));
    }
    json!({
        "services": services,
        "volumes": volumes,
    })
}

/// Returns the variables of the `docker compose` process that starts the
/// service `name` of `containers`: the values of the container's
/// variables, under the names that [`compose_file`] refers to.
pub(crate) fn compose_env(containers: &ContainerSpecs, name: &ServiceName) -> ComposeEnv {
    ComposeEnv(
        containers
            .iter()
            .enumerate()
            .filter(|(_, (service, _))| *service == name)
            .flat_map(|(service, (_, container))| {
                container
                    .env
                    .values()
                    .enumerate()
                    .map(move |(variable, value)| (variable_of(service, variable), value.clone()))
            })
            .collect(),
    )
}

/// Why a `docker compose up` failed, as its standard error tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpFailure {
    /// docker already publishes the host port, which it names when it
    /// can, for another container.
    PortAllocated(Option<u16>),
    /// docker has no free address pool for a network.
    NoAddressPool,
    /// Any other failure.
    Other,
}

/// Returns why the `docker compose up` whose standard error is `stderr`
/// failed.
pub(crate) fn up_failure(stderr: &str) -> UpFailure {
    if stderr.contains("port is already allocated") {
        let port = stderr
            .split("Bind for ")
            .nth(1)
            .and_then(|rest| rest.split(" failed").next())
            .and_then(|address| address.rsplit_once(':'))
            .and_then(|(_, port)| port.parse().ok());
        UpFailure::PortAllocated(port)
    } else if stderr.contains("all predefined address pools have been fully subnetted") {
        UpFailure::NoAddressPool
    } else {
        UpFailure::Other
    }
}

/// What a `docker compose up` that did not fail as an error did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Up {
    /// The container started.
    Started,
    /// docker already publishes the host port, which it names when it
    /// can, for another container.
    PortAllocated {
        /// The host port, when docker names it.
        port: Option<u16>,
        /// Docker's own error line, as [`compose_docker_line`] takes it.
        docker_line: Option<String>,
    },
}

/// Starts the service `service` of the compose file `file` with `docker
/// compose up`, with the variables `env` that [`compose_env`] returned.
/// The project directory is `dir`, which holds no other file that Compose
/// reads.
///
/// The caller has checked the compose plugin with `docker compose version`.
///
/// # Errors
///
/// Returns [`Error::Foreign`] when a container with the name of the
/// service lacks the scope's labels, [`Error::NoAddressPool`], the
/// errors of `docker version`, and [`Error::Failed`] when docker
/// cannot pull the image or start the container.
pub(crate) fn compose_up(
    scope: &Scope,
    dir: &Path,
    file: &Path,
    service: &ServiceName,
    env: ComposeEnv,
) -> Result<Up, Error> {
    let project = scope.project();
    let args: [&OsStr; 13] = [
        "compose".as_ref(),
        "--project-name".as_ref(),
        project.as_str().as_ref(),
        "--project-directory".as_ref(),
        dir.as_os_str(),
        "--file".as_ref(),
        file.as_os_str(),
        "up".as_ref(),
        "--detach".as_ref(),
        "--no-deps".as_ref(),
        "--quiet-pull".as_ref(),
        "--".as_ref(),
        service.as_ref(),
    ];
    let finished = run(args, env, "compose up", PULL_DEADLINE)?;
    up_result(scope, service, &finished)
}

/// What `docker compose up` does with a service of phase's app project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppStep {
    /// It creates the container and leaves it created, so that Docker
    /// copies an image directory into a new volume before phase sets the
    /// volume's owner.
    Create,
    /// It starts the container, creating it if it does not exist, and
    /// keeps a container that exists.
    Start,
}

/// Runs `docker compose up` with `step` for the service `service` of
/// phase's app project in `scope`, reading the compose file `file` from
/// standard input, with the variables `env`. The project directory is
/// `dir`, which holds no file that Compose reads, and Compose reads no
/// `.env` file. `until` holds the time the command may take and the flag
/// that stops it.
///
/// The caller has checked the compose plugin with `docker compose version`,
/// and has pulled the image, so Compose pulls nothing.
///
/// # Errors
///
/// Returns the errors of [`compose_up`].
pub(crate) fn app_up(
    scope: &Scope,
    dir: &Path,
    file: &Value,
    service: &ServiceName,
    env: impl ChildEnv,
    step: AppStep,
    until: (Duration, &Cancel),
) -> Result<Up, Error> {
    let project = scope.project();
    let step_args: [&OsStr; 2] = match step {
        AppStep::Create => ["--no-start".as_ref(), "--no-recreate".as_ref()],
        AppStep::Start => ["--detach".as_ref(), "--no-recreate".as_ref()],
    };
    let args: [&OsStr; 17] = [
        "compose".as_ref(),
        "--project-name".as_ref(),
        project.as_str().as_ref(),
        "--project-directory".as_ref(),
        dir.as_os_str(),
        "--env-file".as_ref(),
        "/dev/null".as_ref(),
        "--file".as_ref(),
        "-".as_ref(),
        "up".as_ref(),
        step_args[0],
        step_args[1],
        "--no-deps".as_ref(),
        "--pull".as_ref(),
        "never".as_ref(),
        "--".as_ref(),
        service.as_ref(),
    ];
    let (deadline, cancel) = until;
    let finished = run_input(
        args,
        env,
        file.to_string().into_bytes(),
        "compose up",
        deadline,
        cancel,
    )?;
    up_result(scope, service, &finished)
}

/// Returns what the `docker compose up` of `service` in `scope` that
/// ended as `finished` did.
fn up_result(scope: &Scope, service: &ServiceName, finished: &Finished) -> Result<Up, Error> {
    if finished.status.success() {
        return Ok(Up::Started);
    }
    if finished.stderr.contains("Conflict") {
        // Another program created a container with the name after the
        // engine removed its own, so the name is foreign.
        inspect_named(scope, service)?;
    }
    match up_failure(&finished.stderr) {
        UpFailure::PortAllocated(port) => Ok(Up::PortAllocated {
            port,
            docker_line: compose_docker_line(&finished.stderr),
        }),
        UpFailure::NoAddressPool => Err(Error::NoAddressPool {
            message: message(&finished.stderr),
            docker_line: compose_docker_line(&finished.stderr),
        }),
        UpFailure::Other => {
            check()?;
            Err(Error::failed(
                "compose up",
                Some(service.as_str()),
                message(&finished.stderr),
            )
            .with_docker_line(compose_docker_line(&finished.stderr)))
        }
    }
}
