//! End-to-end tests of the credential preflight: `mxv preflight`, and the
//! single list of sign-ins that `mxv new` prints. Each test builds a
//! repository in a temporary directory and runs the built `mxv` binary.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, repository};

const MISSING_AND_PRESENT: &str = concat!(
    "[secrets.aws]\ncheck = [\"false\"]\nsign_in = \"aws sso login --profile work\"\n\n",
    "[secrets.vault]\ncheck = [\"true\"]\nsign_in = \"never needed\"\n\n",
    "[secrets.op]\ncheck = [\"false\"]\nsign_in = \"op signin --account my\"\n",
);

struct Setup {
    dir: TempDir,
}

impl Setup {
    fn new() -> Self {
        let setup = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        repository(&setup.root());
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn manifest(&self, text: &str) -> PathBuf {
        let path = self.root().join("preflight.toml");
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes a manifest file that a person would write"
        )]
        fs::write(&path, text).unwrap();
        path
    }

    /// Approves, as a person would at a terminal, every check command that
    /// `text` declares.
    fn approve(&self, text: &str) {
        let dir = self.root().join("state").join("mxv");
        fs::create_dir_all(&dir).unwrap();
        let lines: String = text
            .lines()
            .filter_map(|line| line.strip_prefix("check = "))
            .map(|array| {
                let words: Vec<String> = serde_json::from_str(array).unwrap();
                let scope = self.root().join("app").display().to_string();
                format!("{}\n", serde_json::json!({"scope": scope, "check": words}))
            })
            .collect();
        let file = dir.join("preflight-approved");
        let kept = fs::read_to_string(&file).unwrap_or_default();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes the file that approving at a terminal writes"
        )]
        fs::write(file, format!("{kept}{lines}")).unwrap();
    }

    fn command(&self) -> Command {
        let at = |name: &str| self.root().join(name).display().to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mxv"))
            .current_dir(self.root())
            .env_clear()
            .env("HOME", at("home"))
            .env("PATH", PATH)
            .env("MXV_ROOT", at("mxv"))
            .env("XDG_CONFIG_HOME", at("config"))
            .env("XDG_STATE_HOME", at("state"))
            .env("XDG_DATA_HOME", at("data"))
            .env("XDG_CACHE_HOME", at("cache"));
        for (name, value) in GIT_ENV {
            command = command.env(name, value);
        }
        command
    }

    /// Runs `mxv` with `args`, and returns the exit code, standard output
    /// and standard error.
    fn text(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command().args(args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command().args(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?}: {error}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Creates the workspace `triage` after approving the checks of
    /// `manifest`.
    fn new_workspace(&self, manifest: &str) -> (i32, String, String) {
        self.approve(manifest);
        self.new_unapproved(manifest)
    }

    fn new_unapproved(&self, manifest: &str) -> (i32, String, String) {
        let path = self.manifest(manifest);
        self.text(&[
            "new",
            "triage",
            "--repo",
            "app",
            "--manifest",
            path.to_str().unwrap(),
        ])
    }
}

#[test]
fn mxv_new_prints_one_message_naming_every_missing_sign_in_with_its_command() {
    let setup = Setup::new();
    let (exit, _, stderr) = setup.new_workspace(MISSING_AND_PRESENT);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(stderr.matches("sign-ins are needed").count(), 1, "{stderr}");
    assert!(stderr.contains("2 sign-ins are needed"), "{stderr}");
    assert!(stderr.contains("aws sso login --profile work"), "{stderr}");
    assert!(stderr.contains("op signin --account my"), "{stderr}");
    assert!(!stderr.contains("never needed"), "{stderr}");
}

#[test]
fn mxv_new_is_quiet_when_every_check_works() {
    let setup = Setup::new();
    let (exit, _, stderr) =
        setup.new_workspace("[secrets.vault]\ncheck = [\"true\"]\nsign_in = \"never needed\"\n");
    assert_eq!(exit, 0, "{stderr}");
    assert!(!stderr.contains("sign-in"), "{stderr}");
}

#[test]
fn mxv_new_dry_run_runs_no_check() {
    let setup = Setup::new();
    let path = setup.manifest("[secrets.aws]\ncheck = [\"false\"]\nsign_in = \"aws sso login\"\n");
    let (exit, _, stderr) = setup.text(&[
        "new",
        "triage",
        "--repo",
        "app",
        "--manifest",
        path.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(!stderr.contains("sign-in"), "{stderr}");
}

#[test]
fn mxv_new_does_not_run_a_check_that_no_one_approved() {
    let setup = Setup::new();
    let marker = setup.root().join("ran");
    let manifest = format!(
        "[secrets.evil]\ncheck = [\"touch\", \"{}\"]\nsign_in = \"sign in\"\n",
        marker.display()
    );
    let (exit, _, stderr) = setup.new_unapproved(&manifest);
    assert_eq!(exit, 0, "{stderr}");
    assert!(!marker.exists(), "the unapproved check ran");
    assert!(stderr.contains("did not run the check of evil"), "{stderr}");
    assert!(stderr.contains("mxv preflight"), "{stderr}");
    let (exit, _, stderr) = setup.text(&["preflight", "-w", "triage"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(!marker.exists(), "preflight ran it without a terminal");
}

#[test]
fn mxv_up_prints_the_list_once_and_does_not_change_its_result() {
    let setup = Setup::new();
    let manifest = "[secrets.aws]\ncheck = [\"false\"]\nsign_in = \"aws sso login\"\n";
    setup.new_workspace(manifest);
    let (exit, _, stderr) = setup.text(&["up", "-w", "triage"]);
    assert_eq!(stderr.matches("sign-in is needed").count(), 1, "{stderr}");
    assert!(stderr.contains("`aws sso login`"), "{stderr}");
    let working = "[secrets.vault]\ncheck = [\"true\"]\nsign_in = \"never needed\"\n";
    setup.approve(working);
    let path = setup.manifest(working);
    let (plain, _, _) = setup.text(&[
        "new",
        "plain",
        "--repo",
        "app",
        "--manifest",
        path.to_str().unwrap(),
    ]);
    assert_eq!(plain, 0);
    let (plain_exit, _, plain_stderr) = setup.text(&["up", "-w", "plain"]);
    assert!(!plain_stderr.contains("sign-in"), "{plain_stderr}");
    assert_eq!(exit, plain_exit, "a failing check changed the exit code");
}

#[test]
fn mxv_restore_prints_the_list_once() {
    let setup = Setup::new();
    setup.new_workspace(MISSING_AND_PRESENT);
    let (exit, _, stderr) = setup.text(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{stderr}");
    let (exit, _, stderr) = setup.text(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(stderr.matches("sign-ins are needed").count(), 1, "{stderr}");
    assert!(stderr.contains("aws sso login --profile work"), "{stderr}");
}

#[test]
fn mxv_preflight_lists_each_secret_and_every_sign_in_needed() {
    let setup = Setup::new();
    setup.new_workspace(MISSING_AND_PRESENT);
    let (exit, document) = setup.json(&["preflight", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let data = &document["data"];
    assert_eq!(data["state"], "sign_in_needed", "{document}");
    let secrets = data["secrets"].as_array().unwrap();
    assert_eq!(secrets.len(), 3, "{document}");
    let needing: Vec<&str> = secrets
        .iter()
        .filter(|secret| secret["works"] == false)
        .map(|secret| secret["sign_in"].as_str().unwrap())
        .collect();
    assert_eq!(
        needing,
        ["aws sso login --profile work", "op signin --account my"]
    );
}

#[test]
fn mxv_preflight_reports_ready_when_every_check_works() {
    let setup = Setup::new();
    setup.new_workspace("[secrets.vault]\ncheck = [\"true\"]\nsign_in = \"never needed\"\n");
    let (exit, document) = setup.json(&["preflight", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "ready", "{document}");
}

#[test]
fn mxv_preflight_of_a_workspace_without_secrets_is_ready_and_lists_none() {
    let setup = Setup::new();
    let (exit, _, stderr) = setup.text(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{stderr}");
    let (exit, document) = setup.json(&["preflight", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "ready");
    assert_eq!(document["data"]["secrets"].as_array().unwrap().len(), 0);
}

#[test]
fn the_text_of_mxv_preflight_prints_each_command_and_no_value() {
    let setup = Setup::new();
    setup.new_workspace(
        "[secrets.leaky]\ncheck = [\"sh\", \"-c\", \"echo hunter2; exit 1\"]\nsign_in = \"op signin\"\n",
    );
    let (exit, stdout, stderr) = setup.text(&["preflight", "-w", "triage"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(stdout.contains("op signin"), "{stdout}");
    assert!(!stdout.contains("hunter2") && !stderr.contains("hunter2"));
}
