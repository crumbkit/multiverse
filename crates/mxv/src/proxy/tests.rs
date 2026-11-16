use std::net::SocketAddr;

use multiverse::{ProxyError, SyncMode, WorkspaceName};

use super::{
    AuthorityState, InstallSteps, Installed, ProxySummary, Served, displaced_lines, extra_lines,
    install_with, join, warning,
};

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

#[test]
fn a_warning_after_a_verb_that_stops_units_does_not_say_they_still_run() {
    let lines = warning(
        &name("reading-list"),
        SyncMode::IfRunning,
        ProxyError::PortHeld {
            address: "127.0.0.1:80".parse().unwrap(),
            key: "proxy_http",
        },
    );
    assert!(
        lines.iter().all(|line| !line.contains("still run")),
        "{lines:?}"
    );
    assert!(
        lines[0].contains("could not update the routes"),
        "{lines:?}"
    );
}

#[derive(Debug)]
struct Fake(Vec<&'static str>);

impl multiverse::Resolver for Fake {
    fn resolve(&self, name: &str) -> multiverse::Resolution {
        if self.0.contains(&name) {
            multiverse::Resolution::Loopback
        } else {
            multiverse::Resolution::Unresolved
        }
    }
}

#[test]
fn a_workspace_without_extra_hosts_prints_no_line_about_them() {
    assert!(extra_lines(&[], &[], &Fake(Vec::new())).is_empty());
}

#[test]
fn an_extra_host_that_resolves_to_this_machine_prints_only_where_it_is_served() {
    let urls = vec!["web at https://local.app.example.com".to_owned()];
    let hosts = vec!["local.app.example.com".to_owned()];
    assert_eq!(
        extra_lines(&urls, &hosts, &Fake(vec!["local.app.example.com"])),
        ["mxv also serves web at https://local.app.example.com."]
    );
}

#[test]
fn an_extra_host_that_does_not_resolve_to_this_machine_prints_the_line_to_add() {
    let urls = vec!["web at https://local.app.example.com".to_owned()];
    let hosts = vec!["local.app.example.com".to_owned()];
    assert_eq!(
        extra_lines(&urls, &hosts, &Fake(Vec::new())),
        [
            "mxv also serves web at https://local.app.example.com.",
            "note: local.app.example.com does not resolve to this machine; add this line to /etc/hosts: 127.0.0.1 local.app.example.com"
        ]
    );
}

#[derive(Debug)]
struct Slow;

impl multiverse::Resolver for Slow {
    fn resolve(&self, _: &str) -> multiverse::Resolution {
        multiverse::Resolution::TimedOut
    }
}

#[test]
fn an_extra_host_whose_lookup_times_out_prints_no_line_to_add() {
    let urls = vec!["web at https://local.app.example.com".to_owned()];
    let hosts = vec!["local.app.example.com".to_owned()];
    let lines = extra_lines(&urls, &hosts, &Slow);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[1].starts_with("note: the lookup of local.app.example.com did not finish"),
        "{lines:?}"
    );
    assert!(!lines[1].contains("/etc/hosts"), "{lines:?}");
}

/// The calls that the fake steps of an install saw, in order.
type Calls = std::cell::RefCell<Vec<&'static str>>;

fn refused() -> crumbkit::Error {
    crumbkit::Error::from(super::ProxyFailure::TrustFailed {
        ended: "exit status: 1".to_owned(),
    })
}

fn run_install(
    calls: &Calls,
    trusted: &mut dyn FnMut() -> bool,
    can_answer: bool,
    trust_fails: bool,
) -> Result<Installed, crumbkit::Error> {
    install_with(InstallSteps {
        start: &mut || {
            calls.borrow_mut().push("start");
            Ok(true)
        },
        trusted: &mut || {
            calls.borrow_mut().push("trusted");
            Ok(trusted())
        },
        trust: &mut || {
            calls.borrow_mut().push("trust");
            if trust_fails {
                Err(refused())
            } else {
                Ok("Caddy Local Authority".to_owned())
            }
        },
        can_answer,
    })
}

#[test]
fn an_install_on_a_machine_that_trusts_the_authority_asks_nothing() {
    let calls = Calls::default();
    let done = run_install(&calls, &mut || true, false, false).unwrap();
    assert_eq!(*calls.borrow(), ["start", "trusted"]);
    assert_eq!(
        done,
        Installed {
            started: true,
            already_trusted: true,
            authority: None
        }
    );
}

#[test]
fn an_install_without_a_terminal_leaves_caddy_started_and_asks_for_a_person() {
    let calls = Calls::default();
    let error = run_install(&calls, &mut || false, false, false).unwrap_err();
    assert_eq!(*calls.borrow(), ["start", "trusted"]);
    assert!(
        format!("{error:?}").contains("TrustNeedsTerminal"),
        "{error:?}"
    );
}

#[test]
fn an_install_at_a_terminal_trusts_the_authority_and_names_it() {
    let calls = Calls::default();
    let done = run_install(&calls, &mut || false, true, false).unwrap();
    assert_eq!(*calls.borrow(), ["start", "trusted", "trust"]);
    assert_eq!(done.authority.as_deref(), Some("Caddy Local Authority"));
    assert!(!done.already_trusted);
}

#[test]
fn a_trust_that_fails_after_caddy_started_is_asked_again_by_the_next_install() {
    let calls = Calls::default();
    run_install(&calls, &mut || false, true, true).unwrap_err();
    assert_eq!(*calls.borrow(), ["start", "trusted", "trust"]);
    let again = Calls::default();
    let done = run_install(&again, &mut || false, true, false).unwrap();
    assert_eq!(*again.borrow(), ["start", "trusted", "trust"]);
    assert!(done.authority.is_some());
}

#[test]
fn a_running_workspace_that_lost_a_name_is_told_to_restart_and_a_stopped_one_is_not() {
    let displaced = vec![name("triage"), name("zeta")];
    let lines = displaced_lines(&name("alpha"), &displaced, &mut |other| {
        other.as_str() == "triage"
    });
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("run `mxv restart -w triage`"),
        "{lines:?}"
    );
    assert!(displaced_lines(&name("alpha"), &[], &mut |_| true).is_empty());
}
