#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use phase::{AppName, EnvironmentSource as _, ProcessName, WorkspaceId};
use tempfile::TempDir;

use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::phase_plan::{
    Built, CallerEnvironment, Inputs, MAIN, ProxyUrls, Unrunnable, UnrunnableKind, build,
    permissions,
};
use crate::testing::write;

/// A merged manifest of two checkouts, `catalogue` and `shelf`. The
/// catalogue's api listens on `catalogue-http` and also uses
/// `catalogue-admin`, its worker listens on no port, and the shelf's web
/// process reads `shelf/starmap.toml`.
const MERGED: &str = r#"
ports = ["catalogue-http", "catalogue-admin", "shelf-http"]

[processes.catalogue-api]
argv = ["catalogue"]
ports = { PORT = "catalogue-http", ADMIN_PORT = "catalogue-admin" }
listen = "catalogue-http"

[processes.catalogue-worker]
argv = ["worker"]

[processes.shelf-web]
argv = ["shelf"]
cwd = "shelf"
ports = { PORT = "shelf-http" }
listen = "shelf-http"
env = { CATALOGUE_NOTE = "set by the manifest" }
"#;

/// The host ports of [`MERGED`].
fn ports() -> BTreeMap<String, u16> {
    BTreeMap::from([
        ("catalogue-http".to_owned(), 41_000),
        ("catalogue-admin".to_owned(), 41_001),
        ("shelf-http".to_owned(), 41_002),
    ])
}

/// Returns a starmap file that declares the variable `name` as provided
/// with `bind`, required unless `optional`.
fn variable(name: &str, bind: &str, optional: bool) -> String {
    let unset = if optional { "optional" } else { "required" };
    format!(
        "[variables.{name}]\npurpose = \"A value of another unit.\"\ntype = \"string\"\nunset = \"{unset}\"\nshow = \"shown\"\nsource = \"provided\"\nbind = {bind}\n\n"
    )
}

/// A workspace directory whose `shelf/starmap.toml` holds `spec`.
fn workspace_with(spec: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("shelf")).unwrap();
    write(dir.path().join("shelf/starmap.toml"), spec);
    dir
}

/// Builds the plan of [`MERGED`] in `dir` with `ports` and `urls`.
fn plan_in(
    dir: &Path,
    ports: &BTreeMap<String, u16>,
    urls: ProxyUrls,
) -> Result<Built, Unrunnable> {
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    build(&Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: dir,
        roots: Vec::new(),
        ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls,
        read_starmaps: true,
    })
}

/// Returns the variables that `built` gives the app `name`.
fn resolved(built: &Built, name: &str) -> BTreeMap<String, String> {
    let applied = built
        .environment
        .resolve(
            &AppName::try_from(name.to_owned()).unwrap(),
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

/// Returns the shelf's variables when its starmap file holds `spec`.
fn shelf_variables(spec: &str) -> BTreeMap<String, String> {
    let dir = workspace_with(spec);
    resolved(
        &plan_in(dir.path(), &ports(), ProxyUrls::default()).unwrap(),
        "shelf-web",
    )
}

/// Returns the failure of the plan when the shelf's starmap file holds
/// `spec`.
fn shelf_failure(spec: &str) -> Unrunnable {
    let dir = workspace_with(spec);
    match plan_in(dir.path(), &ports(), ProxyUrls::default()) {
        Ok(_) => panic!("the plan was built"),
        Err(unrunnable) => unrunnable,
    }
}

/// Returns the proxy's routes of the workspace `triage` when the proxy
/// serves `catalogue-http` alone.
fn routed() -> ProxyUrls {
    use crate::proxy::{Declared, derive};
    let declared = Declared::new(
        WorkspaceName::try_from("triage".to_owned()).unwrap(),
        vec!["catalogue-http".to_owned()],
        BTreeMap::from([("catalogue-http".to_owned(), 41_000)]),
        BTreeMap::new(),
        BTreeMap::new(),
    );
    ProxyUrls::of(
        &WorkspaceName::try_from("triage".to_owned()).unwrap(),
        &derive(vec![declared]).served,
        "127.0.0.1:443".parse().unwrap(),
        crate::proxy::DOMAIN,
    )
}

#[test]
fn the_port_provider_returns_the_host_port_of_a_units_listen_port() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue-api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn the_port_provider_returns_the_host_port_of_a_named_port_of_a_unit() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_ADMIN",
        r#"{ provided = "multiverse/port", process = "catalogue-api", port = "catalogue-admin" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_ADMIN"], "41001");
}

#[test]
fn the_url_provider_returns_a_loopback_url_for_a_port_that_the_proxy_does_not_serve() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_URL"], "http://127.0.0.1:41000");
}

#[test]
fn the_url_provider_returns_the_proxy_url_for_a_port_that_the_proxy_serves() {
    let dir = workspace_with(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
        false,
    ));
    let built = plan_in(dir.path(), &ports(), routed()).unwrap();
    assert_eq!(
        resolved(&built, "shelf-web")["CATALOGUE_URL"],
        "https://catalogue-http.triage.mxv.localhost"
    );
}

#[test]
fn a_unit_of_another_checkout_resolves_by_its_merged_name() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown { app, .. } if app == "api"),
        "{failure:?}"
    );
}

#[test]
fn an_unknown_unit_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-search" }"#,
        false,
    ));
    assert_eq!(
        failure.kind,
        UnrunnableKind::ProvidedUnknown {
            variable: "CATALOGUE_URL".to_owned(),
            app: "catalogue-search".to_owned(),
            port: "listen".to_owned(),
        }
    );
    assert_eq!(failure.unit.as_deref(), Some("shelf-web"));
    assert!(
        failure.message.contains("`catalogue-search`"),
        "{failure:?}"
    );
    assert!(failure.message.contains("`listen`"), "{failure:?}");
}

#[test]
fn a_known_unit_is_admitted() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
        false,
    ));
    assert!(vars.contains_key("CATALOGUE_URL"));
}

#[test]
fn an_unknown_port_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api", port = "catalogue-grpc" }"#,
        false,
    ));
    assert_eq!(
        failure.kind,
        UnrunnableKind::ProvidedUnknown {
            variable: "CATALOGUE_PORT".to_owned(),
            app: "catalogue-api".to_owned(),
            port: "catalogue-grpc".to_owned(),
        }
    );
    assert!(failure.message.contains("`catalogue-api`"), "{failure:?}");
    assert!(failure.message.contains("`catalogue-grpc`"), "{failure:?}");
}

#[test]
fn a_known_port_is_admitted() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api", port = "catalogue-http" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn a_unit_with_no_listen_port_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "WORKER_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue-worker" }"#,
        false,
    ));
    assert_eq!(
        failure.kind,
        UnrunnableKind::ProvidedUnknown {
            variable: "WORKER_PORT".to_owned(),
            app: "catalogue-worker".to_owned(),
            port: "listen".to_owned(),
        }
    );
    assert!(
        failure.message.contains("`catalogue-worker`"),
        "{failure:?}"
    );
    assert!(
        failure.message.contains("listens on no port"),
        "{failure:?}"
    );
}

#[test]
fn a_unit_with_a_listen_port_is_admitted_without_a_port() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue-api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn a_bind_with_both_process_and_app_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue-api", app = "catalogue-api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown { app, port, .. } if app == "catalogue-api" && port == "listen"),
        "{failure:?}"
    );
    assert!(failure.message.contains("both"), "{failure:?}");
    assert!(failure.message.contains("`catalogue-api`"), "{failure:?}");
}

#[test]
fn a_bind_with_only_app_is_admitted() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn a_bind_with_an_unknown_qualifier_fails_with_provided_unknown() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api", scheme = "https" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown { .. }),
        "{failure:?}"
    );
    assert!(failure.message.contains("`scheme`"), "{failure:?}");
}

#[test]
fn a_manifest_variable_beats_a_provided_one() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_NOTE",
        r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_NOTE"], "set by the manifest");
}

#[test]
fn a_provided_variable_beats_a_passed_through_one() {
    let dir = workspace_with(&variable(
        "TZ",
        r#"{ provided = "multiverse/port", app = "catalogue-api" }"#,
        false,
    ));
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::from_vars([("TZ".into(), "UTC".into())]);
    let ports = ports();
    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: dir.path(),
        roots: Vec::new(),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &caller,
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    })
    .unwrap();
    assert_eq!(resolved(&built, "shelf-web")["TZ"], "41000");
    let shelf = built
        .plan
        .apps()
        .iter()
        .find(|app| app.name.as_str() == "shelf-web")
        .unwrap();
    assert!(
        !shelf.processes[0]
            .optional_env
            .iter()
            .any(|name| name == "TZ"),
        "{shelf:?}"
    );
}

#[test]
fn an_optional_variable_whose_port_has_no_host_port_yet_is_left_out() {
    let dir = workspace_with(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
        true,
    ));
    let mut ports = ports();
    ports.remove("catalogue-http");
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: dir.path(),
        roots: Vec::new(),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    let process = &manifest.processes["shelf-web"];
    let values = super::values(&inputs, "shelf-web", process).unwrap();
    assert!(values.is_empty(), "{values:?}");
    write(
        dir.path().join("shelf/starmap.toml"),
        variable(
            "CATALOGUE_URL",
            r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
            false,
        ),
    );
    let failure = super::values(&inputs, "shelf-web", process).unwrap_err();
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown { app, .. } if app == "catalogue-api"),
        "{failure:?}"
    );
}

#[test]
fn a_starmap_file_that_does_not_parse_fails_with_starmap_invalid() {
    let failure = shelf_failure("[variables.BROKEN]\npurpose = 3\n");
    assert!(
        matches!(&failure.kind, UnrunnableKind::StarmapInvalid { file } if file.ends_with("shelf/starmap.toml")),
        "{failure:?}"
    );
    assert!(failure.message.contains("does not parse"), "{failure:?}");
}

#[test]
fn a_starmap_file_that_parses_is_admitted() {
    let vars = shelf_variables("");
    assert_eq!(vars["CATALOGUE_NOTE"], "set by the manifest");
}

#[test]
fn a_process_without_a_starmap_file_gets_no_provided_variable() {
    let dir = tempfile::tempdir().unwrap();
    let built = plan_in(dir.path(), &ports(), ProxyUrls::default()).unwrap();
    let vars = resolved(&built, "shelf-web");
    assert!(!vars.contains_key("CATALOGUE_URL"), "{vars:?}");
}

#[test]
fn the_permissions_check_refuses_a_wrong_bind_with_no_ports_given() {
    let dir = workspace_with(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-search" }"#,
        false,
    ));
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let none = BTreeMap::new();
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: dir.path(),
        roots: Vec::new(),
        ports: &none,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    let failure = permissions(&inputs, &["shelf-web".to_owned()]).unwrap_err();
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown { app, .. } if app == "catalogue-search"),
        "{failure:?}"
    );
}
