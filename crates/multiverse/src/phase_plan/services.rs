//! The apps that a manifest's services add to its plan: one container for
//! each backend, and the host oneshot [`PROVISION_APP`], which creates the
//! services' resources once the backends are ready.

use std::num::NonZeroU16;
use std::time::Duration;

use phase::{
    AppName, AppPlan, ImageReference, Probe, ProbeCheck, ProcessKind, ProcessPlan, RunnerKind,
    Source,
};

#[cfg(test)]
mod tests;

use super::{Inputs, Unrunnable, app_name, app_ports, main_process, port_name};
use crate::manifest::{Manifest, Process};
use crate::provision::Spec;
use crate::redacted::Redacted;
use crate::service::{Backend, HOST, PROVISION_APP, localstack_services};

/// How long `up` waits for a backend to become ready. LocalStack takes up
/// to half a minute on a cold start, and pulling an image takes longer,
/// which `up`'s build wait covers.
pub(crate) const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// How long `up` waits for the provisioning to complete. It retries each
/// request while a backend's service still starts.
pub(crate) const PROVISION_TIMEOUT: Duration = Duration::from_secs(120);

/// The command line of DynamoDB Local, which keeps one database for every
/// access key and region, in memory.
const DYNAMODB_COMMAND: &[&str] = &["-jar", "DynamoDBLocal.jar", "-sharedDb", "-inMemory"];

/// The path of LocalStack's health check, which answers once its gateway
/// runs.
const LOCALSTACK_HEALTH: &str = "/_localstack/health";

/// An app and the values of its variables.
type Planned = (AppPlan, Vec<(String, Redacted)>);

/// Returns the apps of the manifest's services: each backend in order,
/// then the provisioning. A manifest without services has none.
///
/// # Errors
///
/// Returns [`Unrunnable`] when the workspace holds no port for a backend,
/// or the provisioning command is unknown.
pub(super) fn service_apps(inputs: &Inputs<'_>) -> Result<Vec<Planned>, Unrunnable> {
    let backends = inputs.manifest.backends();
    if backends.is_empty() {
        return Ok(Vec::new());
    }
    let mut apps = Vec::new();
    for backend in &backends {
        apps.push(backend_app(inputs, *backend)?);
    }
    apps.push(provision_app(inputs)?);
    Ok(apps)
}

/// Returns the apps that `process` depends on for the services it needs:
/// each of their backends, then [`PROVISION_APP`].
///
/// # Errors
///
/// Returns [`Unrunnable`] when an app's name is not one phase accepts,
/// which never happens for mxv's own names.
pub(super) fn service_depends(
    manifest: &Manifest,
    process: &Process,
) -> Result<Vec<AppName>, Unrunnable> {
    let backends = crate::service::backends(manifest.needed(process).map(|(_, service)| service));
    if backends.is_empty() {
        return Ok(Vec::new());
    }
    backends
        .into_iter()
        .map(Backend::app)
        .chain(std::iter::once(PROVISION_APP))
        .map(app_name)
        .collect()
}

/// Returns the backend apps of the services that `process` needs, which a
/// sandboxed process may connect to. phase's sandbox lets an app connect
/// only to the ports of the apps in its permissions' `connect` set, so
/// mxv adds these for a process whose `needs` names the services.
pub(super) fn service_connects(manifest: &Manifest, process: &Process) -> Vec<String> {
    crate::service::backends(manifest.needed(process).map(|(_, service)| service))
        .into_iter()
        .map(|backend| backend.app().to_owned())
        .collect()
}

/// Returns the container app of `backend`, which publishes the backend's
/// port of the workspace's ledger.
fn backend_app(inputs: &Inputs<'_>, backend: Backend) -> Result<Planned, Unrunnable> {
    let name = backend.app();
    let image =
        ImageReference::try_from(backend.image().to_owned()).map_err(|error| Unrunnable {
            unit: Some(name.to_owned()),
            message: format!("the image of `{name}` is not one phase runs: {error}"),
        })?;
    let mut plan = AppPlan::new(app_name(name)?, RunnerKind::Container, Source::image(image));
    plan.ports = app_ports(inputs, name, [name])?;
    let port = port_name(name, name)?;
    let inside = NonZeroU16::new(backend.container_port()).ok_or_else(|| Unrunnable {
        unit: Some(name.to_owned()),
        message: format!("`{name}` publishes the container port 0"),
    })?;
    let mut vars = Vec::new();
    let mut main = match backend {
        Backend::Dynamodb => {
            let mut main = ProcessPlan::new(
                main_process()?,
                DYNAMODB_COMMAND
                    .iter()
                    .map(|word| (*word).to_owned())
                    .collect(),
            );
            main.ready = Some(Probe::new(ProbeCheck::tcp(port.clone())));
            main
        }
        Backend::Aws => {
            let host_port = inputs.ports.get(name).copied().unwrap_or_default();
            let kinds = localstack_services(inputs.manifest.services.values());
            vars = vec![
                ("SERVICES".to_owned(), Redacted::new(kinds.clone())),
                (
                    "SQS_ENDPOINT_STRATEGY".to_owned(),
                    Redacted::new("path".to_owned()),
                ),
                // LocalStack sends no usage events to localstack.cloud.
                ("DISABLE_EVENTS".to_owned(), Redacted::new("1".to_owned())),
                // Queue URLs carry this address, so a client on the host
                // reaches the queues through the published port.
                (
                    "LOCALSTACK_HOST".to_owned(),
                    Redacted::new(format!("{HOST}:{host_port}")),
                ),
            ];
            // phase's plan hash of a container covers its command and the
            // names of its variables, not their values, so the kinds go in
            // the command too: a change of kinds then replaces the
            // container. LocalStack's entrypoint ignores its arguments.
            let mut main = ProcessPlan::new(main_process()?, vec![format!("mxv-services={kinds}")]);
            main.port = Some(port.clone());
            main.ready = Some(Probe::new(ProbeCheck::http(LOCALSTACK_HEALTH.to_owned())));
            main
        }
    };
    main.publish.insert(port, inside);
    main.env = vars.iter().map(|(variable, _)| variable.clone()).collect();
    main.ready_timeout = BACKEND_READY_TIMEOUT;
    main.stop.grace = inputs.grace;
    plan.processes = vec![main];
    Ok((plan, vars))
}

/// Returns the host app [`PROVISION_APP`], a oneshot that runs the
/// provisioning command with the resources of every service and the
/// backends' host ports as one JSON argument. It runs again whenever a
/// backend restarted, which loses the resources of its memory, and
/// whenever the resources change.
fn provision_app(inputs: &Inputs<'_>) -> Result<Planned, Unrunnable> {
    let command = inputs.provisioner.clone().ok_or_else(|| Unrunnable {
        unit: Some(PROVISION_APP.to_owned()),
        message: "mxv cannot find its own program, which creates the services' resources"
            .to_owned(),
    })?;
    let spec = Spec::of(inputs.manifest, inputs.ports);
    let mut run = command;
    run.push(spec.to_argument());
    let mut plan = AppPlan::new(
        app_name(PROVISION_APP)?,
        RunnerKind::Host,
        Source::checkout(inputs.checkout.to_path_buf(), None),
    );
    plan.depends = inputs
        .manifest
        .backends()
        .into_iter()
        .map(|backend| app_name(backend.app()))
        .collect::<Result<_, _>>()?;
    let mut main = ProcessPlan::new(main_process()?, run);
    main.kind = ProcessKind::oneshot(false);
    main.ready_timeout = PROVISION_TIMEOUT;
    main.stop.grace = inputs.grace;
    plan.processes = vec![main];
    Ok((plan, Vec::new()))
}
