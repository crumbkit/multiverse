//! End-to-end tests of the process verbs `up`, `down`, `restart`, `status`
//! and `logs`, and of how `show` and `remove` treat a workspace's
//! processes. Each test builds a repository and a workspace of it in a
//! temporary directory, writes an `mxv.toml` into the workspace's checkout,
//! and runs the built `mxv` binary there. The processes are `sh -c`
//! fixtures and the `listen` example, a tiny TCP server that cargo builds
//! with the tests.

#![cfg(test)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/held_lock.rs"]
mod held_lock;
#[path = "support/git.rs"]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};
use crate::test_ports::PortBlock;

/// How long a test runs `mxv status` or `mxv logs` again and again while
/// it waits for a process to end or print. A count of runs is no bound,
/// because reading the process table takes far less time on Linux than on
/// macOS.
const MOST_WAIT: Duration = Duration::from_secs(30);

/// A deadband socket that no daemon listens on, so a test never waits for
/// a daemon that happens to run on the machine.
const MISSING_SOCKET: &str = "/nonexistent/deadband.sock";

/// Yields until [`MOST_WAIT`] has passed, for a loop that polls until a
/// process changes.
fn waiting() -> impl Iterator<Item = ()> {
    let deadline = Instant::now() + MOST_WAIT;
    std::iter::from_fn(move || (Instant::now() < deadline).then_some(()))
}

/// Writes `text` to the file at `path`, as a person working in a checkout
/// would.
fn write(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, text).unwrap();
}

/// Returns the path of the `listen` example that cargo built beside the
/// `mxv` binary.
fn listen() -> String {
    let mxv = Path::new(env!("CARGO_BIN_EXE_mxv"));
    let mut path = mxv.parent().unwrap().join("examples").join("listen");
    if !path.exists() {
        path = mxv.parent().unwrap().join("examples").join("listen.exe");
    }
    assert!(path.exists(), "cargo did not build {}", path.display());
    path.display().to_string()
}

/// Reports whether the process `pid` exists, by asking `kill -0` to
/// signal it.
fn process_exists(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .env("PATH", PATH)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Reports whether the process group `pid` has a member, by asking `kill -0`
/// to signal it.
fn group_exists(pid: u64) -> bool {
    Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .env("PATH", PATH)
        .output()
        .unwrap()
        .status
        .success()
}

/// A temporary directory with a repository, a workspace root and a state
/// directory, whose config file makes `mxv up` allocate from a block of
/// ports that no other running test uses.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The block of ports that the config file names.
    ports: PortBlock,
    /// Whether `up`, `restart` and `exec` run without the grant that a
    /// person would give, so they meet the refusal.
    ungranted: AtomicBool,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        repository(&dir.path().canonicalize().unwrap());
        let ports = PortBlock::claim();
        let setup = Self {
            dir,
            ports,
            ungranted: AtomicBool::new(false),
        };
        setup.write_config(&setup.ports.config_line());
        setup
    }

    /// Writes `text` as the config file `$XDG_CONFIG_HOME/mxv/config.toml`.
    fn write_config(&self, text: &str) {
        let config = self.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write(&config, text);
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Reports whether phase has started the unit `unit` of some workspace,
    /// which it shows by creating the log of the unit's process.
    fn started(&self, unit: &str) -> bool {
        let logs = self.root().join("state/mxv/phase/logs");
        fs::read_dir(logs).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().join(format!("{unit}.main.log")).exists())
        })
    }

    /// Creates the workspace `name` and writes `manifest` as the `mxv.toml`
    /// of its checkout, and returns the checkout's path.
    fn workspace(&self, name: &str, manifest: &str) -> PathBuf {
        let (exit, document) = self.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        let checkout = self.root().join("workspaces").join(name).join("app");
        write(&checkout.join("mxv.toml"), manifest);
        checkout
    }

    /// Writes `manifest` as the `mxv.toml` of the checkout of the workspace
    /// `name`.
    fn workspace_manifest(&self, name: &str, manifest: &str) {
        let checkout = self.root().join("workspaces").join(name).join("app");
        write(&checkout.join("mxv.toml"), manifest);
    }

    /// Grants what every workspace's units ask for before `args` run, as a
    /// person at a terminal would, when `args` start or run units.
    fn grant_before(&self, args: &[&str]) {
        if !self.ungranted.load(Ordering::Relaxed) {
            grants::before(&self.root().join("state/mxv"), args);
        }
    }

    fn command_in(&self, dir: &Path, args: &[&str]) -> Command {
        let root = self.root();
        self.grant_before(args);
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
            // A test that wants a daemon sets its own socket after this one.
            .env("DEADBAND_SOCKET", MISSING_SOCKET)
    }

    /// Starts `mxv up` for the workspace `name` as a child process that the
    /// test can signal, with its standard error discarded.
    #[expect(
        clippy::disallowed_types,
        reason = "the test starts mxv as a child so that it can signal it"
    )]
    fn spawn_up(&self, name: &str) -> std::process::Child {
        self.spawn_up_on(name, Path::new(MISSING_SOCKET))
    }

    /// Starts `mxv up` as [`Setup::spawn_up`] does, with `socket` as the
    /// deadband daemon's socket.
    #[expect(
        clippy::disallowed_types,
        reason = "the test starts mxv as a child so that it can signal it"
    )]
    fn spawn_up_on(&self, name: &str, socket: &Path) -> std::process::Child {
        self.spawn_through(env!("CARGO_BIN_EXE_mxv"), &["up", "-w", name], socket)
    }

    /// Starts `program` with `args` and mxv's environment as a child that
    /// the test can signal, with `socket` as the deadband daemon's socket
    /// and its standard error discarded.
    #[expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "the test starts mxv as a child so that it can signal it"
    )]
    fn spawn_through(&self, program: &str, args: &[&str], socket: &Path) -> std::process::Child {
        let root = self.root();
        // A wrapper such as `deadband run -- mxv up` puts the verb later.
        if args.contains(&"up") {
            self.grant_before(&["up"]);
        }
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("DEADBAND_SOCKET", socket)
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    /// Runs `mxv` with `args` and `--json` in `dir`, and returns the exit
    /// code and the JSON document.
    fn json_in(&self, dir: &Path, args: &[&str]) -> (i32, Value) {
        let output = self.command_in(dir, args).arg("--json").output().unwrap();
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
        self.json_in(&self.root(), args)
    }

    /// Runs `mxv` with `args` and `--json` with the time zone `zone`, and
    /// returns the exit code and the JSON document.
    fn json_in_zone(&self, zone: &str, args: &[&str]) -> (i32, Value) {
        let output = self
            .command_in(&self.root(), args)
            .env("TZ", zone)
            .arg("--json")
            .output()
            .unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap();
        (output.status.code().unwrap(), document)
    }

    /// Runs `mxv` with `args` in the temporary directory, and returns the
    /// exit code and standard output.
    fn text(&self, args: &[&str]) -> (i32, String) {
        let output = self.command_in(&self.root(), args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }

    /// Runs `mxv` with `args` in the temporary directory, and returns the
    /// exit code and standard error.
    fn errors(&self, args: &[&str]) -> (i32, String) {
        let output = self.command_in(&self.root(), args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// Returns the host port that the workspace `name` holds for `port`.
    fn port(&self, name: &str, port: &str) -> u16 {
        let (_, shown) = self.json(&["show", name]);
        u16::try_from(shown["data"][0]["ports"][port].as_u64().unwrap()).unwrap()
    }

    /// Returns each process's state in the workspace `name`, by name.
    fn states(&self, name: &str) -> Vec<(String, String)> {
        let (_, document) = self.json(&["status", "-w", name]);
        document["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["name"].as_str().unwrap().to_owned(),
                    row["state"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

impl Drop for Setup {
    /// Stops every process that a test left running, also when the test
    /// failed or rewrote a run record: `mxv down` first, then SIGKILL to
    /// the group of every process whose command line names the temporary
    /// directory, as each run's shell does with its exit file.
    fn drop(&mut self) {
        let (_, listed) = self.json(&["list"]);
        for row in listed["data"].as_array().cloned().unwrap_or_default() {
            let _ = self.json(&["down", "-w", row["name"].as_str().unwrap_or_default()]);
        }
        let root = self.root().display().to_string();
        let Ok(table) = Command::new("ps")
            .args(["-A", "-o", "pgid=,command="])
            .env("PATH", PATH)
            .output()
        else {
            return;
        };
        for line in String::from_utf8_lossy(&table.stdout).lines() {
            let mut words = line.split_whitespace();
            if let Some(group) = words.next()
                && line.contains(&root)
            {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{group}")])
                    .env("PATH", PATH)
                    .output();
            }
        }
    }
}

/// Returns a manifest whose `server` listens on the port `api`, and whose
/// `worker` depends on it and prints the port it receives.
fn server_and_worker() -> String {
    format!(
        r#"ports = ["api"]

[processes.server]
argv = ["{listen}"]
ports = {{ PORT = "api" }}
listen = "api"
ready = {{ timeout_seconds = 20 }}

[processes.worker]
argv = ["sh", "-c", "echo worker sees $API_PORT; exec sleep 60"]
ports = {{ API_PORT = "api" }}
depends_on = ["server"]
"#,
        listen = listen()
    )
}

fn row<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {document}"))
}

#[test]
fn up_starts_each_process_after_its_dependency_is_ready_and_passes_the_ports() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["schema"], "mxv/up/v1");
    let names: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["server", "worker"]);
    assert_eq!(row(&document, "server")["state"], "running");
    let port = setup.port("triage", "api");
    let (exit, logs) = setup.text(&["logs", "-w", "triage", "server"]);
    assert_eq!(exit, 0);
    assert_eq!(logs, format!("listening on {port}\n"));
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(row(&status, "worker")["state"], "running");
    assert_eq!(
        row(&status, "worker")["ports"][0],
        format!("API_PORT={port}")
    );
}

#[test]
fn up_in_a_checkout_starts_that_workspaces_named_process_and_its_dependencies() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json_in(&checkout, &["up", "worker"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"].as_array().unwrap().len(), 2);
}

/// Returns a manifest of `first` and `second`, which depends on it, whose
/// shells run `first_script` and `second_script`.
fn pair(first_script: &str, second_script: &str) -> String {
    format!(
        "[processes.first]\nargv = [\"sh\", \"-c\", \"{first_script}\"]\nready = {{ log = \"^ready$\" }}\n\n[processes.second]\nargv = [\"sh\", \"-c\", \"{second_script}\"]\nready = {{ log = \"^ready$\" }}\ndepends_on = [\"first\"]\n"
    )
}

#[test]
fn a_failed_up_leaves_a_process_it_restarted_running_and_says_it_restarted_it() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        &pair("echo ready; exec sleep 60", "echo ready; exec sleep 60"),
    );
    let (exit, up) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{up}");
    let before = row(&up, "first")["pid"].clone();
    setup.workspace_manifest("triage", &pair("echo ready; exec sleep 61", "exit 5"));

    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{errors}");
    assert!(
        errors.contains("restarted first/main because its part of the plan changed"),
        "{errors}"
    );
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(row(&status, "first")["state"], "running", "{status}");
    assert_ne!(row(&status, "first")["pid"], before, "{status}");
    assert_eq!(row(&status, "second")["state"], "exited", "{status}");
}

#[test]
fn up_again_leaves_running_processes_as_they_are() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (_, first) = setup.json(&["up", "-w", "triage"]);
    let (exit, second) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{second}");
    assert_eq!(row(&first, "server")["pid"], row(&second, "server")["pid"]);
}

#[test]
fn a_process_that_exits_before_it_is_ready_fails_up_with_its_code_and_points_to_its_log_without_quoting_it()
 {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"echo cannot bind; exit 4\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\nready = { timeout_seconds = 20 }\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::exited");
    assert_eq!(document["fields"]["process"], "server");
    assert_eq!(document["fields"]["code"], 4);
    assert!(
        document["fields"]["log"]
            .as_str()
            .is_some_and(|log| log.ends_with(".log")),
        "{document}"
    );
    let hint = document["recovery"]["text"].as_str().unwrap_or_default();
    assert!(hint.contains("mxv logs -w triage server"), "{hint}");
    assert!(!document.to_string().contains("cannot bind"), "{document}");
}

#[test]
fn a_failed_up_stops_the_processes_that_it_started_before_the_failure() {
    let setup = Setup::new();
    let manifest = server_and_worker()
        .replace("ports = [\"api\"]", "ports = [\"api\", \"broken\"]")
        + "\n[processes.broken]\nargv = [\"sh\", \"-c\", \"exit 5\"]\nports = { PORT = \"broken\" }\nlisten = \"broken\"\nready = { timeout_seconds = 20 }\ndepends_on = [\"server\"]\n";
    setup.workspace("triage", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::exited");
    assert_eq!(
        setup.states("triage"),
        [
            ("server".to_owned(), "stopped".to_owned()),
            ("broken".to_owned(), "exited".to_owned()),
            ("worker".to_owned(), "stopped".to_owned()),
        ]
    );
}

#[test]
fn a_process_that_misses_its_readiness_timeout_fails_up_and_is_stopped() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"echo starting; exec sleep 60\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\nready = { timeout_seconds = 1 }\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::not_ready");
    assert!(!document.to_string().contains("starting"), "{document}");
    assert_eq!(
        setup.states("triage"),
        [("server".to_owned(), "stopped".to_owned())]
    );
}

#[test]
fn down_stops_every_process_and_status_reports_them_stopped() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.json(&["up", "-w", "triage"]);
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let names: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["worker", "server"]);
    assert_eq!(
        setup.states("triage"),
        [
            ("server".to_owned(), "stopped".to_owned()),
            ("worker".to_owned(), "stopped".to_owned())
        ]
    );
}

#[test]
fn status_shows_that_a_running_process_is_ready_and_runs_without_a_sandbox() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let server = &document["data"][0];
    assert_eq!(server["name"], "server", "{document}");
    assert_eq!(server["ready"], true, "{document}");
    assert_eq!(server["notes"], serde_json::json!([]), "{document}");
    let (exit, document) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let server = &document["data"][0];
    assert_eq!(server["ready"], true, "{document}");
    assert_eq!(
        server["sandbox"], "unsandboxed (its app declares no permissions)",
        "{document}"
    );
    let (exit, text) = setup.text(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains("unsandboxed (its app declares no permissions)"),
        "{text}"
    );
    setup.json(&["down", "-w", "triage"]);
    let (_, document) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(
        document["data"][0]["ready"],
        serde_json::Value::Null,
        "{document}"
    );
}

#[test]
fn status_and_down_in_another_time_zone_than_up_still_find_and_stop_the_process() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json_in_zone("America/Los_Angeles", &["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let server = row(&document, "server")["pid"].as_u64().unwrap();
    let (_, status) = setup.json_in_zone("Asia/Kolkata", &["status", "-w", "triage"]);
    assert_eq!(row(&status, "server")["state"], "running", "{status}");
    let (exit, document) = setup.json_in_zone("Asia/Kolkata", &["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!(
        !group_exists(server),
        "the server's group {server} outlived down"
    );
}

#[test]
fn restart_starts_a_running_process_again_with_a_new_pid() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (_, first) = setup.json(&["up", "-w", "triage"]);
    let (exit, restarted) = setup.json(&["restart", "-w", "triage", "worker"]);
    assert_eq!(exit, 0, "{restarted}");
    assert_eq!(row(&restarted, "worker")["state"], "running");
    assert_ne!(
        row(&first, "worker")["pid"],
        row(&restarted, "worker")["pid"]
    );
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(row(&first, "server")["pid"], row(&status, "server")["pid"]);
}

#[test]
fn status_reports_a_process_that_crashed_as_exited_with_its_code() {
    let setup = Setup::new();
    let checkout = setup.workspace(
        "triage",
        "[processes.flaky]\nargv = [\"sh\", \"-c\", \"while [ ! -e crash ]; do sleep 0.05; done; exit 7\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    write(&checkout.join("crash"), "");
    for () in waiting() {
        let (exit, status) = setup.json(&["status", "-w", "triage"]);
        if row(&status, "flaky")["state"] == "exited" {
            assert_eq!(row(&status, "flaky")["code"], 7);
            assert_eq!(
                exit, 1,
                "an exited process with a failing code is a problem"
            );
            return;
        }
    }
    panic!("the process did not exit");
}

#[test]
fn status_reports_a_process_whose_group_sigkill_ended_as_exited_with_signal_9() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"sleep 60\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    let group = row(&status, "sleeper")["pid"].as_u64().unwrap();
    assert_ne!(group, 0);
    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{group}")])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(killed.status.success(), "{killed:?}");
    for () in waiting() {
        let (exit, status) = setup.json(&["status", "-w", "triage"]);
        let sleeper = row(&status, "sleeper");
        if sleeper["state"] == "exited" {
            assert_eq!(sleeper["code"], Value::Null, "{sleeper}");
            assert_eq!(sleeper["signal"], 9, "{sleeper}");
            assert_eq!(sleeper["reason"], Value::Null, "{sleeper}");
            assert_eq!(exit, 1, "{status}");
            return;
        }
    }
    panic!("the process group did not end");
}

#[test]
fn status_reports_a_process_that_ended_with_its_keeper_as_exited_with_a_reason() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"sleep 60\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    let group = row(&status, "sleeper")["pid"].as_u64().unwrap();
    assert!(group > 1);
    let ps = |field: &str, pid: u64| {
        let output = Command::new("ps")
            .args(["-o", field, "-p", &pid.to_string()])
            .env("PATH", PATH)
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let keeper: u64 = ps("ppid=", group).parse().unwrap();
    assert!(keeper > 1);
    assert!(ps("command=", keeper).contains("phase keep"), "{keeper}");
    for target in [keeper.to_string(), format!("-{group}")] {
        let killed = Command::new("kill")
            .args(["-KILL", "--", &target])
            .env("PATH", PATH)
            .output()
            .unwrap();
        assert!(killed.status.success(), "{killed:?}");
    }
    for () in waiting() {
        let (exit, status) = setup.json(&["status", "-w", "triage"]);
        let sleeper = row(&status, "sleeper");
        if sleeper["state"] == "exited" {
            assert_eq!(sleeper["code"], Value::Null, "{sleeper}");
            assert_eq!(sleeper["signal"], Value::Null, "{sleeper}");
            assert!(
                sleeper["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("not watching")),
                "{sleeper}"
            );
            assert_eq!(exit, 1, "{status}");
            return;
        }
    }
    panic!("the process did not end");
}

#[test]
fn a_workspace_keeps_its_ports_across_ups_and_show_lists_them() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.json(&["up", "-w", "triage"]);
    let port = setup.port("triage", "api");
    setup.json(&["down", "-w", "triage"]);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(setup.port("triage", "api"), port);
}

#[test]
fn a_second_command_on_a_workspace_fails_with_busy_while_up_waits_for_readiness() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\nready = { timeout_seconds = 9 }\n",
    );
    std::thread::scope(|scope| {
        let up = scope.spawn(|| setup.json(&["up", "-w", "triage"]));
        while !setup.started("server") && !up.is_finished() {
            std::thread::yield_now();
        }
        if !setup.started("server") {
            panic!(
                "up ended before phase started the process: {:?}",
                up.join().unwrap()
            );
        }
        let (down_exit, down) = setup.json(&["down", "-w", "triage"]);
        let (up_exit, up) = up.join().unwrap();
        assert_eq!(down_exit, 1, "down: {down}\nup: {up}");
        assert_eq!(
            down["code"], "mxv::workspace::busy",
            "down: {down}\nup: {up}"
        );
        assert_eq!(up_exit, 1, "{up}");
        assert_eq!(up["code"], "mxv::process::not_ready", "{up}");
    });
}

#[test]
fn up_allocates_ports_from_20000_to_29999_by_default() {
    let setup = Setup::new();
    setup.write_config("");
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!((20000..=29999).contains(&setup.port("triage", "api")));
}

#[test]
fn up_allocates_ports_from_the_config_files_ports_range() {
    let setup = Setup::new();
    setup.write_config("ports_range = [24000, 24049]\n");
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!((24000..=24049).contains(&setup.port("triage", "api")));
}

#[test]
fn two_workspaces_of_one_repository_run_side_by_side_on_their_own_ports() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.workspace("reading-list", &server_and_worker());
    assert_eq!(setup.json(&["up", "-w", "triage"]).0, 0);
    let (exit, document) = setup.json(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{document}");
    assert_ne!(
        setup.port("triage", "api"),
        setup.port("reading-list", "api")
    );
    assert_eq!(
        row(&setup.json(&["status", "-w", "triage"]).1, "server")["state"],
        "running"
    );
}

#[test]
fn up_fails_with_port_taken_when_something_else_holds_a_stored_port() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.json(&["up", "-w", "triage"]);
    let port = setup.port("triage", "api");
    setup.json(&["down", "-w", "triage"]);
    let _holder = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::port::taken");
    assert_eq!(document["fields"]["port"], "api");
    assert_eq!(document["fields"]["number"], port);
}

#[test]
fn up_checks_a_listeners_port_even_while_another_process_of_the_workspace_receives_it() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        &server_and_worker().replace("depends_on = [\"server\"]\n", ""),
    );
    let (exit, document) = setup.json(&["up", "-w", "triage", "worker"]);
    assert_eq!(exit, 0, "{document}");
    let port = setup.port("triage", "api");
    let _holder = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage", "server"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::port::taken");
    assert_eq!(document["fields"]["workspace"], "triage");
}

// The wrapper exits, and later a child it left starts the only member that
// the group keeps. That member started after the wrapper's exit file was
// written, so nothing proves that the group is the one `up` started:
// another program's group could have taken the id. `down` must signal
// nothing, keep the record and report the group.

#[test]
fn a_listener_that_a_process_left_in_its_group_ends_with_it_so_the_next_up_starts_again() {
    let setup = Setup::new();
    let manifest = format!(
        "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"\\\"$0\\\" & sleep 2; exit 0\", \"{}\"]\nports = {{ PORT = \"api\" }}\nlisten = \"api\"\nready = {{ timeout_seconds = 20 }}\n",
        listen()
    );
    setup.workspace("triage", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let pid = row(&document, "server")["pid"].as_u64().unwrap();
    for () in waiting() {
        if !group_exists(pid) {
            break;
        }
    }
    assert!(!group_exists(pid), "the listener outlived its process");
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
}

#[test]
fn a_process_that_exits_takes_the_children_it_left_in_its_group_with_it() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.wrapper]\nargv = [\"sh\", \"-c\", \"sleep 60 & sleep 1; exit 0\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let pid = row(&document, "wrapper")["pid"].as_u64().unwrap();
    for () in waiting() {
        if !group_exists(pid) {
            break;
        }
    }
    assert!(!group_exists(pid), "the child outlived its wrapper");
}

#[test]
fn a_process_without_ready_that_exits_within_the_grace_fails_up_as_exited() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.quick]\nargv = [\"sh\", \"-c\", \"exit 3\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::exited");
    assert_eq!(document["fields"]["code"], 3);
}

#[test]
fn a_process_that_a_signal_ended_reports_no_code_and_the_signal() {
    let setup = Setup::new();
    // The process signals itself only once the file `go` exists, which the
    // test writes after `up` has returned, so `up` never sees it end.
    let checkout = setup.workspace(
        "triage",
        "[processes.doomed]\nargv = [\"sh\", \"-c\", \"while [ ! -e go ]; do sleep 0.05; done; kill -TERM $$\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    write(&checkout.join("go"), "");
    for () in waiting() {
        let (_, status) = setup.json(&["status", "-w", "triage"]);
        if row(&status, "doomed")["state"] == "exited" {
            assert_eq!(row(&status, "doomed")["code"], Value::Null, "{status}");
            assert_eq!(row(&status, "doomed")["signal"], 15, "{status}");
            return;
        }
    }
    panic!("the process did not exit");
}

#[test]
fn up_drops_the_ports_of_names_that_the_manifest_no_longer_declares() {
    let setup = Setup::new();
    let checkout = setup.workspace(
        "triage",
        &server_and_worker().replace("ports = [\"api\"]", "ports = [\"api\", \"old\"]"),
    );
    setup.json(&["up", "-w", "triage"]);
    let (_, shown) = setup.json(&["show", "triage"]);
    assert!(shown["data"][0]["ports"].get("old").is_some(), "{shown}");
    setup.json(&["down", "-w", "triage"]);
    write(&checkout.join("mxv.toml"), &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (_, shown) = setup.json(&["show", "triage"]);
    assert!(shown["data"][0]["ports"].get("old").is_none(), "{shown}");
}

#[test]
fn ports_reset_gives_a_stopped_workspace_new_ports_and_refuses_while_processes_run() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.json(&["up", "-w", "triage"]);
    let port = setup.port("triage", "api");
    let (exit, document) = setup.json(&["ports", "reset", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::running");
    setup.json(&["down", "-w", "triage"]);
    let (exit, document) = setup.json(&["ports", "reset", "-w", "triage", "api"]);
    assert_eq!(exit, 0, "{document}");
    assert_ne!(setup.port("triage", "api"), port);
}

#[test]
fn an_unknown_key_in_the_manifest_fails_with_the_key_and_its_line() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = []\n\n[processes.a]\nargv = [\"true\"]\nshell = \"true\"\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::unknown_key");
    assert_eq!(document["fields"]["key"], "shell");
    assert_eq!(document["fields"]["line"], 5);
}

#[test]
fn a_reserved_section_in_the_manifest_fails_with_the_reserved_code() {
    let setup = Setup::new();
    setup.workspace("triage", "[secrets.db]\nsource = \"file\"\n");
    let (_, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(document["code"], "mxv::manifest::reserved");
    assert_eq!(document["fields"]["section"], "secrets");
}

#[test]
fn up_of_a_workspace_without_a_manifest_fails_and_status_lists_no_process() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", "");
    fs::remove_file(checkout.join("mxv.toml")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::not_found");
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    assert_eq!(status["data"].as_array().unwrap().len(), 0);
}

#[test]
fn up_of_a_process_that_the_manifest_does_not_declare_fails_with_process_not_found() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage", "search"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::not_found");
    assert_eq!(document["fields"]["process"], "search");
}

#[test]
fn a_verb_outside_any_workspace_without_a_name_fails_with_not_current() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["status"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::not_current");
}

#[test]
fn logs_prints_the_last_lines_that_lines_asks_for() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.chatty]\nargv = [\"sh\", \"-c\", \"echo one; echo two; echo three; exec sleep 60\"]\n",
    );
    setup.json(&["up", "-w", "triage"]);
    let mut lines = String::new();
    for () in waiting() {
        lines = setup
            .text(&["logs", "-w", "triage", "chatty", "--lines", "2"])
            .1;
        if lines.contains("three") {
            break;
        }
    }
    assert_eq!(lines, "two\nthree\n");
}

#[test]
fn logs_follow_ends_once_the_process_has_exited_and_printed_its_last_lines() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.brief]\nargv = [\"sh\", \"-c\", \"sleep 1; echo done; sleep 1\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, lines) = setup.text(&["logs", "-w", "triage", "brief", "--follow"]);
    assert_eq!(exit, 0);
    assert_eq!(lines, "done\n");
}

#[test]
fn remove_refuses_while_processes_run_and_force_stops_them_first() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.json(&["up", "-w", "triage"]);
    let (exit, refused) = setup.json(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 1, "{refused}");
    assert_eq!(refused["code"], "mxv::workspace::running");
    let port = setup.port("triage", "api");
    let (exit, removed) = setup.json(&["remove", "triage", "--yes", "--force"]);
    assert_eq!(exit, 0, "{removed}");
    assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
    assert!(!setup.root().join("state/mxv/runs/triage").exists());
}

/// A workspace that an older mxv started keeps run files that phase never
/// reads: a run record, an exit file, a container record and the record of
/// its volumes. `up` starts the workspace under phase beside them, and
/// `remove` deletes them with the rest of the run directory.
#[test]
fn a_workspace_with_an_older_mxvs_run_files_starts_under_phase_and_remove_deletes_them() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n",
    );
    let runs = setup.root().join("state/mxv/runs/triage");
    fs::create_dir_all(&runs).unwrap();
    write(
        &runs.join("api.json"),
        r#"{"version":1,"pid":4242,"started":"0.0","ports":{}}"#,
    );
    write(&runs.join("api.exit"), "0\n");
    write(
        &runs.join("db.container"),
        r#"{"version":1,"id":"0123456789abcdef"}"#,
    );
    write(
        &runs.join(".volumes"),
        r#"{"version":1,"volumes":["mxv-0b9f3c8e-triage.data","0123456789abcdef"]}"#,
    );

    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.started("api"));
    assert_eq!(row(&document, "api")["state"], "running", "{document}");
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    let names: Vec<&str> = status["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api"], "{status}");
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["remove", "triage", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!runs.exists(), "remove kept the run directory");
}

#[test]
fn down_and_status_work_from_the_run_records_when_the_manifest_does_not_parse() {
    let setup = Setup::new();
    let checkout = setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let pid = row(&document, "api")["pid"].as_u64().unwrap();
    write(&checkout.join("mxv.toml"), "[processes.api\n");
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    assert_eq!(row(&status, "api")["state"], "running");
    let fallback = status["summary"]["manifest_error"]
        .as_str()
        .unwrap_or_default();
    assert!(fallback.contains("mxv.toml"), "{status}");
    let (exit, text) = setup.text(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains("the units come from the run records"),
        "{text}"
    );
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!group_exists(pid), "down left the process running");
}

#[test]
fn a_command_that_waited_for_the_lock_reads_the_workspace_record_again() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        &format!(
            "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"sleep 2; exec \\\"$0\\\"\", \"{}\"]\nports = {{ PORT = \"api\" }}\nlisten = \"api\"\nready = {{ timeout_seconds = 20 }}\n",
            listen()
        ),
    );
    let record = setup.root().join("state/mxv/workspaces/triage.json");
    let second = std::thread::scope(|scope| {
        let first = scope.spawn(|| setup.json(&["up", "-w", "triage"]));
        while !setup.started("server") && !first.is_finished() {
            std::thread::yield_now();
        }
        let second = scope.spawn(|| setup.json(&["up", "-w", "triage"]));
        // The second up reads the record within milliseconds and then
        // waits for the lock, which the first holds for about two seconds.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            std::thread::yield_now();
        }
        // Another writer changes the record while the second up waits.
        let mut stored: Value =
            serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
        stored["ports"]["api"] = 31999.into();
        write(&record, &stored.to_string());
        let (exit, document) = first.join().unwrap();
        assert_eq!(exit, 0, "{document}");
        second.join().unwrap()
    });
    let (exit, document) = second;
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        row(&document, "server")["ports"][0],
        "PORT=31999",
        "{document}"
    );
}

/// The manifest of a workspace with one unit that sleeps.
const SLEEPER: &str = "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"exec sleep 300\"]\n";

#[test]
fn a_pin_that_waited_for_the_lock_fails_and_writes_no_record_when_the_record_was_deleted() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER);
    let state = setup.root().join("state/mxv");
    let (exit, document) = held_lock::delete_record_while_locked(&state, "triage", || {
        setup.json(&["pin", "-w", "triage"])
    });
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::not_found", "{document}");
    assert!(!state.join("workspaces/triage.json").exists(), "{document}");
}

#[test]
fn an_up_that_waited_for_the_lock_starts_nothing_when_the_record_was_deleted() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER);
    let state = setup.root().join("state/mxv");
    let (exit, document) = held_lock::delete_record_while_locked(&state, "triage", || {
        setup.json(&["up", "-w", "triage"])
    });
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::not_found", "{document}");
    assert!(!state.join("workspaces/triage.json").exists(), "{document}");
    assert!(!setup.started("sleeper"), "{document}");
}

#[test]
fn a_forced_remove_that_waited_for_the_lock_has_nothing_to_do_when_the_record_was_deleted() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let state = setup.root().join("state/mxv");
    let record = state.join("workspaces/triage.json");
    let saved = fs::read_to_string(&record).unwrap();
    let (exit, document) = held_lock::delete_record_while_locked(&state, "triage", || {
        setup.json(&["remove", "triage", "--force", "--yes"])
    });
    let gone = !record.exists();
    // The record goes back, so the setup's drop stops the sleeper, which
    // the command that deleted the record would have stopped.
    write(&record, &saved);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert!(gone, "{document}");
    assert!(setup.root().join("workspaces/triage/app").is_dir());
}

#[test]
fn a_port_that_the_manifest_renames_stays_with_the_workspace_until_its_listener_stops() {
    let setup = Setup::new();
    let manifest = server_and_worker();
    let checkout = setup.workspace("triage", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "triage", "server"]);
    assert_eq!(exit, 0, "{document}");
    let old = setup.port("triage", "api");
    write(
        &checkout.join("mxv.toml"),
        &manifest
            .replace("\"api\"", "\"http\"")
            .replace("[processes.worker]", "[processes.unused]"),
    );
    let (exit, document) = setup.json(&["up", "-w", "triage", "server"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(setup.port("triage", "api"), old);
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["up", "-w", "triage", "server"]);
    assert_eq!(exit, 0, "{document}");
    let (_, shown) = setup.json(&["show", "triage"]);
    assert!(shown["data"][0]["ports"].get("api").is_none(), "{shown}");
}

#[test]
fn up_in_text_names_the_log_of_a_process_that_exited_before_it_was_ready_and_quotes_none_of_it() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.build]\nargv = [\"sh\", \"-c\", \"echo compiling; echo 'secret=hunter2'; exit 101\"]\n",
    );
    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{errors}");
    assert!(
        errors.contains("the unit build exited with code 101 before it was ready"),
        "{errors}"
    );
    assert!(errors.contains("mxv logs -w triage build"), "{errors}");
    assert!(!errors.contains("compiling"), "{errors}");
    assert!(!errors.contains("hunter2"), "{errors}");
}

#[test]
fn an_env_value_receives_the_host_port_that_its_placeholder_names() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.client]\nargv = [\"sh\", \"-c\", \"echo url=$API_URL; echo placeholders=$(env | grep -c port.api); exec sleep 60\"]\nenv = { API_URL = \"http://127.0.0.1:{port.api}/v1?q={{x}}\" }\nready = { log = \"^placeholders=\", timeout_seconds = 20 }\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let port = setup.port("triage", "api");
    let (_, logs) = setup.text(&["logs", "-w", "triage", "client"]);
    assert_eq!(
        logs,
        format!("url=http://127.0.0.1:{port}/v1?q={{x}}\nplaceholders=0\n")
    );
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(
        row(&status, "client")["ports"][0],
        format!("port.api={port}"),
        "{status}"
    );
}

#[test]
fn a_worker_with_a_log_readiness_check_is_ready_once_it_writes_a_matching_line() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.worker]\nargv = [\"sh\", \"-c\", \"echo starting; sleep 1; echo worker ready on jobs; exec sleep 60\"]\nready = { log = \"^worker ready\", timeout_seconds = 20 }\n\n[processes.after]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\ndepends_on = [\"worker\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (_, logs) = setup.text(&["logs", "-w", "triage", "worker"]);
    assert_eq!(logs, "starting\nworker ready on jobs\n");
    assert_eq!(row(&document, "after")["state"], "running");
}

#[test]
fn a_worker_that_writes_no_matching_log_line_fails_up_as_not_ready_with_the_pattern() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.worker]\nargv = [\"sh\", \"-c\", \"echo starting; exec sleep 60\"]\nready = { log = \"^worker ready\", timeout_seconds = 1 }\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::not_ready");
    assert_eq!(document["fields"]["pattern"], "^worker ready");
    assert_eq!(document["fields"]["port"], Value::Null);
    assert!(!document.to_string().contains("starting"), "{document}");
    assert_eq!(
        setup.states("triage"),
        [("worker".to_owned(), "stopped".to_owned())]
    );
}

#[test]
fn up_fails_with_foreign_listener_when_another_program_answers_on_the_listen_port() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\nports = { PORT = \"api\" }\nlisten = \"api\"\nready = { timeout_seconds = 20 }\n",
    );
    std::thread::scope(|scope| {
        let up = scope.spawn(|| setup.json(&["up", "-w", "triage"]));
        while !setup.started("server") && !up.is_finished() {
            std::thread::yield_now();
        }
        assert!(
            setup.started("server"),
            "up ended first: {:?}",
            up.join().unwrap()
        );
        // The test itself is the other program: it is in no group of the
        // workspace, and it listens on the port that `server` should.
        let port = setup.port("triage", "api");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let (exit, document) = up.join().unwrap();
        drop(listener);
        assert_eq!(exit, 1, "{document}");
        assert_eq!(
            document["code"], "mxv::process::foreign_listener",
            "{document}"
        );
        assert_eq!(document["fields"]["port"], "api");
        assert_eq!(document["fields"]["number"], port);
        assert_eq!(document["fields"]["pid"], std::process::id(), "{document}");
        assert!(
            !document["hint"]
                .as_str()
                .unwrap_or_default()
                .contains("lsof"),
            "{document}"
        );
    });
    assert_eq!(
        setup.states("triage"),
        [("server".to_owned(), "stopped".to_owned())]
    );
}

#[test]
fn up_starts_the_processes_of_the_workspace_manifest_and_never_the_checkouts() {
    let setup = Setup::new();
    let file = setup.root().join("triage.toml");
    write(
        &file,
        &format!("{}\n[app]\nname = \"triage\"\n", server_and_worker()),
    );
    let (exit, document) = setup.json(&[
        "new",
        "triage",
        "--repo",
        "app",
        "--manifest",
        file.to_str().unwrap(),
    ]);
    assert_eq!(exit, 0, "{document}");
    let checkout = setup.root().join("workspaces/triage/app");
    write(
        &checkout.join("mxv.toml"),
        "[processes.other]\nargv = [\"sleep\", \"30\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let names: Vec<&str> = document["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["server", "worker"]);
}

#[test]
fn up_warns_about_a_misspelt_table_and_a_manifest_that_declares_nothing() {
    let setup = Setup::new();
    setup.workspace("triage", "[proceses.web]\nargv = [\"true\"]\n");
    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{errors}");
    assert!(
        errors.contains(
            "mxv ignores the table `[proceses]`, whose name is close to mxv's `processes`"
        ),
        "{errors}"
    );
    assert!(
        errors.contains("declares no units and no services, so mxv started nothing"),
        "{errors}"
    );
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    let warnings = status["summary"]["warnings"].to_string();
    assert!(warnings.contains("[proceses]"), "{status}");
}

#[test]
fn up_warns_that_phases_process_table_is_not_mxvs() {
    let setup = Setup::new();
    setup.workspace("triage", "[process.web]\nargv = [\"true\"]\n");
    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{errors}");
    assert!(
        errors.contains("mxv ignores the table `[process.*]`, which is phase's, and runs only the processes of `[processes.*]`"),
        "{errors}"
    );
}

#[test]
fn a_dangling_link_at_the_workspace_manifest_is_unreadable_and_named() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", &server_and_worker());
    let link = checkout.parent().unwrap().join("mxv.toml");
    std::os::unix::fs::symlink(setup.root().join("gone.toml"), &link).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::unreadable", "{document}");
    assert!(document.to_string().contains("gone.toml"), "{document}");
    assert!(!document.to_string().contains("neither"), "{document}");
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    assert_eq!(
        status["summary"]["manifest_error_code"], "mxv::manifest::unreadable",
        "{status}"
    );
    assert!(
        status["summary"]["manifest_error"]
            .as_str()
            .unwrap()
            .contains("gone.toml"),
        "{status}"
    );
    let (exit, shown) = setup.json(&["show", "triage"]);
    assert_eq!(exit, 0, "{shown}");
    assert!(
        shown["data"][0]["warnings"]
            .as_str()
            .unwrap()
            .contains("gone.toml"),
        "{shown}"
    );
}

// A readiness port that nothing in the process's group has to hold counts
// as ready as soon as anything accepts on it.
#[test]
fn a_process_that_only_receives_its_readiness_port_is_ready_when_that_port_accepts() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.a]\nargv = [\"sleep\", \"30\"]\nports = { PORT = \"api\" }\n",
    );
    setup.json(&["up", "-w", "triage"]);
    let port = setup.port("triage", "api");
    setup.json(&["down", "-w", "triage"]);
    setup.workspace_manifest(
        "triage",
        "ports = [\"api\"]\n[processes.a]\nargv = [\"sleep\", \"30\"]\nports = { PORT = \"api\" }\nready = { port = \"api\", timeout_seconds = 5 }\n",
    );
    let _listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "a")["state"], "running", "{document}");
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
}

#[test]
fn up_fails_as_unstoppable_and_starts_nothing_more_when_phase_leaves_a_group_it_cannot_prove() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.first]\nargv = [\"sh\", \"-c\", \"sleep 300 & wait\"]\n[processes.second]\nargv = [\"sh\", \"-c\", \"exec sleep 300\"]\ndepends_on = [\"first\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage", "first"]);
    assert_eq!(exit, 0, "{document}");
    let leader = row(&document, "first")["pid"].as_u64().unwrap();
    assert!(leader > 1);
    let ps = |pid: u64| {
        let output = Command::new("ps")
            .args(["-o", "ppid=", "-p", &pid.to_string()])
            .env("PATH", PATH)
            .output()
            .unwrap();
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap()
    };
    let keeper = ps(leader);
    assert!(keeper > 1);
    // The keeper goes first, so nothing stops the group when its leader
    // ends, and the leader's `sleep` stays in the group.
    for pid in [keeper, leader] {
        let killed = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .env("PATH", PATH)
            .output()
            .unwrap();
        assert!(killed.status.success(), "{killed:?}");
    }
    for () in waiting() {
        if !process_exists(leader) {
            break;
        }
    }
    assert!(group_exists(leader));

    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::process::unstoppable", "{document}");
    assert_eq!(document["fields"]["process"], "first");
    assert_eq!(document["fields"]["pid"], leader);
    assert_eq!(
        document["fields"]["not_started"],
        serde_json::json!(["second"])
    );
    assert!(
        !setup.started("second"),
        "up started a unit after the left group"
    );

    let killed = Command::new("kill")
        .args(["-KILL", "--", &format!("-{leader}")])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(killed.status.success(), "{killed:?}");
}

#[test]
fn down_of_a_named_process_of_a_workspace_that_never_ran_reports_it_stopped() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 300\"]\n[processes.worker]\nargv = [\"sh\", \"-c\", \"exec sleep 300\"]\n",
    );
    let (exit, document) = setup.json(&["down", "-w", "triage", "api"]);
    assert_eq!(exit, 0, "{document}");
    let rows = document["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{document}");
    assert_eq!(rows[0]["name"], "api");
    assert_eq!(rows[0]["state"], "stopped");
}

/// Runs `mxv` with `args` in the temporary directory, and returns the exit
/// code, standard output and standard error.
fn exec_output(setup: &Setup, args: &[&str]) -> (i32, String, String) {
    exec_output_in(setup, &setup.root(), args)
}

/// A manifest whose process `api` runs in the checkout's `sub` directory
/// with the variable `GREETING` and the port `api` as `PORT`.
const EXEC_MANIFEST: &str = "ports = [\"api\"]\n[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 300\"]\ncwd = \"sub\"\nenv = { GREETING = \"hello\" }\nports = { PORT = \"api\" }\n";

#[test]
fn exec_runs_a_command_with_the_processs_variables_and_directory_and_exits_with_its_code() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let port = setup.port("triage", "api");

    let (exit, out, err) = exec_output(
        &setup,
        &[
            "exec",
            "-w",
            "triage",
            "api",
            "--",
            "sh",
            "-c",
            "echo \"$GREETING $PORT $(pwd -P)\"; exit 7",
        ],
    );

    assert_eq!(exit, 7, "{err}");
    assert_eq!(
        out,
        format!("hello {port} {}\n", checkout.join("sub").display())
    );
    assert_eq!(err, "");
}

#[test]
fn exec_gives_the_command_none_of_mxvs_own_variables_outside_the_passed_through_ones() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");

    let output = setup
        .command_in(&setup.root(), &["exec", "-w", "triage", "api", "--", "env"])
        .env("MXV_TEST_SECRET", "not-for-the-command")
        .env("AWS_PROFILE", "production")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let env = String::from_utf8(output.stdout).unwrap();
    assert!(env.lines().any(|line| line == "GREETING=hello"), "{env}");
    assert!(!env.contains("MXV_TEST_SECRET"), "{env}");
    assert!(!env.contains("AWS_PROFILE"), "{env}");
}

#[test]
fn exec_exits_with_128_plus_the_signal_that_ended_the_command() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");

    let (exit, _, err) = exec_output(
        &setup,
        &[
            "exec",
            "-w",
            "triage",
            "api",
            "--",
            "sh",
            "-c",
            "kill -TERM $$",
        ],
    );

    assert_eq!(exit, 143, "{err}");
}

#[test]
fn exec_in_a_checkout_finds_the_workspace_and_passes_the_commands_own_flags_to_it() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");

    let output = setup
        .command_in(&checkout, &["exec", "api", "--", "echo", "-w", "--json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "-w --json\n");
}

#[test]
fn exec_of_a_process_that_is_not_running_fails_with_not_running_and_runs_nothing() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();

    let (exit, out, err) = exec_output(
        &setup,
        &["exec", "-w", "triage", "api", "--", "touch", "ran"],
    );

    assert_eq!(exit, 1, "{err}");
    assert_eq!(out, "");
    assert!(err.contains("mxv::process::not_running"), "{err}");
    assert!(err.contains("mxv up --workspace triage api"), "{err}");
    assert!(!checkout.join("sub/ran").exists());
}

#[test]
fn exec_of_a_process_after_down_fails_with_not_running() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");

    let (exit, _, err) = exec_output(&setup, &["exec", "-w", "triage", "api", "--", "true"]);

    assert_eq!(exit, 1, "{err}");
    assert!(err.contains("mxv::process::not_running"), "{err}");
}

#[test]
fn exec_of_a_process_that_the_manifest_does_not_declare_fails_with_process_not_found() {
    let setup = Setup::new();
    setup.workspace("triage", EXEC_MANIFEST);

    let (exit, _, err) = exec_output(&setup, &["exec", "-w", "triage", "web", "--", "true"]);

    assert_eq!(exit, 1, "{err}");
    assert!(err.contains("mxv::process::not_found"), "{err}");
}

#[test]
fn exec_of_a_program_that_does_not_exist_fails_with_unstartable() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");

    let (exit, _, err) = exec_output(
        &setup,
        &["exec", "-w", "triage", "api", "--", "/nonexistent/program"],
    );

    assert_eq!(exit, 1, "{err}");
    assert!(err.contains("mxv::exec::unstartable"), "{err}");
}

#[test]
fn exec_refuses_json_because_the_output_is_the_commands_own() {
    let setup = Setup::new();
    setup.workspace("triage", EXEC_MANIFEST);

    let output = setup
        .command_in(
            &setup.root(),
            &["exec", "--json", "-w", "triage", "api", "--", "true"],
        )
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["code"], "mxv::cli::usage", "{document}");
}

#[test]
fn exec_under_an_ignored_sighup_leaves_the_signal_ignored_in_the_command() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");

    let ended = setup
        .spawn_through(
            "/usr/bin/nohup",
            &[
                env!("CARGO_BIN_EXE_mxv"),
                "exec",
                "-w",
                "triage",
                "api",
                "--",
                "sh",
                "-c",
                "kill -HUP $$; exit 4",
            ],
            Path::new(MISSING_SOCKET),
        )
        .wait()
        .unwrap();

    assert_eq!(ended.code(), Some(4), "{ended:?}");
}

#[test]
fn exec_keeps_running_after_sigint_to_mxv_alone_and_exits_with_the_commands_code() {
    use std::io::BufRead as _;
    let setup = Setup::new();
    let checkout = setup.workspace("triage", EXEC_MANIFEST);
    fs::create_dir(checkout.join("sub")).unwrap();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let root = setup.root();
    // The command waits for a file that the test writes after the signal,
    // so it is still running when mxv receives SIGINT.
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv exec in the background and sends it SIGINT"
    )]
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"));
    command
        .args([
            "exec",
            "-w",
            "triage",
            "api",
            "--",
            "sh",
            "-c",
            "echo started; while [ ! -f go ]; do sleep 0.05; done; exit 4",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", PATH)
        .envs(GIT_ENV.iter().copied())
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("MXV_ROOT", root.join("workspaces"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv exec in the background and sends it SIGINT"
    )]
    let mut child = command.spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "started\n");

    let sent = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(sent.status.success(), "{sent:?}");
    let still = child.try_wait().unwrap().is_none();
    write(&checkout.join("sub/go"), "");

    let deadline = Instant::now() + MOST_WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            drop(child.kill());
            panic!("mxv exec did not end");
        }
        std::thread::yield_now();
    };
    assert!(still, "mxv exec ended on SIGINT before the command did");
    assert_eq!(status.code(), Some(4), "{status:?}");
}

/// Waits until `path` exists, and fails the test when it does not appear.
fn wait_for(path: &Path) {
    for () in waiting() {
        if path.exists() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("{} did not appear", path.display());
}

/// Returns the pid of the unit `unit` that `mxv status` reports.
fn pid_of(setup: &Setup, workspace: &str, unit: &str) -> u64 {
    let (_, status) = setup.json(&["status", "-w", workspace]);
    row(&status, unit)["pid"].as_u64().unwrap()
}

/// A process that stays up, whose `build` fails once the file `broken`
/// exists in the checkout, with `more` added to its table.
fn breakable(more: &str) -> String {
    format!(
        "[processes.api]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nbuild = [\"sh\", \"-c\", \"test ! -e broken\"]\nready = {{ log = \"^ready$\" }}\n{more}"
    )
}

#[test]
fn up_runs_the_build_before_it_starts_the_process_and_sends_its_output_to_standard_error() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sh\", \"-c\", \"cat built; exec sleep 60\"]\nbuild = [\"sh\", \"-c\", \"echo the-build-ran; echo made > built\"]\n";
    let checkout = setup.workspace("triage", manifest);

    let (exit, stderr) = setup.errors(&["up", "-w", "triage"]);

    assert_eq!(exit, 0, "{stderr}");
    assert!(stderr.contains("the-build-ran"), "{stderr}");
    assert!(checkout.join("built").exists());
    let (_, logs) = setup.text(&["logs", "-w", "triage", "api"]);
    assert_eq!(logs, "made\n");
}

#[test]
fn a_build_receives_the_job_variables_and_a_variable_that_the_process_declares_keeps_its_value() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nenv = { MAKEFLAGS = \"-j1\" }\nbuild = [\"sh\", \"-c\", \"echo $CARGO_BUILD_JOBS $MISE_JOBS $MAKEFLAGS > seen\"]\n";
    let checkout = setup.workspace("triage", manifest);

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 0, "{document}");
    let seen = fs::read_to_string(checkout.join("seen")).unwrap();
    let words: Vec<&str> = seen.split_whitespace().collect();
    assert!(words[0].parse::<u32>().is_ok(), "{seen}");
    assert_eq!(words[1..], ["1", "-j1"], "{seen}");
}

#[test]
fn a_build_that_prints_to_standard_output_leaves_the_json_document_alone() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"sh\", \"-c\", \"echo the-build-ran\"]\n";
    setup.workspace("triage", manifest);

    let output = setup
        .command_in(&setup.root(), &["up", "-w", "triage", "--json"])
        .output()
        .unwrap();

    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{document}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("the-build-ran"),
        "{output:?}"
    );
}

#[test]
fn a_failing_build_still_prints_the_warnings_of_the_manifest() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"cargo\", \"run\"]\nbuild = [\"false\"]\n",
    );

    let (exit, stderr) = setup.errors(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{stderr}");
    assert!(stderr.contains("`cargo run`"), "{stderr}");
}

#[test]
fn a_failing_build_leaves_a_running_process_with_the_same_pid_under_restart() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", &breakable(""));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let before = pid_of(&setup, "triage", "api");
    write(&checkout.join("broken"), "");

    let (exit, failure) = setup.json(&["restart", "-w", "triage"]);

    assert_eq!(exit, 1, "{failure}");
    assert_eq!(failure["code"], "mxv::build::failed", "{failure}");
    assert_eq!(pid_of(&setup, "triage", "api"), before);
}

#[test]
fn a_failing_build_leaves_a_running_process_with_the_same_pid_when_up_would_replace_it() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", &breakable(""));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let before = pid_of(&setup, "triage", "api");
    write(&checkout.join("broken"), "");
    // A process whose argv changes is one that `up` replaces.
    setup.workspace_manifest(
        "triage",
        &breakable("").replace("exec sleep 60", "exec sleep 61"),
    );

    let (exit, failure) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{failure}");
    assert_eq!(failure["code"], "mxv::build::failed", "{failure}");
    assert_eq!(pid_of(&setup, "triage", "api"), before);
}

#[test]
fn a_build_that_cannot_start_fails_up_with_the_reason_and_starts_nothing() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"/no/such/program\"]\n",
    );

    let (exit, failure) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{failure}");
    assert_eq!(failure["code"], "mxv::build::unstartable", "{failure}");
    assert!(!setup.started("api"));
}

#[test]
fn the_ready_timeout_starts_after_the_build_ends() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nbuild = [\"sleep\", \"2\"]\nready = { log = \"^ready$\", timeout_seconds = 1 }\n",
    );

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 0, "{document}");
}

#[test]
fn up_warns_once_for_each_process_whose_argv_is_cargo_run() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"cargo\", \"run\"]\n\n[processes.web]\nargv = [\"sleep\", \"60\"]\n",
    );

    let (_, stderr) = setup.errors(&["up", "-w", "triage"]);

    assert_eq!(stderr.matches("`cargo run`").count(), 1, "{stderr}");
    assert!(stderr.contains("`api`"), "{stderr}");
}

#[test]
fn an_interrupt_during_a_build_ends_its_group_starts_nothing_and_exits_with_130() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"sh\", \"-c\", \"echo $$ > group; touch started; sleep 60\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let mut up = setup.spawn_up("triage");
    wait_for(&checkout.join("started"));
    let group: u64 = fs::read_to_string(checkout.join("group"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    interrupt(up.id());
    let ended = up.wait().unwrap();

    assert_eq!(ended.code(), Some(130));
    assert!(!group_exists(group));
    assert!(!setup.started("api"));
}

#[test]
fn a_build_under_an_ignored_sighup_is_not_cancelled_by_sighup() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"sh\", \"-c\", \"touch started; while [ ! -e go ]; do sleep 0.1; done\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let mut up = setup.spawn_through(
        "/usr/bin/nohup",
        &[env!("CARGO_BIN_EXE_mxv"), "up", "-w", "triage"],
        Path::new(MISSING_SOCKET),
    );
    wait_for(&checkout.join("started"));

    send("HUP", up.id());
    write(&checkout.join("go"), "");
    let ended = up.wait().unwrap();

    assert_eq!(ended.code(), Some(0), "{ended:?}");
    assert!(setup.started("api"));
}

/// Sends `signal` to an `mxv up` during its build, and checks that the
/// build's group ended, nothing started and mxv exited with `code`.
fn a_signal_during_a_build_cancels_it(signal: &str, code: i32) {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"sh\", \"-c\", \"echo $$ > group; touch started; sleep 60\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let mut up = setup.spawn_up("triage");
    wait_for(&checkout.join("started"));
    let group: u64 = fs::read_to_string(checkout.join("group"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    send(signal, up.id());
    let ended = up.wait().unwrap();

    assert_eq!(ended.code(), Some(code), "{ended:?}");
    assert!(!group_exists(group));
    assert!(!setup.started("api"));
}

#[test]
fn sighup_during_a_build_ends_its_group_and_exits_with_129() {
    a_signal_during_a_build_cancels_it("HUP", 129);
}

#[test]
fn sigquit_during_a_build_ends_its_group_and_exits_with_131() {
    a_signal_during_a_build_cancels_it("QUIT", 131);
}

#[test]
fn an_interrupt_during_the_wait_for_a_place_ends_mxv_with_nothing_run_and_leaves_the_queue() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"touch\", \"built\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let socket = setup.root().join("d.sock");
    start_daemon(&socket);
    let job = holder();
    let deadband::client::Admission::Granted(lease) =
        deadband::client::ask(&socket, &job, Duration::from_secs(10), |_, _| {
            panic!("the first job waits")
        })
    else {
        panic!("the daemon did not grant the first job");
    };
    let queued = |count: usize| {
        waiting().any(|()| {
            let jobs = deadband::client::jobs(&socket, Duration::from_secs(10)).unwrap();
            jobs.waiting.len() == count
        })
    };
    let mut up = setup.spawn_up_on("triage", &socket);
    assert!(queued(1), "mxv never joined the queue");

    interrupt(up.id());
    let ended = up.wait().unwrap();

    assert_eq!(ended.signal().or(ended.code()), Some(2), "{ended:?}");
    assert!(queued(0), "mxv's job stayed in the queue");
    assert!(!checkout.join("built").exists());
    assert!(!setup.started("api"));
    lease.end(Some(0));
}

#[test]
fn an_interrupt_after_the_builds_ends_mxv_while_it_waits_for_a_process_to_be_ready() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sh\", \"-c\", \"touch running; exec sleep 60\"]\nbuild = [\"true\"]\nready = { log = \"^never$\", timeout_seconds = 60 }\n";
    let checkout = setup.workspace("triage", manifest);
    let mut up = setup.spawn_up("triage");
    wait_for(&checkout.join("running"));

    interrupt(up.id());
    let ended = up.wait().unwrap();

    assert_eq!(ended.signal().or(ended.code()), Some(2));
}

/// Sends `SIGINT` to the process `pid`, as Ctrl-C does.
fn interrupt(pid: u32) {
    send("INT", pid);
}

/// Sends the signal `name` to the process `pid`.
fn send(name: &str, pid: u32) {
    let status = Command::new("kill")
        .args([&format!("-{name}"), &pid.to_string()])
        .env("PATH", PATH)
        .output()
        .unwrap();
    assert!(status.status.success());
}

/// A machine with plenty of room, so only the daemon's limit on jobs holds
/// a build back.
#[derive(Clone)]
struct Calm;

impl deadband::probe::Probe for Calm {
    fn sample(&self) -> Result<deadband::probe::Sample, deadband::probe::Unreadable> {
        Ok(deadband::probe::Sample {
            load_1m: 0.0,
            memory_pressure: deadband::probe::Pressure::Normal,
            swap_used_bytes: 0,
            swap_total_bytes: 1,
        })
    }

    fn disk(&self, _path: &Path) -> std::io::Result<deadband::probe::Disk> {
        Ok(deadband::probe::Disk {
            free_bytes: u64::MAX / 2,
            total_bytes: u64::MAX,
        })
    }
}

/// Returns the job that a test asks for to hold the daemon's one place.
fn holder() -> deadband::protocol::Job {
    deadband::protocol::Job {
        label: "holder".to_owned(),
        pid: std::process::id(),
        command: "the job that holds the one place".to_owned(),
        lane: deadband::admit::Lane::default(),
        watched: false,
        cwd: None,
        workspace: None,
        short: None,
    }
}

/// Starts a deadband daemon that runs one job at a time on the socket
/// `socket`, in a thread that ends with the test's process.
fn start_daemon(socket: &Path) {
    let bound = deadband::daemon::bind(socket).unwrap();
    let settings = deadband::daemon::Settings::new(
        std::num::NonZeroUsize::MIN,
        std::num::NonZeroUsize::new(8),
        None,
        None,
    );
    let log: deadband::daemon::Log = std::sync::Arc::new(std::sync::Mutex::new(std::io::sink()));
    std::thread::spawn(move || {
        let _ = deadband::daemon::serve(&bound.listener, Calm, settings, &log);
    });
}

/// Waits for `child` to end, for at most [`MOST_WAIT`], and fails the test
/// when it does not.
#[expect(
    clippy::disallowed_types,
    reason = "the test waits for an mxv that it started as a child"
)]
fn wait_for_exit(mut child: std::process::Child) {
    for () in waiting() {
        if child.try_wait().unwrap().is_some() {
            return;
        }
    }
    let _ = child.kill();
    panic!("mxv did not end within the test's limit");
}

#[test]
fn a_gated_task_waits_for_a_place_and_a_task_with_gate_false_does_not() {
    let setup = Setup::new();
    let manifest = "[tasks.free]\nargv = [\"sh\", \"-c\", \"echo ${DEADBAND_HELD:-none} > free\"]\ngate = false\n\n[tasks.held]\nargv = [\"sh\", \"-c\", \"ps -o nice= -p $$ > niceness; echo $DEADBAND_HELD $MISE_JOBS > held\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let socket = setup.root().join("d.sock");
    start_daemon(&socket);
    let job = holder();
    let deadband::client::Admission::Granted(lease) =
        deadband::client::ask(&socket, &job, Duration::from_secs(10), |_, _| {
            panic!("the first job waits")
        })
    else {
        panic!("the daemon did not grant the first job");
    };
    let mxv = env!("CARGO_BIN_EXE_mxv");

    // The only place is held, so this task ends only because it never asks.
    wait_for_exit(setup.spawn_through(mxv, &["run", "-w", "triage", "free"], &socket));
    assert_eq!(
        fs::read_to_string(checkout.join("free")).unwrap().trim(),
        "none"
    );

    let held = setup.spawn_through(mxv, &["run", "-w", "triage", "held"], &socket);
    let queued = waiting().any(|()| {
        let jobs = deadband::client::jobs(&socket, Duration::from_secs(10)).unwrap();
        jobs.waiting.len() == 1
    });
    assert!(queued, "mxv never joined the queue");
    assert!(!checkout.join("held").exists());
    lease.end(Some(0));
    wait_for_exit(held);
    assert_eq!(
        fs::read_to_string(checkout.join("held")).unwrap().trim(),
        "1 1"
    );
    // A test run that deadband itself admitted already has nice 5, and then
    // this cannot tell a lowered task from an unlowered one. A test run at
    // nice 0, as in CI, can.
    let niceness: i32 = fs::read_to_string(checkout.join("niceness"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(niceness, own_niceness().max(5));
}

/// Returns the nice value of this test's process.
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads its own nice value with ps"
)]
fn own_niceness() -> i32 {
    let output = std::process::Command::new("ps")
        .args(["-o", "nice=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap()
}

/// Grants the first job that asks at `socket`, in a thread, and returns the
/// job as it asked and what it reported when it ended.
fn daemon_that_reads_one_job(
    socket: &Path,
) -> std::thread::JoinHandle<(deadband::protocol::Job, deadband::protocol::Request)> {
    use deadband::protocol::{Reply, Request, read_line, write_line};
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
        let asked: Request = read_line(&mut reader).unwrap().unwrap();
        let Request::Ask(job) = asked else {
            panic!("{asked:?}");
        };
        write_line(&mut stream, &Reply::Granted { jobs: 2 }).unwrap();
        (job, read_line(&mut reader).unwrap().unwrap())
    })
}

#[test]
fn the_place_of_a_task_ends_with_the_tasks_exit_code() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[tasks.fails]\nargv = [\"sh\", \"-c\", \"exit 6\"]\n",
    );
    let socket = setup.root().join("d.sock");
    let daemon = daemon_that_reads_one_job(&socket);

    wait_for_exit(setup.spawn_through(
        env!("CARGO_BIN_EXE_mxv"),
        &["run", "-w", "triage", "fails"],
        &socket,
    ));

    assert_eq!(
        daemon.join().unwrap().1,
        deadband::protocol::Request::Done {
            exit_code: Some(6),
            signal: None,
            usage: None,
        }
    );
}

#[test]
fn a_task_that_mxv_run_admits_asks_deadband_with_the_workspace_name() {
    let setup = Setup::new();
    setup.workspace("triage", "[tasks.check]\nargv = [\"true\"]\n");
    let socket = setup.root().join("d.sock");
    let daemon = daemon_that_reads_one_job(&socket);

    wait_for_exit(setup.spawn_through(
        env!("CARGO_BIN_EXE_mxv"),
        &["run", "-w", "triage", "check"],
        &socket,
    ));

    let (job, _) = daemon.join().unwrap();
    assert_eq!(
        job.workspace.as_ref().map(|name| name.as_str()),
        Some("triage")
    );
}

#[test]
fn a_build_that_mxv_admits_asks_deadband_with_the_workspace_name() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"true\"]\n",
    );
    let socket = setup.root().join("d.sock");
    let daemon = daemon_that_reads_one_job(&socket);

    let output = setup
        .command_in(&setup.root(), &["up", "-w", "triage", "--json"])
        .env("DEADBAND_SOCKET", &socket)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let (job, _) = daemon.join().unwrap();
    assert_eq!(
        job.workspace.as_ref().map(|name| name.as_str()),
        Some("triage")
    );
    setup.json(&["down", "-w", "triage"]);
}

#[test]
fn a_job_run_by_hand_in_a_checkout_shows_the_workspace_that_mxv_marked() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", "");
    let deep = checkout.join("src");
    fs::create_dir_all(&deep).unwrap();
    let socket = setup.root().join("d.sock");
    start_daemon(&socket);
    let job = deadband::protocol::Job {
        label: "cargo".to_owned(),
        command: "cargo test".to_owned(),
        cwd: Some(deep.display().to_string()),
        ..holder()
    };

    let deadband::client::Admission::Granted(lease) =
        deadband::client::ask(&socket, &job, Duration::from_secs(10), |_, _| {
            panic!("the job waits")
        })
    else {
        panic!("the daemon did not grant the job");
    };

    let jobs = deadband::client::jobs(&socket, Duration::from_secs(10)).unwrap();
    assert_eq!(jobs.running.len(), 1, "{jobs:?}");
    assert_eq!(
        jobs.running[0].workspace.as_ref().map(|name| name.as_str()),
        Some("triage")
    );
    lease.end(Some(0));
}

#[test]
fn mxv_up_writes_a_missing_marker_and_leaves_a_directory_of_that_name_alone() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", "[processes.api]\nargv = [\"sleep\", \"60\"]\n");
    let marker = checkout.parent().unwrap().join(".deadband-workspace");
    fs::remove_file(&marker).unwrap();

    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!(marker.is_file());
    setup.json(&["down", "-w", "triage"]);

    fs::remove_file(&marker).unwrap();
    fs::create_dir(&marker).unwrap();
    let (exit, stderr) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(marker.is_dir());
    assert!(stderr.contains(".deadband-workspace"), "{stderr}");
    setup.json(&["down", "-w", "triage"]);
}

#[test]
fn a_failed_marker_write_warns_and_up_continues() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = Setup::new();
    let checkout = setup.workspace("triage", "[processes.api]\nargv = [\"sleep\", \"60\"]\n");
    let directory = checkout.parent().unwrap().to_path_buf();
    fs::remove_file(directory.join(".deadband-workspace")).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).unwrap();

    let (exit, stderr) = setup.errors(&["up", "-w", "triage"]);

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(exit, 0, "{stderr}");
    assert!(stderr.contains(".deadband-workspace"), "{stderr}");
    assert!(!directory.join(".deadband-workspace").exists());
    setup.json(&["down", "-w", "triage"]);
}

#[test]
fn a_build_waits_for_a_place_from_a_real_daemon_and_then_gets_its_job_variables() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"sh\", \"-c\", \"echo $DEADBAND_HELD $CARGO_BUILD_JOBS > seen\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let socket = setup.root().join("d.sock");
    start_daemon(&socket);
    let job = holder();
    let deadband::client::Admission::Granted(lease) =
        deadband::client::ask(&socket, &job, Duration::from_secs(10), |_, _| {
            panic!("the first job waits")
        })
    else {
        panic!("the daemon did not grant the first job");
    };

    std::thread::scope(|scope| {
        let up = scope.spawn(|| {
            let output = setup
                .command_in(&setup.root(), &["up", "-w", "triage", "--json"])
                .env("DEADBAND_SOCKET", &socket)
                .output()
                .unwrap();
            (
                output.status.code().unwrap(),
                String::from_utf8(output.stderr).unwrap(),
            )
        });
        for () in waiting() {
            let jobs = deadband::client::jobs(&socket, Duration::from_secs(10)).unwrap();
            if !jobs.waiting.is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        let jobs = deadband::client::jobs(&socket, Duration::from_secs(10)).unwrap();
        assert_eq!(jobs.waiting.len(), 1, "{jobs:?}");
        assert_eq!(jobs.waiting[0].label, "triage");
        assert!(!checkout.join("seen").exists());

        lease.end(Some(0));
        let (exit, stderr) = up.join().unwrap();

        assert_eq!(exit, 0, "{stderr}");
        assert!(stderr.contains("the job waits"), "{stderr}");
    });
    let seen = fs::read_to_string(checkout.join("seen")).unwrap();
    let words: Vec<&str> = seen.split_whitespace().collect();
    assert_eq!(words[0], "1", "{seen}");
    assert!(words[1].parse::<u32>().unwrap() >= 1, "{seen}");
}

/// An admission that grants at once and sends a build's output nowhere.
struct Granting;

/// The place of [`Granting`].
struct Granted;

impl multiverse::Place for Granted {
    fn variables(&self) -> Vec<(multiverse::phase::VariableName, String)> {
        Vec::new()
    }

    fn end(self: Box<Self>, _exit_code: Option<i32>) {}
}

impl multiverse::Admission for Granting {
    fn admit(&mut self, _asked: &multiverse::Asked<'_>) -> Box<dyn multiverse::Place> {
        Box::new(Granted)
    }

    fn output(&self) -> Box<dyn std::io::Write + Send> {
        Box::new(std::io::sink())
    }
}

#[test]
fn a_build_that_outlives_the_deadline_leaves_a_running_process_alone() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", &breakable(""));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let before = pid_of(&setup, "triage", "api");
    write(&checkout.join("broken"), "");
    let root = setup.root();
    let environment = mxv::Environment::read(
        |name| (name == "MXV_ROOT").then(|| root.join("workspaces").into_os_string()),
        Some(mxv::Bases {
            home: root.clone(),
            config: root.join("config"),
            state: root.join("state"),
            data: root.join("data"),
        }),
    );
    let runs = environment
        .runs_for_tests()
        .unwrap()
        .with_build_wait(Duration::from_millis(200));
    let store = multiverse::Store::new(root.join("state/mxv/workspaces"));
    let name: multiverse::WorkspaceName = "triage".parse().unwrap();
    let mut workspace = store.read(&name).unwrap().unwrap();
    let slow = breakable("").replace("test ! -e broken", "sleep 60");
    let manifest = multiverse::Manifest::parse(&slow, Path::new("mxv.toml")).unwrap();
    let lock = runs.lock(&name).unwrap();

    let end = runs
        .build(&lock, &store, &mut workspace, &manifest, &[], &mut Granting)
        .unwrap();

    assert!(
        matches!(
            end,
            multiverse::BuildEnd::Stopped {
                why: multiverse::BuildStop::TimedOut { .. },
                ..
            }
        ),
        "{end:?}"
    );
    drop(lock);
    assert_eq!(pid_of(&setup, "triage", "api"), before);
}

/// Runs `mxv` with `args` in `dir`, and returns the exit code, standard
/// output and standard error.
fn exec_output_in(setup: &Setup, dir: &Path, args: &[&str]) -> (i32, String, String) {
    let output = setup.command_in(dir, args).output().unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// Creates the workspace `multi` with the checkouts `app` and `second`,
/// which declare the ports `web` and `api`, and returns its directory.
fn two_checkouts_with_ports(setup: &Setup) -> PathBuf {
    let (exit, document) = setup.json(&[
        "new", "multi", "--repo", "app", "--repo", "app", "--as", "second",
    ]);
    assert_eq!(exit, 0, "{document}");
    let workspace = setup.root().join("workspaces/multi");
    write(&workspace.join("app/mxv.toml"), "ports = [\"web\"]\n");
    write(&workspace.join("second/mxv.toml"), "ports = [\"api\"]\n");
    workspace
}

#[test]
fn exec_with_no_unit_runs_the_command_in_a_workspace_with_no_manifest() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let checkout = setup.root().join("workspaces/triage/app");

    let (exit, out, err) = exec_output(&setup, &["exec", "-w", "triage", "--", "pwd"]);

    assert_eq!(exit, 0, "{err}");
    assert_eq!(out.trim(), checkout.display().to_string());
}

#[test]
fn exec_with_no_unit_gives_the_command_the_ports_and_every_services_variables() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"web\"]\n\n\
         [service.queues]\nkind = \"sqs\"\nqueues.jobs = {}\nenv = { JOBS_QUEUE = \"jobs\" }\n\n\
         [service.events]\nkind = \"sqs\"\nqueues.events = {}\nenv = { EVENTS_QUEUE = \"events\" }\n\n\
         [processes.api]\nargv = [\"sleep\", \"60\"]\n",
    );

    let (exit, out, err) = exec_output(
        &setup,
        &[
            "exec",
            "-w",
            "triage",
            "--",
            "sh",
            "-c",
            "echo $MXV_PORT_WEB $JOBS_QUEUE $EVENTS_QUEUE",
        ],
    );

    assert_eq!(exit, 0, "{err}");
    let words: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(words.len(), 3, "{out}");
    assert!(words[0].parse::<u16>().is_ok(), "{out}");
    assert_eq!(words[1..], ["jobs", "events"]);
    assert!(!setup.started("api"), "mxv started a unit");
}

#[test]
fn exec_with_no_unit_runs_in_the_current_directory_inside_a_checkout() {
    let setup = Setup::new();
    let workspace = two_checkouts_with_ports(&setup);
    let sub = workspace.join("app/sub");
    fs::create_dir(&sub).unwrap();

    let (exit, out, err) = exec_output_in(
        &setup,
        &sub,
        &[
            "exec",
            "--",
            "sh",
            "-c",
            "pwd; echo $MXV_PORT_WEB $MXV_PORT_SECOND_API",
        ],
    );

    assert_eq!(exit, 0, "{err}");
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some(sub.display().to_string().as_str()));
    let ports: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(ports.len(), 2, "{out}");
}

#[test]
fn exec_with_repo_runs_at_the_top_of_that_checkout() {
    let setup = Setup::new();
    let workspace = two_checkouts_with_ports(&setup);

    let (exit, out, err) = exec_output_in(
        &setup,
        &workspace.join("app"),
        &[
            "exec",
            "--repo",
            "second",
            "--",
            "sh",
            "-c",
            "pwd; echo ${MXV_PORT_API:-none}",
        ],
    );

    assert_eq!(exit, 0, "{err}");
    let mut lines = out.lines();
    assert_eq!(
        lines.next(),
        Some(workspace.join("second").display().to_string().as_str())
    );
    assert!(lines.next().unwrap().parse::<u16>().is_ok(), "{out}");
}

#[test]
fn exec_with_repo_that_names_no_checkout_fails_with_checkout_not_found() {
    let setup = Setup::new();
    two_checkouts_with_ports(&setup);

    let (exit, _, err) = exec_output(
        &setup,
        &["exec", "-w", "multi", "--repo", "other", "--", "true"],
    );

    assert_ne!(exit, 0);
    assert!(err.contains("mxv::checkout::not_found"), "{err}");
}

#[test]
fn exec_outside_a_workspace_with_several_checkouts_runs_in_the_workspace_directory() {
    let setup = Setup::new();
    let workspace = two_checkouts_with_ports(&setup);

    let (exit, out, err) = exec_output(
        &setup,
        &[
            "exec",
            "-w",
            "multi",
            "--",
            "sh",
            "-c",
            "pwd; echo ${MXV_PORT_WEB:-none} $MXV_PORT_APP_WEB",
        ],
    );

    assert_eq!(exit, 0, "{err}");
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some(workspace.display().to_string().as_str()));
    let ports: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(ports[0], "none", "{out}");
    assert!(ports[1].parse::<u16>().is_ok(), "{out}");
}

#[test]
fn exec_with_no_unit_exits_with_the_commands_code_and_with_128_plus_its_signal() {
    let setup = Setup::new();
    setup.workspace("triage", "");

    let run =
        |script: &str| exec_output(&setup, &["exec", "-w", "triage", "--", "sh", "-c", script]).0;

    assert_eq!(run("exit 7"), 7);
    assert_eq!(run("kill -9 $$"), 137);
}

#[test]
fn exec_with_no_unit_takes_one_place_and_no_gate_asks_nothing() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", "");
    let socket = setup.root().join("d.sock");
    start_daemon(&socket);
    let job = holder();
    let deadband::client::Admission::Granted(lease) =
        deadband::client::ask(&socket, &job, Duration::from_secs(10), |_, _| {
            panic!("the first job waits")
        })
    else {
        panic!("the daemon did not grant the first job");
    };
    let mxv = env!("CARGO_BIN_EXE_mxv");

    // The only place is held, so this command ends only because it never
    // asks.
    let free = "echo ${DEADBAND_HELD:-none} > free";
    wait_for_exit(setup.spawn_through(
        mxv,
        &["exec", "-w", "triage", "--no-gate", "--", "sh", "-c", free],
        &socket,
    ));
    assert_eq!(
        fs::read_to_string(checkout.join("free")).unwrap().trim(),
        "none"
    );

    let held = "echo $DEADBAND_HELD $MISE_JOBS > held";
    let waiting_job = setup.spawn_through(
        mxv,
        &["exec", "-w", "triage", "--", "sh", "-c", held],
        &socket,
    );
    let queued = waiting().any(|()| {
        let jobs = deadband::client::jobs(&socket, Duration::from_secs(10)).unwrap();
        jobs.waiting.len() == 1
    });
    assert!(queued, "mxv never joined the queue");
    assert!(!checkout.join("held").exists());
    lease.end(Some(0));
    wait_for_exit(waiting_job);
    assert_eq!(
        fs::read_to_string(checkout.join("held")).unwrap().trim(),
        "1 1"
    );
}

#[test]
fn exec_with_no_unit_runs_with_no_place_when_the_daemon_cannot_be_reached() {
    let setup = Setup::new();
    setup.workspace("triage", "");

    let (exit, out, err) = exec_output(
        &setup,
        &[
            "exec",
            "-w",
            "triage",
            "--",
            "sh",
            "-c",
            "echo ${DEADBAND_HELD:-none}",
        ],
    );

    assert_eq!(exit, 0, "{err}");
    assert_eq!(out.trim(), "none");
}

#[test]
fn exec_with_no_unit_does_not_pass_a_variable_outside_the_allowlist() {
    let setup = Setup::new();
    setup.workspace("triage", "");

    let output = setup
        .command_in(
            &setup.root(),
            &[
                "exec",
                "-w",
                "triage",
                "--",
                "sh",
                "-c",
                "echo ${OUTSIDE_THE_ALLOWLIST:-none}",
            ],
        )
        .env("OUTSIDE_THE_ALLOWLIST", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "none");
}

#[test]
fn exec_with_a_unit_still_fails_when_the_unit_is_stopped() {
    let setup = Setup::new();
    setup.workspace("triage", EXEC_MANIFEST);

    let (exit, _, err) = exec_output(&setup, &["exec", "-w", "triage", "api", "--", "true"]);

    assert_eq!(exit, 1, "{err}");
    assert!(err.contains("mxv::process::not_running"), "{err}");
}

#[test]
fn a_missing_program_with_no_unit_fails_with_unstartable_and_names_the_workspace() {
    let setup = Setup::new();
    setup.workspace("triage", "");

    let (exit, _, err) = exec_output(
        &setup,
        &["exec", "-w", "triage", "--", "/nonexistent/program"],
    );

    assert_eq!(exit, 1, "{err}");
    assert!(err.contains("mxv::exec::unstartable"), "{err}");
    assert!(err.contains("in the workspace triage"), "{err}");
    assert!(!err.contains("in the context of"), "{err}");
}

#[test]
fn remove_during_an_ad_hoc_command_removes_the_workspace_and_the_command_runs_on() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let root = setup.root();
    let script = format!(
        "touch {root}/started; while [ ! -e {root}/finish ]; do sleep 0.05; done; exit 3",
        root = root.display()
    );
    let mut child = setup.spawn_through(
        env!("CARGO_BIN_EXE_mxv"),
        &[
            "exec",
            "-w",
            "triage",
            "--no-gate",
            "--",
            "sh",
            "-c",
            &script,
        ],
        Path::new(MISSING_SOCKET),
    );
    assert!(
        waiting().any(|()| root.join("started").exists()),
        "the command never started"
    );

    let (exit, document) = setup.json(&["remove", "triage", "--yes"]);
    let ran_on = child.try_wait().unwrap().is_none();
    // The command ends here whatever the assertions find, so a failure
    // leaves no process behind.
    write(&root.join("finish"), "");

    assert_eq!(exit, 0, "{document}");
    assert!(!root.join("workspaces/triage").exists());
    assert!(ran_on, "the command ended when the workspace was removed");
    for () in waiting() {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(3));
            return;
        }
    }
    let _ = child.kill();
    panic!("mxv did not end within the test's limit");
}

impl Setup {
    /// Runs `up`, `restart` and `exec` from now on without the grant that a
    /// person at a terminal would give.
    fn without_grants(&self) {
        self.ungranted.store(true, Ordering::Relaxed);
    }

    /// Returns the names of the files in phase's grants directory.
    fn grant_files(&self) -> Vec<String> {
        fs::read_dir(self.root().join("state/mxv/phase/grants")).map_or_else(
            |_| Vec::new(),
            |entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.ends_with(".json"))
                    .collect()
            },
        )
    }
}

/// Returns the `asks` field of the failure `document`, as strings.
fn asks(document: &Value) -> Vec<String> {
    document["fields"]["asks"]
        .as_array()
        .unwrap_or_else(|| panic!("no asks in {document}"))
        .iter()
        .map(|ask| ask.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_first_up_of_a_repository_without_a_terminal_lists_every_host_unit_and_starts_nothing() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    setup.without_grants();

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::permissions::not_granted");
    assert_eq!(document["fields"]["workspace"], "triage");
    assert_eq!(document["fields"]["repository"], "app");
    assert_eq!(document["fields"]["declined"], false);
    assert_eq!(
        asks(&document),
        [
            "server, a new unit, asks to run outside the sandbox, with everything your account can reach",
            "worker, a new unit, asks to run outside the sandbox, with everything your account can reach",
        ]
    );
    assert_eq!(document["recovery"]["kind"], "terminal", "{document}");
    assert!(
        document["recovery"]
            .to_string()
            .contains("mxv up --workspace triage"),
        "{document}"
    );
    assert!(setup.grant_files().is_empty(), "{:?}", setup.grant_files());
    assert!(!setup.started("server"));
    assert!(!setup.started("worker"));
}

#[test]
fn restart_without_a_grant_refuses_and_runs_no_build() {
    let setup = Setup::new();
    let checkout = setup.workspace("triage", &breakable(""));
    setup.without_grants();

    let (exit, document) = setup.json(&["restart", "-w", "triage"]);

    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::permissions::not_granted");
    assert!(!setup.started("api"));
    write(&checkout.join("broken"), "");
    let (exit, document) = setup.json(&["restart", "-w", "triage"]);
    assert_eq!(document["code"], "mxv::permissions::not_granted");
    assert_eq!(exit, 1, "{document}");
}

#[test]
fn a_grant_given_for_one_workspace_lets_another_workspace_of_the_repository_start() {
    let setup = Setup::new();
    setup.workspace("triage", &server_and_worker());
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(setup.grant_files().len(), 1, "{:?}", setup.grant_files());
    setup.workspace("second", &server_and_worker());
    setup.without_grants();

    let (exit, document) = setup.json(&["up", "-w", "second"]);

    assert_eq!(exit, 0, "{document}");
    assert_eq!(setup.grant_files().len(), 1, "{:?}", setup.grant_files());
}

#[test]
fn a_unit_new_to_the_manifest_asks_alone_and_the_running_units_keep_their_pids() {
    let setup = Setup::new();
    let manifest = pair("echo ready; exec sleep 60", "echo ready; exec sleep 60");
    setup.workspace("triage", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let first = pid_of(&setup, "triage", "first");
    let second = pid_of(&setup, "triage", "second");
    setup.workspace_manifest(
        "triage",
        &format!("{manifest}\n[processes.third]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n"),
    );
    setup.without_grants();

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        asks(&document),
        [
            "third, a new unit, asks to run outside the sandbox, with everything your account can reach"
        ]
    );
    assert!(!setup.started("third"));
    assert_eq!(pid_of(&setup, "triage", "first"), first);
    assert_eq!(pid_of(&setup, "triage", "second"), second);
    // A unit that the grant already covers starts, because `up` of it
    // asks for nothing.
    let (exit, document) = setup.json(&["up", "-w", "triage", "second"]);
    assert_eq!(exit, 0, "{document}");
}

/// A manifest whose `api` depends on `store`, and whose `lone` depends on
/// nothing. `api` runs `api_script`.
fn store_and_api(api_script: &str) -> String {
    format!(
        "[processes.store]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nready = {{ log = \"^ready$\" }}\n\n[processes.api]\nargv = [\"sh\", \"-c\", \"{api_script}\"]\nready = {{ log = \"^ready$\" }}\ndepends_on = [\"store\"]\n\n[processes.lone]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nready = {{ log = \"^ready$\" }}\n"
    )
}

/// Returns the state of the unit `unit` that `mxv status` reports.
fn state_of(setup: &Setup, unit: &str) -> String {
    setup
        .states("triage")
        .into_iter()
        .find(|(name, _)| name == unit)
        .map(|(_, state)| state)
        .unwrap_or_else(|| panic!("no unit {unit}"))
}

#[test]
fn down_keeps_a_named_unit_that_a_running_unit_needs_and_stops_one_that_nothing_needs() {
    let setup = Setup::new();
    setup.workspace("triage", &store_and_api("echo ready; exec sleep 60"));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let store = pid_of(&setup, "triage", "store");

    let (exit, document) = setup.json(&["down", "-w", "triage", "store", "lone"]);

    assert_eq!(exit, 1, "{document}");
    let text = document.to_string();
    assert!(text.contains("mxv::process::still_needed"), "{text}");
    assert!(text.contains("\"needed_by\":[\"api\"]"), "{text}");
    assert_eq!(state_of(&setup, "store"), "running");
    assert_eq!(pid_of(&setup, "triage", "store"), store);
    assert_eq!(state_of(&setup, "api"), "running");
    assert_eq!(state_of(&setup, "lone"), "stopped");
}

#[test]
fn down_of_a_unit_with_its_dependants_stops_both_and_restart_of_a_needed_unit_restarts_it() {
    let setup = Setup::new();
    setup.workspace("triage", &store_and_api("echo ready; exec sleep 60"));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let store = pid_of(&setup, "triage", "store");

    let (restarted, restart) = setup.json(&["restart", "-w", "triage", "store"]);
    let (exit, document) = setup.json(&["down", "-w", "triage", "api", "store"]);

    assert_eq!(restarted, 0, "{restart}");
    assert_ne!(row(&restart, "store")["pid"], store, "{restart}");
    assert_eq!(exit, 0, "{document}");
    assert_eq!(state_of(&setup, "store"), "stopped");
    assert_eq!(state_of(&setup, "api"), "stopped");
    assert_eq!(state_of(&setup, "lone"), "running");
}

#[test]
fn down_stops_a_unit_whose_only_dependant_has_exited() {
    let setup = Setup::new();
    setup.workspace("triage", &store_and_api("echo ready; sleep 1; exit 0"));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    for () in waiting() {
        if state_of(&setup, "api") != "running" {
            break;
        }
    }
    assert_ne!(state_of(&setup, "api"), "running");

    let (exit, document) = setup.json(&["down", "-w", "triage", "store"]);

    assert_eq!(exit, 0, "{document}");
    assert_eq!(state_of(&setup, "store"), "stopped");
}

#[test]
fn down_keeps_a_needed_unit_when_the_manifest_no_longer_parses() {
    let setup = Setup::new();
    setup.workspace("triage", &store_and_api("echo ready; exec sleep 60"));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    setup.workspace_manifest("triage", "[processes.store\n");

    let (exit, document) = setup.json(&["down", "-w", "triage", "store"]);

    assert_eq!(exit, 1, "{document}");
    assert!(
        document.to_string().contains("mxv::process::still_needed"),
        "{document}"
    );
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
}

/// The manifest of a workspace with one unit that sleeps and one task.
const SLEEPER_AND_TASK: &str = "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"exec sleep 300\"]\n\n[tasks.check]\nargv = [\"true\"]\n";

/// Sets the state of the record of the workspace `name` to `state`, as a
/// `mxv new` that stopped before its last write leaves `creating`.
fn set_state(setup: &Setup, name: &str, state: &str) {
    let path = setup
        .root()
        .join("state/mxv/workspaces")
        .join(format!("{name}.json"));
    let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record["state"] = Value::from(state);
    write(&path, &serde_json::to_string_pretty(&record).unwrap());
}

/// The commands that start, run or change a workspace's units, which
/// refuse a workspace that `mxv new` did not finish.
/// `ports reset` comes first, because it refuses while a unit runs.
const LIVE_ONLY: &[&[&str]] = &[
    &["ports", "reset", "-w", "triage"],
    &["pin", "-w", "triage"],
    &["run", "-w", "triage", "check"],
    &["exec", "-w", "triage", "--no-gate", "--", "true"],
    &["up", "-w", "triage"],
    &["restart", "-w", "triage"],
    &["exec", "-w", "triage", "sleeper", "--", "true"],
];

#[test]
fn up_restart_ports_reset_pin_run_and_exec_refuse_a_workspace_that_mxv_new_did_not_finish() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    set_state(&setup, "triage", "creating");
    for args in LIVE_ONLY {
        let (exit, errors) = setup.errors(args);
        assert_eq!(exit, 1, "{args:?}: {errors}");
        assert!(
            errors.contains("mxv::workspace::creating"),
            "{args:?}: {errors}"
        );
        assert!(errors.contains("mxv remove triage"), "{args:?}: {errors}");
    }
    assert!(!setup.started("sleeper"));
    let record = fs::read_to_string(setup.root().join("state/mxv/workspaces/triage.json")).unwrap();
    assert!(!record.contains("pinned"), "{record}");
}

#[test]
fn up_restart_ports_reset_pin_run_and_exec_run_on_a_live_workspace() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    set_state(&setup, "triage", "live");
    for args in LIVE_ONLY {
        let (exit, errors) = setup.errors(args);
        assert_eq!(exit, 0, "{args:?}: {errors}");
    }
    assert!(setup.started("sleeper"));
}

#[test]
fn down_and_status_work_on_a_workspace_that_mxv_new_did_not_finish() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    set_state(&setup, "triage", "creating");
    for args in [
        &["status", "-w", "triage"][..],
        &["down", "-w", "triage"][..],
    ] {
        let (exit, document) = setup.json(args);
        assert_eq!(exit, 0, "{args:?}: {document}");
    }
}

impl Setup {
    /// Returns the path of the record of the workspace `name`.
    fn record_path(&self, name: &str) -> PathBuf {
        self.root()
            .join("state/mxv/workspaces")
            .join(format!("{name}.json"))
    }

    /// Returns the record of the workspace `name`, as JSON.
    fn record(&self, name: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(self.record_path(name)).unwrap()).unwrap()
    }

    /// Returns the path of the archive record of `name`.
    fn archive_path(&self, name: &str) -> PathBuf {
        self.root()
            .join("state/mxv/archives")
            .join(name)
            .join("archive.json")
    }

    /// Rewrites the record of the workspace `name` without a stable id and
    /// a state, at version 2, as an older mxv wrote it.
    fn strip_id(&self, name: &str) {
        let mut record = self.record(name);
        let fields = record.as_object_mut().unwrap();
        fields.remove("id");
        fields.remove("state");
        fields.insert("version".to_owned(), 2.into());
        write(
            &self.record_path(name),
            &serde_json::to_string_pretty(&record).unwrap(),
        );
    }

    /// Writes beside the workspace `name` the record of an archive of it
    /// that an older mxv started and did not finish: retiring, at version
    /// 2, with no workspace id.
    fn plant_older_archive(&self, name: &str) {
        let archive = serde_json::json!({
            "version": 2,
            "id": "a".repeat(32),
            "state": "retiring",
            "time": 0,
            "name": name,
            "kind": "clone",
            "path": self.root().join("workspaces").join(name),
            "checkouts": [],
        });
        let path = self.archive_path(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write(&path, &serde_json::to_string_pretty(&archive).unwrap());
    }
}

#[test]
fn pin_ports_reset_and_down_beside_an_older_unfinished_archive_without_ids_leave_the_record_without_an_id()
 {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    setup.strip_id("triage");
    setup.plant_older_archive("triage");
    let mut record = setup.record("triage");
    record["reclaimed"] = serde_json::json!({ "processes": ["sleeper"] });
    write(
        &setup.record_path("triage"),
        &serde_json::to_string_pretty(&record).unwrap(),
    );
    for args in [
        &["pin", "-w", "triage"][..],
        &["ports", "reset", "-w", "triage"][..],
        &["down", "-w", "triage"][..],
    ] {
        let (exit, errors) = setup.errors(args);
        assert_eq!(exit, 0, "{args:?}: {errors}");
    }
    let record = setup.record("triage");
    assert_eq!(record["id"], Value::Null, "{record}");
    assert_eq!(record["pinned"], true, "{record}");
    assert_eq!(record["reclaimed"], Value::Null, "{record}");
}

#[test]
fn up_restart_and_run_beside_an_older_unfinished_archive_without_ids_refuse_until_archive_resumes()
{
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    setup.strip_id("triage");
    setup.plant_older_archive("triage");
    for args in [
        &["up", "-w", "triage"][..],
        &["restart", "-w", "triage"][..],
        &["run", "-w", "triage", "check"][..],
    ] {
        let (exit, errors) = setup.errors(args);
        assert_eq!(exit, 1, "{args:?}: {errors}");
        assert!(
            errors.contains("mxv::workspace::archive_unfinished"),
            "{args:?}: {errors}"
        );
        assert!(errors.contains("mxv archive triage"), "{args:?}: {errors}");
        assert!(!errors.contains("DockerFailed"), "{args:?}: {errors}");
    }
    assert!(!setup.started("sleeper"));
    let (exit, document) = setup.json(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let archive: Value =
        serde_json::from_str(&fs::read_to_string(setup.archive_path("triage")).unwrap()).unwrap();
    assert_eq!(archive["version"], 3, "{archive}");
    assert_eq!(archive["state"], "archived", "{archive}");
    assert!(archive["workspace_id"].is_string(), "{archive}");
}

#[test]
fn up_of_a_record_without_an_id_and_no_archive_mints_the_id() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    setup.strip_id("triage");
    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{errors}");
    assert!(setup.record("triage")["id"].is_string());
    assert!(setup.started("sleeper"));
}

#[test]
fn up_of_a_record_without_an_id_beside_an_archive_record_that_cannot_be_read_names_mxv_remove() {
    let setup = Setup::new();
    setup.workspace("triage", SLEEPER_AND_TASK);
    setup.strip_id("triage");
    let path = setup.archive_path("triage");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write(&path, "not a record\n");
    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{errors}");
    assert!(
        errors.contains("mxv::workspace::archive_unfinished"),
        "{errors}"
    );
    assert!(errors.contains("mxv remove triage"), "{errors}");
    assert!(!setup.started("sleeper"));
    assert_eq!(setup.record("triage")["id"], Value::Null);
}
