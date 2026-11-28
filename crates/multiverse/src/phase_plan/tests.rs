#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use phase::{
    AppName, EnvironmentSource as _, Plan, PortName, ProcessName, RunnerKind, Source, WorkspaceId,
};

use crate::manifest::{Manifest, ManifestProblem};
use crate::name::WorkspaceName;
use crate::needs::{Check, Met, ProvidedVariable, SecretKind, SecretVariable};
use crate::phase_plan::{
    Built, CallerEnvironment, Inputs, MAIN, PASSED_THROUGH, ProxyUrls, TASK_APP, TaskEntry,
    UnrunnableKind, build, build_with_task, roots,
};
use crate::record::{Checkout, Workspace};
use crate::testing::{unmade_clone, write};

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
        roots: Vec::new(),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller,
        own_command: Some(vec!["/bin/mxv".to_owned(), "phase".to_owned()]),
        urls: ProxyUrls::default(),
        read_starmaps: true,
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
    let process = &api.processes[0];
    let required: BTreeSet<&str> = process
        .env
        .difference(&process.optional_env)
        .map(String::as_str)
        .collect();
    assert_eq!(
        required,
        BTreeSet::from(["DATABASE_URL", "PORT", "RUST_LOG"])
    );
    let values = resolved(&built, "api");
    assert_eq!(values["PORT"], "41000");
    assert_eq!(values["DATABASE_URL"], "postgres://127.0.0.1:41001/app");
    assert_eq!(values["RUST_LOG"], "info");
}

#[test]
fn a_containers_owner_reaches_the_phase_plan_unchanged() {
    let caller = CallerEnvironment::from_vars(vars(&[]));
    for owner in ["999:999", "keep"] {
        let text = APP.replace(
            "ready = { port = \"db\" }\n",
            &format!("ready = {{ port = \"db\" }}\nowner = \"{owner}\"\n"),
        );
        let built = plan_of(&manifest(&text), &caller);
        let db = built.plan.app(&app("db")).unwrap();
        assert_eq!(db.container.owner.as_deref(), Some(owner));
    }
}

#[test]
fn a_container_without_an_owner_key_has_no_owner_in_the_phase_plan() {
    let caller = CallerEnvironment::from_vars(vars(&[]));
    let built = plan_of(&manifest(APP), &caller);
    let db = built.plan.app(&app("db")).unwrap();
    assert_eq!(db.container.owner, None);
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
fn the_plan_and_its_json_are_the_same_whichever_passed_through_variables_the_caller_holds() {
    let manifest = manifest(APP);
    let without = plan_of(&manifest, &CallerEnvironment::default()).plan;
    let with = plan_of(
        &manifest,
        &CallerEnvironment::from_vars(vars(&[
            ("TZ", "UTC"),
            ("LANG", "C.UTF-8"),
            ("LC_TIME", "C"),
            ("SSH_AUTH_SOCK", "/private/agent.sock"),
        ])),
    )
    .plan;
    assert_eq!(without, with);
    let json = |plan: &Plan| serde_json::to_string(plan).unwrap();
    assert_eq!(json(&without), json(&with));
    assert!(json(&without).contains("\"TZ\""));
}

#[test]
fn a_passed_through_variable_is_optional_unless_the_manifest_sets_it() {
    let built = plan_of(&manifest(APP), &CallerEnvironment::default());
    let api = &built.plan.app(&app("api")).unwrap().processes[0];
    for name in ["TZ", "LANG", "LC_TIME", "SSH_AUTH_SOCK"] {
        assert!(api.env.contains(name), "{name} is not declared");
        assert!(api.optional_env.contains(name), "{name} is required");
    }
    for name in ["RUST_LOG", "DATABASE_URL", "PORT"] {
        assert!(api.env.contains(name), "{name} is not declared");
        assert!(!api.optional_env.contains(name), "{name} is optional");
    }
    let db = &built.plan.app(&app("db")).unwrap().processes[0];
    assert!(db.optional_env.is_empty());
    assert!(!db.env.contains("TZ"));
}

#[test]
fn a_locale_variable_outside_the_standard_categories_does_not_pass() {
    let caller = CallerEnvironment::from_vars(vars(&[("LC_TIME", "C"), ("LC_TERMINAL", "iTerm2")]));
    let built = plan_of(&manifest(APP), &caller);
    let values = resolved(&built, "api");
    assert_eq!(values["LC_TIME"], "C");
    assert!(!values.contains_key("LC_TERMINAL"));
    assert!(PASSED_THROUGH.contains(&"LC_ALL"));
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
    // process names and does not mark optional. The source names each
    // required variable, and each optional one that the caller holds.
    let caller = CallerEnvironment::from_vars(vars(&[("LANG", "C.UTF-8"), ("LC_TIME", "C")]));
    let built = plan_of(&manifest(APP), &caller);
    for app in built.plan.apps() {
        for process in &app.processes {
            let applied = built
                .environment
                .resolve(&app.name, &process.name, Instant::now())
                .unwrap();
            let names = applied.names();
            let required: BTreeSet<String> = process
                .env
                .difference(&process.optional_env)
                .cloned()
                .collect();
            assert!(names.is_subset(&process.env), "{}", app.name);
            assert!(required.is_subset(&names), "{}", app.name);
        }
    }
    let api = built
        .environment
        .resolve(
            &app("api"),
            &ProcessName::try_from(MAIN.to_owned()).unwrap(),
            Instant::now(),
        )
        .unwrap()
        .names();
    assert!(api.contains("LANG") && api.contains("LC_TIME"));
    assert!(!api.contains("TZ"));
}

#[test]
fn the_environment_of_a_process_writes_each_value_and_reports_a_name_it_lacks() {
    let caller = CallerEnvironment::from_vars(vars(&[("LANG", "C.UTF-8")]));
    let built = plan_of(&manifest(APP), &caller);
    let applied = built
        .environment
        .resolve(
            &app("api"),
            &ProcessName::try_from(MAIN.to_owned()).unwrap(),
            Instant::now(),
        )
        .unwrap();

    let mut written = Vec::new();
    applied.write_value("LANG", &mut written).unwrap();

    assert_eq!(written, b"C.UTF-8");
    let missing = applied
        .write_value("NOT_A_VARIABLE", &mut Vec::new())
        .unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

/// The manifest of a checkout whose process `server` runs in `server/` and
/// listens on its port `http`.
const SERVER: &str = r#"ports = ["http"]

[processes.server]
argv = ["cargo", "run"]
cwd = "server"
ports = { PORT = "http" }
listen = "http"
"#;

/// Returns a workspace `triage` in `dir` with the checkouts `api` and
/// `web`, each with the manifest [`SERVER`], and a workspace manifest whose
/// process `proxy` runs in `web/` after `api-server`.
fn two_checkouts(dir: &Path) -> Workspace {
    let path = dir.join("triage");
    let checkouts = ["api", "web"]
        .iter()
        .map(|folder| {
            std::fs::create_dir_all(path.join(folder).join("server")).unwrap();
            write(path.join(folder).join("mxv.toml"), SERVER);
            Checkout::new(
                (*folder).to_owned(),
                &dir.join("source").join(folder),
                path.join(folder),
                "triage".to_owned(),
                "refs/remotes/origin/main".to_owned(),
                unmade_clone(),
            )
        })
        .collect();
    write(
        path.join("mxv.toml"),
        "[processes.proxy]\nargv = [\"caddy\", \"run\"]\ncwd = \"web\"\ndepends_on = [\"api-server\"]\n",
    );
    Workspace::new("triage".parse().unwrap(), path, checkouts)
}

#[test]
fn a_merged_manifest_maps_onto_apps_and_ports_whose_names_do_not_clash() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = two_checkouts(&dir.path().canonicalize().unwrap());
    let manifest = workspace.manifest().unwrap().unwrap();
    let ports = BTreeMap::from([
        ("api-http".to_owned(), 41_000),
        ("web-http".to_owned(), 41_001),
    ]);
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let roots = roots(&workspace);
    let caller = CallerEnvironment::default();
    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &name,
        id: &id,
        checkout: &workspace.path,
        roots,
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    })
    .unwrap();
    let names: BTreeSet<String> = built
        .plan
        .apps()
        .iter()
        .map(|app| app.name.to_string())
        .collect();
    assert_eq!(
        names,
        BTreeSet::from(["api-server", "web-server", "proxy"].map(str::to_owned))
    );
    let mut port_names = BTreeSet::new();
    for app in built.plan.apps() {
        for port in app.ports.keys() {
            assert!(port_names.insert(port.to_string()), "{port} twice");
        }
    }
    assert_eq!(
        port_names,
        BTreeSet::from(["api-http", "web-http"].map(str::to_owned))
    );
    // The workspace manifest's process `proxy` runs in the checkout `web`,
    // so its source is that checkout.
    for (app_name, source, cwd) in [
        ("api-server", workspace.path.join("api"), Some("server")),
        ("web-server", workspace.path.join("web"), Some("server")),
        ("proxy", workspace.path.join("web"), None),
    ] {
        let plan = built.plan.app(&app(app_name)).unwrap();
        assert_eq!(plan.source, Source::checkout(source, None), "{app_name}");
        assert_eq!(
            plan.processes[0].cwd.as_deref(),
            cwd.map(Path::new),
            "{app_name}"
        );
    }
    assert_eq!(
        built.plan.app(&app("proxy")).unwrap().depends,
        vec![app("api-server")]
    );
}

#[test]
fn a_process_whose_directory_is_outside_every_root_is_unrunnable() {
    let mut process = manifest(APP);
    process.processes.get_mut("api").unwrap().cwd = PathBuf::from("/elsewhere");
    let ports = BTreeMap::from([("api".to_owned(), 41_000), ("db".to_owned(), 41_001)]);
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let roots = vec![PathBuf::from("/work/triage/app")];
    let caller = CallerEnvironment::default();
    let Err(unrunnable) = build(&Inputs {
        manifest: &process,
        workspace: &name,
        id: &id,
        checkout: Path::new("/work/triage/app"),
        roots,
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    }) else {
        panic!("a directory outside every root built a plan");
    };
    assert_eq!(unrunnable.unit.as_deref(), Some("api"));
    assert!(
        unrunnable.message.contains("/elsewhere"),
        "{}",
        unrunnable.message
    );
}

/// Returns the source and the `cwd` that the plan gives the process `api`
/// of [`APP`] when its `cwd` is `cwd`, in a workspace whose checkouts are
/// `api` and `web`.
fn placed_at(cwd: &str) -> Result<(Source, Option<PathBuf>), crate::phase_plan::Unrunnable> {
    let mut manifest = manifest(APP);
    manifest.processes.get_mut("api").unwrap().cwd = PathBuf::from(cwd);
    let ports = BTreeMap::from([("api".to_owned(), 41_000), ("db".to_owned(), 41_001)]);
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::default();
    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &name,
        id: &id,
        checkout: Path::new("/work/triage"),
        roots: vec![
            PathBuf::from("/work/triage/api"),
            PathBuf::from("/work/triage/web"),
            PathBuf::from("/work/triage"),
        ],
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    })?;
    let plan = built.plan.app(&app("api")).unwrap();
    Ok((plan.source.clone(), plan.processes[0].cwd.clone()))
}

#[test]
fn a_cwd_that_passes_through_one_checkout_into_another_runs_from_the_other() {
    assert_eq!(
        placed_at("/work/triage/api/../web/src").unwrap(),
        (
            Source::checkout(PathBuf::from("/work/triage/web"), None),
            Some(PathBuf::from("src"))
        )
    );
}

#[test]
fn a_cwd_that_leaves_the_workspace_is_unrunnable() {
    let Err(unrunnable) = placed_at("/work/triage/../elsewhere") else {
        panic!("a cwd outside the workspace built a plan");
    };
    assert_eq!(unrunnable.unit.as_deref(), Some("api"));
}

/// A checkout's manifest whose process `server` runs sandboxed: it writes
/// `target`, listens on `http` and connects to the container `db`.
const SANDBOXED_SERVER: &str = r#"ports = ["http", "db"]

[containers.db]
image = "postgres:17.2"
ports = { 5432 = "db" }

[processes.server]
argv = ["cargo", "run"]
cwd = "server"
ports = { PORT = "http", DB_PORT = "db" }
listen = "http"
depends_on = ["db"]
permissions = { write = ["target", "$STATE"], listen = ["http"], connect = ["db"] }

[processes.tool]
argv = ["true"]
"#;

/// Returns the strings of `names`.
fn strings<const N: usize>(names: [&str; N]) -> BTreeSet<String> {
    names.into_iter().map(str::to_owned).collect()
}

#[test]
fn a_process_with_permissions_gives_its_app_its_writes_listen_ports_and_connections() {
    let ports = BTreeMap::from([("http".to_owned(), 41_000), ("db".to_owned(), 41_001)]);
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::default();
    let manifest = manifest(SANDBOXED_SERVER);
    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &name,
        id: &id,
        checkout: Path::new("/work/triage/app"),
        roots: Vec::new(),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    })
    .unwrap();
    let server = built.plan.app(&app("server")).unwrap();
    let permissions = server.permissions.as_ref().unwrap();
    assert_eq!(permissions.write, strings(["target", "$STATE"]));
    assert_eq!(
        permissions.listen,
        BTreeSet::from([PortName::try_from("http".to_owned()).unwrap()])
    );
    assert_eq!(permissions.connect, strings(["db"]));
    assert!(permissions.secrets.is_empty());
    assert_eq!(server.depends, vec![app("db")]);
}

#[test]
fn the_plan_of_an_existing_manifest_and_so_its_digest_are_unchanged() {
    let ports = BTreeMap::from([("http".to_owned(), 41_000), ("db".to_owned(), 41_001)]);
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::default();
    let manifest = manifest(SANDBOXED_SERVER);

    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &name,
        id: &id,
        checkout: Path::new("/work/triage/app"),
        roots: Vec::new(),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    })
    .unwrap();

    // Phase's plan digest, which the run record keeps to tell that a plan
    // changed, is a hash of these JSON bytes. The file holds the bytes that
    // multiverse 0.1.1 built for this manifest, so equal bytes mean an
    // unchanged digest and no restart of a running unit.
    assert_eq!(
        serde_json::to_string(&built.plan).unwrap(),
        include_str!("sandboxed-server-plan.json").trim_end(),
    );
}

#[test]
fn the_requested_permissions_of_host_units_need_no_ports_and_match_the_plan() {
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::default();
    let manifest = manifest(SANDBOXED_SERVER);
    let no_ports = BTreeMap::new();
    let units = manifest.start_order(&[]).unwrap();

    let requested = crate::phase_plan::permissions(
        &Inputs {
            manifest: &manifest,
            workspace: &name,
            id: &id,
            checkout: Path::new("/work/triage/app"),
            roots: Vec::new(),
            ports: &no_ports,
            workspace_dir: None,
            grace: Duration::from_secs(5),
            caller: &caller,
            own_command: None,
            urls: crate::phase_plan::ProxyUrls::default(),
            read_starmaps: true,
        },
        &units,
    )
    .unwrap();

    let mut server = phase::Permissions::default();
    server.write = strings(["target"]);
    server.listen = BTreeSet::from([PortName::try_from("http".to_owned()).unwrap()]);
    server.connect = strings(["db"]);
    assert_eq!(
        requested,
        BTreeMap::from([
            (app("server"), phase::Grant::Sandboxed(server)),
            (app("tool"), phase::Grant::Unsandboxed),
        ])
    );
}

#[test]
fn a_process_without_permissions_and_a_container_give_their_apps_none() {
    let built = plan_of(&manifest(APP), &CallerEnvironment::default());
    assert_eq!(built.plan.app(&app("api")).unwrap().permissions, None);
    assert_eq!(built.plan.app(&app("db")).unwrap().permissions, None);
}

#[test]
fn a_merged_manifest_gives_each_checkouts_permissions_its_prefixed_port_and_unit_names() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let path = root.join("triage");
    let checkouts = ["api", "web"]
        .iter()
        .map(|folder| {
            std::fs::create_dir_all(path.join(folder).join("server")).unwrap();
            write(path.join(folder).join("mxv.toml"), SANDBOXED_SERVER);
            Checkout::new(
                (*folder).to_owned(),
                &root.join("source").join(folder),
                path.join(folder),
                "triage".to_owned(),
                "refs/remotes/origin/main".to_owned(),
                unmade_clone(),
            )
        })
        .collect();
    // The workspace manifest's process runs in the checkout `web`, and its
    // write path is relative to the workspace's directory.
    write(
        path.join("mxv.toml"),
        "[processes.proxy]\nargv = [\"caddy\", \"run\"]\ncwd = \"web\"\ndepends_on = [\"api-server\"]\npermissions = { write = [\"web/dist\"], connect = [\"api-server\"] }\n",
    );
    let workspace = Workspace::new("triage".parse().unwrap(), path, checkouts);
    let manifest = workspace.manifest().unwrap().unwrap();
    let ports: BTreeMap<String, u16> = ["api-http", "api-db", "web-http", "web-db"]
        .into_iter()
        .zip(41_000..)
        .map(|(port, number)| (port.to_owned(), number))
        .collect();
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::default();
    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &name,
        id: &id,
        checkout: &workspace.path,
        roots: roots(&workspace),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    })
    .unwrap();
    for prefix in ["api", "web"] {
        let server = built.plan.app(&app(&format!("{prefix}-server"))).unwrap();
        let permissions = server.permissions.as_ref().unwrap();
        assert_eq!(permissions.write, strings(["target", "$STATE"]), "{prefix}");
        assert_eq!(
            permissions.listen,
            BTreeSet::from([PortName::try_from(format!("{prefix}-http")).unwrap()]),
            "{prefix}"
        );
        assert_eq!(
            permissions.connect,
            BTreeSet::from([format!("{prefix}-db")]),
            "{prefix}"
        );
        assert_eq!(
            built
                .plan
                .app(&app(&format!("{prefix}-tool")))
                .unwrap()
                .permissions,
            None
        );
    }
    let proxy = built.plan.app(&app("proxy")).unwrap();
    let permissions = proxy.permissions.as_ref().unwrap();
    assert_eq!(permissions.write, strings(["dist"]));
    assert_eq!(permissions.connect, strings(["api-server"]));
}

/// Returns why the plan refuses the process `server` of
/// [`SANDBOXED_SERVER`] when it runs in the checkout `web` of a workspace
/// and may write the absolute path `write`.
fn unrunnable_write(write: &str) -> crate::phase_plan::Unrunnable {
    let mut manifest = manifest(SANDBOXED_SERVER);
    let server = manifest.processes.get_mut("server").unwrap();
    server.cwd = PathBuf::from("/work/triage/web");
    server.permissions.as_mut().unwrap().write = BTreeSet::from([write.to_owned()]);
    let ports = BTreeMap::from([("http".to_owned(), 41_000), ("db".to_owned(), 41_001)]);
    let name = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::default();
    let Err(unrunnable) = build(&Inputs {
        manifest: &manifest,
        workspace: &name,
        id: &id,
        checkout: Path::new("/work/triage"),
        roots: vec![
            PathBuf::from("/work/triage/api"),
            PathBuf::from("/work/triage/web"),
            PathBuf::from("/work/triage"),
        ],
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: crate::phase_plan::ProxyUrls::default(),
        read_starmaps: true,
    }) else {
        panic!("the write path {write} built a plan");
    };
    assert_eq!(unrunnable.unit.as_deref(), Some("server"));
    unrunnable
}

#[test]
fn a_workspace_manifests_write_path_outside_the_checkout_its_process_runs_in_is_unrunnable() {
    let message = unrunnable_write("/work/triage/api/target").message;
    assert!(message.contains("/work/triage/api/target"), "{message}");
    assert!(message.contains("outside /work/triage/web"), "{message}");
}

#[test]
fn a_write_path_that_is_the_whole_checkout_its_process_runs_in_is_unrunnable_and_says_so() {
    let message = unrunnable_write("/work/triage/web").message;
    assert!(
        message.contains("the whole checkout it runs in"),
        "{message}"
    );
    assert!(!message.contains("outside"), "{message}");
}

const WITH_TASK: &str = "[processes.api]\nargv = [\"true\"]\n\n[tasks.test]\nargv = [\"true\"]\n";

#[test]
fn a_tasks_section_leaves_the_plan_that_up_runs_unchanged() {
    let caller = CallerEnvironment::default();
    let with = plan_of(&manifest(WITH_TASK), &caller);
    let without = plan_of(&manifest("[processes.api]\nargv = [\"true\"]\n"), &caller);
    assert_eq!(with.plan.apps().len(), 1);
    assert_eq!(
        serde_json::to_string(&with.plan).unwrap(),
        serde_json::to_string(&without.plan).unwrap()
    );
}

/// Returns the plan of `APP` with a task that needs the services
/// `services`, run in the directory `dir` of `/work/triage/app`.
fn task_plan(dir: &Path) -> Built {
    task_plan_with(dir, &BTreeMap::new())
}

fn task_plan_with(dir: &Path, own_ports: &BTreeMap<String, String>) -> Built {
    let manifest = manifest(APP);
    let ports = BTreeMap::from([
        ("api".to_owned(), 41_000),
        ("web-ui".to_owned(), 41_001),
        ("db".to_owned(), 41_002),
    ]);
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::from_vars(vars(&[("PATH", "/bin"), ("TERM", "dumb")]));
    build_with_task(
        &Inputs {
            manifest: &manifest,
            workspace: &workspace,
            id: &id,
            checkout: Path::new("/work/triage/app"),
            roots: Vec::new(),
            ports: &ports,
            workspace_dir: None,
            grace: Duration::from_secs(5),
            caller: &caller,
            own_command: None,
            urls: crate::phase_plan::ProxyUrls::default(),
            read_starmaps: true,
        },
        &TaskEntry {
            root: Path::new("/work/triage/app"),
            dir,
            argv: vec!["mise".to_owned(), "run".to_owned(), "test".to_owned()],
            services: Vec::new(),
            own_ports,
        },
    )
    .unwrap()
    .built
}

#[test]
fn a_task_receives_its_own_ports_under_the_names_in_its_own_file() {
    let own = BTreeMap::from([("ui".to_owned(), "web-ui".to_owned())]);
    let built = task_plan_with(Path::new("."), &own);
    let task = built
        .plan
        .apps()
        .iter()
        .find(|plan| plan.name.as_str() == TASK_APP)
        .unwrap();
    assert!(task.processes[0].env.contains("MXV_PORT_UI"));
    assert!(task.processes[0].env.contains("MXV_PORT_WEB_UI"));
    assert!(task.processes[0].env.contains("MXV_PORT_API"));
}

#[test]
fn a_task_enters_the_plan_under_the_reserved_app_with_no_sandbox() {
    let built = task_plan(Path::new("."));
    let task = built
        .plan
        .apps()
        .iter()
        .find(|plan| plan.name.as_str() == TASK_APP)
        .unwrap();
    assert_eq!(task.runner, RunnerKind::Host);
    assert_eq!(task.permissions, None);
    assert_eq!(task.processes[0].cwd, None);
    assert_eq!(task.processes[0].run, ["mise", "run", "test"]);
}

#[test]
fn a_task_runs_in_its_dir_and_names_each_port_as_a_variable() {
    let built = task_plan(Path::new("server"));
    let task = built
        .plan
        .apps()
        .iter()
        .find(|plan| plan.name.as_str() == TASK_APP)
        .unwrap();
    assert_eq!(task.processes[0].cwd.as_deref(), Some(Path::new("server")));
    assert!(task.processes[0].env.contains("MXV_PORT_WEB_UI"));
    assert!(task.processes[0].env.contains("MXV_PORT_API"));
    assert!(task.processes[0].optional_env.contains("LANG"));
}

const ROUTED: &str = r#"
ports = ["web-ui", "api", "db"]
routes = ["web-ui", "api"]

[processes.web]
argv = ["pnpm", "dev"]
ports = { PORT = "web-ui" }
listen = "web-ui"

[processes.api]
argv = ["api"]
ports = { PORT = "api" }
listen = "api"

[processes.worker]
argv = ["worker"]

[containers.db]
image = "postgres:17.2"
ports = { 5432 = "db" }
"#;

fn https(port: u16) -> std::net::SocketAddr {
    std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))
}

/// Returns the proxy's names for the workspace `triage` of `ROUTED`, served
/// beside the workspaces `others`.
fn urls_of(workspace: &str, port: u16, others: &[&str]) -> ProxyUrls {
    use crate::proxy::{Declared, derive};
    let declare = |name: &str| {
        Declared::new(
            WorkspaceName::try_from(name.to_owned()).unwrap(),
            vec!["web-ui".to_owned(), "api".to_owned()],
            BTreeMap::from([("web-ui".to_owned(), 41_001), ("api".to_owned(), 41_000)]),
            BTreeMap::new(),
            BTreeMap::new(),
        )
    };
    let mut declared = vec![declare(workspace)];
    declared.extend(others.iter().map(|name| declare(name)));
    ProxyUrls::of(
        &WorkspaceName::try_from(workspace.to_owned()).unwrap(),
        &derive(declared).served,
        https(port),
        crate::proxy::DOMAIN,
    )
}

fn routed_plan(urls: ProxyUrls, text: &str) -> Built {
    let manifest = manifest(text);
    let ports = BTreeMap::from([
        ("api".to_owned(), 41_000),
        ("web-ui".to_owned(), 41_001),
        ("db".to_owned(), 41_002),
    ]);
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
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls,
        read_starmaps: true,
    })
    .unwrap()
}

fn proxied(values: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    values
        .iter()
        .filter(|(name, _)| {
            name.starts_with("MXV_HOST") || name.starts_with("MXV_URL") || *name == "MXV_DOMAIN"
        })
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

#[test]
fn a_process_of_a_workspace_with_routes_receives_its_domain_and_every_route_name() {
    let built = routed_plan(urls_of("triage", 443, &[]), ROUTED);
    let worker = resolved(&built, "worker");
    assert_eq!(
        proxied(&worker),
        BTreeMap::from([
            ("MXV_DOMAIN", "triage.mxv.localhost"),
            ("MXV_HOST_API", "api.triage.mxv.localhost"),
            ("MXV_URL_API", "https://api.triage.mxv.localhost"),
            ("MXV_HOST_WEB_UI", "web-ui.triage.mxv.localhost"),
            ("MXV_URL_WEB_UI", "https://web-ui.triage.mxv.localhost"),
        ])
    );
}

#[test]
fn a_process_that_listens_on_a_served_route_also_receives_its_own_host_and_url() {
    let built = routed_plan(urls_of("triage", 443, &[]), ROUTED);
    let web = resolved(&built, "web");
    assert_eq!(web["MXV_HOST"], "web-ui.triage.mxv.localhost");
    assert_eq!(web["MXV_URL"], "https://web-ui.triage.mxv.localhost");
    let worker = resolved(&built, "worker");
    assert!(!worker.contains_key("MXV_HOST") && !worker.contains_key("MXV_URL"));
}

#[test]
fn the_variables_are_in_the_plan_of_the_process_that_receives_them() {
    let built = routed_plan(urls_of("triage", 443, &[]), ROUTED);
    let web = built
        .plan
        .apps()
        .iter()
        .find(|app| app.name.as_str() == "web")
        .unwrap();
    assert!(web.processes[0].env.contains("MXV_URL"));
    assert!(web.processes[0].env.contains("MXV_DOMAIN"));
}

#[test]
fn a_url_carries_the_https_port_when_it_is_not_443() {
    let built = routed_plan(urls_of("triage", 8443, &[]), ROUTED);
    let web = resolved(&built, "web");
    assert_eq!(web["MXV_URL"], "https://web-ui.triage.mxv.localhost:8443");
    assert_eq!(web["MXV_HOST"], "web-ui.triage.mxv.localhost");
}

#[test]
fn a_container_receives_no_proxy_variable() {
    let built = routed_plan(urls_of("triage", 443, &[]), ROUTED);
    assert!(proxied(&resolved(&built, "db")).is_empty());
}

#[test]
fn a_workspace_without_routes_receives_no_proxy_variable() {
    let built = routed_plan(ProxyUrls::default(), ROUTED);
    for unit in ["web", "worker", "db"] {
        assert!(proxied(&resolved(&built, unit)).is_empty(), "{unit}");
    }
}

#[test]
fn a_workspace_whose_host_label_another_keeps_receives_no_proxy_variable() {
    assert_eq!(
        urls_of("my_notes", 443, &["my-notes"]),
        ProxyUrls::default()
    );
    assert_ne!(
        urls_of("my-notes", 443, &["my_notes"]),
        ProxyUrls::default()
    );
}

#[test]
fn a_variable_of_the_manifest_with_the_same_name_wins() {
    let text = ROUTED.replace(
        "[processes.worker]\nargv = [\"worker\"]\n",
        "[processes.worker]\nargv = [\"worker\"]\nenv = { MXV_URL_API = \"https://mine.example.com\", MXV_DOMAIN = \"mine.example.com\" }\n",
    );
    let built = routed_plan(urls_of("triage", 443, &[]), &text);
    let worker = resolved(&built, "worker");
    assert_eq!(worker["MXV_URL_API"], "https://mine.example.com");
    assert_eq!(worker["MXV_DOMAIN"], "mine.example.com");
    assert_eq!(worker["MXV_HOST_API"], "api.triage.mxv.localhost");
}

#[test]
fn a_task_receives_the_domain_and_each_route_but_no_host_or_url_of_its_own() {
    let manifest = manifest(ROUTED);
    let ports = BTreeMap::from([
        ("api".to_owned(), 41_000),
        ("web-ui".to_owned(), 41_001),
        ("db".to_owned(), 41_002),
    ]);
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let own = BTreeMap::new();
    let built = build_with_task(
        &Inputs {
            manifest: &manifest,
            workspace: &workspace,
            id: &id,
            checkout: Path::new("/work/triage/app"),
            roots: Vec::new(),
            ports: &ports,
            workspace_dir: None,
            grace: Duration::from_secs(5),
            caller: &CallerEnvironment::default(),
            own_command: None,
            urls: urls_of("triage", 443, &[]),
            read_starmaps: true,
        },
        &TaskEntry {
            root: Path::new("/work/triage/app"),
            dir: Path::new("."),
            argv: vec!["mise".to_owned(), "run".to_owned(), "test".to_owned()],
            services: Vec::new(),
            own_ports: &own,
        },
    )
    .unwrap()
    .built;
    let task = resolved(&built, TASK_APP);
    assert_eq!(task["MXV_DOMAIN"], "triage.mxv.localhost");
    assert_eq!(
        task["MXV_URL_WEB_UI"],
        "https://web-ui.triage.mxv.localhost"
    );
    assert_eq!(task["MXV_HOST_API"], "api.triage.mxv.localhost");
    assert!(!task.contains_key("MXV_HOST") && !task.contains_key("MXV_URL"));
}

/// A merged manifest of the checkouts `catalogue` and `shelf`. The
/// catalogue's api listens on `catalogue-http` and starts after its
/// store, and the shelf's web process reads `shelf/starmap.toml`.
const NEEDS_MERGED: &str = r#"
ports = ["catalogue-http", "catalogue-db", "shelf-http"]

[containers.catalogue-store]
image = "postgres:17.2"
ports = { 5432 = "catalogue-db" }

[processes.catalogue-api]
argv = ["catalogue"]
cwd = "catalogue"
ports = { PORT = "catalogue-http" }
listen = "catalogue-http"
depends_on = ["catalogue-store"]

[processes.shelf-web]
argv = ["shelf"]
ports = { PORT = "shelf-http" }
listen = "shelf-http"
env = { RUST_LOG = "info" }
"#;

/// A value of the manifest that a check's failure must never quote.
const LITERAL: &str = "hunter2-literal";

/// The shelf's starmap file: the catalogue's URL, which it requires, a
/// ledger's URL, which may stay unset, a token from 1Password and a
/// generated session key.
const SHELF_SPEC: &str = r#"
[variables.CATALOGUE_URL]
purpose = "The catalogue's URL."
type = "string"
unset = "required"
show = "shown"
source = "provided"
bind = { provided = "multiverse/url", app = "catalogue/api" }

[variables.LEDGER_URL]
purpose = "The ledger's URL."
type = "string"
unset = "optional"
show = "shown"
source = "provided"
bind = { provided = "multiverse/url", app = "ledger/api" }

[variables.API_TOKEN]
purpose = "The token the shelf sends."
type = "string"
unset = "required"
show = "withheld"
source = "secret"
bind = { one-password = "op://acme-staging/shelf/token" }

[variables.SESSION_KEY]
purpose = "The key the shelf signs sessions with."
type = "string"
unset = "required"
show = "withheld"
source = "generated"
"#;

/// Builds the plan of `text`, a merged manifest of the checkouts
/// `catalogue` and `shelf` in a workspace directory whose
/// `shelf/starmap.toml` holds [`SHELF_SPEC`].
fn needs_plan_of(text: &str) -> Result<Built, crate::phase_plan::Unrunnable> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("catalogue")).unwrap();
    std::fs::create_dir_all(dir.path().join("shelf")).unwrap();
    write(dir.path().join("shelf/starmap.toml"), SHELF_SPEC);
    let manifest = manifest(text);
    let ports = BTreeMap::from([
        ("catalogue-http".to_owned(), 41_000),
        ("catalogue-db".to_owned(), 41_001),
        ("shelf-http".to_owned(), 41_002),
    ]);
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    build(&Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &dir.path().join("shelf"),
        roots: vec![
            dir.path().join("catalogue"),
            dir.path().join("shelf"),
            dir.path().to_path_buf(),
        ],
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    })
}

#[test]
fn the_needs_plan_lists_each_provided_variable_and_how_it_was_met() {
    let built = needs_plan_of(NEEDS_MERGED).unwrap();
    let web = &built.needs.units["shelf-web"];
    assert_eq!(
        web.provided,
        [
            ProvidedVariable {
                variable: "CATALOGUE_URL".to_owned(),
                required: true,
                met: Met::Port {
                    checkout: Some("catalogue".to_owned()),
                    unit: "catalogue-api".to_owned(),
                    port: "catalogue-http".to_owned(),
                },
            },
            ProvidedVariable {
                variable: "LEDGER_URL".to_owned(),
                required: false,
                met: Met::Unset,
            },
        ]
    );
}

#[test]
fn the_needs_plan_lists_each_secret_by_name_and_reference_and_holds_no_value() {
    let built = needs_plan_of(NEEDS_MERGED).unwrap();
    let web = &built.needs.units["shelf-web"];
    assert_eq!(
        web.secrets,
        [
            SecretVariable {
                variable: "API_TOKEN".to_owned(),
                store: "1password".to_owned(),
                reference: Some("op://acme-staging/shelf/token".to_owned()),
            },
            SecretVariable {
                variable: "SESSION_KEY".to_owned(),
                store: "keychain".to_owned(),
                reference: None,
            },
        ]
    );
    assert_eq!(
        web.declared_secrets,
        BTreeMap::from([
            ("API_TOKEN".to_owned(), SecretKind::Secret),
            ("SESSION_KEY".to_owned(), SecretKind::Generated),
        ])
    );
    assert_eq!(web.literals, BTreeSet::from(["RUST_LOG".to_owned()]));
}

#[test]
fn the_needs_plan_lists_the_units_each_unit_starts_after_and_what_the_workspace_has() {
    let built = needs_plan_of(NEEDS_MERGED).unwrap();
    assert_eq!(
        built.needs.units["catalogue-api"].after,
        ["catalogue-store"]
    );
    assert!(built.needs.units.contains_key("catalogue-store"));
    assert_eq!(
        built.needs.known.checkouts,
        BTreeSet::from(["catalogue".to_owned(), "shelf".to_owned()])
    );
    assert!(built.needs.known.ports["catalogue-api"].contains("catalogue-http"));
}

#[test]
fn a_manifest_value_for_a_secret_of_the_starmap_file_fails_the_plan_check_without_the_value() {
    let text = NEEDS_MERGED.replace(
        "env = { RUST_LOG = \"info\" }",
        &format!("env = {{ API_TOKEN = \"{LITERAL}\" }}"),
    );
    let Err(failure) = needs_plan_of(&text) else {
        panic!("the plan passed its checks");
    };
    let UnrunnableKind::Check(failures) = &failure.kind else {
        panic!("not a check failure: {failure:?}");
    };
    let listed: Vec<(Check, &str, Option<&str>)> = failures
        .iter()
        .map(|failure| {
            (
                failure.check,
                failure.unit.as_str(),
                failure.variable.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [(Check::SecretValue, "shelf-web", Some("API_TOKEN"))]
    );
    assert_eq!(failure.unit.as_deref(), Some("shelf-web"));
    assert!(failure.message.contains("API_TOKEN"), "{}", failure.message);
    assert!(!failure.message.contains(LITERAL), "{}", failure.message);
    assert!(!format!("{failure:?}").contains(LITERAL), "{failure:?}");
}
