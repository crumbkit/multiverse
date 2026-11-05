use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use phase::{AppName, EnvironmentSource as _, Plan, ProcessName, RunnerKind, WorkspaceId};

use crate::manifest::{Manifest, ManifestProblem};
use crate::name::WorkspaceName;
use crate::phase_plan::{Built, CallerEnvironment, Inputs, MAIN, PASSED_THROUGH, build};

const APP: &str = r#"
ports = ["api", "db"]

[processes.api]
argv = ["cargo", "run"]
cwd = "server"
env = { RUST_LOG = "info", DATABASE_URL = "postgres://127.0.0.1:{port.db}/app" }
ports = { PORT = "api" }
listen = "api"
ready = { port = "api", timeout_seconds = 30 }
depends_on = ["db"]

[containers.db]
image = "postgres:17.2"
env = { POSTGRES_PASSWORD = "local" }
ports = { 5432 = "db" }
volumes = { data = "/var/lib/postgresql/data" }
ready = { port = "db" }
"#;

fn manifest(text: &str) -> Manifest {
    Manifest::parse(text, Path::new("/repo/mxv.toml")).unwrap()
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
}

fn plan_of(manifest: &Manifest, caller: &CallerEnvironment) -> Built {
    let ports = BTreeMap::from([("api".to_owned(), 41_000), ("db".to_owned(), 41_001)]);
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    build(&Inputs {
        manifest,
        workspace: &workspace,
        id: &id,
        checkout: Path::new("/work/triage/app"),
        ports: &ports,
        grace: Duration::from_secs(5),
        caller,
    })
    .unwrap()
}

fn app(name: &str) -> AppName {
    AppName::try_from(name.to_owned()).unwrap()
}

/// Returns the variables that `built` resolves for the app `name`, with
/// their values.
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
fn each_unit_is_an_app_of_its_own_name_with_one_process_named_main_in_start_order() {
    let built = plan_of(&manifest(APP), &CallerEnvironment::default());
    let names: Vec<&str> = built
        .plan
        .apps()
        .iter()
        .map(|app| app.name.as_str())
        .collect();
    assert_eq!(names, ["db", "api"]);
    for app in built.plan.apps() {
        assert_eq!(app.processes.len(), 1);
        assert_eq!(app.processes[0].name.as_str(), MAIN);
    }
}

#[test]
fn a_host_process_keeps_its_command_directory_listen_port_readiness_and_dependencies() {
    let built = plan_of(&manifest(APP), &CallerEnvironment::default());
    let api = built.plan.app(&app("api")).unwrap();
    assert_eq!(api.runner, RunnerKind::Host);
    assert_eq!(api.depends, [app("db")]);
    let main = &api.processes[0];
    assert_eq!(main.run, ["cargo", "run"]);
    assert_eq!(main.cwd.as_deref(), Some(Path::new("server")));
    assert_eq!(main.port.as_ref().map(|port| port.as_str()), Some("api"));
    assert!(main.ready.is_some());
    assert_eq!(main.ready_timeout, Duration::from_secs(30));
    assert_eq!(main.stop.grace, Duration::from_secs(5));
    let ports: Vec<(&str, u16)> = api
        .ports
        .iter()
        .map(|(name, number)| (name.as_str(), number.get()))
        .collect();
    assert_eq!(ports, [("api", 41_000), ("db", 41_001)]);
}

#[test]
fn a_host_process_receives_its_env_with_ports_rendered_and_its_port_variables_by_name() {
    let built = plan_of(&manifest(APP), &CallerEnvironment::default());
    let api = built.plan.app(&app("api")).unwrap();
    assert_eq!(
        api.processes[0].env,
        BTreeSet::from(["DATABASE_URL", "PORT", "RUST_LOG"].map(str::to_owned))
    );
    let values = resolved(&built, "api");
    assert_eq!(values["PORT"], "41000");
    assert_eq!(values["DATABASE_URL"], "postgres://127.0.0.1:41001/app");
    assert_eq!(values["RUST_LOG"], "info");
}

#[test]
fn a_container_publishes_its_ports_mounts_its_volumes_and_receives_only_its_env() {
    let caller = CallerEnvironment::from_vars(vars(&[("LANG", "C.UTF-8")]));
    let built = plan_of(&manifest(APP), &caller);
    let db = built.plan.app(&app("db")).unwrap();
    assert_eq!(db.runner, RunnerKind::Container);
    let main = &db.processes[0];
    let published: Vec<(&str, u16)> = main
        .publish
        .iter()
        .map(|(name, number)| (name.as_str(), number.get()))
        .collect();
    assert_eq!(published, [("db", 5432)]);
    let volumes: Vec<(&str, &Path)> = main
        .volumes
        .iter()
        .map(|(name, mount)| (name.as_str(), mount.as_path()))
        .collect();
    assert_eq!(
        volumes,
        [("data", PathBuf::from("/var/lib/postgresql/data").as_path())]
    );
    assert!(main.ready.is_some());
    assert_eq!(main.env, BTreeSet::from(["POSTGRES_PASSWORD".to_owned()]));
    assert_eq!(
        resolved(&built, "db"),
        BTreeMap::from([("POSTGRES_PASSWORD".to_owned(), "local".to_owned())])
    );
}

#[test]
fn a_host_process_receives_the_passed_through_variables_of_the_caller_and_no_others() {
    let caller = CallerEnvironment::from_vars(vars(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/someone"),
        ("LANG", "C.UTF-8"),
        ("LC_TIME", "C"),
        ("NODE_USE_SYSTEM_CA", "1"),
        ("AWS_SECRET_ACCESS_KEY", "never-passed"),
        ("RUSTFLAGS", "-C never-passed"),
    ]));
    let built = plan_of(&manifest(APP), &caller);
    let values = resolved(&built, "api");
    assert_eq!(values["LANG"], "C.UTF-8");
    assert_eq!(values["LC_TIME"], "C");
    assert_eq!(values["NODE_USE_SYSTEM_CA"], "1");
    for name in ["PATH", "HOME", "AWS_SECRET_ACCESS_KEY", "RUSTFLAGS"] {
        assert!(!values.contains_key(name), "{name} passed through");
    }
    assert_eq!(
        caller.inherited(),
        BTreeMap::from([
            ("HOME".to_owned(), "/home/someone".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ])
    );
}

#[test]
fn the_plan_and_its_json_depend_on_which_passed_through_variables_the_caller_holds() {
    let manifest = manifest(APP);
    let without = plan_of(&manifest, &CallerEnvironment::default()).plan;
    let with = plan_of(
        &manifest,
        &CallerEnvironment::from_vars(vars(&[("TZ", "UTC")])),
    )
    .plan;
    let other_value = plan_of(
        &manifest,
        &CallerEnvironment::from_vars(vars(&[("TZ", "Europe/Helsinki")])),
    )
    .plan;
    assert_ne!(without, with);
    let json = |plan: &Plan| serde_json::to_string(plan).unwrap();
    assert_ne!(json(&without), json(&with));
    assert!(json(&with).contains("\"TZ\""));
    assert_eq!(json(&with), json(&other_value));
}

#[test]
fn every_passed_through_name_is_one_that_the_caller_environment_keeps() {
    let pairs: Vec<(&str, &str)> = PASSED_THROUGH.iter().map(|name| (*name, "x")).collect();
    let caller = CallerEnvironment::from_vars(vars(&pairs));
    let built = plan_of(&manifest(APP), &caller);
    let env = &built.plan.app(&app("api")).unwrap().processes[0].env;
    for name in PASSED_THROUGH {
        assert!(env.contains(*name), "{name} is missing");
    }
}

#[test]
fn the_debug_form_of_the_caller_environment_shows_names_and_no_values() {
    let caller = CallerEnvironment::from_vars(vars(&[("SSH_AUTH_SOCK", "/private/agent.sock")]));
    let shown = format!("{caller:?}");
    assert!(shown.contains("SSH_AUTH_SOCK"));
    assert!(!shown.contains("/private/agent.sock"));
}

#[test]
fn a_process_that_sets_path_or_home_is_unrunnable() {
    for text in [
        "[processes.api]\nargv = [\"true\"]\nenv = { PATH = \"/bin\" }\n",
        "ports = [\"api\"]\n[processes.api]\nargv = [\"true\"]\nports = { HOME = \"api\" }\n",
    ] {
        let error = Manifest::parse(text, Path::new("/repo/mxv.toml")).unwrap_err();
        assert!(
            matches!(&error.problem, ManifestProblem::Unrunnable { message } if message.contains("phase gives every process")),
            "{error:?}"
        );
        assert_eq!(error.line, Some(1 + usize::from(text.starts_with("ports"))));
    }
}

#[test]
fn a_unit_name_with_an_uppercase_letter_is_refused_with_its_line() {
    let error = Manifest::parse(
        "[processes.ok]\nargv = [\"true\"]\n[processes.Api]\nargv = [\"true\"]\n",
        Path::new("/repo/mxv.toml"),
    )
    .unwrap_err();
    assert_eq!(
        error.problem,
        ManifestProblem::UnitName {
            name: "Api".to_owned()
        }
    );
    assert_eq!(error.line, Some(3));
}

#[test]
fn docker_variables_reach_only_the_docker_environment_and_never_a_host_process() {
    let caller = CallerEnvironment::from_vars(vars(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/someone"),
        ("DOCKER_HOST", "unix:///docker.sock"),
        ("LANG", "C.UTF-8"),
    ]));
    assert_eq!(
        caller.docker_environment(),
        BTreeMap::from([
            ("DOCKER_HOST".to_owned(), "unix:///docker.sock".to_owned()),
            ("HOME".to_owned(), "/home/someone".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ])
    );
    let built = plan_of(&manifest(APP), &caller);
    assert!(!resolved(&built, "api").contains_key("DOCKER_HOST"));
    assert!(
        !built.plan.app(&app("api")).unwrap().processes[0]
            .env
            .contains("DOCKER_HOST")
    );
}

#[test]
fn the_environment_returns_every_variable_that_each_process_of_the_plan_names() {
    // phase fails a start when the source leaves out a variable that a
    // process names and does not mark optional, and mxv marks none optional.
    let caller = CallerEnvironment::from_vars(vars(&[("LANG", "C.UTF-8"), ("LC_TIME", "C")]));
    let built = plan_of(&manifest(APP), &caller);
    for app in built.plan.apps() {
        for process in &app.processes {
            assert!(process.optional_env.is_empty());
            let applied = built
                .environment
                .resolve(&app.name, &process.name, Instant::now())
                .unwrap();
            assert_eq!(applied.names(), process.env, "{}", app.name);
        }
    }
}
