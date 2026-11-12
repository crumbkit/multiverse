//! End-to-end tests of `mxv proxy` and of the routes that `mxv up`,
//! `mxv down` and the other lifecycle verbs keep in line with the
//! workspaces. The first tests need no Caddy process: they meet the
//! refusals and warnings, and read the routes that the records declare. The
//! last tests start a real Caddy and run only when `MXV_CADDY_TESTS` is `1`;
//! they take the program from `MXV_CADDY_BIN`, or else from `PATH`. They
//! listen on high loopback ports and never trust the certificate authority.

#![cfg(test)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/git.rs"]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};
use crate::test_ports::PortBlock;

/// How long a test waits for Caddy to answer.
const MOST_WAIT: Duration = Duration::from_secs(30);

/// A deadband socket that no daemon listens on.
const MISSING_SOCKET: &str = "/nonexistent/deadband.sock";

/// Writes `text` to the file at `path`.
fn write(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write"
    )]
    fs::write(path, text).unwrap();
}

/// Returns a loopback port that nothing listens on now.
fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Reads the environment variable `name`, for the gate of the real-Caddy
/// tests.
fn variable(name: &str) -> Option<String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the real-Caddy tests are gated by a variable of the test run"
    )]
    std::env::var(name).ok()
}

/// A temporary directory with a repository, a workspace root and a state
/// directory.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The block of ports that the config file names.
    ports: PortBlock,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        repository(&dir.path().canonicalize().unwrap());
        let setup = Self {
            dir,
            ports: PortBlock::claim(),
        };
        setup.config(&[]);
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Writes the config file with the port range and `lines`.
    fn config(&self, lines: &[String]) {
        let config = self.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let mut text = self.ports.config_line();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        write(&config, &text);
    }

    /// Points mxv at a Caddy program that does not exist, and at free high
    /// ports, so that no test binds port 80 or 443.
    fn without_caddy(&self) {
        self.config(&[
            "caddy = \"/nonexistent/caddy\"".to_owned(),
            format!("proxy_http = \"127.0.0.1:{}\"", free_port()),
            format!("proxy_https = \"127.0.0.1:{}\"", free_port()),
        ]);
    }

    /// Returns the HTTPS port that `without_caddy` wrote to the config file.
    fn https_port(&self) -> u16 {
        let text = fs::read_to_string(self.root().join("config/mxv/config.toml")).unwrap();
        let line = text
            .lines()
            .find(|line| line.starts_with("proxy_https"))
            .unwrap();
        line.trim_end_matches('"')
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Creates the workspace `name` with `manifest`.
    fn workspace(&self, name: &str, manifest: &str) {
        let (exit, document) = self.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        write(
            &self
                .root()
                .join("workspaces")
                .join(name)
                .join("app/mxv.toml"),
            manifest,
        );
    }

    fn command(&self, args: &[&str]) -> Command {
        grants::before(&self.root().join("state/mxv"), args);
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("DEADBAND_SOCKET", MISSING_SOCKET)
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code, the
    /// document and standard error.
    fn json_and_errors(&self, args: &[&str]) -> (i32, Value, String) {
        let output = self.command(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (
            output.status.code().unwrap(),
            document,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let (exit, document, _) = self.json_and_errors(args);
        (exit, document)
    }

    /// Runs `mxv` with `args`, and returns the exit code, standard output
    /// and standard error.
    fn text(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command(args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// Returns the rows of `mxv proxy status`.
    fn routes(&self) -> Vec<Value> {
        let (exit, document) = self.json(&["proxy", "status"]);
        assert_eq!(exit, 0, "{document}");
        document["data"].as_array().cloned().unwrap_or_default()
    }
}

impl Drop for Setup {
    /// Stops the proxy and every unit that a test left running.
    fn drop(&mut self) {
        let _ = self.json(&["proxy", "stop"]);
        let (_, listed) = self.json(&["list"]);
        for row in listed["data"].as_array().cloned().unwrap_or_default() {
            let _ = self.json(&["down", "-w", row["name"].as_str().unwrap_or_default()]);
        }
    }
}

/// Returns a manifest whose `web` unit serves HTTP on the port `web` and
/// whose route `web` points at it, and whose unit `api` listens on the port
/// `api`, which has no route.
fn site() -> String {
    r#"ports = ["web", "api"]
routes = ["web"]

[processes.web]
argv = ["sh", "-c", "mkdir -p /tmp/mxv-proxy-site-$PORT && echo hello-from-web > /tmp/mxv-proxy-site-$PORT/index.html && cd /tmp/mxv-proxy-site-$PORT && exec python3 -m http.server --bind 127.0.0.1 $PORT"]
ports = { PORT = "web" }
listen = "web"
ready = { timeout_seconds = 20 }
"#
    .to_owned()
}

/// Returns a manifest with a route and no unit that listens on its port.
fn unlistened() -> String {
    r#"ports = ["web-ui"]
routes = ["web-ui"]

[processes.idle]
argv = ["sleep", "60"]
"#
    .to_owned()
}

#[test]
fn trust_without_a_terminal_refuses_and_names_the_person_to_ask() {
    let setup = Setup::new();
    let (exit, document, errors) = setup.json_and_errors(&["proxy", "trust"]);
    assert_eq!(exit, 1, "{document}{errors}");
    let text = document.to_string();
    assert!(text.contains("mxv::proxy::trust_needs_terminal"), "{text}");
    assert!(text.contains("mxv proxy trust"), "{text}");
}

#[test]
fn status_says_the_proxy_does_not_run_when_no_workspace_declares_a_route() {
    let setup = Setup::new();
    setup.workspace("plain", "ports = [\"api\"]\n");
    let (exit, text, _) = setup.text(&["proxy", "status"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains(
            "proxy: not running; no workspace declares a route, and `mxv up` starts it when one does"
        ),
        "{text}"
    );
}

#[test]
fn up_warns_that_caddy_is_missing_and_still_starts_the_unit() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("reading-list", &site());
    let (exit, document, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{document}{errors}");
    assert_eq!(document["data"][0]["state"], "running");
    assert!(
        errors.contains("mxv could not serve the routes of reading-list"),
        "{errors}"
    );
    assert!(
        errors.contains("error code: mxv::proxy::caddy_missing"),
        "{errors}"
    );
}

#[test]
fn up_warns_that_an_address_is_held_and_still_starts_the_unit() {
    let setup = Setup::new();
    let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let held = holder.local_addr().unwrap();
    setup.config(&[format!("proxy_http = \"{held}\"")]);
    setup.workspace("reading-list", &site());
    let (exit, document, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{document}{errors}");
    assert!(
        errors.contains(&format!("{held} is held by another process")),
        "{errors}"
    );
    assert!(
        errors.contains("error code: mxv::proxy::port_held"),
        "{errors}"
    );
}

#[test]
fn status_lists_the_route_of_a_stopped_workspace_with_its_unit_state() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("triage", &site());
    let (exit, _, _) = setup.json_and_errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 0);
    let (exit, _) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0);
    let routes = setup.routes();
    assert_eq!(routes.len(), 1, "{routes:?}");
    assert_eq!(routes[0]["route"], "web");
    assert_eq!(routes[0]["workspace"], "triage");
    assert_eq!(
        routes[0]["url"],
        format!("https://web.triage.mxv.localhost:{}", setup.https_port())
    );
    assert_eq!(routes[0]["unit"], "web");
    assert_eq!(routes[0]["state"], "stopped");
    let (_, text, _) = setup.text(&["proxy", "status"]);
    assert!(
        text.contains("proxy: not running, although 1 route exists; `mxv proxy start` starts it"),
        "{text}"
    );
}

#[test]
fn a_route_that_no_unit_listens_on_says_so() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("triage", &unlistened());
    let (exit, _, _) = setup.json_and_errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 0);
    let routes = setup.routes();
    assert_eq!(routes[0]["unit"], Value::Null);
    assert_eq!(routes[0]["state"], "no unit listens on web-ui");
}

#[test]
fn two_workspaces_that_make_one_host_label_serve_the_first_and_shadow_the_second() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("my_notes", &site());
    setup.workspace("my.notes", &site());
    for name in ["my_notes", "my.notes"] {
        let (exit, _, _) = setup.json_and_errors(&["up", "-w", name]);
        assert_eq!(exit, 0);
    }
    let routes = setup.routes();
    let served = routes
        .iter()
        .find(|row| row["workspace"] == "my.notes")
        .unwrap();
    let shadowed = routes
        .iter()
        .find(|row| row["workspace"] == "my_notes")
        .unwrap();
    assert_eq!(
        served["url"],
        format!("https://web.my-notes.mxv.localhost:{}", setup.https_port())
    );
    assert_eq!(shadowed["url"], "(not served)");
    assert!(
        shadowed["notes"][0].as_str().unwrap().contains(
            "my_notes and my.notes both map to the host label my-notes; my.notes serves it"
        ),
        "{shadowed}"
    );
}

#[test]
fn down_of_a_workspace_with_no_routes_leaves_no_proxy_files() {
    let setup = Setup::new();
    setup.workspace("plain", "ports = [\"api\"]\n");
    let (exit, _) = setup.json(&["down", "-w", "plain"]);
    assert_eq!(exit, 0);
    assert!(!setup.root().join("state/mxv/proxy").exists());
}

#[test]
fn an_undeclared_route_fails_up_with_the_manifest_code() {
    let setup = Setup::new();
    setup.workspace("bad", "ports = [\"api\"]\nroutes = [\"web\"]\n");
    let (exit, document, errors) = setup.json_and_errors(&["up", "-w", "bad"]);
    assert_ne!(exit, 0, "{document}{errors}");
    assert!(
        document.to_string().contains("mxv::manifest::invalid"),
        "{document}"
    );
}

/// What a real-Caddy test holds: the program and the two ports.
struct Real {
    /// The path of the program.
    caddy: String,
    /// The HTTP port.
    http: u16,
    /// The HTTPS port.
    https: u16,
}

/// Returns the real Caddy to test with, or `None` when `MXV_CADDY_TESTS` is
/// not `1`, which is the case of the pull request lane.
fn real() -> Option<Real> {
    (variable("MXV_CADDY_TESTS").as_deref() == Some("1")).then(|| Real {
        caddy: variable("MXV_CADDY_BIN").unwrap_or_else(|| "caddy".to_owned()),
        http: free_port(),
        https: free_port(),
    })
}

impl Real {
    /// Writes the config that points mxv at this Caddy and these ports.
    fn configure(&self, setup: &Setup) {
        setup.config(&[
            format!("caddy = \"{}\"", self.caddy),
            format!("proxy_http = \"127.0.0.1:{}\"", self.http),
            format!("proxy_https = \"127.0.0.1:{}\"", self.https),
        ]);
    }

    /// Asks `curl` for `host` over `scheme`, resolving the name to the
    /// loopback address, and returns the status code and the body. HTTPS
    /// does not verify the certificate, so no root is trusted.
    fn get(&self, scheme: &str, host: &str) -> (String, String) {
        let port = if scheme == "https" {
            self.https
        } else {
            self.http
        };
        let output = Command::new("curl")
            .args([
                "-sk",
                "--max-time",
                "5",
                "--resolve",
                &format!("{host}:{port}:127.0.0.1"),
                "-w",
                "\n%{http_code}",
                &format!("{scheme}://{host}:{port}/"),
            ])
            .env("PATH", PATH)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", "000"));
        (code.to_owned(), body.to_owned())
    }

    /// Asks again until `host` answers with `code`, or fails after
    /// [`MOST_WAIT`].
    fn wait_for(&self, scheme: &str, host: &str, code: &str) -> String {
        let deadline = Instant::now() + MOST_WAIT;
        loop {
            let (got, body) = self.get(scheme, host);
            if got == code {
                return body;
            }
            assert!(
                Instant::now() < deadline,
                "{scheme}://{host} answered {got} and not {code}: {body}"
            );
            #[expect(
                clippy::disallowed_methods,
                reason = "the test polls Caddy, which has no event to wait for"
            )]
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn a_real_caddy_answers_a_route_over_http_and_https_and_explains_a_stopped_workspace() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("reading-list", &site());
    let (exit, document, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{document}{errors}");
    let host = "web.reading-list.mxv.localhost";
    assert!(
        real.wait_for("http", host, "200")
            .contains("hello-from-web")
    );
    assert!(
        real.wait_for("https", host, "200")
            .contains("hello-from-web")
    );
    let (_, text, _) = setup.text(&["proxy", "status"]);
    assert!(text.contains("proxy: running (caddy "), "{text}");
    assert!(
        text.contains("certificate authority: created, not trusted by this machine"),
        "{text}"
    );
    let (exit, _) = setup.json(&["down", "-w", "reading-list"]);
    assert_eq!(exit, 0);
    let body = real.wait_for("https", host, "502");
    assert!(
        body.contains("is a route of the stopped workspace reading-list"),
        "{body}"
    );
    assert!(body.contains("mxv up -w reading-list"), "{body}");
}

#[test]
fn a_restarted_caddy_serves_the_routes_of_the_last_load() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("reading-list", &site());
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{errors}");
    let host = "web.reading-list.mxv.localhost";
    real.wait_for("https", host, "200");
    let (_, status) = setup.json(&["proxy", "status"]);
    let pid = status["summary"]["pid"].as_u64().unwrap();
    let killed = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(killed.status.success());
    assert!(
        real.wait_for("https", host, "200")
            .contains("hello-from-web")
    );
}

#[test]
fn starting_the_proxy_again_with_the_routes_unchanged_keeps_the_same_caddy() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("reading-list", &site());
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{errors}");
    let (_, first) = setup.json(&["proxy", "status"]);
    let (exit, second) = setup.json(&["proxy", "start"]);
    assert_eq!(exit, 0, "{second}");
    assert_eq!(first["summary"]["pid"], second["summary"]["pid"]);
}
