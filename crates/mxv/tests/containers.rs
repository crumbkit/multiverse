//! End-to-end tests of the containers of a manifest: `up`, `down`,
//! `status`, `logs` and `remove` with `[containers.<name>]`. The tests that
//! start a container use the `busybox:1.37.0` image and skip, with a line
//! that says so, when no docker daemon answers. Each test's workspace has a
//! stable id of its own, which labels and names its containers and
//! volumes, so tests that run at once in one docker daemon, from one
//! checkout or two, never touch each other's.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};
use crate::test_ports::PortBlock;

/// The image of the tests, pinned by its tag.
const IMAGE: &str = "busybox:1.37.0";

/// How long a test waits for a container's log to change.
const MOST_WAIT: Duration = Duration::from_secs(30);

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

/// Returns docker's configuration directory as the person who runs the
/// tests has it, which holds the CLI plugins that a per-user install of
/// docker, such as Docker Desktop's, puts there. mxv runs with a
/// temporary `HOME`, so without it docker would find no `compose` plugin.
#[expect(
    clippy::disallowed_methods,
    reason = "the test hands mxv the docker configuration of the person who runs it"
)]
fn docker_config() -> PathBuf {
    std::env::var_os("DOCKER_CONFIG").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".docker"),
        PathBuf::from,
    )
}

/// A temporary directory with a repository, a workspace root and a state
/// directory, and one workspace whose containers and volumes it removes
/// when it drops. It finds them only by its own store id, so it never
/// removes another setup's.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The workspace's name.
    name: String,
    /// The block of host ports that the state directory allocates from.
    ports: PortBlock,
}

impl Setup {
    /// Creates the workspace `name` with `manifest` as its `mxv.toml`, in
    /// a state directory that allocates host ports from a block that no
    /// other running test, in this checkout or another, uses.
    fn new(name: &str, manifest: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        repository(&dir.path().canonicalize().unwrap());
        let setup = Self {
            dir,
            name: name.to_owned(),
            ports: PortBlock::claim(),
        };
        let config = setup.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write(&config, &setup.ports.config_line());
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
            .env("DOCKER_CONFIG", docker_config())
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

    /// Runs `mxv` with `args` and `--json` against a docker whose daemon
    /// does not answer.
    fn json_without_daemon(&self, args: &[&str]) -> (i32, Value) {
        let socket = self.root().join("no-daemon.sock");
        let command = self
            .command(PATH, args)
            .env("DOCKER_HOST", format!("unix://{}", socket.display()));
        self.json_with(command, args)
    }

    /// Returns the host port that the workspace holds for `port`.
    fn port(&self, port: &str) -> u16 {
        let (_, shown) = self.json(&["show", &self.name]);
        u16::try_from(shown["data"][0]["ports"][port].as_u64().unwrap()).unwrap()
    }

    /// Returns the workspace's stable id, or `None` before its first `up`.
    fn workspace_id(&self) -> Option<String> {
        let text = fs::read_to_string(
            self.root()
                .join(format!("state/mxv/workspaces/{}.json", self.name)),
        )
        .ok()?;
        let record: Value = serde_json::from_str(&text).unwrap();
        Some(record["id"].as_str()?.to_owned())
    }

    /// Returns the 16 hex digits of phase's hash of the workspace's stable
    /// id, which its docker names hold.
    fn hash(&self) -> String {
        let id = self.workspace_id().unwrap();
        let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("{hash:016x}")
    }

    /// Returns the name of the compose project of the container `app`,
    /// `phase-<hash>-<app>`.
    fn project(&self, app: &str) -> String {
        format!("phase-{}-{app}", self.hash())
    }

    /// Returns the docker name of the container `app`, whose one process
    /// is `main`.
    fn container_name(&self, app: &str) -> String {
        format!("{}.main", self.project(app))
    }

    /// Returns the docker name of the workspace's volume `volume`.
    fn volume_name(&self, volume: &str) -> String {
        format!("phase-{}.{volume}", self.hash())
    }

    /// Returns the `docker ps` and `docker volume ls` filters of the
    /// workspace's stable id.
    fn filters(&self) -> Option<[String; 2]> {
        let id = self.workspace_id()?;
        Some([
            "--filter".to_owned(),
            format!("label=mxv.workspace-id={id}"),
        ])
    }

    /// Returns the names of the workspace's containers that docker lists.
    fn containers(&self) -> Vec<String> {
        let Some(filters) = self.filters() else {
            return Vec::new();
        };
        let mut args = vec!["ps", "--all", "--format", "{{.Names}}"];
        args.extend(filters.iter().map(String::as_str));
        let (_, printed) = docker(&args);
        printed.lines().map(str::to_owned).collect()
    }

    /// Returns the names of the workspace's volumes that docker lists.
    fn volumes(&self) -> Vec<String> {
        let Some(filters) = self.filters() else {
            return Vec::new();
        };
        let mut args = vec!["volume", "ls", "--quiet"];
        args.extend(filters.iter().map(String::as_str));
        let (_, printed) = docker(&args);
        printed.lines().map(str::to_owned).collect()
    }

    /// Returns the names of the workspace's networks that docker lists.
    fn networks(&self) -> Vec<String> {
        let Some(filters) = self.filters() else {
            return Vec::new();
        };
        let mut args = vec!["network", "ls", "--format", "{{.Name}}"];
        args.extend(filters.iter().map(String::as_str));
        let (_, printed) = docker(&args);
        printed.lines().map(str::to_owned).collect()
    }

    /// Removes the containers, volumes and networks that carry the
    /// workspace's stable id, and with each container its anonymous
    /// volumes.
    fn clean_docker(&self) {
        for container in self.containers() {
            let _ = docker(&["rm", "--force", "--volumes", &container]);
        }
        for volume in self.volumes() {
            let _ = docker(&["volume", "rm", &volume]);
        }
        for network in self.networks() {
            let _ = docker(&["network", "rm", &network]);
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.clean_docker();
    }
}

/// Removes the docker container or volume `name` when it drops, for an
/// object that a test made without mxv's labels.
struct Foreign {
    /// `container` or `volume`.
    kind: &'static str,
    /// Its docker name.
    name: String,
}

impl Drop for Foreign {
    fn drop(&mut self) {
        if self.kind == "container" {
            let _ = docker(&["rm", "--force", &self.name]);
        } else {
            let _ = docker(&["volume", "rm", &self.name]);
        }
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
    assert_eq!(setup.containers(), [setup.container_name("web")]);
    let (_, bindings) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{json .HostConfig.PortBindings}}",
        &setup.container_name("web"),
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

    let logs = setup
        .command(PATH, &["logs", "-w", "mxvtest-up-down", "web"])
        .output()
        .unwrap();
    let logs = String::from_utf8_lossy(&logs.stdout);
    assert!(logs.contains("hello from the container"), "{logs}");

    let (exit, document) = setup.json(&["down", "-w", "mxvtest-up-down"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "web")["state"], "stopped");
    assert!(setup.containers().is_empty());
    assert_eq!(setup.volumes(), [setup.volume_name("srv")]);
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
    // A failed up removes what it started, so the container is stopped.
    let (_, status) = setup.json(&["status", "-w", "mxvtest-exits"]);
    assert_eq!(row(&status, "web")["state"], "stopped");
    assert!(setup.containers().is_empty());
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
    assert_eq!(document["fields"]["volumes"][0], setup.volume_name("srv"));
    assert_eq!(setup.volumes(), [setup.volume_name("srv")]);
    let hint = document["recovery"]["text"].as_str().unwrap();
    assert!(
        hint.contains("mxv remove mxvtest-volumes --force --delete-volumes --yes"),
        "the recovery keeps --force: {hint}"
    );

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
    let (exit, document) = setup.json_without_daemon(&["up", "-w", "mxvtest-unavailable"]);
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

#[test]
fn two_state_directories_with_one_workspace_name_never_touch_each_others_containers_or_volumes() {
    if !docker_runs("two_state_directories_with_one_workspace_name") {
        return;
    }
    let first = Setup::new("mxvtest-twin", &web());
    let second = Setup::new("mxvtest-twin", &web());
    for setup in [&first, &second] {
        let (exit, document) = setup.json(&["up", "-w", "mxvtest-twin"]);
        assert_eq!(exit, 0, "{document}");
    }
    assert_ne!(first.container_name("web"), second.container_name("web"));

    let (exit, document) = first.json(&["down", "-w", "mxvtest-twin"]);
    assert_eq!(exit, 0, "{document}");
    assert!(first.containers().is_empty());
    let (_, status) = second.json(&["status", "-w", "mxvtest-twin"]);
    assert_eq!(row(&status, "web")["state"], "running", "{status}");

    let (exit, document) = first.json(&[
        "remove",
        "mxvtest-twin",
        "--yes",
        "--force",
        "--delete-volumes",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert!(first.volumes().is_empty());
    assert_eq!(second.containers(), [second.container_name("web")]);
    assert_eq!(second.volumes(), [second.volume_name("srv")]);
}

#[test]
fn up_leaves_alone_a_container_with_its_name_that_lacks_its_labels() {
    if !docker_runs("up_leaves_alone_a_container_with_its_name") {
        return;
    }
    let setup = Setup::new("mxvtest-foreign", &web());
    setup.json(&["up", "-w", "mxvtest-foreign"]);
    setup.json(&["down", "-w", "mxvtest-foreign"]);
    let foreign = Foreign {
        kind: "container",
        name: setup.container_name("web"),
    };
    let (created, _) = docker(&["create", "--name", &foreign.name, IMAGE, "true"]);
    assert!(created);

    let (exit, document) = setup.json(&["up", "-w", "mxvtest-foreign"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::docker::foreign_object",
        "{document}"
    );
    assert_eq!(document["fields"]["kind"], "container");
    assert_eq!(document["fields"]["name"], foreign.name);
    let (exit, document) = setup.json(&["down", "-w", "mxvtest-foreign", "web"]);
    assert_eq!(exit, 0, "{document}");
    let (exists, _) = docker(&["container", "inspect", &foreign.name]);
    assert!(exists, "mxv removed a container that it did not make");
}

#[test]
fn up_leaves_alone_a_volume_with_its_name_that_lacks_its_labels() {
    if !docker_runs("up_leaves_alone_a_volume_with_its_name") {
        return;
    }
    let setup = Setup::new("mxvtest-foreign-volume", &web());
    setup.json(&["up", "-w", "mxvtest-foreign-volume"]);
    setup.json(&["down", "-w", "mxvtest-foreign-volume"]);
    let name = setup.volume_name("srv");
    let (removed, _) = docker(&["volume", "rm", &name]);
    assert!(removed);
    let foreign = Foreign {
        kind: "volume",
        name,
    };
    let (created, _) = docker(&["volume", "create", &foreign.name]);
    assert!(created);

    let (exit, document) = setup.json(&["up", "-w", "mxvtest-foreign-volume"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::docker::foreign_object",
        "{document}"
    );
    assert_eq!(document["fields"]["kind"], "volume");
    assert!(setup.containers().is_empty());
}

#[test]
fn down_stops_a_running_container_that_the_manifest_no_longer_declares() {
    if !docker_runs("down_stops_a_running_container_that_the_manifest_no_longer_declares") {
        return;
    }
    let setup = Setup::new("mxvtest-dropped", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-dropped"]);
    assert_eq!(exit, 0, "{document}");
    write(
        &setup.checkout().join("mxv.toml"),
        "[processes.idle]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n",
    );
    let (_, status) = setup.json(&["status", "-w", "mxvtest-dropped"]);
    assert_eq!(row(&status, "web")["state"], "running", "{status}");
    assert_eq!(row(&status, "web")["kind"], "container", "{status}");
    let (exit, document) = setup.json(&["down", "-w", "mxvtest-dropped"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "web")["state"], "stopped");
    assert!(setup.containers().is_empty());
}

#[test]
fn down_and_remove_fail_as_docker_unavailable_once_the_workspace_has_run_a_container() {
    if !docker_runs("down_and_remove_fail_as_docker_unavailable") {
        return;
    }
    let setup = Setup::new("mxvtest-gone-daemon", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-gone-daemon"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json_without_daemon(&["down", "-w", "mxvtest-gone-daemon"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::docker::unavailable", "{document}");
    let (exit, document) = setup.json_without_daemon(&[
        "remove",
        "mxvtest-gone-daemon",
        "--yes",
        "--force",
        "--delete-volumes",
    ]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::docker::unavailable", "{document}");
    assert_eq!(setup.containers(), [setup.container_name("web")]);
}

#[test]
fn status_reports_unknown_and_down_and_remove_fail_as_docker_unavailable_when_dockers_context_is_unknown()
 {
    if !docker_runs("status_reports_unknown_and_down_and_remove_fail") {
        return;
    }
    let setup = Setup::new("mxvtest-no-context", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-no-context"]);
    assert_eq!(exit, 0, "{document}");
    // docker says `context "...": context not found` for an unknown
    // context. That is no answer from the daemon, so mxv never reads it as
    // a container that is gone. `status` reports the container's state as
    // unknown, and `down` and `remove` fail.
    let args = ["status", "-w", "mxvtest-no-context"];
    let command = setup
        .command(PATH, &args)
        .env("DOCKER_CONTEXT", "mxvtest-no-such-context");
    let (exit, document) = setup.json_with(command, &args);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "web")["state"], "unknown", "{document}");
    for args in [
        &["down", "-w", "mxvtest-no-context"][..],
        &[
            "remove",
            "mxvtest-no-context",
            "--yes",
            "--force",
            "--delete-volumes",
        ],
    ] {
        let command = setup
            .command(PATH, args)
            .env("DOCKER_CONTEXT", "mxvtest-no-such-context");
        let (exit, document) = setup.json_with(command, args);
        assert_eq!(exit, 1, "{args:?}: {document}");
        assert_eq!(
            document["code"], "mxv::docker::unavailable",
            "{args:?}: {document}"
        );
    }
    assert_eq!(setup.containers(), [setup.container_name("web")]);
    let (_, status) = setup.json(&["status", "-w", "mxvtest-no-context"]);
    assert_eq!(row(&status, "web")["state"], "running", "{status}");
}

#[test]
fn a_workspace_that_has_never_run_a_container_never_runs_docker() {
    let setup = Setup::new("mxvtest-no-calls", &web());
    let bin = setup.root().join("fake-bin");
    fs::create_dir(&bin).unwrap();
    let calls = setup.root().join("docker-calls");
    let fake = bin.join("docker");
    write(
        &fake,
        &format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", calls.display()),
    );
    let made = Command::new("chmod")
        .args(["+x", &fake.display().to_string()])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(made.status.success());
    let path = format!("{}:{PATH}", bin.display());
    let logs = setup
        .command(&path, &["logs", "-w", "mxvtest-no-calls", "web"])
        .output()
        .unwrap();
    assert!(logs.status.success(), "{logs:?}");
    for args in [
        &["status", "-w", "mxvtest-no-calls"][..],
        &["down", "-w", "mxvtest-no-calls"],
        &["remove", "mxvtest-no-calls", "--yes", "--force"],
    ] {
        let (exit, document) = setup.json_with(setup.command(&path, args), args);
        assert_eq!(exit, 0, "{args:?}: {document}");
    }
    assert!(
        !calls.exists(),
        "mxv ran docker: {}",
        fs::read_to_string(&calls).unwrap_or_default()
    );
}

#[test]
fn a_failed_docker_run_names_the_container_and_dockers_own_message() {
    if !docker_runs("a_failed_docker_run_names_the_container") {
        return;
    }
    let manifest = web().replace(IMAGE, "busybox:0.0.0-mxv-test-missing");
    let setup = Setup::new("mxvtest-no-image", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-no-image"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::docker::failed", "{document}");
    assert_eq!(document["fields"]["operation"], "pull");
    assert_eq!(document["fields"]["container"], "web");
    let message = document["fields"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("docker:") || message.starts_with("Error"),
        "{message}"
    );
}

#[test]
fn logs_of_a_container_that_restarted_outside_mxv_hold_both_starts() {
    if !docker_runs("logs_of_a_container_that_restarted_outside_mxv") {
        return;
    }
    let setup = Setup::new("mxvtest-restarted", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-restarted"]);
    assert_eq!(exit, 0, "{document}");
    let (restarted, _) = docker(&["restart", "--time", "1", &setup.container_name("web")]);
    assert!(restarted);
    let deadline = Instant::now() + MOST_WAIT;
    let mut logs = String::new();
    while Instant::now() < deadline {
        let output = setup
            .command(PATH, &["logs", "-w", "mxvtest-restarted", "web"])
            .output()
            .unwrap();
        logs = String::from_utf8_lossy(&output.stdout).into_owned();
        if logs.matches("hello from the container").count() == 2 {
            return;
        }
    }
    panic!("the log holds no line of the second start: {logs}");
}

#[test]
fn down_deletes_a_containers_anonymous_volumes_with_the_container() {
    if !docker_runs("down_deletes_a_containers_anonymous_volumes") {
        return;
    }
    let tag = format!("mxvtest-anonymous:{}", std::process::id());
    let built = Command::new("docker")
        .args(["build", "--quiet", "--tag", &tag, "-"])
        .env("PATH", PATH)
        .stdin(format!("FROM {IMAGE}\nVOLUME /data\n"))
        .output()
        .unwrap();
    assert!(built.status.success(), "{built:?}");
    let manifest = web()
        .replace(IMAGE, &tag)
        .replace("volumes = { srv = \"/srv\" }\n", "");
    let setup = Setup::new("mxvtest-anonymous", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-anonymous"]);
    assert_eq!(exit, 0, "{document}");
    let (_, mounts) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{range .Mounts}}{{.Name}}{{end}}",
        &setup.container_name("web"),
    ]);
    assert!(!mounts.is_empty());

    let (exit, document) = setup.json(&["down", "-w", "mxvtest-anonymous"]);
    assert_eq!(exit, 0, "{document}");
    let (kept, _) = docker(&["volume", "inspect", &mounts]);
    assert!(!kept, "down left the anonymous volume behind");
    let (exit, document) = setup.json(&["remove", "mxvtest-anonymous", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    let _ = docker(&["image", "rm", &tag]);
}

#[test]
fn a_container_that_exits_with_137_reports_code_137_and_no_signal() {
    if !docker_runs("a_container_that_exits_with_137") {
        return;
    }
    let manifest = format!(
        "ports = [\"web\"]\n\n[containers.web]\nimage = \"{IMAGE}\"\ncommand = [\"sh\", \"-c\", \"exit 137\"]\nports = {{ 80 = \"web\" }}\n"
    );
    let setup = Setup::new("mxvtest-137", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-137"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["fields"]["code"], 137, "{document}");
    assert_eq!(document["fields"]["signal"], Value::Null, "{document}");
    // A failed up removes what it started, so the container is stopped.
    let (_, status) = setup.json(&["status", "-w", "mxvtest-137"]);
    assert_eq!(row(&status, "web")["state"], "stopped", "{status}");
}

#[test]
fn up_runs_each_container_as_a_compose_project_of_its_own_and_writes_no_value_to_a_file() {
    if !docker_runs("up_runs_each_container_as_a_compose_project") {
        return;
    }
    let manifest = web().replace(
        "ports = { 80 = \"web\" }",
        "env = { TOKEN = \"t0ken-value-$HOME\" }\nports = { 80 = \"web\" }",
    );
    let setup = Setup::new("mxvtest-compose", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-compose"]);
    assert_eq!(exit, 0, "{document}");
    let (_, labels) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{index .Config.Labels \"com.docker.compose.project\"}} {{index .Config.Labels \"mxv.container\"}}",
        &setup.container_name("web"),
    ]);
    assert_eq!(labels, format!("{} main", setup.project("web")));
    let (_, env) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
        &setup.container_name("web"),
    ]);
    assert!(
        env.lines().any(|line| line == "TOKEN=t0ken-value-$HOME"),
        "{env}"
    );
    let state = setup.root().join("state");
    for file in files(&state) {
        let bytes = fs::read(&file).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("t0ken-value"),
            "{} holds the value",
            file.display()
        );
    }
    // A clone's `.git` names its branch, `mxvtest-compose`, in its refs, so
    // only the files outside `.git` count.
    let git_dir = setup.checkout().join(".git");
    assert!(!files(&setup.checkout()).iter().any(|file| {
        !file.starts_with(&git_dir)
            && file
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains("compose"))
    }));
    assert!(setup.networks().is_empty(), "{:?}", setup.networks());
    let (_, mode) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{.HostConfig.NetworkMode}}",
        &setup.container_name("web"),
    ]);
    assert_eq!(mode, "bridge");
    let (_, other) = docker(&[
        "network",
        "ls",
        "--quiet",
        "--filter",
        &format!("name={}", setup.project("web")),
    ]);
    assert!(other.is_empty(), "compose made a network: {other}");
    assert_eq!(setup.volumes(), [setup.volume_name("srv")]);
}

/// Returns every file under `dir`, at any depth.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(files(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[test]
fn up_with_containers_fails_as_compose_missing_when_docker_has_no_compose() {
    if !docker_installed("up_with_containers_fails_as_compose_missing") {
        return;
    }
    let setup = Setup::new("mxvtest-no-compose", &web());
    let (_, real) = {
        let output = Command::new("sh")
            .args(["-c", "command -v docker"])
            .env("PATH", PATH)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
    };
    let bin = setup.root().join("fake-bin");
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("docker");
    write(
        &fake,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = compose ]; then echo \"docker: unknown command: docker compose\" >&2; exit 1; fi\nexec '{real}' \"$@\"\n"
        ),
    );
    let made = Command::new("chmod")
        .args(["+x", &fake.display().to_string()])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(made.status.success());
    let path = format!("{}:{PATH}", bin.display());
    let args = ["up", "-w", "mxvtest-no-compose"];
    let (exit, document) = setup.json_with(setup.command(&path, &args), &args);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::docker::compose_missing",
        "{document}"
    );
}

#[test]
fn logs_follow_of_a_running_container_prints_its_lines_and_keeps_following() {
    if !docker_runs("logs_follow_of_a_running_container") {
        return;
    }
    let setup = Setup::new("mxvtest-follow", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-follow"]);
    assert_eq!(exit, 0, "{document}");
    let printed = setup.root().join("followed.txt");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test opens the file that receives the output of mxv logs -f"
    )]
    let output = fs::File::create(&printed).unwrap();
    // `logs -f` runs until the test stops it, so the test spawns it.
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv logs -f in the background and stops it"
    )]
    let mut logs = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"));
    logs.args(["logs", "-w", "mxvtest-follow", "web", "-f"])
        .current_dir(setup.root())
        .env_clear()
        .env("PATH", PATH)
        .envs(GIT_ENV.iter().copied())
        .env("HOME", setup.root())
        .env("XDG_CONFIG_HOME", setup.root().join("config"))
        .env("XDG_STATE_HOME", setup.root().join("state"))
        .env("MXV_ROOT", setup.root().join("workspaces"))
        .env("DOCKER_CONFIG", docker_config())
        .stdout(output)
        .stderr(std::process::Stdio::null());
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv logs -f in the background and stops it"
    )]
    let mut child = logs.spawn().unwrap();
    let deadline = Instant::now() + MOST_WAIT;
    let mut seen = false;
    while Instant::now() < deadline {
        if fs::read_to_string(&printed)
            .unwrap_or_default()
            .contains("hello from the container")
        {
            seen = true;
            break;
        }
        std::thread::yield_now();
    }
    let still = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    assert!(seen, "logs -f printed no line of the container");
    assert!(still, "logs -f ended while the container runs");
}

/// Returns the workspace record of `setup`'s workspace, as JSON.
fn record(setup: &Setup) -> Value {
    let path = setup
        .root()
        .join(format!("state/mxv/workspaces/{}.json", setup.name));
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Returns the label `label` of the docker object `object`, which `kind`
/// such as `container` or `volume` inspects.
fn label(kind: &str, object: &str, label: &str) -> String {
    let format = format!(
        "{{{{index .{} \"{label}\"}}}}",
        if kind == "container" {
            "Config.Labels"
        } else {
            "Labels"
        }
    );
    docker(&[kind, "inspect", "--format", &format, "--", object]).1
}

#[test]
fn up_labels_containers_and_volumes_with_the_stable_id_that_the_record_holds() {
    if !docker_runs("up_labels_with_the_stable_id") {
        return;
    }
    let setup = Setup::new("mxvtest-stable-id", &web());
    let minted = record(&setup)["id"].as_str().unwrap().to_owned();
    assert_eq!(minted.len(), 32, "mxv new mints a stable id: {minted}");
    // A record that an earlier mxv wrote has no id.
    let path = setup
        .root()
        .join("state/mxv/workspaces/mxvtest-stable-id.json");
    let mut legacy = record(&setup);
    legacy.as_object_mut().unwrap().remove("id");
    write(&path, &serde_json::to_string_pretty(&legacy).unwrap());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-stable-id"]);
    assert_eq!(exit, 0, "{document}");
    let id = record(&setup)["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 32);
    let run_record = setup
        .root()
        .join(format!("state/mxv/phase/runs/{id}.json"))
        .display()
        .to_string();
    // phase labels a container with its app, the unit, and labels no
    // object with the workspace's name, which the stable id replaces.
    for (kind, object, app) in [
        ("container", setup.container_name("web"), "web"),
        ("volume", setup.volume_name("srv"), ""),
    ] {
        assert_eq!(label(kind, &object, "mxv.workspace-id"), id, "{kind}");
        assert_eq!(label(kind, &object, "mxv.app"), app, "{kind}");
        assert_eq!(label(kind, &object, "mxv.run-record"), run_record, "{kind}");
        assert_eq!(label(kind, &object, "mxv.workspace"), "", "{kind}");
    }
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-stable-id"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(record(&setup)["id"], id, "the id never changes");
    let (exit, document) = setup.json(&["down", "-w", "mxvtest-stable-id"]);
    assert_eq!(exit, 0, "{document}");
}

impl Setup {
    /// Starts the workspace with the runner that mxv used before phase,
    /// which creates its volumes under the names and labels of an earlier
    /// mxv.
    fn legacy_up(&self) {
        let state = self.root().join("state/mxv");
        let store = multiverse::Store::new(state.join("workspaces"));
        let name: multiverse::WorkspaceName = self.name.parse().unwrap();
        let mut workspace = store.read(&name).unwrap().unwrap();
        let checkout = self.checkout();
        let text = fs::read_to_string(checkout.join("mxv.toml")).unwrap();
        let manifest = multiverse::Manifest::parse(&text, &checkout.join("mxv.toml")).unwrap();
        let runs = multiverse::Runs::new(&state);
        let lock = runs.lock(&name).unwrap();
        let outcome = runs
            .legacy_up(&lock, &store, &mut workspace, &checkout, &manifest, &[])
            .unwrap();
        assert!(
            matches!(outcome, multiverse::UpOutcome::Up(_)),
            "{outcome:?}"
        );
    }
}

#[test]
fn the_commands_of_the_volume_not_moved_hint_move_the_data_so_that_up_starts_on_it() {
    if !docker_runs("the_commands_of_the_volume_not_moved_hint") {
        return;
    }
    let setup = Setup::new("mxvtest-moved", &web());
    setup.legacy_up();
    let (exit, document) = setup.json(&["down", "-w", "mxvtest-moved"]);
    assert_eq!(exit, 0, "{document}");

    let (exit, document) = setup.json(&["up", "-w", "mxvtest-moved"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::volume::not_moved", "{document}");
    let legacy = document["fields"]["legacy_volume"]
        .as_str()
        .unwrap()
        .to_owned();
    let _legacy = Foreign {
        kind: "volume",
        name: legacy.clone(),
    };
    assert_eq!(document["fields"]["phase_volume"], setup.volume_name("srv"));
    let (wrote, _) = docker(&[
        "run",
        "--rm",
        "-v",
        &format!("{legacy}:/v"),
        IMAGE,
        "sh",
        "-c",
        "echo kept > /v/marker",
    ]);
    assert!(wrote);

    // The hint names the copy as its first two commands, in backticks.
    let hint = document["recovery"]["text"].as_str().unwrap();
    let commands: Vec<&str> = hint.split('`').skip(1).step_by(2).take(2).collect();
    assert!(commands[0].starts_with("docker volume create"), "{hint}");
    for command in commands {
        let output = Command::new("sh")
            .args(["-c", command])
            .env("PATH", PATH)
            .output()
            .unwrap();
        assert!(output.status.success(), "{command}: {output:?}");
    }

    let (exit, document) = setup.json(&["up", "-w", "mxvtest-moved"]);
    assert_eq!(exit, 0, "{document}");
    let (_, marker) = docker(&["exec", &setup.container_name("web"), "cat", "/srv/marker"]);
    assert_eq!(marker, "kept");
}
