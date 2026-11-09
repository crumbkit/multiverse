//! End-to-end tests of the process verbs `up`, `down`, `restart`, `status`
//! and `logs`, and of how `show` and `remove` treat a workspace's
//! processes. Each test builds a repository and a workspace of it in a
//! temporary directory, writes an `mxv.toml` into the workspace's checkout,
//! and runs the built `mxv` binary there. The processes are `sh -c`
//! fixtures and the `listen` example, a tiny TCP server that cargo builds
//! with the tests.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
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
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        repository(&dir.path().canonicalize().unwrap());
        let ports = PortBlock::claim();
        let setup = Self { dir, ports };
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

    fn command_in(&self, dir: &Path, args: &[&str]) -> Command {
        let root = self.root();
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
    }

    /// Starts `mxv up` for the workspace `name` as a child process that the
    /// test can signal, with its standard error discarded.
    #[expect(
        clippy::disallowed_types,
        reason = "the test starts mxv as a child so that it can signal it"
    )]
    fn spawn_up(&self, name: &str) -> std::process::Child {
        self.spawn_up_on(name, Path::new("/nonexistent/deadband.sock"))
    }

    /// Starts `mxv up` as [`Setup::spawn_up`] does, with `socket` as the
    /// deadband daemon's socket.
    #[expect(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "the test starts mxv as a child so that it can signal it"
    )]
    fn spawn_up_on(&self, name: &str, socket: &Path) -> std::process::Child {
        let root = self.root();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"));
        command
            .args(["up", "-w", name])
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

#[test]
fn run_is_no_longer_a_command() {
    let setup = Setup::new();
    let (exit, _) = setup.text(&["run"]);
    assert_eq!(exit, 2);
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
    let output = setup.command_in(&setup.root(), args).output().unwrap();
    (
        output.status.code().unwrap(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
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
    let job = deadband::protocol::Job {
        label: "holder".to_owned(),
        pid: std::process::id(),
        command: "the job that holds the one place".to_owned(),
        lane: deadband::admit::Lane::default(),
    };
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

#[test]
fn a_build_waits_for_a_place_from_a_real_daemon_and_then_gets_its_job_variables() {
    let setup = Setup::new();
    let manifest = "[processes.api]\nargv = [\"sleep\", \"60\"]\nbuild = [\"sh\", \"-c\", \"echo $DEADBAND_HELD $CARGO_BUILD_JOBS > seen\"]\n";
    let checkout = setup.workspace("triage", manifest);
    let socket = setup.root().join("d.sock");
    start_daemon(&socket);
    let job = deadband::protocol::Job {
        label: "holder".to_owned(),
        pid: std::process::id(),
        command: "the job that holds the one place".to_owned(),
        lane: deadband::admit::Lane::default(),
    };
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
