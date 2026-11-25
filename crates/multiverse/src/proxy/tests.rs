#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread;

use super::{
    Declared, Derived, Proxy, ProxyError, ProxyProblem, Route, Settings, SyncMode, admin,
    caddy_config, derive, process,
};
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace};
use crate::store::{Created, Store};
use crate::testing::{unmade_clone, write};

fn name(text: &str) -> WorkspaceName {
    text.parse().unwrap()
}

fn declared(workspace: &str, routes: &[&str], ports: &[(&str, u16)]) -> Declared {
    Declared::new(
        name(workspace),
        routes.iter().map(|route| (*route).to_owned()).collect(),
        ports
            .iter()
            .map(|(port, number)| ((*port).to_owned(), *number))
            .collect(),
        BTreeMap::new(),
        BTreeMap::new(),
    )
}

/// Returns `declared` with the extra host names `names` on its route
/// `route`.
fn hosted(mut declared: Declared, route: &str, names: &[&str]) -> Declared {
    declared.hosts.insert(
        route.to_owned(),
        names.iter().map(|name| (*name).to_owned()).collect(),
    );
    declared
}

fn hosts(derived: &Derived) -> Vec<&str> {
    derived
        .served
        .iter()
        .map(|route| route.host.as_str())
        .collect()
}

fn settings(dir: &Path) -> Settings {
    Settings::new(dir.join("state"), dir.join("data"))
}

/// Creates the workspace `workspace` in `store`, whose checkout holds the
/// manifest `manifest`, with the ports `ports` in its ledger.
pub(super) fn workspace(
    store: &Store,
    dir: &Path,
    workspace: &str,
    manifest: &str,
    ports: &[(&str, u16)],
) {
    let name = name(workspace);
    let path = dir.join("workspaces").join(name.as_str());
    let checkout_dir = path.join("app");
    std::fs::create_dir_all(&checkout_dir).unwrap();
    write(checkout_dir.join("mxv.toml"), manifest);
    let checkout = Checkout::new(
        "app".to_owned(),
        &PathBuf::from("/nowhere/app"),
        checkout_dir,
        name.to_string(),
        "origin/main".to_owned(),
        unmade_clone(),
    );
    let mut record = Workspace::new(name, path, vec![checkout]);
    record.ports = ports
        .iter()
        .map(|(port, number)| ((*port).to_owned(), *number))
        .collect();
    assert_eq!(store.create(&mut record).unwrap(), Created::Written);
}

#[test]
fn underscores_and_dots_in_names_become_hyphens_in_the_host() {
    let derived = derive(vec![declared(
        "my_notes.v2",
        &["web_ui", "api.v1"],
        &[("web_ui", 21001), ("api.v1", 21002)],
    )]);
    assert_eq!(
        hosts(&derived),
        [
            "api-v1.my-notes-v2.mxv.localhost",
            "web-ui.my-notes-v2.mxv.localhost"
        ]
    );
}

#[test]
fn the_name_that_sorts_first_keeps_a_host_label_that_two_workspaces_make() {
    let derived = derive(vec![
        declared("my_notes", &["web"], &[("web", 21001)]),
        declared("my.notes", &["web"], &[("web", 21002)]),
    ]);
    assert_eq!(hosts(&derived), ["web.my-notes.mxv.localhost"]);
    assert_eq!(derived.served[0].port, 21002);
    assert_eq!(derived.shadowed.len(), 1);
    assert_eq!(derived.shadowed[0].route.workspace.as_str(), "my_notes");
    assert_eq!(derived.shadowed[0].by.as_str(), "my.notes");
}

#[test]
fn a_route_whose_port_the_ledger_lacks_is_left_out() {
    let derived = derive(vec![declared("triage", &["web", "api"], &[("web", 21001)])]);
    assert_eq!(hosts(&derived), ["web.triage.mxv.localhost"]);
}

#[test]
fn the_url_carries_the_https_port_unless_it_is_443() {
    let derived = derive(vec![declared("triage", &["web"], &[("web", 21001)])]);
    let route = &derived.served[0];
    assert_eq!(
        route.url(SocketAddr::from((Ipv4Addr::LOCALHOST, 443))),
        "https://web.triage.mxv.localhost"
    );
    assert_eq!(
        route.url(SocketAddr::from((Ipv4Addr::LOCALHOST, 8443))),
        "https://web.triage.mxv.localhost:8443"
    );
}

#[test]
fn the_config_renders_to_the_fixture_json_for_fixed_records() {
    let settings = Settings::new(PathBuf::from("/state/mxv"), PathBuf::from("/data/mxv"))
        .with_addresses(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 8443)),
        );
    let derived = derive(vec![
        declared(
            "reading-list",
            &["web", "api"],
            &[("web", 24310), ("api", 21877)],
        ),
        declared("triage", &["web"], &[("web", 24311)]),
    ]);
    let rendered = caddy_config::render(&settings, &derived.served);
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("caddy-config.json")).unwrap();
    assert_eq!(rendered, fixture);
}

#[test]
fn a_config_without_routes_serves_nothing() {
    let rendered = caddy_config::render(&settings(Path::new("/x")), &[]);
    assert!(rendered["apps"].get("http").is_none());
    assert!(
        rendered["admin"]["listen"]
            .as_str()
            .unwrap()
            .ends_with("admin.sock|0600")
    );
}

#[test]
fn a_sync_with_no_routes_and_no_socket_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let proxy = Proxy::new(settings(dir.path()));
    let store = Store::new(dir.path().join("records"));
    let synced = proxy.sync(&store, SyncMode::IfRunning, false).unwrap();
    assert!(synced.served.is_empty());
    assert!(!dir.path().join("state").exists());
}

#[test]
fn a_sync_with_routes_declared_but_no_ports_allocated_starts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n",
        &[],
    );
    let proxy = Proxy::new(settings(dir.path()));
    let synced = proxy.sync(&store, SyncMode::Start, true).unwrap();
    assert!(synced.served.is_empty());
}

#[test]
fn a_held_address_fails_with_port_held_naming_the_address_and_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n",
        &[("web", 21001)],
    );
    let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = held.local_addr().unwrap();
    let free = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let proxy =
        Proxy::new(settings(dir.path()).with_addresses(address, free.local_addr().unwrap()));
    drop(free);
    let error = proxy.sync(&store, SyncMode::Start, true).unwrap_err();
    assert!(
        matches!(error, ProxyError::PortHeld { address: found, key: "proxy_http", .. } if found == address)
    );
    let text = error.to_string();
    assert!(text.contains(&address.to_string()), "{text}");
    assert!(text.contains("proxy_http"), "{text}");
}

#[test]
fn a_missing_caddy_fails_naming_the_install_command_and_the_config_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n",
        &[("web", 21001)],
    );
    let free = |port| {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        listener.local_addr().unwrap()
    };
    let proxy = Proxy::new(
        settings(dir.path())
            .with_addresses(free(0), free(0))
            .with_search(vec![dir.path().join("empty")]),
    );
    let error = proxy.sync(&store, SyncMode::Start, true).unwrap_err();
    assert!(matches!(
        error,
        ProxyError::CaddyMissing { named: None, .. }
    ));
    let text = error.to_string();
    assert!(text.contains("brew install caddy"), "{text}");
    assert!(text.contains("`caddy`"), "{text}");
}

#[test]
fn a_caddy_older_than_2_8_fails_with_caddy_too_old() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n",
        &[("web", 21001)],
    );
    let caddy = dir.path().join("caddy");
    write(&caddy, "#!/bin/sh\necho 'v2.7.6 h1:abc'\n");
    std::fs::set_permissions(&caddy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let free = || {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    };
    let proxy = Proxy::new(
        settings(dir.path())
            .with_addresses(free(), free())
            .with_caddy(Some(caddy)),
    );
    let error = proxy.sync(&store, SyncMode::Start, true).unwrap_err();
    assert!(matches!(error, ProxyError::CaddyTooOld { ref found, .. } if found == "2.7.6"));
}

/// Writes an executable `caddy` in `dir` that prints `line` and returns it.
pub(super) fn fake_caddy(dir: &Path, line: &str) -> PathBuf {
    let caddy = dir.join("caddy");
    write(&caddy, format!("#!/bin/sh\necho '{line}'\n"));
    std::fs::set_permissions(&caddy, std::fs::Permissions::from_mode(0o755)).unwrap();
    caddy
}

#[test]
fn a_version_with_a_major_and_minor_is_parsed_once() {
    let dir = tempfile::tempdir().unwrap();
    let found = |line: &str| match process::version(&fake_caddy(dir.path(), line)) {
        process::Version::Found(version) => Some((version.major_minor(), version.to_string())),
        process::Version::Unusable(_) => None,
    };
    assert_eq!(found("v2.10.0 h1:x"), Some(((2, 10), "2.10.0".to_owned())));
    assert_eq!(
        found("2.8.0-beta.1"),
        Some(((2, 8), "2.8.0-beta.1".to_owned()))
    );
}

#[test]
fn a_version_without_a_major_and_minor_is_unusable_and_names_what_was_printed() {
    let dir = tempfile::tempdir().unwrap();
    let reason = |line: &str| match process::version(&fake_caddy(dir.path(), line)) {
        process::Version::Unusable(reason) => reason,
        process::Version::Found(version) => panic!("found {version}"),
    };
    let devel = reason("devel");
    assert!(devel.contains("`devel`"), "{devel}");
    assert!(devel.contains("no major and minor"), "{devel}");
    assert_eq!(reason(""), "it did not print a version");
}

#[test]
fn a_caddy_whose_background_process_holds_its_output_still_gives_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let caddy = dir.path().join("caddy");
    write(&caddy, "#!/bin/sh\nsleep 20 &\necho 'v2.10.0 h1:x'\n");
    std::fs::set_permissions(&caddy, std::fs::Permissions::from_mode(0o755)).unwrap();
    let begun = std::time::Instant::now();
    let version = process::version(&caddy);
    let took = begun.elapsed();
    assert!(
        matches!(&version, process::Version::Found(found) if found.major_minor() == (2, 10)),
        "{version:?}"
    );
    assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
}

#[test]
fn a_caddy_that_fails_is_unusable_and_names_the_first_line_it_wrote_on_standard_error() {
    let dir = tempfile::tempdir().unwrap();
    let caddy = dir.path().join("caddy");
    write(
        &caddy,
        "#!/bin/sh\necho 'broken install' >&2\necho 'second line' >&2\nexit 1\n",
    );
    std::fs::set_permissions(&caddy, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(process::version(&caddy), process::Version::Unusable(reason) if reason == "broken install")
    );
}

#[test]
fn a_caddy_with_an_unparseable_version_is_refused_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n",
        &[("web", 21001)],
    );
    let caddy = fake_caddy(dir.path(), "devel");
    let free = || {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    };
    let proxy = Proxy::new(
        settings(dir.path())
            .with_addresses(free(), free())
            .with_caddy(Some(caddy)),
    );
    let error = proxy.sync(&store, SyncMode::Start, true).unwrap_err();
    assert!(
        matches!(error, ProxyError::CaddyMissing { ref reason, .. } if reason.contains("`devel`")),
        "{error}"
    );
}

#[test]
fn a_socket_path_longer_than_a_socket_address_fails_before_anything_starts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n",
        &[("web", 21001)],
    );
    let long = dir.path().join("s".repeat(100));
    let proxy = Proxy::new(Settings::new(long, dir.path().join("data")));
    let error = proxy.sync(&store, SyncMode::Start, true).unwrap_err();
    assert!(
        matches!(error, ProxyError::SocketPathTooLong { .. }),
        "{error}"
    );
}

#[test]
fn the_admin_client_sends_the_expected_bytes_to_a_local_listener() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("a.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let mut seen = Vec::new();
        for answer in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            "HTTP/1.1 400 Bad Request\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nbad\r\n5\r\n json\r\n0\r\n\r\n",
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 1024];
            let read = stream.read(&mut buffer).unwrap();
            seen.push(String::from_utf8_lossy(&buffer[..read]).into_owned());
            stream.write_all(answer.as_bytes()).unwrap();
        }
        seen
    });
    let got = admin::get(&socket, "/config/").unwrap();
    assert_eq!((got.status, got.body.as_slice()), (200, &b"{}"[..]));
    let posted = admin::post(&socket, "/load", b"{\"a\":1}").unwrap();
    assert_eq!(posted.status, 400);
    assert_eq!(posted.body, b"bad json");
    let seen = server.join().unwrap();
    assert_eq!(
        seen[0],
        "GET /config/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(
        seen[1],
        "POST /load HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n{\"a\":1}"
    );
}

#[test]
fn route_urls_list_only_the_routes_of_the_asked_workspace() {
    let derived = derive(vec![
        declared("reading-list", &["web", "api"], &[("web", 1), ("api", 2)]),
        declared("triage", &["web"], &[("web", 3)]),
    ]);
    let synced = super::Synced {
        served: derived.served,
        https: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 443))),
        ..super::Synced::default()
    };
    assert_eq!(
        synced.urls_of(&name("reading-list")),
        [
            "api at https://api.reading-list.mxv.localhost",
            "web at https://web.reading-list.mxv.localhost"
        ]
    );
    let _: Option<Route> = None;
}

#[test]
fn a_route_carries_the_extra_hosts_of_its_unit() {
    let derived = derive(vec![hosted(
        declared("contacts", &["web"], &[("web", 21001)]),
        "web",
        &["local.app.example.com", "alt.app.example.com"],
    )]);
    assert_eq!(
        derived.served[0].extra,
        ["local.app.example.com", "alt.app.example.com"]
    );
    assert!(derived.shadowed_hosts.is_empty());
}

#[test]
fn the_workspace_that_sorts_first_serves_an_extra_host_that_two_list() {
    let derived = derive(vec![
        hosted(
            declared("triage", &["web"], &[("web", 21002)]),
            "web",
            &["local.app.example.com"],
        ),
        hosted(
            declared("contacts", &["web"], &[("web", 21001)]),
            "web",
            &["local.app.example.com"],
        ),
    ]);
    let contacts = derived
        .served
        .iter()
        .find(|route| route.workspace.as_str() == "contacts")
        .unwrap();
    let triage = derived
        .served
        .iter()
        .find(|route| route.workspace.as_str() == "triage")
        .unwrap();
    assert_eq!(contacts.extra, ["local.app.example.com"]);
    assert!(triage.extra.is_empty());
    assert_eq!(derived.shadowed_hosts.len(), 1);
    let unserved = &derived.shadowed_hosts[0];
    assert_eq!(unserved.host, "local.app.example.com");
    assert_eq!(unserved.workspace.as_str(), "triage");
    assert_eq!(unserved.route, "web");
    assert_eq!(unserved.by.as_str(), "contacts");
}

#[test]
fn the_extra_hosts_of_a_route_that_a_host_label_shadows_are_not_served() {
    let derived = derive(vec![
        hosted(
            declared("my_notes", &["web"], &[("web", 21001)]),
            "web",
            &["first.example.com"],
        ),
        hosted(
            declared("my-notes", &["web"], &[("web", 21003)]),
            "web",
            &["second.example.com"],
        ),
    ]);
    assert_eq!(derived.served.len(), 1);
    assert_eq!(derived.served[0].extra, ["second.example.com"]);
    assert!(derived.shadowed_hosts.is_empty());
    assert_eq!(derived.shadowed.len(), 1);
}

#[test]
fn the_url_of_an_extra_host_carries_the_https_port_unless_it_is_443() {
    let derived = derive(vec![hosted(
        declared("contacts", &["web"], &[("web", 21001)]),
        "web",
        &["local.app.example.com"],
    )]);
    let route = &derived.served[0];
    assert_eq!(
        route.extra_urls(SocketAddr::from((Ipv4Addr::LOCALHOST, 443))),
        ["https://local.app.example.com"]
    );
    assert_eq!(
        route.extra_urls(SocketAddr::from((Ipv4Addr::LOCALHOST, 8443))),
        ["https://local.app.example.com:8443"]
    );
}

#[test]
fn the_config_matches_extra_hosts_in_the_route_the_error_page_and_the_tls_subjects() {
    let derived = derive(vec![hosted(
        declared("contacts", &["web"], &[("web", 21001)]),
        "web",
        &["local.app.example.com"],
    )]);
    let rendered = caddy_config::render(&settings(Path::new("/x")), &derived.served);
    let hosts = serde_json::json!(["web.contacts.mxv.localhost", "local.app.example.com"]);
    let server = &rendered["apps"]["http"]["servers"]["mxv"];
    assert_eq!(server["routes"][0]["match"][0]["host"], hosts);
    assert_eq!(server["errors"]["routes"][0]["match"][0]["host"], hosts);
    assert_eq!(
        rendered["apps"]["tls"]["automation"]["policies"][0]["subjects"],
        hosts
    );
}

#[test]
fn the_extra_hosts_of_a_workspace_list_as_routes_with_their_urls() {
    let derived = derive(vec![
        hosted(
            declared("reading-list", &["web", "api"], &[("web", 1), ("api", 2)]),
            "web",
            &["local.app.example.com"],
        ),
        declared("triage", &["web"], &[("web", 3)]),
    ]);
    let synced = super::Synced {
        served: derived.served,
        https: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 8443))),
        ..super::Synced::default()
    };
    assert_eq!(
        synced.extra_urls_of(&name("reading-list")),
        ["web at https://local.app.example.com:8443"]
    );
    assert_eq!(
        synced.extra_hosts_of(&name("reading-list")),
        ["local.app.example.com"]
    );
    assert!(synced.extra_urls_of(&name("triage")).is_empty());
}

#[test]
fn a_workspace_that_takes_a_host_label_displaces_the_workspaces_it_shadows() {
    let derived = derive(vec![
        declared("my.notes", &["web"], &[("web", 1)]),
        declared("my_notes", &["web"], &[("web", 2)]),
    ]);
    let synced = super::Synced {
        served: derived.served,
        shadowed: derived.shadowed,
        shadowed_hosts: derived.shadowed_hosts,
        ..super::Synced::default()
    };
    assert_eq!(synced.displaced_by(&name("my.notes")), [name("my_notes")]);
    assert!(synced.displaced_by(&name("my_notes")).is_empty());
}

#[test]
fn a_workspace_that_takes_only_an_extra_host_displaces_no_workspace() {
    let derived = derive(vec![
        hosted(
            declared("alpha", &["web"], &[("web", 1)]),
            "web",
            &["app.example.com"],
        ),
        hosted(
            declared("beta", &["api"], &[("api", 2)]),
            "api",
            &["app.example.com"],
        ),
    ]);
    let synced = super::Synced {
        served: derived.served,
        shadowed: derived.shadowed,
        shadowed_hosts: derived.shadowed_hosts,
        ..super::Synced::default()
    };
    assert!(synced.displaced_by(&name("alpha")).is_empty());
}

#[test]
fn the_served_routes_come_with_a_sentence_for_each_manifest_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    workspace(
        &store,
        dir.path(),
        "triage",
        "ports = [\"web\"]\nroutes = [\"web\"]\n[processes.web]\nargv = [\"web\"]\nports = { PORT = \"web\" }\nlisten = \"web\"\n",
        &[("web", 21001)],
    );
    let (routes, problems) = super::served(&store, &crate::Host::mxv());
    assert_eq!(routes.len(), 1);
    assert!(problems.is_empty(), "{problems:?}");
    write(
        dir.path().join("workspaces/triage/app/mxv.toml"),
        "routes = [",
    );
    let (routes, problems) = super::served(&store, &crate::Host::mxv());
    assert!(routes.is_empty());
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        matches!(&problems[0], ProxyProblem::ManifestLeftOut { workspace, .. } if workspace == &name("triage")),
        "{problems:?}"
    );
    assert!(
        problems[0]
            .to_string()
            .starts_with("the manifest of triage is left out: "),
        "{problems:?}"
    );
}

#[test]
fn the_problems_of_a_workspace_leave_out_those_of_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    for workspace_name in ["triage", "triage-two"] {
        workspace(
            &store,
            dir.path(),
            workspace_name,
            "ports = [\"web\"]\nroutes = [\"web\"]\n",
            &[("web", 21001)],
        );
        write(
            dir.path()
                .join("workspaces")
                .join(workspace_name)
                .join("app/mxv.toml"),
            "routes = [",
        );
    }
    let (_, problems) = super::served(&store, &crate::Host::mxv());
    assert_eq!(problems.len(), 2, "{problems:?}");
    let synced = super::Synced {
        problems,
        ..super::Synced::default()
    };
    let own = synced.problems_of(&name("triage"));
    assert_eq!(own.len(), 1, "{own:?}");
    assert_eq!(own[0].workspace(), Some(&name("triage")));
    assert!(synced.problems_of(&name("other")).is_empty());
}

#[test]
fn the_problems_of_a_workspace_leave_out_those_that_name_no_workspace() {
    let synced = super::Synced {
        problems: vec![
            ProxyProblem::RecordLeftOut {
                error: "bad".to_owned(),
            },
            ProxyProblem::RecordsUnreadable {
                error: "bad".to_owned(),
            },
            ProxyProblem::LabelTooLong {
                workspace: name("triage"),
                bytes: 70,
            },
            ProxyProblem::ManifestLeftOut {
                workspace: name("other"),
                error: "bad".to_owned(),
            },
        ],
        ..super::Synced::default()
    };
    assert_eq!(
        synced.problems_of(&name("triage")),
        [&ProxyProblem::LabelTooLong {
            workspace: name("triage"),
            bytes: 70
        }]
    );
}

#[test]
fn each_problem_writes_the_sentence_a_person_reads() {
    let text = |problem: ProxyProblem| problem.to_string();
    assert_eq!(
        text(ProxyProblem::ManifestLeftOut {
            workspace: name("triage"),
            error: "bad".to_owned()
        }),
        "the manifest of triage is left out: bad"
    );
    assert_eq!(
        text(ProxyProblem::LabelTooLong {
            workspace: name("triage"),
            bytes: 70
        }),
        "the workspace triage makes a host label of 70 bytes, and a label holds at most 63"
    );
    assert_eq!(
        text(ProxyProblem::RecordLeftOut {
            error: "bad".to_owned()
        }),
        "a workspace record is left out: bad"
    );
    assert_eq!(
        text(ProxyProblem::RecordsUnreadable {
            error: "bad".to_owned()
        }),
        "the workspace records cannot be read: bad"
    );
}

#[test]
fn a_workspace_name_that_makes_a_label_over_63_bytes_is_a_label_too_long_problem() {
    let long = "a".repeat(64);
    let derived = derive(vec![declared(&long, &["web"], &[("web", 21001)])]);
    assert!(derived.served.is_empty());
    assert_eq!(
        derived.problems,
        [ProxyProblem::LabelTooLong {
            workspace: name(&long),
            bytes: 64
        }]
    );
}

#[test]
fn routes_under_a_second_profile_use_its_domain() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("records"));
    let routed = |port: &str| {
        format!(
            "ports = [\"{port}\"]\nroutes = [\"{port}\"]\n[processes.{port}]\nargv = [\"{port}\"]\nports = {{ PORT = \"{port}\" }}\nlisten = \"{port}\"\n"
        )
    };
    workspace(
        &store,
        dir.path(),
        "triage",
        &routed("web"),
        &[("web", 21001), ("api", 21002)],
    );
    write(
        dir.path().join("workspaces/triage/app/example.toml"),
        routed("api"),
    );
    let host = crate::Host::named("example").unwrap();
    let proxy = Proxy::new(settings(dir.path()).with_search(vec![dir.path().join("empty")]))
        .with_host(host.clone());

    let status = proxy.status(&store).unwrap();

    let served: Vec<&str> = status
        .routes
        .served
        .iter()
        .map(|route| route.host.as_str())
        .collect();
    assert_eq!(served, ["api.triage.example.localhost"]);
    let (served, problems) = super::served(&store, &host);
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(served[0].host, "api.triage.example.localhost");
    let mxv = Proxy::new(settings(dir.path()).with_search(vec![dir.path().join("empty")]));
    let status = mxv.status(&store).unwrap();
    assert_eq!(status.routes.served[0].host, "web.triage.mxv.localhost");
}
