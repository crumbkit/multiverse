//! End-to-end tests of `mxv reclaim`, `mxv pin` and `mxv unpin`. Each test
//! builds a repository and workspaces of it in a temporary directory,
//! writes an `mxv.toml` whose process sleeps, starts it with `mxv up`, and
//! runs the built `mxv` binary there.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};
use crate::test_ports::PortBlock;

/// The manifest of each workspace: one process that sleeps.
const SLEEPER: &str = "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"exec sleep 600\"]\n";

/// Writes `text` to the file at `path`, and creates its directory.
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, text).unwrap();
}

/// Waits for `wait`, so that a workspace that nothing touches meanwhile
/// becomes idle for an idle age shorter than `wait`.
fn wait(wait: Duration) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test lets real time pass, because the idle rule reads real file and touch times"
    )]
    std::thread::sleep(wait);
}

/// A temporary directory with a repository, a workspace root and a state
/// directory, whose config file makes `mxv up` allocate from a block of
/// ports that no other running test uses.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The block of ports that the config file names.
    _ports: PortBlock,
    /// The processes outside phase that a test started.
    children: Vec<u32>,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clone = repository(&dir.path().canonicalize().unwrap());
        write(&clone.join(".git/info/exclude"), "target/\nmxv.toml\n");
        let ports = PortBlock::claim();
        let setup = Self {
            dir,
            _ports: ports,
            children: Vec::new(),
        };
        write(
            &setup.root().join("config/mxv/config.toml"),
            &setup._ports.config_line(),
        );
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn checkout(&self, name: &str) -> PathBuf {
        self.root().join("workspaces").join(name).join("app")
    }

    /// Creates the workspace `name` with the sleeper's manifest.
    fn workspace(&self, name: &str) -> PathBuf {
        let (exit, document) = self.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        let checkout = self.checkout(name);
        write(&checkout.join("mxv.toml"), SLEEPER);
        checkout
    }

    /// Creates the workspace `name` and starts its sleeper.
    fn running(&self, name: &str) -> PathBuf {
        let checkout = self.workspace(name);
        let (exit, document) = self.json(&["up", "-w", name]);
        assert_eq!(exit, 0, "{document}");
        checkout
    }

    fn command(&self, args: &[&str]) -> Command {
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
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?} printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Returns the state of the sleeper of the workspace `name`.
    fn state(&self, name: &str) -> String {
        let (_, document) = self.json(&["status", "-w", name]);
        document["data"][0]["state"].as_str().unwrap().to_owned()
    }

    /// Starts `sleep` with its working directory `dir`, as an agent's shell
    /// that works in a workspace would be.
    fn shell_in(&mut self, dir: &Path) -> u32 {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts a process outside phase that works in the workspace"
        )]
        let child = std::process::Command::new("sleep")
            .arg("600")
            .current_dir(dir)
            .env("PATH", PATH)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.children.push(pid);
        pid
    }
}

impl Drop for Setup {
    /// Stops the workspaces' processes and the processes outside phase that
    /// the test started.
    fn drop(&mut self) {
        let (_, listed) = self.json(&["list"]);
        for row in listed["data"].as_array().cloned().unwrap_or_default() {
            let _ = self.json(&["down", "-w", row["name"].as_str().unwrap_or_default()]);
        }
        for pid in &self.children {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .env("PATH", PATH)
                .output();
        }
    }
}

/// Returns the row of the workspace `name` in `document`.
fn row<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {document}"))
}

/// Starts the workspaces `active` and `idle`, lets more than two seconds
/// pass, and then names `active` in an mxv command, so with an idle age of
/// two seconds only `idle` is idle.
fn active_and_idle() -> Setup {
    let setup = Setup::new();
    setup.running("active");
    setup.running("idle");
    wait(Duration::from_secs(3));
    assert_eq!(setup.state("active"), "running");
    setup
}

#[test]
fn a_dry_run_names_only_the_idle_workspace_and_stops_nothing() {
    let setup = active_and_idle();
    let (exit, document) = setup.json(&[
        "reclaim",
        "--need",
        "memory=1K",
        "--idle",
        "2s",
        "--dry-run",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["schema"], "mxv/reclaim/v1");
    let idle = row(&document, "idle");
    assert_eq!(idle["state"], "would_stop", "{document}");
    assert!(idle["freed"].as_u64().unwrap() > 0, "{document}");
    let why = idle["reason"].as_str().unwrap();
    assert!(why.contains("no process outside phase"), "{why}");
    let active = row(&document, "active");
    assert_eq!(active["state"], "kept", "{document}");
    assert_eq!(active["freed"], 0, "{document}");
    let reason = active["reason"].as_str().unwrap();
    assert!(reason.contains("an mxv command named it"), "{reason}");
    assert_eq!(setup.state("idle"), "running");
    assert_eq!(setup.state("active"), "running");
}

#[test]
fn reclaim_stops_the_idle_workspace_and_marks_it_and_up_starts_it_again() {
    let setup = active_and_idle();
    let (exit, document) = setup.json(&["reclaim", "--need", "memory=1K", "--idle", "2s"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(row(&document, "idle")["state"], "stopped", "{document}");
    assert_eq!(row(&document, "active")["state"], "kept", "{document}");
    let (_, status) = setup.json(&["status", "-w", "idle"]);
    assert_eq!(status["data"][0]["state"], "stopped", "{status}");
    assert_eq!(
        status["summary"]["reclaimed"]["processes"][0], "sleeper",
        "{status}"
    );
    assert_eq!(setup.state("active"), "running");
    let (_, status) = setup.json(&["status", "-w", "active"]);
    assert_eq!(status["summary"]["reclaimed"], Value::Null, "{status}");
    let (exit, document) = setup.json(&["up", "-w", "idle"]);
    assert_eq!(exit, 0, "{document}");
    let (_, status) = setup.json(&["status", "-w", "idle"]);
    assert_eq!(status["data"][0]["state"], "running", "{status}");
    assert_eq!(status["summary"]["reclaimed"], Value::Null, "{status}");
}

#[test]
fn a_process_outside_phase_that_works_in_a_workspace_keeps_it_running() {
    let mut setup = Setup::new();
    let checkout = setup.running("busy");
    let pid = setup.shell_in(&checkout);
    let (exit, document) = setup.json(&["reclaim", "--need", "memory=1K", "--idle", "0s"]);
    assert_eq!(exit, 0, "{document}");
    let busy = row(&document, "busy");
    assert_eq!(busy["state"], "kept", "{document}");
    let reason = busy["reason"].as_str().unwrap();
    assert!(reason.contains(&pid.to_string()), "{reason}");
    assert_eq!(setup.state("busy"), "running");
}

#[test]
fn a_pinned_workspace_is_never_chosen_until_it_is_unpinned() {
    let setup = Setup::new();
    setup.running("kept");
    let (exit, document) = setup.json(&["pin", "-w", "kept"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["pinned"], true, "{document}");
    let (_, document) = setup.json(&["reclaim", "--need", "memory=1K", "--idle", "0s"]);
    let kept = row(&document, "kept");
    assert_eq!(kept["state"], "kept", "{document}");
    assert!(
        kept["reason"].as_str().unwrap().contains("pinned"),
        "{document}"
    );
    assert_eq!(setup.state("kept"), "running");
    let (exit, document) = setup.json(&["unpin", "-w", "kept"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["pinned"], false, "{document}");
    let (_, document) = setup.json(&["reclaim", "--need", "memory=1K", "--idle", "0s"]);
    assert_eq!(row(&document, "kept")["state"], "stopped", "{document}");
    assert_eq!(setup.state("kept"), "stopped");
}

#[test]
fn a_disk_need_cleans_the_build_output_of_idle_workspaces_and_reports_what_it_freed() {
    let setup = Setup::new();
    let checkout = setup.workspace("built");
    write(&checkout.join("target/debug/app"), &"x".repeat(64 * 1024));
    let (exit, document) = setup.json(&["reclaim", "--need", "disk=1K", "--idle", "0s"]);
    assert_eq!(exit, 0, "{document}");
    let built = row(&document, "built");
    assert_eq!(built["state"], "cleaned", "{document}");
    assert!(built["freed"].as_u64().unwrap() >= 64 * 1024, "{document}");
    assert!(!checkout.join("target").exists());
    assert!(checkout.join("mxv.toml").exists());
}

#[test]
fn a_need_without_a_resource_or_a_size_is_refused() {
    let setup = Setup::new();
    let output = setup
        .command(&["reclaim", "--need", "cpu=1K"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let output = setup
        .command(&["reclaim", "--need", "memory=lots"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
