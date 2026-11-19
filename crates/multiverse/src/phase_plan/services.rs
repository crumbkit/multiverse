//! The apps that a manifest's services add to its plan: the host app
//! [`SERVICES_APP`], which serves the dynamodb services, the LocalStack
//! container, which serves the s3, sqs and kms services, and the host
//! oneshot [`PROVISION_APP`], which creates the services' resources once
//! the backends are ready.

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
use crate::service::{
    Backend, HOST, LOCALSTACK_IMAGE, PROVISION_APP, SERVICES_APP, SERVICES_READY, ServiceKind,
    localstack_services,
};

/// How long `up` waits for a backend to become ready. LocalStack takes up
/// to half a minute on a cold start, and pulling an image takes longer,
/// which `up`'s build wait covers. `mxv-services` is ready in well under a
/// second.
pub(crate) const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// How long `up` waits for the provisioning to complete. It retries each
/// request while a backend's service still starts.
pub(crate) const PROVISION_TIMEOUT: Duration = Duration::from_secs(120);

/// The word that follows the own command on the command line of
/// [`PROVISION_APP`].
const PROVISION_WORD: &str = "provision";

/// The word that follows the own command on the command line of
/// [`SERVICES_APP`].
const SERVICES_WORD: &str = "services";

/// The port inside LocalStack's container that its one port publishes.
const LOCALSTACK_PORT: u16 = 4566;

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
/// or the command of mxv's own apps is unknown.
pub(super) fn service_apps(inputs: &Inputs<'_>) -> Result<Vec<Planned>, Unrunnable> {
    let backends = inputs.manifest.backends();
    if backends.is_empty() {
        return Ok(Vec::new());
    }
    let mut apps = Vec::new();
    for backend in &backends {
        apps.push(match backend {
            Backend::Services => services_app(inputs)?,
            Backend::Aws => localstack_app(inputs)?,
        });
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

/// Returns the command that starts one of mxv's own apps, `word` and then
/// `arguments` after the own command.
fn own_command(
    inputs: &Inputs<'_>,
    app: &str,
    word: &str,
    arguments: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, Unrunnable> {
    let mut command = inputs.own_command.clone().ok_or_else(|| Unrunnable {
        unit: Some(app.to_owned()),
        message: format!("mxv cannot find its own program, which runs `{app}`"),
    })?;
    command.push(word.to_owned());
    command.extend(arguments);
    Ok(command)
}

/// Returns the host app [`SERVICES_APP`], which runs mxv as
/// `mxv phase services` with one `<kind>=<port>` argument for each kind it
/// serves, and listens on each kind's port of the workspace's ledger. It
/// is ready once it writes [`SERVICES_READY`], after every listener is
/// bound, and it receives no variable.
fn services_app(inputs: &Inputs<'_>) -> Result<Planned, Unrunnable> {
    let kinds: std::collections::BTreeSet<ServiceKind> = inputs
        .manifest
        .services
        .values()
        .map(|service| service.kind)
        .filter(|kind| kind.backend() == Backend::Services)
        .collect();
    let mut arguments = Vec::new();
    for kind in &kinds {
        let port = inputs
            .ports
            .get(kind.port())
            .copied()
            .ok_or_else(|| Unrunnable {
                unit: Some(SERVICES_APP.to_owned()),
                message: format!(
                    "the workspace holds no host port for the port `{}` of `{SERVICES_APP}`",
                    kind.port()
                ),
            })?;
        arguments.push(format!("{kind}={port}"));
    }
    let run = own_command(inputs, SERVICES_APP, SERVICES_WORD, arguments)?;
    let mut plan = AppPlan::new(
        app_name(SERVICES_APP)?,
        RunnerKind::Host,
        Source::checkout(inputs.checkout.to_path_buf(), None),
    );
    plan.ports = app_ports(inputs, SERVICES_APP, kinds.iter().map(|kind| kind.port()))?;
    let mut main = ProcessPlan::new(main_process()?, run);
    // phase checks before the start that nothing else holds the port.
    main.port = kinds
        .first()
        .map(|kind| port_name(SERVICES_APP, kind.port()))
        .transpose()?;
    main.ready = Some(Probe::new(ProbeCheck::log(format!("^{SERVICES_READY}$"))));
    main.ready_timeout = BACKEND_READY_TIMEOUT;
    main.stop.grace = inputs.grace;
    plan.processes = vec![main];
    Ok((plan, Vec::new()))
}

/// Returns the container app of LocalStack, which publishes its port of
/// the workspace's ledger and serves the manifest's s3, sqs and kms kinds.
fn localstack_app(inputs: &Inputs<'_>) -> Result<Planned, Unrunnable> {
    let name = Backend::Aws.app();
    let image =
        ImageReference::try_from(LOCALSTACK_IMAGE.to_owned()).map_err(|error| Unrunnable {
            unit: Some(name.to_owned()),
            message: format!("the image of `{name}` is not one phase runs: {error}"),
        })?;
    let mut plan = AppPlan::new(app_name(name)?, RunnerKind::Container, Source::image(image));
    let ledger = ServiceKind::S3.port();
    plan.ports = app_ports(inputs, name, [ledger])?;
    let port = port_name(name, ledger)?;
    let inside = NonZeroU16::new(LOCALSTACK_PORT).ok_or_else(|| Unrunnable {
        unit: Some(name.to_owned()),
        message: format!("`{name}` publishes the container port 0"),
    })?;
    let host_port = inputs.ports.get(ledger).copied().unwrap_or_default();
    let kinds = localstack_services(inputs.manifest.services.values());
    let vars = vec![
        ("SERVICES".to_owned(), Redacted::new(kinds.clone())),
        (
            "SQS_ENDPOINT_STRATEGY".to_owned(),
            Redacted::new("path".to_owned()),
        ),
        // LocalStack sends no usage events to localstack.cloud.
        ("DISABLE_EVENTS".to_owned(), Redacted::new("1".to_owned())),
        // Queue URLs carry this address, so a client on the host reaches
        // the queues through the published port.
        (
            "LOCALSTACK_HOST".to_owned(),
            Redacted::new(format!("{HOST}:{host_port}")),
        ),
    ];
    // The kinds also go in the command, which phase's plan hash covers, so
    // a change of kinds replaces the container. phase now compares the
    // values too, and this is no longer needed. LocalStack's entrypoint
    // ignores its arguments.
    let mut main = ProcessPlan::new(main_process()?, vec![format!("mxv-services={kinds}")]);
    main.port = Some(port.clone());
    main.ready = Some(Probe::new(ProbeCheck::http(LOCALSTACK_HEALTH.to_owned())));
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
    let spec = Spec::of(inputs.manifest, inputs.ports);
    let run = own_command(inputs, PROVISION_APP, PROVISION_WORD, [spec.to_argument()])?;
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
