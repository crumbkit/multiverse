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
ports = ["catalogue-http", "catalogue-admin", "catalogue-db", "shelf-http"]

[containers.catalogue-store]
image = "postgres:17.2"
ports = { 5432 = "catalogue-db" }

[processes.catalogue-api]
argv = ["catalogue"]
cwd = "catalogue"
ports = { PORT = "catalogue-http", ADMIN_PORT = "catalogue-admin" }
listen = "catalogue-http"

[processes.catalogue-worker]
argv = ["worker"]
cwd = "catalogue"

[processes.shelf-web]
argv = ["shelf"]
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
        ("catalogue-db".to_owned(), 41_003),
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

/// Returns the roots of a workspace in `dir` with the checkouts
/// `catalogue` and `shelf`: each checkout, then the workspace's directory.
fn roots_of(dir: &Path) -> Vec<std::path::PathBuf> {
    vec![dir.join("catalogue"), dir.join("shelf"), dir.to_path_buf()]
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
        checkout: &dir.join("shelf"),
        roots: roots_of(dir),
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
        r#"{ provided = "multiverse/port", process = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn the_port_provider_returns_the_host_port_of_a_named_port_of_a_unit() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_ADMIN",
        r#"{ provided = "multiverse/port", process = "catalogue/api", port = "admin" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_ADMIN"], "41001");
}

#[test]
fn the_url_provider_returns_a_loopback_url_for_a_port_that_the_proxy_does_not_serve() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_URL"], "http://127.0.0.1:41000");
}

#[test]
fn the_url_provider_returns_the_proxy_url_for_a_port_that_the_proxy_serves() {
    let dir = workspace_with(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
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
        r#"{ provided = "multiverse/port", app = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "api"),
        "{failure:?}"
    );
}

#[test]
fn an_unknown_unit_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/search" }"#,
        false,
    ));
    assert_eq!(
        bind_names(&failure),
        [("CATALOGUE_URL", "catalogue/search", "listen")],
        "{failure:?}"
    );
    assert_eq!(failure.unit.as_deref(), Some("shelf-web"));
    assert!(
        failure.message.contains("`catalogue/search`"),
        "{failure:?}"
    );
    assert!(
        failure
            .message
            .contains("the listen port of `catalogue/search`"),
        "{failure:?}"
    );
}

#[test]
fn a_known_unit_is_admitted() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
        false,
    ));
    assert!(vars.contains_key("CATALOGUE_URL"));
}

#[test]
fn an_unknown_port_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api", port = "grpc" }"#,
        false,
    ));
    assert_eq!(
        bind_names(&failure),
        [("CATALOGUE_PORT", "catalogue/api", "grpc")],
        "{failure:?}"
    );
    assert!(failure.message.contains("`catalogue/api`"), "{failure:?}");
    assert!(failure.message.contains("`grpc`"), "{failure:?}");
}

#[test]
fn a_known_port_is_admitted() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api", port = "http" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn a_unit_with_no_listen_port_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "WORKER_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue/worker" }"#,
        false,
    ));
    assert_eq!(
        bind_names(&failure),
        [("WORKER_PORT", "catalogue/worker", "listen")],
        "{failure:?}"
    );
    assert!(
        failure.message.contains("`catalogue/worker`"),
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
        r#"{ provided = "multiverse/port", process = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn a_bind_with_both_process_and_app_fails_with_a_message_that_names_the_unit_and_the_port() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue/api", app = "catalogue/api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "catalogue/api" && binds.first().port == "listen"),
        "{failure:?}"
    );
    assert!(failure.message.contains("both"), "{failure:?}");
    assert!(failure.message.contains("`catalogue/api`"), "{failure:?}");
}

#[test]
fn a_bind_with_only_app_is_admitted() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_PORT"], "41000");
}

#[test]
fn a_bind_with_an_unknown_qualifier_fails_with_provided_unknown() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api", scheme = "https" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(_)),
        "{failure:?}"
    );
    assert!(failure.message.contains("`scheme`"), "{failure:?}");
}

#[test]
fn a_manifest_variable_beats_a_provided_one() {
    let vars = shelf_variables(&variable(
        "CATALOGUE_NOTE",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(vars["CATALOGUE_NOTE"], "set by the manifest");
}

#[test]
fn a_provided_variable_beats_a_passed_through_one() {
    let dir = workspace_with(&variable(
        "TZ",
        r#"{ provided = "multiverse/port", app = "catalogue/api" }"#,
        false,
    ));
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let caller = CallerEnvironment::from_vars([("TZ".into(), "UTC".into())]);
    let ports = ports();
    let shelf = dir.path().join("shelf");
    let built = build(&Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
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
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
        true,
    ));
    let mut ports = ports();
    ports.remove("catalogue-http");
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let shelf = dir.path().join("shelf");
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    let process = &manifest.processes["shelf-web"];
    let values = super::values(&inputs, "shelf-web", process).unwrap().values;
    assert!(values.is_empty(), "{values:?}");
    write(
        dir.path().join("shelf/starmap.toml"),
        variable(
            "CATALOGUE_URL",
            r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
            false,
        ),
    );
    let failure = super::values(&inputs, "shelf-web", process).unwrap_err();
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnassigned(bind) if bind.app == "catalogue/api" && bind.port == "http"),
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
        r#"{ provided = "multiverse/url", app = "catalogue/search" }"#,
        false,
    ));
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let none = BTreeMap::new();
    let shelf = dir.path().join("shelf");
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
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
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "catalogue/search"),
        "{failure:?}"
    );
}

#[test]
fn a_container_named_with_one_of_its_published_ports_gives_that_host_port() {
    let vars = shelf_variables(&variable(
        "STORE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/store", port = "db" }"#,
        false,
    ));
    assert_eq!(vars["STORE_PORT"], "41003");
}

#[test]
fn a_container_named_without_a_port_fails_because_it_listens_on_no_port() {
    let failure = shelf_failure(&variable(
        "STORE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/store" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "catalogue/store" && binds.first().port == "listen"),
        "{failure:?}"
    );
    assert!(failure.message.contains("`db`"), "{failure:?}");
}

#[test]
fn a_starmap_key_names_the_file_that_the_process_reads_in_place_of_the_default() {
    let dir = workspace_with(&variable(
        "DEFAULT_ONLY",
        r#"{ provided = "multiverse/port", app = "catalogue/api" }"#,
        false,
    ));
    std::fs::create_dir_all(dir.path().join("shelf/config")).unwrap();
    write(
        dir.path().join("shelf/config/shelf.toml"),
        variable(
            "NAMED_ONLY",
            r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
            false,
        ),
    );
    let text = MERGED.replace(
        "[processes.shelf-web]\n",
        "[processes.shelf-web]\nstarmap = \"config/shelf.toml\"\n",
    );
    let manifest = Manifest::parse(&text, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let ports = ports();
    let shelf = dir.path().join("shelf");
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
        ports: &ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    let found = super::values(&inputs, "shelf-web", &manifest.processes["shelf-web"])
        .unwrap()
        .values;
    assert_eq!(
        found,
        BTreeMap::from([("NAMED_ONLY".to_owned(), "http://127.0.0.1:41000".to_owned())])
    );
}

#[test]
fn a_provided_variable_named_path_is_unrunnable() {
    let failure = shelf_failure(&variable(
        "PATH",
        r#"{ provided = "multiverse/port", app = "catalogue/api" }"#,
        false,
    ));
    assert_eq!(failure.kind, UnrunnableKind::Plan);
    assert!(failure.message.contains("PATH"), "{failure:?}");
}

#[test]
fn a_default_starmap_file_that_is_a_symbolic_link_fails_with_starmap_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(
        outside.path().join("starmap.toml"),
        variable(
            "CATALOGUE_PORT",
            r#"{ provided = "multiverse/port", app = "catalogue/api" }"#,
            false,
        ),
    );
    std::fs::create_dir_all(dir.path().join("shelf")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("starmap.toml"),
        dir.path().join("shelf/starmap.toml"),
    )
    .unwrap();
    let Err(failure) = plan_in(dir.path(), &ports(), ProxyUrls::default()) else {
        panic!("the plan was built");
    };
    assert!(
        matches!(&failure.kind, UnrunnableKind::StarmapInvalid { .. }),
        "{failure:?}"
    );
    assert!(failure.message.contains("symbolic link"), "{failure:?}");
}

#[test]
fn a_bare_unit_name_names_a_unit_of_the_processs_own_checkout() {
    let vars = shelf_variables(&variable(
        "OWN_PORT",
        r#"{ provided = "multiverse/port", process = "web" }"#,
        false,
    ));
    assert_eq!(vars["OWN_PORT"], "41002");
}

#[test]
fn a_checkout_that_the_workspace_does_not_have_fails_with_provided_unknown() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "library/api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "library/api"),
        "{failure:?}"
    );
    assert!(
        failure.message.contains("no checkout `library`"),
        "{failure:?}"
    );
}

#[test]
fn a_merged_name_does_not_name_a_unit_of_another_checkout() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "catalogue-api"),
        "{failure:?}"
    );
}

/// Returns the plan of [`MERGED`] when the shelf's starmap file holds
/// `spec`, with every host port.
fn shelf_plan(spec: &str) -> Built {
    let dir = workspace_with(spec);
    plan_in(dir.path(), &ports(), ProxyUrls::default()).unwrap()
}

#[test]
fn an_optional_variable_bound_to_a_checkout_that_the_workspace_lacks_is_left_unset_with_one_note() {
    let built = shelf_plan(&variable(
        "LEDGER_URL",
        r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
        true,
    ));
    let vars = resolved(&built, "shelf-web");
    assert!(!vars.contains_key("LEDGER_URL"), "{vars:?}");
    let notes = built.notes.get("shelf-web").cloned().unwrap_or_default();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let crate::runs::ProcessNote::ProvidedSkipped {
        variable,
        checkout,
        message,
    } = &notes[0]
    else {
        panic!("{notes:?}");
    };
    assert_eq!(variable, "LEDGER_URL");
    assert_eq!(checkout, "ledger");
    assert_eq!(
        message,
        "left LEDGER_URL unset, because the workspace has no checkout `ledger`"
    );
    assert!(!notes[0].is_warning());
}

#[test]
fn an_optional_variable_bound_to_a_checkout_that_the_workspace_has_resolves_to_its_host_port() {
    let built = shelf_plan(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
        true,
    ));
    assert_eq!(
        resolved(&built, "shelf-web")["CATALOGUE_URL"],
        "http://127.0.0.1:41000"
    );
    assert!(built.notes.get("shelf-web").is_none_or(Vec::is_empty));
}

#[test]
fn a_required_variable_bound_to_a_checkout_that_the_workspace_lacks_still_fails_with_provided_unknown()
 {
    let failure = shelf_failure(&variable(
        "LEDGER_URL",
        r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
        false,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "ledger/api"),
        "{failure:?}"
    );
}

#[test]
fn an_optional_bind_to_a_unit_that_a_present_checkout_lacks_still_fails_with_provided_unknown() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/apii" }"#,
        true,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().app == "catalogue/apii"),
        "{failure:?}"
    );
    let built = shelf_plan(&variable(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue/api" }"#,
        true,
    ));
    assert!(resolved(&built, "shelf-web").contains_key("CATALOGUE_URL"));
}

#[test]
fn an_optional_bind_to_a_port_that_a_present_unit_lacks_still_fails_with_provided_unknown() {
    let failure = shelf_failure(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api", port = "grpc" }"#,
        true,
    ));
    assert!(
        matches!(&failure.kind, UnrunnableKind::ProvidedUnknown(binds) if binds.first().port == "grpc"),
        "{failure:?}"
    );
    let built = shelf_plan(&variable(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue/api", port = "admin" }"#,
        true,
    ));
    assert_eq!(resolved(&built, "shelf-web")["CATALOGUE_PORT"], "41001");
}

#[test]
fn the_permissions_check_admits_an_optional_bind_to_a_missing_checkout_with_no_ports_given() {
    let dir = workspace_with(&variable(
        "LEDGER_URL",
        r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
        true,
    ));
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let none = BTreeMap::new();
    let shelf = dir.path().join("shelf");
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
        ports: &none,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    let granted = permissions(&inputs, &["shelf-web".to_owned()]);
    assert!(granted.is_ok(), "{:?}", granted.err());
}

/// Returns the variable, unit and port of each bind that `failure` lists,
/// or panics when it is not [`UnrunnableKind::ProvidedUnknown`].
fn bind_names(failure: &Unrunnable) -> Vec<(&str, &str, &str)> {
    let UnrunnableKind::ProvidedUnknown(binds) = &failure.kind else {
        panic!("not provided_unknown: {failure:?}");
    };
    binds
        .iter()
        .map(|bind| {
            (
                bind.variable.as_str(),
                bind.app.as_str(),
                bind.port.as_str(),
            )
        })
        .collect()
}

/// Returns the failure of the permissions check of `shelf-web`, with no
/// host ports given, when the shelf's starmap file holds `spec`.
fn permissions_failure(spec: &str) -> Unrunnable {
    let dir = workspace_with(spec);
    let manifest = Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let none = BTreeMap::new();
    let shelf = dir.path().join("shelf");
    let inputs = Inputs {
        manifest: &manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
        ports: &none,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    match permissions(&inputs, &["shelf-web".to_owned()]) {
        Ok(_) => panic!("the permissions check passed"),
        Err(unrunnable) => unrunnable,
    }
}

#[test]
fn two_required_binds_to_two_missing_checkouts_fail_once_and_the_failure_lists_both() {
    let spec = variable(
        "LEDGER_URL",
        r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
        false,
    ) + &variable(
        "LIBRARY_PORT",
        r#"{ provided = "multiverse/port", app = "library/api", port = "http" }"#,
        false,
    );
    for failure in [permissions_failure(&spec), shelf_failure(&spec)] {
        assert_eq!(
            bind_names(&failure),
            [
                ("LEDGER_URL", "ledger/api", "listen"),
                ("LIBRARY_PORT", "library/api", "http"),
            ],
            "{failure:?}"
        );
        let UnrunnableKind::ProvidedUnknown(binds) = &failure.kind else {
            panic!("{failure:?}");
        };
        assert_eq!(binds.len(), 2);
        for (bind, checkout) in binds.iter().zip(["ledger", "library"]) {
            assert_eq!(bind.unit, "shelf-web", "{bind:?}");
            assert_eq!(bind.checkout.as_deref(), Some(checkout), "{bind:?}");
            assert!(
                bind.reason
                    .contains(&format!("the workspace has no checkout `{checkout}`")),
                "{bind:?}"
            );
            assert!(bind.message.contains(&bind.variable), "{bind:?}");
            assert!(
                bind.message.contains("starmap.toml of `shelf-web`"),
                "{bind:?}"
            );
        }
        assert_eq!(failure.unit.as_deref(), Some("shelf-web"));
        assert!(
            failure.message.contains("2 provided variables"),
            "{failure:?}"
        );
    }
}

#[test]
fn a_wrong_qualifier_and_a_missing_checkout_in_one_file_are_both_listed() {
    let failure = permissions_failure(
        &(variable(
            "CATALOGUE_PORT",
            r#"{ provided = "multiverse/port", app = "catalogue/api", scheme = "https" }"#,
            false,
        ) + &variable(
            "LEDGER_URL",
            r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
            false,
        )),
    );
    assert_eq!(
        bind_names(&failure),
        [
            ("CATALOGUE_PORT", "catalogue/api", "listen"),
            ("LEDGER_URL", "ledger/api", "listen"),
        ],
        "{failure:?}"
    );
    let UnrunnableKind::ProvidedUnknown(binds) = &failure.kind else {
        panic!("{failure:?}");
    };
    assert!(binds.first().reason.contains("`scheme`"), "{failure:?}");
}

#[test]
fn an_optional_bind_to_a_missing_checkout_is_not_listed_beside_a_refused_bind() {
    let spec = variable(
        "LEDGER_URL",
        r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
        true,
    ) + &variable(
        "LIBRARY_URL",
        r#"{ provided = "multiverse/url", app = "library/api" }"#,
        false,
    );
    for failure in [permissions_failure(&spec), shelf_failure(&spec)] {
        assert_eq!(
            bind_names(&failure),
            [("LIBRARY_URL", "library/api", "listen")],
            "{failure:?}"
        );
    }
}

#[test]
fn an_optional_bind_to_a_unit_that_a_present_checkout_lacks_is_listed_beside_a_required_one() {
    let failure = permissions_failure(
        &(variable(
            "CATALOGUE_URL",
            r#"{ provided = "multiverse/url", app = "catalogue/apii" }"#,
            true,
        ) + &variable(
            "LIBRARY_URL",
            r#"{ provided = "multiverse/url", app = "library/api" }"#,
            false,
        )),
    );
    assert_eq!(
        bind_names(&failure),
        [
            ("CATALOGUE_URL", "catalogue/apii", "listen"),
            ("LIBRARY_URL", "library/api", "listen"),
        ],
        "{failure:?}"
    );
}

/// Returns the result of the permissions check of `units`, with no host
/// ports given, in a workspace whose merged manifest is `manifest` and whose files
/// below the shelf's checkout are `files`, each a relative path and its
/// text.
fn permissions_of(
    manifest: &Manifest,
    files: &[(&str, &str)],
    units: &[&str],
) -> Result<BTreeMap<AppName, phase::Grant>, Unrunnable> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("shelf/catalogue")).unwrap();
    for (path, body) in files {
        write(dir.path().join("shelf").join(path), body);
    }
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let none = BTreeMap::new();
    let shelf = dir.path().join("shelf");
    let inputs = Inputs {
        manifest,
        workspace: &workspace,
        id: &id,
        checkout: &shelf,
        roots: roots_of(dir.path()),
        ports: &none,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller: &CallerEnvironment::default(),
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    };
    let units: Vec<String> = units.iter().map(|unit| (*unit).to_owned()).collect();
    permissions(&inputs, &units)
}

/// A starmap file that binds the required `LEDGER_URL` to the checkout
/// `ledger`, which the workspace does not have.
fn ledger_bind() -> String {
    variable(
        "LEDGER_URL",
        r#"{ provided = "multiverse/url", app = "ledger/api" }"#,
        false,
    )
}

#[test]
fn the_permissions_check_reports_an_invalid_starmap_file_alone_beside_another_units_wrong_bind() {
    let manifest = merged();
    let bind = ledger_bind();
    let files = [
        ("catalogue/starmap.toml", bind.as_str()),
        ("starmap.toml", "[variables.BROKEN]\npurpose = 3\n"),
    ];
    for units in [
        ["catalogue-api", "shelf-web"],
        ["shelf-web", "catalogue-api"],
    ] {
        let failure = permissions_of(&manifest, &files, &units).unwrap_err();
        assert!(
            matches!(&failure.kind, UnrunnableKind::StarmapInvalid { file } if file.ends_with("starmap.toml") && !file.to_string_lossy().contains("catalogue")),
            "{units:?}: {failure:?}"
        );
        assert_eq!(failure.unit.as_deref(), Some("shelf-web"), "{failure:?}");
    }
    let failure =
        permissions_of(&manifest, &files[..1], &["catalogue-api", "shelf-web"]).unwrap_err();
    assert_eq!(
        bind_names(&failure),
        [("LEDGER_URL", "ledger/api", "listen")],
        "{failure:?}"
    );
}

/// Returns [`MERGED`], parsed.
fn merged() -> Manifest {
    Manifest::parse(MERGED, Path::new("/repo/mxv.toml")).unwrap()
}

/// Returns [`MERGED`] with a process `stray` that runs in a directory
/// outside every checkout of the workspace, which the plan cannot place.
/// A workspace manifest gives each process an absolute `cwd`, which a
/// checkout's own manifest cannot, so the test sets it after parsing.
fn with_stray() -> Manifest {
    let text = format!("{MERGED}\n[processes.stray]\nargv = [\"stray\"]\ncwd = \"stray\"\n");
    let mut manifest = Manifest::parse(&text, Path::new("/repo/mxv.toml")).unwrap();
    manifest.processes.get_mut("stray").unwrap().cwd = std::path::PathBuf::from("/nowhere/stray");
    manifest
}

#[test]
fn a_wrong_bind_is_reported_ahead_of_another_units_plan_failure_in_either_order() {
    let manifest = with_stray();
    let bind = ledger_bind();
    let files = [("starmap.toml", bind.as_str())];
    for units in [["shelf-web", "stray"], ["stray", "shelf-web"]] {
        let failure = permissions_of(&manifest, &files, &units).unwrap_err();
        assert_eq!(
            bind_names(&failure),
            [("LEDGER_URL", "ledger/api", "listen")],
            "{units:?}: {failure:?}"
        );
    }
    let failure = permissions_of(&manifest, &[], &["shelf-web", "stray"]).unwrap_err();
    assert_eq!(failure.kind, UnrunnableKind::Plan, "{failure:?}");
    assert_eq!(failure.unit.as_deref(), Some("stray"), "{failure:?}");
}
