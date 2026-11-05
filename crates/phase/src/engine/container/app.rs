//! phase's compose project of one container app: its names, the Docker
//! objects that it creates, and its compose file.
//!
//! Each container app of a plan is one compose project, and each of its
//! processes is one service of the project, whose container is named
//! `<project>.<process>` (see [`Scope`]). Every container runs the app's
//! image on Docker's default bridge network, with `network_mode: bridge`,
//! so Compose creates no network, and publishes the app's named ports on
//! 127.0.0.1. The workspace's volumes, named in a process's `volumes` and
//! in the app's cache volumes, are created by phase before `docker compose
//! up` and declared `external`, because every app of the workspace may
//! mount them. The app's bind mounts, run-as user, stop signal, stop grace
//! and restart policy apply to every process.
//!
//! The compose file holds no value of a variable. Each variable that a
//! process receives refers to a variable of the `docker compose up` child,
//! `MXV_ENV_<service>_<variable>`, where `<service>` is the process's
//! position in the app and `<variable>` the variable's position in the
//! process's variables in name order. [`reference()`] returns that name, and
//! only that service's child receives the values.
//!
//! A variable that the app's permissions name as a secret reaches the
//! container as a file in a tmpfs at `/run/secrets` (decision P3), and its
//! name never appears in the service's `environment`. The service of a
//! process that receives such a file mounts the tmpfs and starts through
//! phase's wrapper, which waits for the files, as [`super::secrets`] says.
//! [`file_secrets`] returns the names that `up` writes after the container
//! starts. The service carries the label `mxv.secrets=files`, which
//! `status` reads, and Docker's `no` restart policy, because a Docker
//! restart empties the tmpfs. A process whose plan sets
//! [`SecretDelivery::Env`] receives its
//! secrets through private references like its other variables instead.

use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::engine::container::compose::{literal, variable_of};
use crate::engine::container::names::{CONTAINER_LABEL, Scope, ServiceName, VolumeKey};
use crate::engine::container::objects::stop_seconds;
use crate::engine::container::secrets::{ImageConfig, SECRETS_DIR, SECRETS_MODE, entrypoint};
use crate::engine::containers::ContainerObjects;
use crate::engine::run::order::sorted;
use crate::error::{NotRunnable, PhaseError};
use crate::name::{ProcessName, VolumeName};
use crate::plan::restart::{Restart, StopSignal};
use crate::plan::{AppPlan, ProcessKind, ProcessPlan, SecretDelivery, digest};
use crate::record::run::{PlanHash, VolumeLifetime};
use crate::record::{DockerName, MOST_DOCKER_NAME_BYTES};
use crate::source::Source;

#[cfg(test)]
mod tests;

/// The extension field of a service that holds its process's
/// `secret_owner`, which Compose ignores, so that a new owner changes the
/// service's plan hash and `up` replaces its container.
const SECRET_OWNER_FIELD: &str = "x-phase-secret-owner";

/// The label of a container whose process receives secret files, which
/// `status` reads to tell a container that waits for its files after a
/// restart from one that runs.
pub(crate) const SECRETS_LABEL: &str = "mxv.secrets";

/// The value of [`SECRETS_LABEL`].
pub(crate) const SECRET_FILES: &str = "files";

/// Returns the service of `process`, whose name follows the same rule.
pub(crate) fn service(app: &AppPlan, process: &ProcessName) -> Result<ServiceName, PhaseError> {
    ServiceName::new(process.as_str()).map_err(|_| PhaseError::ProcessUnstartable {
        app: app.name.clone(),
        process: process.clone(),
        reason: "the process's name is not a compose service name".to_owned(),
    })
}

/// Returns the key of the workspace's volume `volume`, whose name follows
/// the same rule.
fn volume_key(volume: &VolumeName) -> VolumeKey {
    VolumeKey::of(volume)
}

/// Returns the variables of `process` that reach its container's
/// environment, in name order: those that the plan names and the app's
/// permissions do not name as secrets, and the secrets too when the
/// process receives them through its environment.
fn variables<'p>(app: &AppPlan, process: &'p ProcessPlan) -> impl Iterator<Item = &'p String> {
    let secrets_too = process.secret_delivery == SecretDelivery::Env;
    process
        .env
        .iter()
        .filter(move |name| secrets_too || !app.permissions.secrets.contains(*name))
}

/// Returns the variables of `process` that phase writes as files into its
/// container's `/run/secrets`, in name order: those that the plan names
/// and the app's permissions name as secrets, unless the process receives
/// them through its environment.
pub(crate) fn file_secrets<'p>(
    app: &'p AppPlan,
    process: &'p ProcessPlan,
) -> impl Iterator<Item = &'p String> {
    let files = process.secret_delivery == SecretDelivery::File;
    process
        .env
        .iter()
        .filter(move |name| files && app.permissions.secrets.contains(*name))
}

/// Returns the name of the variable of the `docker compose up` child of
/// `process` that holds the value of its variable `name`, or `None` when
/// the variable does not reach the container.
pub(crate) fn reference(app: &AppPlan, process: &ProcessName, name: &str) -> Option<String> {
    let (service, plan) = app
        .processes
        .iter()
        .enumerate()
        .find(|(_, plan)| &plan.name == process)?;
    let variable = variables(app, plan).position(|known| known == name)?;
    Some(variable_of(service, variable))
}

/// Returns the name of every variable of a `docker compose up` child that
/// the compose file of `app` refers to, for every process. A child removes
/// each of them from what it inherits, and then sets the ones of its own
/// process, so Compose reads no inherited value under one of these names.
pub(crate) fn references(app: &AppPlan) -> Vec<String> {
    app.processes
        .iter()
        .enumerate()
        .flat_map(|(service, process)| {
            variables(app, process)
                .enumerate()
                .map(move |(variable, _)| variable_of(service, variable))
        })
        .collect()
}

/// Returns the workspace's volumes that `process` of `app` mounts: its
/// own and the app's cache volumes.
fn mounted<'p>(
    app: &'p AppPlan,
    process: &'p ProcessPlan,
) -> impl Iterator<Item = (&'p VolumeName, &'p std::path::PathBuf)> {
    process.volumes.iter().chain(&app.container.caches)
}

/// Returns the workspace's volumes that `app` mounts, by key, in name
/// order.
pub(crate) fn volume_keys(app: &AppPlan) -> Vec<VolumeKey> {
    let keys: std::collections::BTreeSet<VolumeKey> = app
        .processes
        .iter()
        .flat_map(|process| mounted(app, process))
        .map(|(volume, _)| volume_key(volume))
        .collect();
    keys.into_iter().collect()
}

/// Returns why phase cannot run `process` of the container app `app` yet,
/// if it cannot.
fn runnable(app: &AppPlan, process: &ProcessPlan) -> Result<(), PhaseError> {
    let refuse = |reason| {
        Err(PhaseError::NotRunnable {
            app: app.name.clone(),
            reason,
        })
    };
    if let ProcessKind::Oneshot { .. } = process.kind {
        return refuse(NotRunnable::Oneshot);
    }
    if process.cwd.is_some() {
        return refuse(NotRunnable::ContainerWorkingDirectory);
    }
    Ok(())
}

/// Returns `text` as a Docker name, or the failure of `process` of `app`
/// whose name it is.
fn docker_name(app: &AppPlan, process: &ProcessName, text: &str) -> Result<DockerName, PhaseError> {
    text.parse().map_err(|_| PhaseError::ProcessUnstartable {
        app: app.name.clone(),
        process: process.clone(),
        reason: format!(
            "the name {text} that phase gives its container or volume is longer than the {MOST_DOCKER_NAME_BYTES} bytes that a Docker name may hold"
        ),
    })
}

/// Returns the Docker objects that starting `app` in `scope` creates: a
/// container for each process and the workspace's volumes that it mounts,
/// which outlive the run.
///
/// # Errors
///
/// Returns [`PhaseError::NotRunnable`] for a process that phase cannot run
/// in a container yet, and [`PhaseError::ProcessUnstartable`] for one
/// whose container name is too long.
pub(crate) fn objects(scope: &Scope, app: &AppPlan) -> Result<ContainerObjects, PhaseError> {
    let mut objects = ContainerObjects::default();
    // The containers are in dependency order, which the run record keeps,
    // so `down` stops them in reverse.
    let ordered = sorted(
        &app.processes,
        |process| &process.name,
        |process| &process.depends,
    );
    for process in ordered {
        runnable(app, process)?;
        let name = scope.container_name(&service(app, &process.name)?);
        objects.containers.push((
            process.name.clone(),
            docker_name(app, &process.name, name.as_str())?,
        ));
        for (volume, _) in mounted(app, process) {
            let name = docker_name(
                app,
                &process.name,
                scope.volume_name(&volume_key(volume)).as_str(),
            )?;
            if !objects.volumes.iter().any(|(known, _)| known == &name) {
                objects.volumes.push((name, VolumeLifetime::Workspace));
            }
        }
    }
    objects
        .volumes
        .sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(objects)
}

/// Returns Compose's name of `signal`.
const fn signal_name(signal: StopSignal) -> &'static str {
    match signal {
        StopSignal::Term => "SIGTERM",
        StopSignal::Int => "SIGINT",
        StopSignal::Quit => "SIGQUIT",
        StopSignal::Hup => "SIGHUP",
        StopSignal::Usr1 => "SIGUSR1",
        StopSignal::Usr2 => "SIGUSR2",
    }
}

/// Returns Docker's restart policy for `restart`, which the docs of
/// [`Restart`] state: `no` is `no`, `on-failure` is `on-failure`, or
/// `on-failure:N` with a maximum of `N`, and `always` is `unless-stopped`.
/// Docker's backoff replaces the plan's backoff, and the plan's maximum
/// does not apply to `always`.
fn restart_policy(restart: &Restart) -> String {
    match restart {
        Restart::No => "no".to_owned(),
        Restart::OnFailure(limits) => match limits.max {
            Some(max) => format!("on-failure:{max}"),
            None => "on-failure".to_owned(),
        },
        Restart::Always(_) => "unless-stopped".to_owned(),
    }
}

/// Returns a stop grace as Compose reads it, in the whole seconds that
/// `docker stop` takes: rounded up, and at least one second.
fn grace(grace: Duration) -> String {
    format!("{}s", stop_seconds(grace))
}

/// Returns `labels` with each value as compose file text, in which Compose
/// interpolates nothing, so a label holds its value as written.
fn escaped(labels: Value) -> Value {
    match labels {
        Value::Object(labels) => Value::Object(
            labels
                .into_iter()
                .map(|(key, value)| match value {
                    Value::String(text) => (key, Value::String(literal(&text))),
                    other => (key, other),
                })
                .collect(),
        ),
        other => other,
    }
}

/// Returns a path inside a container or on the host as compose file text,
/// in which Compose interpolates nothing.
fn path(path: &Path) -> String {
    literal(&path.to_string_lossy())
}

/// Returns the compose file of `app` in `scope`. It holds no value of a
/// variable, as the module docs say.
///
/// A process that receives secrets as files starts through the wrapper,
/// which replaces the image's entrypoint, and Docker then drops the
/// image's command too. With `image_config`, the image's entrypoint and command
/// as Docker reports them, the service's command is what the container
/// would run without the wrapper. Without it, the service's command is the
/// plan's, which is the form that [`plan_hash`] hashes.
///
/// # Errors
///
/// Returns [`PhaseError::NotRunnable`] for an app that does not run from
/// an image, and the errors of [`objects`].
pub(crate) fn compose_file(
    scope: &Scope,
    app: &AppPlan,
    image_config: Option<&ImageConfig>,
) -> Result<Value, PhaseError> {
    let Source::Image { image } = &app.source else {
        return Err(PhaseError::NotRunnable {
            app: app.name.clone(),
            reason: NotRunnable::ContainerWithoutImage,
        });
    };
    let mut services = Map::new();
    for (index, process) in app.processes.iter().enumerate() {
        runnable(app, process)?;
        let service = service(app, &process.name)?;
        let environment: Map<String, Value> = variables(app, process)
            .enumerate()
            .map(|(variable, name)| {
                (
                    name.clone(),
                    Value::String(format!("${{{}:-}}", variable_of(index, variable))),
                )
            })
            .collect();
        let ports: Vec<Value> = process
            .publish
            .iter()
            .filter_map(|(port, inside)| {
                app.ports
                    .get(port)
                    .map(|host| Value::String(format!("127.0.0.1:{host}:{inside}/tcp")))
            })
            .collect();
        let mut mounts = Vec::new();
        for (volume, target) in mounted(app, process) {
            let key = volume_key(volume);
            let mount = json!({"type": "volume", "source": key.as_str(), "target": path(target)});
            if !mounts.contains(&mount) {
                mounts.push(mount);
            }
        }
        let secrets: Vec<&String> = file_secrets(app, process).collect();
        if !secrets.is_empty() {
            mounts.push(json!({
                "type": "tmpfs",
                "target": SECRETS_DIR,
                "tmpfs": {"mode": SECRETS_MODE},
            }));
        }
        for mount in &app.container.mounts {
            mounts.push(json!({
                "type": "bind",
                "source": path(&mount.source),
                "target": path(&mount.target),
                "read_only": mount.read_only,
            }));
        }
        let mut definition = Map::new();
        definition.insert("image".to_owned(), Value::String(literal(image.as_str())));
        definition.insert(
            "container_name".to_owned(),
            Value::String(scope.container_name(&service).as_str().to_owned()),
        );
        definition.insert(
            "network_mode".to_owned(),
            Value::String("bridge".to_owned()),
        );
        let mut labels = scope.labels(CONTAINER_LABEL, service.as_str());
        if let (false, Value::Object(labels)) = (secrets.is_empty(), &mut labels) {
            labels.insert(
                SECRETS_LABEL.to_owned(),
                Value::String(SECRET_FILES.to_owned()),
            );
        }
        definition.insert("labels".to_owned(), escaped(labels));
        let command = match image_config {
            Some(image) if !secrets.is_empty() => image.argv(&process.run),
            _ => process.run.clone(),
        };
        if !secrets.is_empty() {
            if let Some(owner) = &process.secret_owner {
                definition.insert(SECRET_OWNER_FIELD.to_owned(), Value::String(literal(owner)));
            }
            definition.insert(
                "entrypoint".to_owned(),
                entrypoint(secrets.iter().copied()).into_iter().collect(),
            );
        }
        if !command.is_empty() {
            definition.insert(
                "command".to_owned(),
                command.iter().map(|argument| literal(argument)).collect(),
            );
        }
        if let Some(user) = &app.container.user {
            definition.insert("user".to_owned(), Value::String(literal(user)));
        }
        definition.insert("environment".to_owned(), Value::Object(environment));
        definition.insert("ports".to_owned(), Value::Array(ports));
        definition.insert("volumes".to_owned(), Value::Array(mounts));
        // Docker's restart empties the tmpfs, and phase is not there to
        // write the files again, so Docker never restarts a process that
        // receives secret files.
        let restart = if secrets.is_empty() {
            restart_policy(&process.restart)
        } else {
            "no".to_owned()
        };
        definition.insert("restart".to_owned(), Value::String(restart));
        definition.insert(
            "stop_signal".to_owned(),
            Value::String(signal_name(process.stop.signal).to_owned()),
        );
        definition.insert(
            "stop_grace_period".to_owned(),
            Value::String(grace(process.stop.grace)),
        );
        services.insert(service.as_str().to_owned(), Value::Object(definition));
    }
    let mut volumes = Map::new();
    for key in volume_keys(app) {
        volumes.insert(
            key.as_str().to_owned(),
            json!({"name": scope.volume_name(&key).as_str(), "external": true}),
        );
    }
    Ok(json!({"services": services, "volumes": volumes}))
}

/// Returns the hash of the part of `app` that makes the container of
/// `process` in `scope`: its service in the compose file, which holds the
/// image, command, user, ports, mounts, labels, restart policy and stop,
/// and the names of its variables and secrets but no value. The hash
/// covers the plan's command and not the image's, which the compose file
/// of a process with secret files also holds. A container whose
/// recorded hash differs runs another plan, and `up` replaces it.
///
/// # Errors
///
/// Returns the errors of [`compose_file`].
pub(crate) fn plan_hash(
    scope: &Scope,
    app: &AppPlan,
    process: &ProcessName,
) -> Result<PlanHash, PhaseError> {
    let file = compose_file(scope, app, None)?;
    let service = service(app, process)?;
    let part = file
        .get("services")
        .and_then(|services| services.get(service.as_str()))
        .map(Value::to_string)
        .unwrap_or_default();
    Ok(PlanHash::new(digest(part.as_bytes())))
}
