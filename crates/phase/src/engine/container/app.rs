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
//! A variable that the app's permissions name as a secret is never in the
//! compose file. phase delivers a container's secrets as files in a tmpfs
//! (decision P3), which this module does not do yet, so [`objects`]
//! refuses a process that receives a secret with
//! [`NotRunnable::ContainerSecrets`].

use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::engine::container::compose::{literal, variable_of};
use crate::engine::container::names::{CONTAINER_LABEL, Scope, ServiceName, VolumeKey};
use crate::engine::containers::ContainerObjects;
use crate::error::{NotRunnable, PhaseError};
use crate::name::{ProcessName, VolumeName};
use crate::plan::restart::{Restart, StopSignal};
use crate::plan::{AppPlan, ProcessKind, ProcessPlan};
use crate::record::run::VolumeLifetime;
use crate::record::{DockerName, MOST_DOCKER_NAME_BYTES};
use crate::source::Source;

#[cfg(test)]
mod tests;

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

/// Returns the variables of `process` that reach its container: those that
/// the plan names and the app's permissions do not name as secrets, in
/// name order.
fn variables<'p>(app: &AppPlan, process: &'p ProcessPlan) -> impl Iterator<Item = &'p String> {
    process
        .env
        .iter()
        .filter(|name| !app.permissions.secrets.contains(*name))
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
    if process
        .env
        .iter()
        .any(|name| app.permissions.secrets.contains(name))
    {
        return refuse(NotRunnable::ContainerSecrets);
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
    for process in &app.processes {
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

/// Returns Docker's restart policy for `restart`. Docker restarts with
/// its own backoff, and keeps no count for `always`.
fn restart_policy(restart: &Restart) -> String {
    match restart {
        Restart::No => "no".to_owned(),
        Restart::OnFailure(limits) => match limits.max {
            Some(max) => format!("on-failure:{max}"),
            None => "on-failure".to_owned(),
        },
        Restart::Always(_) => "always".to_owned(),
    }
}

/// Returns a duration as Compose reads it, in milliseconds.
fn duration(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

/// Returns a path inside a container or on the host as compose file text,
/// in which Compose interpolates nothing.
fn path(path: &Path) -> String {
    literal(&path.to_string_lossy())
}

/// Returns the compose file of `app` in `scope`. It holds no value of a
/// variable, as the module docs say.
///
/// # Errors
///
/// Returns [`PhaseError::NotRunnable`] for an app that does not run from
/// an image, and the errors of [`objects`].
pub(crate) fn compose_file(scope: &Scope, app: &AppPlan) -> Result<Value, PhaseError> {
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
        definition.insert(
            "labels".to_owned(),
            scope.labels(CONTAINER_LABEL, service.as_str()),
        );
        if !process.run.is_empty() {
            definition.insert(
                "command".to_owned(),
                process
                    .run
                    .iter()
                    .map(|argument| literal(argument))
                    .collect(),
            );
        }
        if let Some(user) = &app.container.user {
            definition.insert("user".to_owned(), Value::String(literal(user)));
        }
        definition.insert("environment".to_owned(), Value::Object(environment));
        definition.insert("ports".to_owned(), Value::Array(ports));
        definition.insert("volumes".to_owned(), Value::Array(mounts));
        definition.insert(
            "restart".to_owned(),
            Value::String(restart_policy(&process.restart)),
        );
        definition.insert(
            "stop_signal".to_owned(),
            Value::String(signal_name(process.stop.signal).to_owned()),
        );
        definition.insert(
            "stop_grace_period".to_owned(),
            Value::String(duration(process.stop.grace)),
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
