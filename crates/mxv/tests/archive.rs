//! End-to-end tests of `mxv archive`, `mxv restore`, `mxv list --archived`
//! and `mxv remove --archived`. Each test builds a repository in a temporary
//! directory and runs the built `mxv` binary there.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git, repository};

/// Writes `text` to the file at `path`, and creates its directory.
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, text).unwrap();
}

/// A temporary directory with a repository and a workspace `triage` of it.
struct Setup {
    dir: TempDir,
}

impl Setup {
    fn new() -> Self {
        let setup = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        let clone = repository(&setup.root());
        assert_eq!(clone, setup.root().join("app"), "the fixture's clone moved");
        let (exit, document) = setup.run(&["new", "triage", "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn workspace(&self) -> PathBuf {
        self.root().join("mxv/triage")
    }

    fn checkout(&self) -> PathBuf {
        self.workspace().join("app")
    }

    /// Returns the variables of each run: the workspace root, the XDG
    /// directories and git without the person's configuration.
    fn variables(&self) -> Vec<(String, String)> {
        let root = self.root();
        let at = |name: &str| root.join(name).display().to_string();
        let mut variables = vec![
            ("HOME".to_owned(), at("home")),
            ("PATH".to_owned(), PATH.to_owned()),
            ("MXV_ROOT".to_owned(), at("mxv")),
            ("XDG_CONFIG_HOME".to_owned(), at("config")),
            ("XDG_STATE_HOME".to_owned(), at("state")),
            ("XDG_DATA_HOME".to_owned(), at("data")),
            ("XDG_CACHE_HOME".to_owned(), at("cache")),
        ];
        variables.extend(
            GIT_ENV
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
        variables
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mxv"))
            .current_dir(dir)
            .env_clear();
        for (name, value) in self.variables() {
            command = command.env(name, value);
        }
        command
    }

    /// Runs `mxv` with `args` and `--json` at the root, and returns the
    /// exit code and the document that it printed.
    fn run(&self, args: &[&str]) -> (i32, Value) {
        self.run_in(&self.root(), args)
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> (i32, Value) {
        let output = self.command(dir).args(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?}: {error}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    fn archive(&self) -> Value {
        let (exit, document) = self.run(&["archive", "-w", "triage", "--yes"]);
        assert_eq!(exit, 0, "{document}");
        document
    }

    fn head(&self) -> String {
        git(&self.checkout(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn commit_locally(&self) -> String {
        write(&self.checkout().join("local.txt"), "only here\n");
        git(&self.checkout(), &["add", "local.txt"]);
        git(&self.checkout(), &["commit", "--quiet", "-m", "Local"]);
        self.head()
    }

    /// Runs `script` with `sh` in the checkout. `mxv` in the script is the
    /// built binary, and the script's standard output comes back.
    fn sh(&self, script: &str) -> (i32, String) {
        let binary = env!("CARGO_BIN_EXE_mxv");
        let script = script.replace("mxv", binary);
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts a shell, so the shell's relation to its children is what it checks"
        )]
        let output = std::process::Command::new("sh")
            .args(["-c", &script])
            .env_clear()
            .envs(self.variables())
            .current_dir(self.checkout())
            .output()
            .unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        )
    }
}

#[test]
fn archive_then_restore_brings_back_the_commit_and_the_branch() {
    let setup = Setup::new();
    let commit = setup.commit_locally();
    let document = setup.archive();
    assert_eq!(document["schema"], "mxv/archive/v1");
    assert_eq!(document["data"]["pins"][0]["commit"], commit);
    let local = document["data"]["local_only"][0].as_str().unwrap();
    assert!(local.contains(&commit), "{document}");
    assert!(!setup.workspace().exists());
    let (exit, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{listed}");
    assert_eq!(listed["data"][0]["name"], "triage");
    assert_eq!(listed["data"][0]["local_only"], 1);
    let (exit, restored) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{restored}");
    assert_eq!(restored["schema"], "mxv/restore/v1");
    assert_eq!(setup.head(), commit);
    let (_, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(listed["data"].as_array().unwrap().len(), 0);
}

#[test]
fn a_dry_run_of_each_verb_changes_nothing() {
    let setup = Setup::new();
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.checkout().is_dir());
    setup.archive();
    let (exit, document) = setup.run(&["restore", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!setup.workspace().exists());
    let (exit, document) = setup.run(&["remove", "--archived", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    let (_, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[test]
fn an_ignored_env_file_refuses_the_archive_with_its_code_and_wip_does_not_help() {
    let setup = Setup::new();
    write(&setup.checkout().join(".git/info/exclude"), ".env\n");
    write(&setup.checkout().join(".env"), "SECRET=1\n");
    for flags in [&[][..], &["--wip"][..]] {
        let mut args = vec!["archive", "-w", "triage", "--yes"];
        args.extend(flags);
        let (exit, document) = setup.run(&args);
        assert_eq!(exit, 1, "{document}");
        assert_eq!(document["code"], "mxv::archive::unsaved_work", "{document}");
        assert!(document.to_string().contains(".env"), "{document}");
    }
    assert!(setup.checkout().join(".env").is_file());
}

#[test]
fn removing_an_archive_clears_it_and_clean_never_touches_it() {
    let setup = Setup::new();
    setup.archive();
    let (exit, document) = setup.run(&["clean", "--all", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (_, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    let (exit, document) = setup.run(&["remove", "--archived", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let pin = document["data"]["pins"][0].as_str().unwrap();
    assert!(pin.starts_with("refs/mxv/archives/"), "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::restore::not_found");
}

#[test]
fn removing_an_archive_refuses_a_name_that_no_archive_has() {
    let setup = Setup::new();
    let (exit, document) = setup.run(&["remove", "--archived", "other", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::not_found");
}

#[test]
fn an_archive_refuses_while_a_name_is_taken_by_another_archive() {
    let setup = Setup::new();
    setup.archive();
    let (exit, document) = setup.run(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::name_taken");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::restore::name_taken");
}

#[test]
fn a_shell_that_stands_in_the_workspace_does_not_refuse_its_own_archive() {
    let setup = Setup::new();
    let (exit, out) = setup.sh("mxv archive --yes; echo done");
    assert_eq!(exit, 0, "{out}");
    assert!(out.contains("done"), "{out}");
    assert!(!setup.workspace().exists(), "{out}");
    assert!(
        out.contains("The current directory was inside the workspace"),
        "{out}"
    );
}

#[test]
fn a_background_job_of_the_invoking_shell_refuses_the_archive() {
    let setup = Setup::new();
    let (_, out) = setup.sh(
        "sleep 30 & job=$!; mxv archive --yes --json; status=$?; kill $job; echo status=$status",
    );
    assert!(out.contains("mxv::archive::in_use"), "{out}");
    assert!(out.contains("status=1"), "{out}");
    assert!(setup.checkout().is_dir(), "{out}");
}

#[test]
fn an_outside_process_refuses_the_archive_and_names_itself() {
    let setup = Setup::new();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts a process outside mxv whose working directory is in the workspace"
    )]
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(setup.checkout())
        .spawn()
        .unwrap();
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::in_use");
    assert!(
        document.to_string().contains(&child.id().to_string()),
        "{document}"
    );
    setup.archive();
}

#[test]
fn a_record_from_a_newer_version_refuses_with_the_record_code() {
    let setup = Setup::new();
    setup.archive();
    let path = setup.root().join("state/mxv/archives/triage/archive.json");
    let record = fs::read_to_string(&path)
        .unwrap()
        .replace("\"version\": 1", "\"version\": 99");
    write(&path, &record);
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::record::newer");
}

#[test]
fn an_unfinished_archive_is_a_degraded_row_that_names_the_command_that_finishes_it() {
    let setup = Setup::new();
    setup.archive();
    let path = setup.root().join("state/mxv/archives/triage/archive.json");
    let record = fs::read_to_string(&path)
        .unwrap()
        .replace("\"state\": \"archived\"", "\"state\": \"retiring\"");
    write(&path, &record);
    let (exit, document) = setup.run(&["list"]);
    assert_eq!(exit, 1, "{document}");
    let row = &document["data"][0];
    assert_eq!(row["name"], "triage");
    assert!(
        row["problem"]
            .as_str()
            .unwrap()
            .contains("mxv archive -w triage")
    );
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::restore::unfinished");
    let (exit, document) = setup.run(&["remove", "--archived", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::unfinished");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
}

#[test]
fn archive_outside_every_workspace_names_the_flag_that_chooses_one() {
    let setup = Setup::new();
    let (exit, document) = setup.run(&["archive", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::not_current");
}
