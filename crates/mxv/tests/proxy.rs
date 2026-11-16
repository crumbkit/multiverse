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

/// Returns a manifest whose unit `web` serves the route `web`, lists the
/// extra host name `host`, and records its proxy variables in `proxy.txt`
/// in its checkout.
fn hosted(host: &str) -> String {
    format!(
        r#"ports = ["web"]
routes = ["web"]

[processes.web]
argv = ["sh", "-c", "echo \"$MXV_URL $MXV_DOMAIN $MXV_HOST_WEB\" > proxy.txt && exec sleep 60"]
ports = {{ PORT = "web" }}
listen = "web"
ready = {{ timeout_seconds = 1 }}
hosts = ["{host}"]
"#
    )
}

#[test]
fn a_unit_receives_the_proxy_names_of_its_workspace_in_its_environment() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("triage", &hosted("app.localhost"));
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "triage"]);
    let _ = errors;
    assert!(exit == 0 || exit == 1);
    let file = setup.root().join("workspaces/triage/app/proxy.txt");
    let deadline = Instant::now() + MOST_WAIT;
    while !file.exists() && Instant::now() < deadline {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test polls for a file that the unit writes"
        )]
        std::thread::sleep(Duration::from_millis(100));
    }
    let port = setup.https_port();
    assert_eq!(
        fs::read_to_string(&file).unwrap().trim(),
        format!(
            "https://web.triage.mxv.localhost:{port} triage.mxv.localhost web.triage.mxv.localhost"
        )
    );
}

#[test]
fn status_lists_the_extra_host_names_under_the_route() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("triage", &hosted("app.localhost"));
    let (exit, _, _) = setup.json_and_errors(&["up", "-w", "triage"]);
    assert!(exit == 0 || exit == 1);
    let routes = setup.routes();
    assert_eq!(
        routes[0]["notes"][0],
        format!("https://app.localhost:{}", setup.https_port())
    );
}

#[test]
fn status_names_the_workspace_that_serves_an_extra_host_name_that_two_list() {
    let setup = Setup::new();
    setup.without_caddy();
    setup.workspace("alpha", &hosted("app.localhost"));
    setup.workspace("beta", &hosted("app.localhost"));
    for name in ["alpha", "beta"] {
        let (exit, _, _) = setup.json_and_errors(&["up", "-w", name]);
        assert!(exit == 0 || exit == 1);
    }
    let routes = setup.routes();
    let beta = routes
        .iter()
        .find(|row| row["workspace"] == "beta")
        .unwrap();
    assert!(
        beta["notes"][0]
            .as_str()
            .unwrap()
            .contains("app.localhost is not served for beta; alpha lists it too and serves it"),
        "{beta}"
    );
}

#[test]
fn check_without_caddy_prints_a_row_for_each_check_and_fails_with_its_code() {
    let setup = Setup::new();
    setup.without_caddy();
    let (exit, text, errors) = setup.text(&["proxy", "check"]);
    assert_eq!(exit, 1, "{text}{errors}");
    for row in ["caddy", "http", "https", "authority", "caddy runs", "hosts"] {
        assert!(text.contains(row), "{row}: {text}");
    }
    assert!(text.contains("no extra host names"), "{text}");
    assert!(errors.contains("mxv::proxy::check_failed"), "{errors}");
    assert!(errors.contains("2 of 6 proxy checks failed"), "{errors}");
    assert!(
        !setup.root().join("state/mxv/proxy").exists(),
        "a check created the proxy directory"
    );
}

#[test]
fn check_reports_an_address_that_another_process_holds() {
    let setup = Setup::new();
    let holder = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let held = holder.local_addr().unwrap();
    setup.config(&[format!("proxy_https = \"{held}\"")]);
    let (exit, printed, _) = setup.text(&["proxy", "check", "--json"]);
    assert_eq!(exit, 1, "{printed}");
    let document: Value = serde_json::from_str(printed.lines().next().unwrap()).unwrap();
    let rows = document["data"].as_array().unwrap();
    let https = rows.iter().find(|row| row["check"] == "https").unwrap();
    assert_eq!(https["state"], "failed");
    let config = setup.root().join("config/mxv/config.toml");
    assert!(
        https["detail"]
            .as_str()
            .unwrap()
            .contains(&config.display().to_string()),
        "the row names the config file under XDG_CONFIG_HOME: {https}"
    );
    assert!(
        https["detail"]
            .as_str()
            .unwrap()
            .contains(&format!("{held} is held by another process")),
        "{https}"
    );
}

#[test]
fn install_without_caddy_refuses_with_the_missing_caddy_code() {
    let setup = Setup::new();
    setup.without_caddy();
    let (exit, document, _) = setup.json_and_errors(&["proxy", "install"]);
    assert_eq!(exit, 1, "{document}");
    assert!(
        document.to_string().contains("mxv::proxy::caddy_missing"),
        "{document}"
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
    // Only macOS can ask the system whether it trusts the root, so any other
    // system says that mxv cannot tell.
    let authority = if cfg!(target_os = "macos") {
        "certificate authority: created, not trusted by this machine"
    } else {
        "certificate authority: created; mxv cannot tell whether this machine trusts it"
    };
    assert!(text.contains(authority), "{text}");
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

#[test]
fn starting_the_proxy_again_with_the_routes_unchanged_loads_nothing() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("reading-list", &site());
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{errors}");
    let bootstrap = setup.root().join("state/mxv/proxy/bootstrap.json");
    let before = fs::metadata(&bootstrap).unwrap().modified().unwrap();
    for _ in 0..2 {
        let (exit, document) = setup.json(&["proxy", "start"]);
        assert_eq!(exit, 0, "{document}");
    }
    assert_eq!(
        fs::metadata(&bootstrap).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn a_real_caddy_serves_an_extra_host_name_and_prints_the_hosts_line_for_it() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    let manifest = site().replace(
        "listen = \"web\"\n",
        "listen = \"web\"\nhosts = [\"local.app.invalid\"]\n",
    );
    setup.workspace("reading-list", &manifest);
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{errors}");
    assert!(
        errors.contains(&format!(
            "mxv also serves web at https://local.app.invalid:{}",
            real.https
        )),
        "{errors}"
    );
    assert!(
        errors.contains(
            "note: local.app.invalid does not resolve to this machine; add this line to /etc/hosts: 127.0.0.1 local.app.invalid"
        ),
        "{errors}"
    );
    assert!(
        real.wait_for("https", "local.app.invalid", "200")
            .contains("hello-from-web")
    );
    let (exit, text, _) = setup.text(&["proxy", "check"]);
    assert_eq!(exit, 1, "{text}");
    assert!(
        text.contains("local.app.invalid does not resolve"),
        "{text}"
    );
    assert!(
        text.contains("add this line to /etc/hosts: 127.0.0.1 local.app.invalid"),
        "{text}"
    );
}

#[test]
fn a_real_caddy_that_install_starts_before_any_route_exists_leaves_the_trust_to_a_person() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    let (exit, document, _) = setup.json_and_errors(&["proxy", "install"]);
    assert_eq!(exit, 1, "{document}");
    assert!(
        document
            .to_string()
            .contains("mxv::proxy::trust_needs_terminal"),
        "{document}"
    );
    let (exit, status) = setup.json(&["proxy", "status"]);
    assert_eq!(exit, 0, "{status}");
    assert!(status.to_string().contains("\"running\":true"), "{status}");
    let (_, text, _) = setup.text(&["proxy", "check"]);
    let running = text
        .lines()
        .find(|line| line.starts_with("caddy runs"))
        .unwrap();
    assert!(running.contains("ok"), "{running}");
    let authority = text
        .lines()
        .find(|line| line.starts_with("authority"))
        .unwrap();
    assert!(!authority.contains("not created yet"), "{authority}");
    let before = caddy_pid(&setup);
    let (exit, _, errors) = setup.json_and_errors(&["proxy", "install"]);
    assert_eq!(
        exit, 1,
        "a second install finds Caddy running and asks again"
    );
    assert!(errors.contains("Caddy already runs"), "{errors}");
    assert_eq!(
        caddy_pid(&setup),
        before,
        "a second install leaves the same Caddy running"
    );
    setup.json(&["proxy", "stop"]);
}

/// Returns the process id that `mxv proxy status` reports for Caddy.
fn caddy_pid(setup: &Setup) -> u64 {
    fn find(value: &Value) -> Option<u64> {
        match value {
            Value::Object(map) => map
                .get("pid")
                .and_then(Value::as_u64)
                .or_else(|| map.values().find_map(find)),
            Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    let (_, status) = setup.json(&["proxy", "status"]);
    find(&status).unwrap_or_else(|| panic!("Caddy has no pid: {status}"))
}

#[test]
fn a_real_caddy_that_install_started_stops_when_a_sync_finds_no_route() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("plain", "[processes.idle]\nargv = [\"sleep\", \"60\"]\n");
    let (exit, _, _) = setup.json_and_errors(&["proxy", "install"]);
    assert_eq!(exit, 1);
    let (_, status) = setup.json(&["proxy", "status"]);
    assert!(status.to_string().contains("\"running\":true"), "{status}");
    let (exit, _, errors) = setup.json_and_errors(&["down", "-w", "plain"]);
    assert_eq!(exit, 0, "{errors}");
    let (_, status) = setup.json(&["proxy", "status"]);
    assert!(
        status.to_string().contains("\"running\":false"),
        "a sync with no route stops the route-free Caddy: {status}"
    );
}

#[test]
fn a_real_caddy_that_holds_a_route_the_records_no_longer_declare_fails_the_check() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("reading-list", &site());
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{errors}");
    let (_, text, _) = setup.text(&["proxy", "check"]);
    let running = text
        .lines()
        .find(|line| line.starts_with("caddy runs"))
        .unwrap();
    assert!(running.contains("1 route loaded"), "{running}");
    write(
        &setup.root().join("workspaces/reading-list/app/mxv.toml"),
        "[processes.idle]\nargv = [\"sleep\", \"60\"]\n",
    );
    let (_, text, _) = setup.text(&["proxy", "check"]);
    let running = text
        .lines()
        .find(|line| line.starts_with("caddy runs"))
        .unwrap();
    assert!(running.contains("failed"), "{running}");
    assert!(running.contains("mxv proxy start"), "{running}");
    setup.json(&["proxy", "stop"]);
}

#[test]
fn a_real_caddy_that_install_starts_leaves_the_trust_to_a_person_at_a_terminal() {
    let Some(real) = real() else { return };
    let setup = Setup::new();
    real.configure(&setup);
    setup.workspace("reading-list", &site());
    let (exit, _, errors) = setup.json_and_errors(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{errors}");
    let (exit, _) = setup.json(&["proxy", "stop"]);
    assert_eq!(exit, 0);
    let (exit, document, _) = setup.json_and_errors(&["proxy", "install"]);
    assert_eq!(exit, 1, "{document}");
    assert!(
        document
            .to_string()
            .contains("mxv::proxy::trust_needs_terminal"),
        "{document}"
    );
    assert!(
        document.to_string().contains("mxv proxy install"),
        "{document}"
    );
    let (_, text, _) = setup.text(&["proxy", "check"]);
    let running = text
        .lines()
        .find(|line| line.starts_with("caddy runs"))
        .unwrap();
    assert!(running.contains("ok"), "{running}");
}
