//! End-to-end tests of the containers of a manifest: `up`, `down`,
//! `status`, `logs` and `remove` with `[containers.<name>]`. The tests that
//! start a container use the `busybox:1.37.0` image and skip, with a line
//! that says so, when no docker daemon answers. Each test names its
//! workspace after itself, because containers and volumes live in one
//! docker daemon that tests running at once share.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};

/// The image of the tests, pinned by its tag.
const IMAGE: &str = "busybox:1.37.0";

/// Writes `text` to the file at `path`, as a person working in a checkout
/// would.
fn write(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, text).unwrap();
}

/// Runs `docker <args>` and returns its exit status and standard output.
fn docker(args: &[&str]) -> (bool, String) {
    match Command::new("docker").args(args).env("PATH", PATH).output() {
        Ok(output) => (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ),
        Err(_) => (false, String::new()),
    }
}

/// Reports whether a docker daemon answers, and says that the test skips
/// when none does.
fn docker_runs(test: &str) -> bool {
    let (answered, version) = docker(&["version", "--format", "{{.Server.Version}}"]);
    let runs = answered && !version.is_empty();
    if !runs {
        println!("skipping {test}: no docker daemon answers");
    }
    runs
}

/// Reports whether a `docker` program is on `PATH`.
fn docker_installed(test: &str) -> bool {
    let installed = Command::new("docker")
        .arg("--version")
        .env("PATH", PATH)
        .output()
        .is_ok_and(|output| output.status.success());
    if !installed {
        println!("skipping {test}: docker is not installed");
    }
    installed
}

/// A temporary directory with a repository, a workspace root and a state
/// directory, and one workspace whose containers and volumes it removes
/// when it drops.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The workspace's name.
    name: String,
}

impl Setup {
    /// Creates the workspace `name` with `manifest` as its `mxv.toml`.
    fn new(name: &str, manifest: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        repository(&dir.path().canonicalize().unwrap());
        let setup = Self {
            dir,
            name: name.to_owned(),
        };
        setup.clean_docker();
        // The tests of other files allocate from the default range at the
        // same time, each with records of its own, so these tests take
        // ports from a range that no other test uses.
        let config = setup.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write(&config, "ports_range = [31000, 31999]\n");
        let (exit, document) = setup.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        write(&setup.checkout().join("mxv.toml"), manifest);
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn checkout(&self) -> PathBuf {
        self.root().join("workspaces").join(&self.name).join("app")
    }

    fn command(&self, path: &str, args: &[&str]) -> Command {
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", path)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json_with(&self, command: Command, args: &[&str]) -> (i32, Value) {
        let output = command.arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        self.json_with(self.command(PATH, args), args)
    }

    /// Returns the host port that the workspace holds for `port`.
    fn port(&self, port: &str) -> u16 {
        let (_, shown) = self.json(&["show", &self.name]);
        u16::try_from(shown["data"][0]["ports"][port].as_u64().unwrap()).unwrap()
    }

    /// Returns the names of the workspace's containers that docker lists.
    fn containers(&self) -> Vec<String> {
        let filter = format!("label=mxv.workspace={}", self.name);
        let (_, printed) = docker(&["ps", "--all", "--filter", &filter, "--format", "{{.Names}}"]);
        printed.lines().map(str::to_owned).collect()
    }

    /// Returns the names of the workspace's volumes that docker lists.
    fn volumes(&self) -> Vec<String> {
        let filter = format!("label=mxv.workspace={}", self.name);
        let (_, printed) = docker(&["volume", "ls", "--quiet", "--filter", &filter]);
        printed.lines().map(str::to_owned).collect()
    }

    /// Removes the containers and volumes of the workspace.
    fn clean_docker(&self) {
        for container in self.containers() {
            let _ = docker(&["rm", "--force", &container]);
        }
        for volume in self.volumes() {
            let _ = docker(&["volume", "rm", &volume]);
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.clean_docker();
    }
}

/// Returns the row of `name` in `document`.
fn row<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {document}"))
}

/// A manifest whose container `web` prints a line and serves HTTP on the
/// port `web`, with the volume `srv`. The container's shell is its first
/// process, which ignores SIGTERM without a trap, so the trap lets `down`
/// stop it at once rather than after the grace.
fn web() -> String {
    format!(
        r#"ports = ["web"]

[containers.web]
image = "{IMAGE}"
command = ["sh", "-c", "echo hello from the container; mkdir -p /srv; trap 'exit 0' TERM; httpd -f -p 80 -h /srv & wait"]
ports = {{ 80 = "web" }}
volumes = {{ srv = "/srv" }}
ready = {{ timeout_seconds = 60 }}
"#
    )
}

#[test]
fn up_runs_a_container_on_the_loopback_address_and_down_removes_it_and_keeps_its_volume() {
    if !docker_runs("up_runs_a_container") {
        return;
    }
    let setup = Setup::new("mxvtest-up-down", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-up-down"]);
    assert_eq!(exit, 0, "{document}");
    let web = row(&document, "web");
    assert_eq!(web["kind"], "container");
    assert_eq!(web["state"], "running");
    assert_eq!(web["pid"], Value::Null);
    let port = setup.port("web");
    assert_eq!(web["ports"][0], format!("web={port}"));
    assert_eq!(setup.containers(), ["mxv-mxvtest-up-down.web"]);
    let (_, bindings) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{json .HostConfig.PortBindings}}",
        "mxv-mxvtest-up-down.web",
    ]);
    assert!(
        bindings.contains(&format!("\"HostIp\":\"127.0.0.1\",\"HostPort\":\"{port}\"")),
        "{bindings}"
    );
    let mut stream = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    assert!(reply.starts_with("HTTP/1."), "{reply}");

    let (_, status) = setup.json(&["status", "-w", "mxvtest-up-down"]);
    assert_eq!(row(&status, "web")["state"], "running");
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-up-down"]);
    assert_eq!(
        exit, 0,
        "a second up skips the running container: {document}"
    );

    let (exit, document) = setup.json(&["down", "-w", "mxvtest-up-down"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "web")["state"], "stopped");
    assert!(setup.containers().is_empty());
    assert_eq!(setup.volumes(), ["mxv-mxvtest-up-down.srv"]);
    let (_, logs) = setup.json(&["logs", "-w", "mxvtest-up-down", "web"]);
    assert!(
        logs.to_string().contains("hello from the container"),
        "{logs}"
    );
}

#[test]
fn a_process_that_depends_on_a_container_starts_after_it_and_receives_its_port() {
    if !docker_runs("a_process_that_depends_on_a_container") {
        return;
    }
    let manifest = web().replace("ports = [\"web\"]", "ports = [\"web\", \"api\"]")
        + "\n[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\nports = { WEB_PORT = \"web\" }\ndepends_on = [\"web\"]\n";
    let setup = Setup::new("mxvtest-depends", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-depends", "api"]);
    assert_eq!(exit, 0, "{document}");
    let names: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["web", "api"]);
    let port = setup.port("web");
    assert_eq!(
        row(&document, "api")["ports"][0],
        format!("WEB_PORT={port}")
    );
    let (exit, document) = setup.json(&["down", "-w", "mxvtest-depends"]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.containers().is_empty());
}

#[test]
fn a_container_that_exits_before_it_is_ready_fails_up_with_its_exit_code() {
    if !docker_runs("a_container_that_exits_before_it_is_ready") {
        return;
    }
    let manifest = format!(
        "ports = [\"web\"]\n\n[containers.web]\nimage = \"{IMAGE}\"\ncommand = [\"sh\", \"-c\", \"echo cannot serve; exit 3\"]\nports = {{ 80 = \"web\" }}\n"
    );
    let setup = Setup::new("mxvtest-exits", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-exits"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::exited");
    assert_eq!(document["fields"]["process"], "web");
    assert_eq!(document["fields"]["code"], 3);
    let (_, status) = setup.json(&["status", "-w", "mxvtest-exits"]);
    assert_eq!(row(&status, "web")["state"], "exited");
    assert_eq!(row(&status, "web")["code"], 3);
}

#[test]
fn remove_refuses_a_workspace_with_volumes_until_delete_volumes_allows_deleting_them() {
    if !docker_runs("remove_refuses_a_workspace_with_volumes") {
        return;
    }
    let setup = Setup::new("mxvtest-volumes", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-volumes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["down", "-w", "mxvtest-volumes"]);
    assert_eq!(exit, 0, "{document}");

    let (exit, document) = setup.json(&["remove", "mxvtest-volumes", "--yes", "--force"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::volumes");
    assert_eq!(document["fields"]["volumes"][0], "mxv-mxvtest-volumes.srv");
    assert_eq!(setup.volumes(), ["mxv-mxvtest-volumes.srv"]);

    let (exit, document) = setup.json(&[
        "remove",
        "mxvtest-volumes",
        "--yes",
        "--force",
        "--delete-volumes",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.volumes().is_empty());
    assert!(setup.containers().is_empty());
}

#[test]
fn remove_force_stops_a_running_container() {
    if !docker_runs("remove_force_stops_a_running_container") {
        return;
    }
    let manifest = web().replace("volumes = { srv = \"/srv\" }\n", "");
    let setup = Setup::new("mxvtest-remove-running", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-remove-running"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["remove", "mxvtest-remove-running", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::running");
    assert_eq!(document["fields"]["processes"][0], "web");
    let (exit, document) = setup.json(&["remove", "mxvtest-remove-running", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.containers().is_empty());
}

#[test]
fn up_with_containers_fails_as_docker_unavailable_when_the_daemon_does_not_answer() {
    if !docker_installed("up_with_containers_fails_as_docker_unavailable") {
        return;
    }
    let setup = Setup::new("mxvtest-unavailable", &web());
    let socket = setup.root().join("no-daemon.sock");
    let args = ["up", "-w", "mxvtest-unavailable"];
    let command = setup
        .command(PATH, &args)
        .env("DOCKER_HOST", format!("unix://{}", socket.display()));
    let (exit, document) = setup.json_with(command, &args);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::docker::unavailable");
}

#[test]
fn up_with_containers_fails_as_docker_not_found_when_docker_is_not_on_path() {
    let setup = Setup::new("mxvtest-no-docker", &web());
    let empty = setup.root().join("empty-bin");
    fs::create_dir(&empty).unwrap();
    let args = ["up", "-w", "mxvtest-no-docker"];
    let command = setup.command(&empty.display().to_string(), &args);
    let (exit, document) = setup.json_with(command, &args);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::docker::not_found");
}

#[test]
fn up_waits_until_the_container_answers_even_though_docker_accepts_connections_first() {
    if !docker_runs("up_waits_until_the_container_answers") {
        return;
    }
    let manifest = web().replace("echo hello from the container;", "sleep 3;");
    let setup = Setup::new("mxvtest-slow", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-slow"]);
    assert_eq!(exit, 0, "{document}");
    let mut stream =
        TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, setup.port("web")))).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut reply = String::new();
    let _ = stream.read_to_string(&mut reply);
    assert!(reply.starts_with("HTTP/1."), "{reply}");
}

#[test]
fn a_container_that_misses_its_readiness_timeout_fails_up_and_is_removed() {
    if !docker_runs("a_container_that_misses_its_readiness_timeout") {
        return;
    }
    let manifest = format!(
        "ports = [\"web\"]\n\n[containers.web]\nimage = \"{IMAGE}\"\ncommand = [\"sh\", \"-c\", \"trap 'exit 0' TERM; sleep 60 & wait\"]\nports = {{ 80 = \"web\" }}\nready = {{ timeout_seconds = 2 }}\n"
    );
    let setup = Setup::new("mxvtest-not-ready", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-not-ready"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::not_ready");
    assert!(setup.containers().is_empty());
}
