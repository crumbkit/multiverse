use std::net::SocketAddr;

use multiverse::{ProxyError, SyncMode, WorkspaceName};

use super::{AuthorityState, ProxySummary, Served, join, warning};

fn name(text: &str) -> WorkspaceName {
    text.parse().unwrap()
}

fn summary(running: bool, routes: usize, authority: AuthorityState) -> ProxySummary {
    ProxySummary {
        running,
        version: running.then(|| "2.10.0".to_owned()),
        pid: running.then_some(48213),
        http: "127.0.0.1:80".to_owned(),
        https: "127.0.0.1:443".to_owned(),
        authority,
        routes,
        problems: Vec::new(),
    }
}

#[test]
fn the_urls_of_a_workspace_read_as_a_list_in_a_sentence() {
    let one = vec!["web at https://web.a.mxv.localhost".to_owned()];
    assert_eq!(join(&one), "web at https://web.a.mxv.localhost");
    let two = vec!["web at A".to_owned(), "api at B".to_owned()];
    assert_eq!(join(&two), "web at A and api at B");
    let three = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
    assert_eq!(join(&three), "a, b and c");
}

#[test]
fn a_held_address_warns_with_its_code_and_says_the_units_still_run() {
    let address: SocketAddr = "127.0.0.1:80".parse().unwrap();
    let lines = warning(
        &name("reading-list"),
        SyncMode::Start,
        ProxyError::PortHeld {
            address,
            key: "proxy_http",
        },
    );
    assert!(
        lines[0].starts_with("warning: mxv could not serve the routes of reading-list, because"),
        "{lines:?}"
    );
    assert!(lines[0].contains("127.0.0.1:80 is held by another process"));
    assert!(lines[0].contains("proxy_http"));
    assert!(lines[1].contains("The units still run."), "{lines:?}");
    assert!(
        lines[1].ends_with("error code: mxv::proxy::port_held"),
        "{lines:?}"
    );
}

#[test]
fn a_missing_caddy_warns_with_its_code_and_how_to_install_it() {
    let lines = warning(
        &name("reading-list"),
        SyncMode::Start,
        ProxyError::CaddyMissing {
            named: None,
            reason: "not found".to_owned(),
        },
    );
    assert!(lines[0].contains("brew install caddy"), "{lines:?}");
    assert!(
        lines[1].ends_with("error code: mxv::proxy::caddy_missing"),
        "{lines:?}"
    );
}

#[test]
fn a_failure_while_routes_leave_says_it_could_not_update_them() {
    let lines = warning(
        &name("reading-list"),
        SyncMode::IfRunning,
        ProxyError::LoadFailed {
            message: "bad route".to_owned(),
        },
    );
    assert!(
        lines[0].contains("could not update the routes of reading-list"),
        "{lines:?}"
    );
    assert!(
        lines[1].ends_with("error code: mxv::proxy::load_failed"),
        "{lines:?}"
    );
}

#[test]
fn a_running_proxy_shows_its_version_pid_and_addresses() {
    let text = summary(true, 2, AuthorityState::Untrusted).to_string();
    assert_eq!(
        text,
        "proxy: running (caddy 2.10.0, pid 48213), http 127.0.0.1:80, https 127.0.0.1:443\ncertificate authority: created, not trusted by this machine; a person runs `mxv proxy trust`"
    );
}

#[test]
fn a_stopped_proxy_says_whether_routes_wait_for_it() {
    assert_eq!(
        summary(false, 0, AuthorityState::None).to_string(),
        "proxy: not running; no workspace declares a route, and `mxv up` starts it when one does"
    );
    assert_eq!(
        summary(false, 2, AuthorityState::None).to_string(),
        "proxy: not running, although 2 routes exist; `mxv proxy start` starts it"
    );
    assert_eq!(
        summary(false, 1, AuthorityState::None).to_string(),
        "proxy: not running, although 1 route exists; `mxv proxy start` starts it"
    );
}

#[test]
fn a_trusted_authority_says_this_machine_trusts_it() {
    let text = summary(true, 1, AuthorityState::Trusted).to_string();
    assert!(
        text.ends_with("certificate authority: created, trusted by this machine"),
        "{text}"
    );
}

#[test]
fn nothing_is_said_before_a_lifecycle_verb_syncs() {
    let served = Served::default();
    assert!(served.lines().is_empty());
}
