//! Starts a sandboxed app through the run files that mxv's environment
//! builds, so phase's sandbox runs under mxv's real layout: phase keeps its
//! state in `<mxv state>/phase`, mxv protects its state directory and its
//! mirrors, and the built `mxv` binary is the keeper and the launcher.
//! The first test gives the app its permissions in phase's plan. The
//! second declares them in a manifest's `[processes.<name>.permissions]`
//! and runs the built `mxv` binary, on macOS, where phase's sandbox is
//! Seatbelt.

#![cfg(test)]
#![cfg(any(target_os = "macos", target_os = "linux"))]

#[cfg(target_os = "macos")]
#[path = "support/git.rs"]
mod support;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use multiverse::CallerEnvironment;
use multiverse::phase::{
    AppName, AppOutcome, AppPlan, AppliedEnvironment, Cancel, EnvironmentError, EnvironmentSource,
    Permissions, Plan, ProcessName, ProcessPlan, RunnerKind, Sandboxing, Selection, Source,
    UpDeadlines, WorkspaceId,
};
use mxv::{Bases, Environment};

/// An environment source that gives each process no variables.
struct Nothing;

impl EnvironmentSource for Nothing {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(Nothing))
    }
}

impl AppliedEnvironment for Nothing {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}
}

#[test]
fn a_sandboxed_app_starts_under_mxvs_own_layout_and_writes_its_own_state() {
    // The per-user temporary directory is protected, so the test's files
    // lie in the build's target directory.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = root.path().canonicalize().unwrap();
    let bases = Bases {
        config: home.join(".config"),
        state: home.join(".local/state"),
        data: home.join(".local/share"),
        home: home.clone(),
    };
    let workspace = bases.data.join("mxv/workspaces/reading-list");
    let checkout = workspace.join("api");
    std::fs::create_dir_all(&checkout).unwrap();
    let caller = CallerEnvironment::from_vars([
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("HOME"), home.clone().into_os_string()),
    ]);
    let environment = Environment::read(|_| None, Some(bases.clone()))
        .with_caller(caller, Some(PathBuf::from(env!("CARGO_BIN_EXE_mxv"))));
    let runs = environment.runs_for_tests().unwrap();
    let phase = runs.phase();
    let mxv_state = bases.state.join("mxv");
    let mut api = AppPlan::new(
        "api".parse().unwrap(),
        RunnerKind::Host,
        Source::checkout(checkout.clone(), None),
    );
    api.permissions = Some(Permissions::default());
    let script = "echo state > \"$TMPDIR/../state/file\" && echo ready; exec sleep 30".to_owned();
    let mut server = ProcessPlan::new(
        "server".parse().unwrap(),
        vec!["/bin/sh".into(), "-c".into(), script],
    );
    server.ready = Some(multiverse::phase::Probe::new(
        multiverse::phase::ProbeCheck::log("^ready$".to_owned()),
    ));
    api.processes.push(server);
    let id: WorkspaceId = "0b9f3c8e".parse().unwrap();
    let plan = Plan::new("reading-list".parse().unwrap(), id.clone(), vec![api])
        .unwrap()
        .with_workspace_dir(workspace.clone(), vec![workspace.join("mxv.toml")]);
    let soon = Instant::now() + Duration::from_secs(30);

    let up = phase
        .up(
            &plan,
            &Selection::All,
            &Nothing,
            UpDeadlines::new(soon, soon),
            &Cancel::new(),
        )
        .unwrap();
    let state_file = mxv_state.join(format!("phase/apps/{id}/api/state/file"));
    let written = std::fs::read_to_string(&state_file);
    let status = phase.status(&id, Some(&plan));
    phase.down(&id, &Selection::All, soon).unwrap();

    assert_eq!(up.apps[0].outcome, AppOutcome::Started, "{up:?}");
    assert_eq!(written.unwrap(), "state\n");
    let rows = status.unwrap().rows;
    let sandbox = rows[0].sandbox.clone().unwrap();
    assert!(
        matches!(sandbox, Sandboxing::Seatbelt { .. })
            || cfg!(target_os = "linux") && matches!(sandbox, Sandboxing::Unsandboxed { .. }),
        "{sandbox:?}"
    );
}

/// A workspace that the built `mxv` binary runs in a directory of the
/// build's target directory, with its own config, state and workspace
/// root, which it stops when it is dropped.
#[cfg(target_os = "macos")]
struct Sandboxed {
    /// The temporary directory.
    dir: tempfile::TempDir,
}

#[cfg(target_os = "macos")]
impl Sandboxed {
    /// Creates a repository and the workspace `triage` of it, and writes
    /// `manifest` as the `mxv.toml` of its checkout. The per-user
    /// temporary directory is protected, so the files lie in the build's
    /// target directory.
    fn new(manifest: &str) -> Self {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let sandboxed = Self { dir };
        support::repository(&sandboxed.root());
        let (exit, document) = sandboxed.json(&["new", "triage", "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        sandboxed.write_manifest(manifest);
        sandboxed
    }

    /// Writes `manifest` as the `mxv.toml` of the checkout.
    fn write_manifest(&self, manifest: &str) {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the manifest that a person would write in a checkout"
        )]
        std::fs::write(self.checkout().join("mxv.toml"), manifest).unwrap();
    }

    /// Returns the row of the unit `name` in the JSON document of a
    /// process verb.
    fn row(document: &serde_json::Value, name: &str) -> serde_json::Value {
        document["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no row {name} in {document}"))
            .clone()
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn checkout(&self) -> PathBuf {
        self.root().join("workspaces/triage/app")
    }

    fn command(&self, args: &[&str]) -> snapbox::cmd::Command {
        let root = self.root();
        snapbox::cmd::Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", support::PATH)
            .envs(support::GIT_ENV.iter().copied())
            .env("HOME", &root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("MXV_ROOT", root.join("workspaces"))
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json(&self, args: &[&str]) -> (i32, serde_json::Value) {
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

    /// Runs `mxv` with `args`, and returns its standard output.
    fn text(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        String::from_utf8(output.stdout).unwrap()
    }
}

#[cfg(target_os = "macos")]
impl Drop for Sandboxed {
    fn drop(&mut self) {
        let _ = self.json(&["down", "-w", "triage"]);
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_process_whose_manifest_grants_permissions_runs_sandboxed_and_cannot_write_outside_its_grants()
{
    let script = "echo granted > out/file && echo granted=written; \
        if echo denied > denied 2>/dev/null; then echo denied=written; else echo denied=refused; fi; \
        echo ready; exec sleep 60";
    let sandboxed = Sandboxed::new(&format!(
        r#"[processes.writer]
argv = ["/bin/sh", "-c", {script:?}]
ready = {{ log = "^ready$" }}
permissions = {{ write = ["out"] }}

[processes.plain]
argv = ["/bin/sh", "-c", "echo ready; exec sleep 60"]
ready = {{ log = "^ready$" }}
"#
    ));

    let (exit, up) = sandboxed.json(&["up", "-w", "triage"]);
    let logs = sandboxed.text(&["logs", "-w", "triage", "writer"]);
    let (_, status) = sandboxed.json(&["status", "-w", "triage"]);

    assert_eq!(exit, 0, "{up}");
    // The shell reports the refused redirection itself.
    assert_eq!(
        logs,
        "granted=written\n/bin/sh: denied: Operation not permitted\ndenied=refused\nready\n"
    );
    let checkout = sandboxed.checkout();
    assert!(checkout.join("out/file").exists());
    assert!(!checkout.join("denied").exists());
    let sandbox = |name: &str| {
        status["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no row {name} in {status}"))["sandbox"]
            .clone()
    };
    assert!(
        sandbox("writer")
            .as_str()
            .is_some_and(|sandbox| sandbox.starts_with("seatbelt (partial: ")),
        "{status}"
    );
    assert_eq!(
        sandbox("plain"),
        "unsandboxed (its app declares no permissions)"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_node_reports_its_version_into_the_log_that_mxv_keeps() {
    if !support::PATH
        .split(':')
        .any(|dir| std::path::Path::new(dir).join("node").is_file())
    {
        println!("node=absent");
        return;
    }
    let sandboxed = Sandboxed::new(
        r#"[processes.node]
argv = ["/bin/sh", "-c", "node --version && echo ready; exec sleep 60"]
ready = { log = "^ready$" }
permissions = {}
"#,
    );

    let (exit, up) = sandboxed.json(&["up", "-w", "triage"]);
    let logs = sandboxed.text(&["logs", "-w", "triage", "node"]);

    assert_eq!(exit, 0, "{up}\n{logs}");
    let version = logs.lines().next().unwrap_or_default();
    assert!(
        version
            .strip_prefix('v')
            .is_some_and(|rest| rest.starts_with(|first: char| first.is_ascii_digit())),
        "{logs}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn up_warns_that_a_running_process_whose_manifest_gained_permissions_needs_a_restart_to_be_sandboxed()
 {
    let without = "[processes.writer]\nargv = [\"/bin/sh\", \"-c\", \"echo ready; exec sleep 60\"]\nready = { log = \"^ready$\" }\n";
    let sandboxed = Sandboxed::new(without);
    let (exit, first) = sandboxed.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{first}");
    let pid = Sandboxed::row(&first, "writer")["pid"].clone();

    sandboxed.write_manifest(&format!("{without}permissions = {{}}\n"));
    let (exit, second) = sandboxed.json(&["up", "-w", "triage"]);
    let (restarted, third) = sandboxed.json(&["restart", "-w", "triage", "writer"]);

    assert_eq!(exit, 0, "{second}");
    let writer = Sandboxed::row(&second, "writer");
    assert_eq!(writer["pid"], pid, "{second}");
    assert_eq!(
        writer["sandbox"], "unsandboxed (its app declares no permissions)",
        "{second}"
    );
    assert_eq!(
        writer["notes"],
        serde_json::json!([{
            "level": "warning",
            "message": "writer still runs unsandboxed, because it was running before its manifest gave it permissions; run `mxv restart -w triage writer` to run it under the sandbox",
        }]),
        "{second}"
    );
    assert_eq!(restarted, 0, "{third}");
    let writer = Sandboxed::row(&third, "writer");
    assert!(
        writer["sandbox"]
            .as_str()
            .is_some_and(|sandbox| sandbox.starts_with("seatbelt (partial: ")),
        "{third}"
    );
    assert_eq!(writer["notes"], serde_json::json!([]), "{third}");
}

#[cfg(target_os = "macos")]
#[test]
fn exec_in_a_sandboxed_process_runs_the_command_under_the_processs_sandbox() {
    let sandboxed = Sandboxed::new(
        "[processes.writer]\nargv = [\"/bin/sh\", \"-c\", \"echo ready; exec sleep 60\"]\nready = { log = \"^ready$\" }\npermissions = { write = [\"out\"] }\n",
    );
    let (exit, up) = sandboxed.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{up}");

    let output = sandboxed
        .command(&[
            "exec",
            "-w",
            "triage",
            "writer",
            "--",
            "/bin/sh",
            "-c",
            "echo granted > out/file && echo granted=written; \
             if echo denied > denied 2>/dev/null; then echo denied=written; else echo denied=refused; fi",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "granted=written\ndenied=refused\n"
    );
    assert!(sandboxed.checkout().join("out/file").exists());
    assert!(!sandboxed.checkout().join("denied").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn sigint_to_mxv_stops_a_sandboxed_command_that_ignores_sigint() {
    use std::io::BufRead as _;
    let sandboxed = Sandboxed::new(
        "[processes.writer]\nargv = [\"/bin/sh\", \"-c\", \"echo ready; exec sleep 60\"]\nready = { log = \"^ready$\" }\npermissions = { write = [\"out\"] }\n",
    );
    let (exit, up) = sandboxed.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{up}");
    let root = sandboxed.root();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv exec in the background and sends it SIGINT"
    )]
    let mut command = Command::new(env!("CARGO_BIN_EXE_mxv"));
    command
        .args([
            "exec",
            "-w",
            "triage",
            "writer",
            "--",
            "/bin/sh",
            "-c",
            "trap '' INT; echo started; sleep 60",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", support::PATH)
        .envs(support::GIT_ENV.iter().copied())
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
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

    let sent = snapbox::cmd::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .env("PATH", support::PATH)
        .output()
        .unwrap();
    assert!(sent.status.success(), "{sent:?}");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            drop(child.kill());
            panic!("mxv exec did not stop the sandboxed command");
        }
        std::thread::yield_now();
    };
    assert_eq!(status.code(), Some(143), "{status:?}");
}
