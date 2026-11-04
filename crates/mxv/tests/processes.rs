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

    /// Creates the workspace `name` and writes `manifest` as the `mxv.toml`
    /// of its checkout, and returns the checkout's path.
    fn workspace(&self, name: &str, manifest: &str) -> PathBuf {
        let (exit, document) = self.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        let checkout = self.root().join("workspaces").join(name).join("app");
        write(&checkout.join("mxv.toml"), manifest);
        checkout
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
fn a_process_that_exits_before_it_is_ready_fails_up_with_its_code_and_log_tail() {
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
    assert_eq!(document["fields"]["tail"][0], "cannot bind");
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
    assert_eq!(document["fields"]["tail"][0], "starting");
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
    let record = setup.root().join("state/mxv/runs/triage/server.json");
    std::thread::scope(|scope| {
        let up = scope.spawn(|| setup.json(&["up", "-w", "triage"]));
        while !record.exists() && !up.is_finished() {
            std::thread::yield_now();
        }
        if !record.exists() {
            panic!(
                "up ended before it wrote the run record: {:?}",
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

#[test]
fn down_stops_the_children_that_an_exited_process_left_in_its_group() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.wrapper]\nargv = [\"sh\", \"-c\", \"sleep 60 & sleep 1; exit 0\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let pid = row(&document, "wrapper")["pid"].as_u64().unwrap();
    for () in waiting() {
        if row(&setup.json(&["status", "-w", "triage"]).1, "wrapper")["state"] == "exited" {
            break;
        }
    }
    assert!(group_exists(pid), "the child should outlive its wrapper");
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(row(&status, "wrapper")["group_alive"], true, "{status}");
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!(
        !group_exists(pid),
        "down left the child of the exited wrapper"
    );
}

#[test]
fn a_port_held_by_a_leftover_child_of_the_workspace_names_that_process_and_not_an_outside_program()
{
    let setup = Setup::new();
    let manifest = format!(
        "ports = [\"api\"]\n[processes.server]\nargv = [\"sh\", \"-c\", \"\\\"$0\\\" & sleep 2; exit 0\", \"{}\"]\nports = {{ PORT = \"api\" }}\nlisten = \"api\"\nready = {{ timeout_seconds = 20 }}\n",
        listen()
    );
    setup.workspace("triage", &manifest);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    for () in waiting() {
        if row(&setup.json(&["status", "-w", "triage"]).1, "server")["state"] == "exited" {
            break;
        }
    }
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::port::taken");
    assert_eq!(document["fields"]["holders"], serde_json::json!(["server"]));
    let message = document["message"].as_str().unwrap();
    assert!(!message.contains("outside"), "{message}"); // The recovery is a command whose words are separate, so running it
    // word by word stops the process that holds the port.
    let call = document["recovery"]["call"].as_str().unwrap();
    assert_eq!(call, "mxv down --workspace triage server");
    let words: Vec<&str> = call.split_whitespace().skip(1).collect();
    let (exit, document) = setup.json(&words);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
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
fn each_up_moves_the_previous_log_to_log_1() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.chatty]\nargv = [\"sh\", \"-c\", \"echo run; exec sleep 60\"]\n",
    );
    setup.json(&["up", "-w", "triage"]);
    setup.json(&["down", "-w", "triage"]);
    setup.json(&["up", "-w", "triage"]);
    let runs = setup.root().join("state/mxv/runs/triage");
    assert_eq!(
        fs::read_to_string(runs.join("chatty.log.1")).unwrap(),
        "run\n"
    );
    assert!(runs.join("chatty.log").exists());
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

/// Returns once more than a second has passed. `ps` prints a start time to
/// the second on macOS, so two processes that start within one second have
/// the same recorded start time.
fn after_a_second() {
    let deadline = Instant::now() + Duration::from_millis(1100);
    while Instant::now() < deadline {
        std::thread::yield_now();
    }
}

/// Rewrites the pid of the run record of `process` in the workspace `name`
/// to `pid`, as a record left behind looks once another process took its
/// id.
fn rewrite_pid(setup: &Setup, name: &str, process: &str, pid: u64) {
    let path = setup
        .root()
        .join(format!("state/mxv/runs/{name}/{process}.json"));
    let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record["pid"] = pid.into();
    write(&path, &record.to_string());
}

#[test]
fn down_leaves_alone_a_group_whose_leader_took_the_pid_of_a_stale_record() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.short]\nargv = [\"sh\", \"-c\", \"sleep 1\"]\n",
    );
    setup.workspace(
        "reading-list",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "reading-list"]);
    assert_eq!(exit, 0, "{document}");
    let other = row(&document, "api")["pid"].as_u64().unwrap();
    after_a_second();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    for () in waiting() {
        if row(&setup.json(&["status", "-w", "triage"]).1, "short")["state"] == "exited" {
            break;
        }
    }
    rewrite_pid(&setup, "triage", "short", other);

    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(row(&status, "short")["state"], "exited", "{status}");
    assert_eq!(row(&status, "short")["group_alive"], false, "{status}");
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert!(
        group_exists(other),
        "down killed another workspace's process"
    );
    assert!(
        !setup
            .root()
            .join("state/mxv/runs/triage/short.json")
            .exists(),
        "down keeps the stale record"
    );
    let (_, status) = setup.json(&["status", "-w", "reading-list"]);
    assert_eq!(row(&status, "api")["state"], "running", "{status}");
}

#[test]
fn a_stale_record_does_not_count_as_running_for_remove_or_logs_follow() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.short]\nargv = [\"sh\", \"-c\", \"echo done; sleep 1\"]\n",
    );
    setup.workspace(
        "reading-list",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n",
    );
    let (_, document) = setup.json(&["up", "-w", "reading-list"]);
    let other = row(&document, "api")["pid"].as_u64().unwrap();
    after_a_second();
    setup.json(&["up", "-w", "triage"]);
    for () in waiting() {
        if row(&setup.json(&["status", "-w", "triage"]).1, "short")["state"] == "exited" {
            break;
        }
    }
    rewrite_pid(&setup, "triage", "short", other);
    let (exit, text) = setup.text(&["logs", "-w", "triage", "short", "-f"]);
    assert_eq!(exit, 0, "logs -f must end: {text}");
    assert!(text.contains("done"), "{text}");
    let (exit, document) = setup.json(&["ports", "reset", "-w", "triage", "--yes"]);
    assert_ne!(
        document["code"], "mxv::workspace::running",
        "{exit} {document}"
    );
    assert!(group_exists(other));
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
        text.contains("the processes come from the run records"),
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
    let run = setup.root().join("state/mxv/runs/triage/server.json");
    let second = std::thread::scope(|scope| {
        let first = scope.spawn(|| setup.json(&["up", "-w", "triage"]));
        while !run.exists() && !first.is_finished() {
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
fn group_alive_is_null_while_the_process_runs() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.api]\nargv = [\"sh\", \"-c\", \"exec sleep 60\"]\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        row(&document, "api")["group_alive"],
        Value::Null,
        "{document}"
    );
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "api")["group_alive"], false, "{document}");
}

#[test]
fn up_in_text_prints_the_last_lines_of_the_log_of_a_process_that_exited_before_it_was_ready() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "[processes.build]\nargv = [\"sh\", \"-c\", \"echo compiling; echo 'error: cannot find value x'; exit 101\"]\n",
    );
    let (exit, errors) = setup.errors(&["up", "-w", "triage"]);
    assert_eq!(exit, 1, "{errors}");
    let compiling = errors
        .find("compiling")
        .unwrap_or_else(|| panic!("{errors}"));
    let error = errors
        .find("error: cannot find value x")
        .unwrap_or_else(|| panic!("{errors}"));
    assert!(compiling < error, "{errors}");
}

#[test]
fn an_env_value_receives_the_host_port_that_its_placeholder_names() {
    let setup = Setup::new();
    setup.workspace(
        "triage",
        "ports = [\"api\"]\n[processes.client]\nargv = [\"sh\", \"-c\", \"echo url=$API_URL; exec sleep 60\"]\nenv = { API_URL = \"http://127.0.0.1:{port.api}/v1?q={{x}}\" }\nready = { log = \"^url=\", timeout_seconds = 20 }\n",
    );
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let port = setup.port("triage", "api");
    let (_, logs) = setup.text(&["logs", "-w", "triage", "client"]);
    assert_eq!(logs, format!("url=http://127.0.0.1:{port}/v1?q={{x}}\n"));
    let (_, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(
        row(&status, "client")["ports"][0],
        format!("{{port.api}}={port}"),
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
    assert_eq!(document["fields"]["tail"][0], "starting");
    assert_eq!(
        setup.states("triage"),
        [("worker".to_owned(), "stopped".to_owned())]
    );
}
