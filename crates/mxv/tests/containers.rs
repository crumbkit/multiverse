//! End-to-end tests of the containers of a manifest: `up`, `down`,
//! `status`, `logs` and `remove` with `[containers.<name>]`. The tests that
//! start a container use the `busybox:1.37.0` image and skip, with a line
//! that says so, when no docker daemon answers. Each test's setup has a
//! state directory of its own, so its store id labels and names its
//! containers and volumes, and tests that run at once in one docker daemon,
//! from one checkout or two, never touch each other's.

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

    /// Returns the store id of the setup's state directory, or `None`
    /// before its first container started.
    fn store_id(&self) -> Option<String> {
        let text = fs::read_to_string(self.root().join("state/mxv/runs/.store-id")).ok()?;
        let record: Value = serde_json::from_str(&text).unwrap();
        Some(record["id"].as_str().unwrap().to_owned())
    }

    /// Returns the name of the workspace's Compose project,
    /// `mxv-<id8>-<workspace>`, where the workspace's `_` reads `__` and
    /// its `.` reads `_d`.
    fn project(&self) -> String {
        let id = self.store_id().unwrap();
        let workspace = self.name.replace('_', "__").replace('.', "_d");
        format!("mxv-{}-{workspace}", &id[..8])
    }

    /// Returns the docker name of the container or volume `object` of the
    /// workspace.
    fn docker_name(&self, object: &str) -> String {
        format!("{}.{object}", self.project())
    }

    /// Returns the `docker ps` and `docker volume ls` filters of the
    /// setup's store and workspace.
    fn filters(&self) -> Option<[String; 4]> {
        let id = self.store_id()?;
        Some([
            "--filter".to_owned(),
            format!("label=mxv.store={id}"),
            "--filter".to_owned(),
            format!("label=mxv.workspace={}", self.name),
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

    /// Returns the volumes that the run records name.
    fn anonymous_volumes(&self) -> Vec<String> {
        let path = self
            .root()
            .join(format!("state/mxv/runs/{}/.volumes", self.name));
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|record| serde_json::from_value(record["volumes"].clone()).ok())
            .unwrap_or_default()
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

    /// Removes the containers, volumes and networks that carry the setup's
    /// store id, and the anonymous volumes that `down` recorded.
    fn clean_docker(&self) {
        for container in self.containers() {
            let _ = docker(&["rm", "--force", "--volumes", &container]);
        }
        for volume in self.volumes().into_iter().chain(self.anonymous_volumes()) {
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
    assert_eq!(setup.containers(), [setup.docker_name("web")]);
    let (_, bindings) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{json .HostConfig.PortBindings}}",
        &setup.docker_name("web"),
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
    assert_eq!(setup.volumes(), [setup.docker_name("srv")]);
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
    assert_eq!(document["fields"]["volumes"][0], setup.docker_name("srv"));
    assert_eq!(setup.volumes(), [setup.docker_name("srv")]);
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
    assert_ne!(first.docker_name("web"), second.docker_name("web"));

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
    assert_eq!(second.containers(), [second.docker_name("web")]);
    assert_eq!(second.volumes(), [second.docker_name("srv")]);
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
        name: setup.docker_name("web"),
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
    let name = setup.docker_name("srv");
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
    assert_eq!(setup.containers(), [setup.docker_name("web")]);
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
    assert_eq!(document["fields"]["operation"], "compose up");
    assert_eq!(document["fields"]["container"], "web");
    let message = document["fields"]["message"].as_str().unwrap();
    assert!(
        message.starts_with("docker:") || message.starts_with("Error"),
        "{message}"
    );
}

#[test]
fn status_gives_a_container_that_restarted_outside_mxv_its_log_follower_back() {
    if !docker_runs("status_gives_a_container_that_restarted_outside_mxv") {
        return;
    }
    let setup = Setup::new("mxvtest-restarted", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-restarted"]);
    assert_eq!(exit, 0, "{document}");
    let (restarted, _) = docker(&["restart", "--time", "1", &setup.docker_name("web")]);
    assert!(restarted);
    let log = setup
        .root()
        .join("state/mxv/runs/mxvtest-restarted/web.log");
    let deadline = Instant::now() + MOST_WAIT;
    while Instant::now() < deadline {
        setup.json(&["status", "-w", "mxvtest-restarted"]);
        let text = fs::read_to_string(&log).unwrap_or_default();
        if text.matches("hello from the container").count() == 2 {
            return;
        }
    }
    panic!(
        "the log holds no line of the second start: {}",
        fs::read_to_string(&log).unwrap_or_default()
    );
}

#[test]
fn down_keeps_a_containers_anonymous_volumes_until_remove_delete_volumes() {
    if !docker_runs("down_keeps_a_containers_anonymous_volumes") {
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
        &setup.docker_name("web"),
    ]);
    assert!(!mounts.is_empty());

    let (exit, document) = setup.json(&[
        "remove",
        "mxvtest-anonymous",
        "--force",
        "--delete-volumes",
        "--dry-run",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["containers"], serde_json::json!(["web"]));
    assert_eq!(document["data"]["volumes"], serde_json::json!([mounts]));

    let (exit, document) = setup.json(&["down", "-w", "mxvtest-anonymous"]);
    assert_eq!(exit, 0, "{document}");
    let (kept, _) = docker(&["volume", "inspect", &mounts]);
    assert!(kept, "down deleted the anonymous volume");
    let (exit, document) = setup.json(&["remove", "mxvtest-anonymous", "--yes", "--force"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::volumes");
    assert_eq!(document["fields"]["volumes"][0], mounts.as_str());
    let (exit, document) = setup.json(&[
        "remove",
        "mxvtest-anonymous",
        "--yes",
        "--force",
        "--delete-volumes",
    ]);
    assert_eq!(exit, 0, "{document}");
    let (kept, _) = docker(&["volume", "inspect", &mounts]);
    assert!(!kept, "remove --delete-volumes kept the anonymous volume");
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
    let (_, status) = setup.json(&["status", "-w", "mxvtest-137"]);
    assert_eq!(row(&status, "web")["code"], 137, "{status}");
    assert_eq!(row(&status, "web")["signal"], Value::Null, "{status}");
}

#[test]
fn up_runs_the_containers_as_one_compose_project_whose_file_names_the_variables_only() {
    if !docker_runs("up_runs_the_containers_as_one_compose_project") {
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
        &setup.docker_name("web"),
    ]);
    assert_eq!(labels, format!("{} web", setup.project()));
    let (_, env) = docker(&[
        "container",
        "inspect",
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
        &setup.docker_name("web"),
    ]);
    assert!(
        env.lines().any(|line| line == "TOKEN=t0ken-value-$HOME"),
        "{env}"
    );
    let state = setup.root().join("state");
    let compose = setup
        .root()
        .join("state/mxv/runs/mxvtest-compose/compose.yaml");
    let text = fs::read_to_string(&compose).unwrap();
    assert!(text.contains("\"TOKEN\""), "{text}");
    assert!(!text.contains("t0ken-value"), "{text}");
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
        &setup.docker_name("web"),
    ]);
    assert_eq!(mode, "bridge");
    let (_, other) = docker(&[
        "network",
        "ls",
        "--quiet",
        "--filter",
        &format!("name={}", setup.project()),
    ]);
    assert!(other.is_empty(), "compose made a network: {other}");
    assert_eq!(setup.volumes(), [setup.docker_name("srv")]);
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
fn remove_delete_volumes_leaves_a_recorded_volume_that_is_neither_named_by_mxv_nor_anonymous() {
    if !docker_runs("remove_delete_volumes_leaves_a_recorded_volume") {
        return;
    }
    let setup = Setup::new("mxvtest-plain-volume", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-plain-volume"]);
    assert_eq!(exit, 0, "{document}");
    setup.json(&["down", "-w", "mxvtest-plain-volume"]);
    let foreign = Foreign {
        kind: "volume",
        name: format!("mxvtest-plain-{}", std::process::id()),
    };
    let (created, _) = docker(&["volume", "create", &foreign.name]);
    assert!(created);
    let record = setup
        .root()
        .join("state/mxv/runs/mxvtest-plain-volume/.volumes");
    let mut volumes: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    volumes["volumes"]
        .as_array_mut()
        .unwrap()
        .push(Value::String(foreign.name.clone()));
    write(&record, &volumes.to_string());

    let (exit, document) = setup.json(&[
        "remove",
        "mxvtest-plain-volume",
        "--yes",
        "--force",
        "--delete-volumes",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["volumes"],
        serde_json::json!([setup.docker_name("srv")]),
        "{document}"
    );
    let (exists, _) = docker(&["volume", "inspect", &foreign.name]);
    assert!(exists, "mxv deleted a volume that is not anonymous");
}

#[test]
fn logs_follow_gives_a_running_container_its_follower_back() {
    if !docker_runs("logs_follow_gives_a_running_container_its_follower_back") {
        return;
    }
    let setup = Setup::new("mxvtest-follow", &web());
    let (exit, document) = setup.json(&["up", "-w", "mxvtest-follow"]);
    assert_eq!(exit, 0, "{document}");
    let record = setup
        .root()
        .join("state/mxv/runs/mxvtest-follow/web.follower");
    let follower = || -> u64 {
        let text = fs::read_to_string(&record).unwrap_or_default();
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|record| record["pid"].as_u64())
            .unwrap_or_default()
    };
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
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv logs -f in the background and stops it"
    )]
    let mut child = logs.spawn().unwrap();
    // `logs -f` runs for a while before the follower ends, so the follower
    // ends while it follows the log.
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        std::thread::yield_now();
    }
    let old = follower();
    assert_ne!(old, 0, "up recorded no follower");
    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{old}")])
        .env("PATH", PATH)
        .output()
        .unwrap()
        .status
        .success();
    assert!(killed);
    let deadline = Instant::now() + MOST_WAIT;
    let mut restarted = false;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("logs -f ended with {status} while the container runs");
        }
        let new = follower();
        if new != 0 && new != old {
            restarted = true;
            break;
        }
        std::thread::yield_now();
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(restarted, "logs -f did not start a new follower");
}
