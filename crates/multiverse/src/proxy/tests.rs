use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::thread;

use super::{
    Declared, Derived, Proxy, ProxyError, Route, Settings, SyncMode, admin, caddy_config, derive,
};
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace};
use crate::store::{Created, Store};
use crate::testing::write;

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
    )
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
fn workspace(store: &Store, dir: &Path, workspace: &str, manifest: &str, ports: &[(&str, u16)]) {
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
        matches!(error, ProxyError::PortHeld { address: found, key: "proxy_http" } if found == address)
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
