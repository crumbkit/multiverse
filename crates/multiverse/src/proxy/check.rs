//! `mxv proxy check`: what a person must do before the proxy works.
//!
//! A check only reads. It never starts Caddy, loads a configuration, takes
//! the proxy's lock or trusts the authority.

use std::io;

use super::hosts::{self, Resolution, Resolver};
use super::{Proxy, ProxyError, Store, caddy_config, declared, derive, ports, process};

/// How one check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    /// The check passed.
    Ok,
    /// Something a person or `mxv proxy install` has yet to set up.
    Missing,
    /// Something is wrong that setting up does not fix.
    Failed,
    /// mxv cannot tell. A person looks, or `mxv proxy install` asks.
    Unknown,
}

/// One row of the check.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckRow {
    /// What the row checks, such as `caddy`.
    pub name: &'static str,
    /// How the check came out.
    pub state: CheckState,
    /// One line about what was found.
    pub detail: String,
    /// Lines that follow the row, such as the line to add to `/etc/hosts`.
    pub notes: Vec<String>,
}

impl CheckRow {
    fn new(name: &'static str, state: CheckState, detail: impl Into<String>) -> Self {
        Self {
            name,
            state,
            detail: detail.into(),
            notes: Vec::new(),
        }
    }
}

/// The rows of one check.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Check {
    /// One row for each check, and one for each extra host name that does
    /// not resolve to this machine.
    pub rows: Vec<CheckRow>,
}

impl Check {
    /// Returns the rows that did not pass.
    #[must_use]
    pub fn failing(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| !matches!(row.state, CheckState::Ok | CheckState::Unknown))
            .count()
    }
}

impl Proxy {
    /// Reads the state of the proxy and of the machine around it, and
    /// returns a row for each thing that `mxv proxy install` or the person
    /// must set up. Extra host names are looked up with `resolver`.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the store.
    pub fn check(&self, store: &Store, resolver: &dyn Resolver) -> Result<Check, ProxyError> {
        let (found, _) = declared(store)?;
        let derived = derive(found);
        let running = self.answering();
        let mut rows = vec![self.caddy_row()];
        rows.push(self.address_row("http", self.settings.http, "proxy_http", running));
        rows.push(self.address_row("https", self.settings.https, "proxy_https", running));
        rows.push(self.authority_row());
        rows.push(self.runs_row(running, &derived.served));
        let mut names: Vec<String> = derived
            .served
            .iter()
            .flat_map(|route| route.extra.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        let missing = hosts::missing(&names, resolver);
        if names.is_empty() {
            rows.push(CheckRow::new(
                "hosts",
                CheckState::Ok,
                "no extra host names",
            ));
        } else if missing.is_empty() {
            rows.push(CheckRow::new(
                "hosts",
                CheckState::Ok,
                format!(
                    "{} extra host {} to this machine",
                    names.len(),
                    if names.len() == 1 {
                        "name resolves"
                    } else {
                        "names resolve"
                    }
                ),
            ));
        } else {
            for (name, found) in missing {
                let state = if found == Resolution::TimedOut {
                    CheckState::Unknown
                } else {
                    CheckState::Missing
                };
                let mut row = CheckRow::new("hosts", state, found.detail(&name));
                row.notes.extend(found.advice(&name));
                rows.push(row);
            }
        }
        Ok(Check { rows })
    }

    /// Returns the row for a running Caddy: ok when the configuration it
    /// holds is the one the workspace records give, and failed when it
    /// differs, because a route changed while Caddy ran and no sync loaded it.
    fn runs_row(&self, running: bool, served: &[super::Route]) -> CheckRow {
        if !running {
            return if served.is_empty() {
                CheckRow::new(
                    "caddy runs",
                    CheckState::Ok,
                    "not needed until a workspace declares a route",
                )
            } else {
                CheckRow::new(
                    "caddy runs",
                    CheckState::Missing,
                    "not running; a person runs `mxv proxy install`",
                )
            };
        }
        let pid = self
            .pid()
            .map_or_else(String::new, |pid| format!("pid {pid}, "));
        let routes = served.len();
        let noun = if routes == 1 { "route" } else { "routes" };
        let wanted = caddy_config::render(&self.settings, served);
        if self.current().as_ref() == Some(&wanted) {
            CheckRow::new(
                "caddy runs",
                CheckState::Ok,
                format!("{pid}{routes} {noun} loaded"),
            )
        } else {
            CheckRow::new(
                "caddy runs",
                CheckState::Failed,
                format!(
                    "{pid}the routes Caddy holds differ from the {routes} {noun} of the workspace records; `mxv proxy start` loads the current ones"
                ),
            )
        }
    }

    /// Returns the row for the Caddy program.
    fn caddy_row(&self) -> CheckRow {
        match self.program() {
            Ok((path, version)) => CheckRow::new(
                "caddy",
                CheckState::Ok,
                format!("{}, version {version}", path.display()),
            ),
            Err(ProxyError::CaddyTooOld { path, found, .. }) => CheckRow::new(
                "caddy",
                CheckState::Failed,
                format!(
                    "{} is version {found}; mxv needs 2.8 or later",
                    path.display()
                ),
            ),
            Err(ProxyError::CaddyMissing {
                named,
                reason,
                config,
            }) => CheckRow::new(
                "caddy",
                CheckState::Missing,
                match named {
                    Some(path) => format!(
                        "{} does not run: {reason}; set `caddy` in {} to Caddy 2.8 or later",
                        path.display(),
                        config.display()
                    ),
                    None => "no executable named caddy on PATH; install Caddy 2.8 or later, for example `brew install caddy`".to_owned(),
                },
            ),
            Err(error) => CheckRow::new("caddy", CheckState::Failed, error.to_string()),
        }
    }

    /// Returns the row for the address `address`, which `key` configures.
    fn address_row(
        &self,
        name: &'static str,
        address: std::net::SocketAddr,
        key: &str,
        running: bool,
    ) -> CheckRow {
        if running {
            return CheckRow::new(
                name,
                CheckState::Ok,
                format!("{address} is held by mxv's Caddy"),
            );
        }
        match ports::can_bind(address) {
            Ok(()) => CheckRow::new(name, CheckState::Ok, format!("{address} is free")),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => CheckRow::new(
                name,
                CheckState::Failed,
                format!(
                    "{address} is held by another process; stop it, or set {key} in {}",
                    self.settings.config.display()
                ),
            ),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => CheckRow::new(
                name,
                CheckState::Failed,
                format!(
                    "the system does not let this user listen on {address}; set {key} in {} to a port above 1023",
                    self.settings.config.display()
                ),
            ),
            Err(error) => CheckRow::new(name, CheckState::Failed, format!("{address}: {error}")),
        }
    }

    /// Returns the row for the local certificate authority.
    fn authority_row(&self) -> CheckRow {
        let certificate = self.settings.root_certificate();
        if !certificate.exists() {
            return CheckRow::new(
                "authority",
                CheckState::Missing,
                "not created yet; a person runs `mxv proxy install`",
            );
        }
        Self::trust_row(process::trusted(&certificate))
    }

    /// Returns the authority row for a certificate that exists and whose
    /// trust the system reports as `trusted`.
    fn trust_row(trusted: Option<bool>) -> CheckRow {
        match trusted {
            Some(true) => CheckRow::new("authority", CheckState::Ok, "trusted by this machine"),
            Some(false) => CheckRow::new(
                "authority",
                CheckState::Missing,
                "created, not trusted by this machine; a person runs `mxv proxy install`",
            ),
            None => CheckRow::new(
                "authority",
                CheckState::Unknown,
                "created; mxv cannot tell whether this machine trusts it, and `mxv proxy install` asks",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::{Ipv4Addr, TcpListener};
    use std::path::Path;

    use super::super::hosts::{Resolution, Resolver};
    use super::super::tests::workspace;
    use super::super::{Proxy, Settings};
    use super::CheckState;
    use crate::store::Store;

    #[derive(Debug)]
    struct Fake(BTreeMap<&'static str, Resolution>);

    impl Resolver for Fake {
        fn resolve(&self, name: &str) -> Resolution {
            self.0.get(name).copied().unwrap_or(Resolution::Unresolved)
        }
    }

    fn free() -> std::net::SocketAddr {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    }

    fn proxy(dir: &Path) -> Proxy {
        Proxy::new(
            Settings::new(dir.join("state"), dir.join("data"))
                .with_addresses(free(), free())
                .with_search(vec![dir.join("empty")]),
        )
    }

    fn state_of(check: &super::Check, name: &str) -> Vec<CheckState> {
        check
            .rows
            .iter()
            .filter(|row| row.name == name)
            .map(|row| row.state)
            .collect()
    }

    #[test]
    fn a_machine_with_nothing_set_up_misses_caddy_the_authority_and_a_running_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        let check = proxy(dir.path())
            .check(&store, &Fake(BTreeMap::new()))
            .unwrap();
        assert_eq!(state_of(&check, "caddy"), [CheckState::Missing]);
        assert_eq!(state_of(&check, "http"), [CheckState::Ok]);
        assert_eq!(state_of(&check, "https"), [CheckState::Ok]);
        assert_eq!(state_of(&check, "authority"), [CheckState::Missing]);
        assert_eq!(state_of(&check, "caddy runs"), [CheckState::Ok]);
        assert_eq!(state_of(&check, "hosts"), [CheckState::Ok]);
        assert_eq!(check.failing(), 2);
        let runs = check
            .rows
            .iter()
            .find(|row| row.name == "caddy runs")
            .unwrap();
        assert_eq!(runs.detail, "not needed until a workspace declares a route");
        let authority = check
            .rows
            .iter()
            .find(|row| row.name == "authority")
            .unwrap();
        assert_eq!(
            authority.detail,
            "not created yet; a person runs `mxv proxy install`"
        );
    }

    #[test]
    fn a_held_address_fails_its_row_and_names_the_config_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = held.local_addr().unwrap();
        let proxy = Proxy::new(
            Settings::new(dir.path().join("state"), dir.path().join("data"))
                .with_addresses(free(), address),
        );
        let check = proxy.check(&store, &Fake(BTreeMap::new())).unwrap();
        assert_eq!(state_of(&check, "https"), [CheckState::Failed]);
        let row = check.rows.iter().find(|row| row.name == "https").unwrap();
        assert!(row.detail.contains("is held by another process"), "{row:?}");
        assert!(row.detail.contains("proxy_https"), "{row:?}");
    }

    #[test]
    fn an_extra_host_that_does_not_resolve_to_loopback_gets_a_row_with_the_line_to_add() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        workspace(
            &store,
            dir.path(),
            "triage",
            "ports = [\"web\"]\nroutes = [\"web\"]\n[processes.web]\nargv = [\"web\"]\nports = { PORT = \"web\" }\nlisten = \"web\"\nhosts = [\"local.app.example.com\", \"other.app.example.com\"]\n",
            &[("web", 21001)],
        );
        let fake = Fake(BTreeMap::from([(
            "other.app.example.com",
            Resolution::Loopback,
        )]));
        let check = proxy(dir.path()).check(&store, &fake).unwrap();
        assert_eq!(state_of(&check, "hosts"), [CheckState::Missing]);
        let row = check.rows.iter().find(|row| row.name == "hosts").unwrap();
        assert_eq!(
            row.detail,
            "local.app.example.com does not resolve to this machine"
        );
        assert_eq!(
            row.notes,
            ["add this line to /etc/hosts: 127.0.0.1 local.app.example.com"]
        );
    }

    #[test]
    fn a_stopped_caddy_is_missing_once_a_workspace_declares_a_route() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        workspace(
            &store,
            dir.path(),
            "triage",
            "ports = [\"web\"]\nroutes = [\"web\"]\n[processes.web]\nargv = [\"web\"]\nports = { PORT = \"web\" }\nlisten = \"web\"\n",
            &[("web", 21001)],
        );
        let check = proxy(dir.path())
            .check(&store, &Fake(BTreeMap::new()))
            .unwrap();
        assert_eq!(state_of(&check, "caddy runs"), [CheckState::Missing]);
    }

    #[test]
    fn a_host_lookup_that_times_out_is_unknown_and_gives_no_line_to_add() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        workspace(
            &store,
            dir.path(),
            "triage",
            "ports = [\"web\"]\nroutes = [\"web\"]\n[processes.web]\nargv = [\"web\"]\nports = { PORT = \"web\" }\nlisten = \"web\"\nhosts = [\"slow.app.example.com\"]\n",
            &[("web", 21001)],
        );
        let fake = Fake(BTreeMap::from([(
            "slow.app.example.com",
            Resolution::TimedOut,
        )]));
        let check = proxy(dir.path()).check(&store, &fake).unwrap();
        assert_eq!(state_of(&check, "hosts"), [CheckState::Unknown]);
        let row = check.rows.iter().find(|row| row.name == "hosts").unwrap();
        assert!(row.detail.contains("did not finish"), "{row:?}");
        assert!(row.notes.is_empty());
    }

    #[test]
    fn an_authority_whose_trust_the_system_cannot_report_is_unknown_and_does_not_fail_the_check() {
        let row = Proxy::trust_row(None);
        assert_eq!(row.state, CheckState::Unknown);
        assert!(row.detail.contains("cannot tell"), "{row:?}");
        assert!(row.detail.contains("`mxv proxy install` asks"), "{row:?}");
        let check = super::Check { rows: vec![row] };
        assert_eq!(check.failing(), 0);
        assert_eq!(Proxy::trust_row(Some(false)).state, CheckState::Missing);
        assert_eq!(Proxy::trust_row(Some(true)).state, CheckState::Ok);
    }

    #[test]
    fn the_messages_about_an_address_and_a_program_name_the_config_file_of_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = held.local_addr().unwrap();
        let config = dir.path().join("xdg/mxv/config.toml");
        let proxy = Proxy::new(
            Settings::new(dir.path().join("state"), dir.path().join("data"))
                .with_addresses(free(), address)
                .with_config(config.clone())
                .with_search(vec![dir.path().join("empty")]),
        );
        let check = proxy.check(&store, &Fake(BTreeMap::new())).unwrap();
        let row = check.rows.iter().find(|row| row.name == "https").unwrap();
        assert!(
            row.detail.contains(&config.display().to_string()),
            "{row:?}"
        );
        assert!(!row.detail.contains("~/.config"), "{row:?}");
        let error = proxy.caddy().unwrap_err();
        assert!(error.to_string().contains("no working `caddy`"), "{error}");
        assert!(
            error.to_string().contains(&config.display().to_string()),
            "{error}"
        );
    }

    /// Answers every request on `proxy`'s admin socket with `body`, as a
    /// Caddy that holds that configuration does.
    fn serve(proxy: &Proxy, body: String) {
        use std::io::{Read as _, Write as _};
        let socket = proxy.settings.socket();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
    }

    fn routed(dir: &Path) -> (Store, Proxy) {
        let store = Store::new(dir.join("records"));
        workspace(
            &store,
            dir,
            "triage",
            "ports = [\"web\"]\nroutes = [\"web\"]\n[processes.web]\nargv = [\"web\"]\nports = { PORT = \"web\" }\nlisten = \"web\"\n",
            &[("web", 21001)],
        );
        (store, proxy(dir))
    }

    #[test]
    fn a_caddy_that_holds_the_current_routes_is_ok_and_says_they_are_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let (store, proxy) = routed(dir.path());
        let (found, _) = super::declared(&store).unwrap();
        let wanted =
            super::super::caddy_config::render(&proxy.settings, &super::derive(found).served);
        serve(&proxy, wanted.to_string());
        let check = proxy.check(&store, &Fake(BTreeMap::new())).unwrap();
        let row = check
            .rows
            .iter()
            .find(|row| row.name == "caddy runs")
            .unwrap();
        assert_eq!(row.state, CheckState::Ok, "{row:?}");
        assert!(row.detail.ends_with("1 route loaded"), "{row:?}");
    }

    #[test]
    fn a_caddy_that_holds_other_routes_than_the_records_fails_and_names_the_start_command() {
        let dir = tempfile::tempdir().unwrap();
        let (store, proxy) = routed(dir.path());
        serve(&proxy, "{}".to_owned());
        let check = proxy.check(&store, &Fake(BTreeMap::new())).unwrap();
        let row = check
            .rows
            .iter()
            .find(|row| row.name == "caddy runs")
            .unwrap();
        assert_eq!(row.state, CheckState::Failed, "{row:?}");
        assert!(row.detail.contains("`mxv proxy start`"), "{row:?}");
        assert_eq!(check.failing(), 3);
    }

    #[test]
    fn a_check_never_starts_caddy_or_creates_the_proxy_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("records"));
        proxy(dir.path())
            .check(&store, &Fake(BTreeMap::new()))
            .unwrap();
        assert!(!dir.path().join("state").exists());
    }
}
