#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use phase::{
    AppName, EnvironmentSource as _, ProbeCheck, ProcessKind, ProcessName, RunnerKind, WorkspaceId,
};

use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::phase_plan::{Built, CallerEnvironment, Inputs, MAIN, build};
use crate::provision::Spec;
use crate::service::LOCALSTACK_IMAGE;

const APP: &str = r#"
ports = ["api"]

[service.store]
kind = "dynamodb"
tables.inbox = { hash = "pk:S" }
env = { INBOX_TABLE = "{table.inbox}" }

[service.mail]
kind = "s3"
buckets = ["inbox-mail"]

[service.keys]
kind = "kms"
keys = ["inbox-mail"]

[processes.worker]
argv = ["worker"]
env = { RUST_LOG = "debug" }
needs = ["store", "mail"]

[processes.api]
argv = ["api"]
ports = { PORT = "api" }
listen = "api"
"#;

fn ports() -> BTreeMap<String, u16> {
    BTreeMap::from([
        ("api".to_owned(), 41_000),
        ("mxv-dynamodb".to_owned(), 41_001),
        ("mxv-aws".to_owned(), 41_002),
    ])
}

fn plan_with(
    text: &str,
    caller: &CallerEnvironment,
    own_command: Option<Vec<String>>,
) -> Result<Built, crate::phase_plan::Unrunnable> {
    let manifest = Manifest::parse(text, Path::new("/repo/mxv.toml")).unwrap();
    let ports = ports();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    build(&Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: Path::new("/work/triage/app"),
        roots: Vec::new(),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller,
        own_command,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    })
}

fn own_command() -> Option<Vec<String>> {
    Some(vec!["/bin/mxv".to_owned(), "phase".to_owned()])
}

fn plan(text: &str) -> Built {
    plan_with(text, &CallerEnvironment::default(), own_command()).unwrap()
}

fn app(name: &str) -> AppName {
    AppName::try_from(name.to_owned()).unwrap()
}

/// Returns the variables that `built` resolves for the app `name`.
fn resolved(built: &Built, name: &str) -> BTreeMap<String, String> {
    let applied = built
        .environment
        .resolve(
            &app(name),
            &ProcessName::try_from(MAIN.to_owned()).unwrap(),
            Instant::now(),
        )
        .unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test builds a command only to read the variables that the environment applies to it, and never runs it"
    )]
    let mut command = Command::new("true");
    command.env_clear();
    applied.apply_as(&mut command, &|name| Some(name.to_owned()));
    command
        .get_envs()
        .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value?.to_str()?.to_owned())))
        .collect()
}

#[test]
fn the_services_add_each_backend_and_the_provisioning_before_the_units() {
    let built = plan(APP);
    let names: Vec<&str> = built
        .plan
        .apps()
        .iter()
        .map(|app| app.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["mxv-services", "mxv-aws", "mxv-provision", "api", "worker"]
    );
}

#[test]
fn dynamodb_runs_in_the_host_app_mxv_services_on_its_ledger_port_until_it_says_it_is_ready() {
    let built = plan(APP);
    let services = built.plan.app(&app("mxv-services")).unwrap();
    assert_eq!(services.runner, RunnerKind::Host);
    assert!(services.depends.is_empty() && services.permissions.is_none());
    let main = &services.processes[0];
    assert_eq!(
        main.run,
        ["/bin/mxv", "phase", "services", "dynamodb=41001"]
    );
    assert_eq!(main.port.as_ref().unwrap().as_str(), "mxv-dynamodb");
    let ports: Vec<(&str, u16)> = services
        .ports
        .iter()
        .map(|(port, number)| (port.as_str(), number.get()))
        .collect();
    assert_eq!(ports, [("mxv-dynamodb", 41_001)]);
    assert!(main.publish.is_empty());
    assert!(matches!(
        &main.ready.as_ref().unwrap().check,
        ProbeCheck::Log { pattern, .. } if pattern == "^mxv-services ready$"
    ));
    assert!(main.env.is_empty());
}

#[test]
fn localstack_serves_the_declared_kinds_with_path_queue_urls_on_its_published_address() {
    let built = plan(APP);
    let aws = built.plan.app(&app("mxv-aws")).unwrap();
    assert_eq!(
        serde_json::to_value(&aws.source).unwrap()["image"]["image"],
        LOCALSTACK_IMAGE
    );
    let main = &aws.processes[0];
    // LocalStack's entrypoint ignores its arguments, and phase's plan hash
    // covers them, so a change of kinds replaces the container.
    assert_eq!(main.run, ["mxv-services=s3,kms"]);
    assert_eq!(main.port.as_ref().unwrap().as_str(), "mxv-aws");
    assert!(matches!(
        &main.ready.as_ref().unwrap().check,
        ProbeCheck::Http { path, .. } if path == "/_localstack/health"
    ));
    assert_eq!(
        resolved(&built, "mxv-aws"),
        BTreeMap::from(
            [
                ("LOCALSTACK_HOST", "127.0.0.1:41002"),
                ("SERVICES", "s3,kms"),
                ("SQS_ENDPOINT_STRATEGY", "path"),
                ("DISABLE_EVENTS", "1"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        )
    );
}

#[test]
fn localstacks_environment_writes_the_value_of_services_for_phases_digest() {
    let built = plan(APP);
    let applied = built
        .environment
        .resolve(
            &app("mxv-aws"),
            &ProcessName::try_from(MAIN.to_owned()).unwrap(),
            Instant::now(),
        )
        .unwrap();

    let mut written = Vec::new();
    applied.write_value("SERVICES", &mut written).unwrap();

    assert_eq!(written, b"s3,kms");
}

#[test]
fn the_provisioning_is_a_host_oneshot_that_reruns_and_depends_on_the_backends() {
    let built = plan(APP);
    let provision = built.plan.app(&app("mxv-provision")).unwrap();
    assert_eq!(provision.runner, RunnerKind::Host);
    assert_eq!(provision.depends, [app("mxv-services"), app("mxv-aws")]);
    let main = &provision.processes[0];
    assert_eq!(main.kind, ProcessKind::oneshot(false));
    assert!(main.ready.is_none() && main.port.is_none());
    assert_eq!(main.run[..3], ["/bin/mxv", "phase", "provision"]);
    let spec = Spec::parse(&main.run[3]).unwrap();
    assert_eq!((spec.dynamodb, spec.aws), (Some(41_001), Some(41_002)));
    assert!(spec.tables.contains_key("inbox"));
    assert!(main.env.is_empty());
}

#[test]
fn a_manifest_with_services_and_no_command_for_mxvs_own_apps_is_unrunnable() {
    let error = plan_with(APP, &CallerEnvironment::default(), None)
        .err()
        .unwrap();
    assert_eq!(error.unit.as_deref(), Some("mxv-services"));
}

#[test]
fn a_process_that_needs_services_depends_on_their_backends_and_the_provisioning() {
    let built = plan(APP);
    let worker = built.plan.app(&app("worker")).unwrap();
    assert_eq!(
        worker.depends,
        [app("mxv-services"), app("mxv-aws"), app("mxv-provision")]
    );
    assert!(built.plan.app(&app("api")).unwrap().depends.is_empty());
}

#[test]
fn a_sandboxed_process_that_needs_services_may_connect_to_their_backends() {
    let built = plan(
        "[service.store]\nkind = \"dynamodb\"\n[service.mail]\nkind = \"s3\"\n\
         [processes.worker]\nargv = [\"worker\"]\nneeds = [\"store\", \"mail\"]\npermissions = {}\n",
    );
    let worker = built.plan.app(&app("worker")).unwrap();
    let connect: Vec<&str> = worker
        .permissions
        .as_ref()
        .unwrap()
        .connect
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(connect, ["mxv-aws", "mxv-services"]);
}

#[test]
fn an_unsandboxed_process_that_needs_services_gets_no_permissions() {
    let built = plan(APP);
    assert!(
        built
            .plan
            .app(&app("worker"))
            .unwrap()
            .permissions
            .is_none()
    );
}

#[test]
fn a_process_that_needs_services_receives_only_the_aws_variables_that_mxv_sets() {
    let caller = CallerEnvironment::from_vars(
        [
            ("AWS_PROFILE", "production"),
            ("AWS_ACCESS_KEY_ID", "AKIAREAL"),
            ("AWS_REGION", "eu-west-1"),
            ("AWS_ENDPOINT_URL", "https://example.invalid"),
            ("LANG", "en_AU.UTF-8"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    let built = plan_with(APP, &caller, own_command()).unwrap();
    let worker = resolved(&built, "worker");
    let aws: BTreeMap<&str, &str> = worker
        .iter()
        .filter(|(name, _)| name.starts_with("AWS_"))
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        aws,
        BTreeMap::from([
            ("AWS_ACCESS_KEY_ID", "test"),
            ("AWS_CONFIG_FILE", "/dev/null"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("AWS_ENDPOINT_URL", "http://127.0.0.1:9"),
            ("AWS_ENDPOINT_URL_DYNAMODB", "http://127.0.0.1:41001"),
            ("AWS_ENDPOINT_URL_S3", "http://127.0.0.1:41002"),
            ("AWS_REGION", "us-east-1"),
            ("AWS_SECRET_ACCESS_KEY", "test"),
            ("AWS_SHARED_CREDENTIALS_FILE", "/dev/null"),
        ])
    );
    assert_eq!(worker["INBOX_TABLE"], "inbox");
    assert_eq!(worker["RUST_LOG"], "debug");
    assert_eq!(worker["LANG"], "en_AU.UTF-8");
    // A process that needs no service receives no AWS variable at all.
    let api = resolved(&built, "api");
    assert!(api.keys().all(|name| !name.starts_with("AWS_")), "{api:?}");
    let process = &built.plan.app(&app("worker")).unwrap().processes[0];
    assert!(process.env.contains("AWS_REGION"));
    assert!(!process.optional_env.contains("AWS_REGION"));
}

#[test]
fn the_provisioning_and_the_backends_receive_no_variable_of_the_caller() {
    let caller = CallerEnvironment::from_vars(
        [("AWS_PROFILE", "production"), ("LANG", "C")]
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
    );
    let built = plan_with(APP, &caller, own_command()).unwrap();
    assert!(resolved(&built, "mxv-provision").is_empty());
    assert!(resolved(&built, "mxv-services").is_empty());
}

#[test]
fn a_manifest_without_services_adds_no_app() {
    let built = plan("[processes.api]\nargv = [\"api\"]\n");
    let names: Vec<&str> = built
        .plan
        .apps()
        .iter()
        .map(|app| app.name.as_str())
        .collect();
    assert_eq!(names, ["api"]);
}
