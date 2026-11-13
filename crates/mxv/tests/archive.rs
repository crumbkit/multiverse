//! End-to-end tests of `mxv archive`, `mxv restore`, `mxv list --archived`
//! and `mxv remove --archived`. Each test builds a repository in a temporary
//! directory and runs the built `mxv` binary there.

#![cfg(test)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/ssh.rs"]
mod ssh;
#[path = "support/git.rs"]
mod support;

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Stdio};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::ssh::{SSH_ORIGIN, fake_ssh};
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
        grants::before(&self.root().join("state/mxv"), args);
        Self::finished(self.command(dir), args)
    }

    /// Runs `mxv` with `args` and `--json` at the root, with git reading
    /// `config` as its global configuration, and returns the exit code and
    /// the document that it printed.
    fn run_with_git_config(&self, config: &Path, args: &[&str]) -> (i32, Value) {
        let command = self.command(&self.root()).env("GIT_CONFIG_GLOBAL", config);
        Self::finished(command, args)
    }

    /// Runs `command` with `args` and `--json`, and returns the exit code
    /// and the document that it printed.
    fn finished(command: Command, args: &[&str]) -> (i32, Value) {
        let output = command.args(args).arg("--json").output().unwrap();
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

    /// Commits an `mxv.toml` in the checkout that alone declares the ignored
    /// directory `cache-dir/`, which holds a file, disposable.
    fn declare_manifest_disposable(&self) {
        let checkout = self.checkout();
        write(&checkout.join(".git/info/exclude"), "cache-dir/\n");
        write(
            &checkout.join("mxv.toml"),
            "disposable = [\"cache-dir/\"]\n",
        );
        git(&checkout, &["add", "mxv.toml"]);
        git(&checkout, &["commit", "--quiet", "-m", "Manifest"]);
        write(&checkout.join("cache-dir/data"), "x\n");
    }

    /// Commits a `.gitignore` that ignores `.env` and `target/` in the
    /// checkout, and writes an `.env` file of 9 bytes there.
    fn ignore_env(&self) {
        let checkout = self.checkout();
        write(&checkout.join(".gitignore"), ".env\ntarget/\n");
        git(&checkout, &["add", ".gitignore"]);
        git(&checkout, &["commit", "--quiet", "-m", "Ignore"]);
        write(&checkout.join(".env"), "SECRET=1\n");
    }

    /// Returns the path of the tar that holds the ignored files of the
    /// archive `triage`.
    fn ignored_tar(&self) -> PathBuf {
        let path = self.root().join("state/mxv/archives/triage/archive.json");
        let record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let id = record["id"].as_str().unwrap();
        self.root()
            .join("data/mxv/archives")
            .join(id)
            .join("ignored.tar")
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
    assert_eq!(document["schema"], "mxv/archive/v2");
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
fn archive_restore_and_removal_work_on_a_mirror_when_git_refuses_to_discover_bare_repositories() {
    let setup = Setup::new();
    let config = setup.root().join("explicit.gitconfig");
    write(&config, "[safe]\n\tbareRepository = explicit\n");
    let succeeds = |args: &[&str]| {
        let (exit, document) = setup.run_with_git_config(&config, args);
        assert_eq!(exit, 0, "{args:?}: {document}");
    };
    succeeds(&["new", "explicit", "--repo", "app"]);
    let (_, shown) = setup.run(&["show", "explicit", "--full"]);
    let mirror = PathBuf::from(shown["data"][0]["checkouts"][0]["mirror"].as_str().unwrap());
    let pins = || git(&mirror, &["for-each-ref", "refs/mxv/archives"]);
    let before = pins();
    succeeds(&["archive", "-w", "explicit", "--yes"]);
    assert_ne!(pins(), before, "the archive left no pin in the mirror");
    succeeds(&["restore", "explicit", "--yes"]);
    assert_eq!(pins(), before, "the restore left its pin in the mirror");
    succeeds(&["archive", "-w", "explicit", "--yes"]);
    assert_ne!(pins(), before, "the archive left no pin in the mirror");
    succeeds(&["remove", "--archived", "explicit", "--yes"]);
    assert_eq!(pins(), before, "the removal left its pin in the mirror");
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
fn an_archive_keeps_an_ignored_env_file_and_restore_writes_it_back() {
    let setup = Setup::new();
    setup.ignore_env();
    let document = setup.archive();
    let kept = &document["data"]["ignored"][0];
    assert_eq!(kept["folder"], "app", "{document}");
    assert_eq!(kept["paths"], serde_json::json!(["app/.env"]), "{document}");
    assert_eq!(kept["bytes"], 9, "{document}");
    let tar = setup.ignored_tar();
    assert!(tar.is_file(), "the archive wrote no tar at {tar:?}");
    let (exit, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{listed}");
    assert_eq!(listed["data"][0]["ignored"], 1, "{listed}");
    assert_eq!(listed["data"][0]["ignored_bytes"], 9, "{listed}");
    let (exit, restored) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{restored}");
    assert_eq!(
        restored["data"]["put_back"][0]["ignored"],
        serde_json::json!(["app/.env"]),
        "{restored}"
    );
    assert_eq!(restored["data"]["lost"], serde_json::json!([]));
    assert_eq!(
        fs::read_to_string(setup.checkout().join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert!(!tar.exists(), "the restore left the tar at {tar:?}");
}

#[test]
fn a_damaged_tar_refuses_the_restore_with_its_code_and_force_restores_without_the_file() {
    let setup = Setup::new();
    setup.ignore_env();
    setup.archive();
    let tar = setup.ignored_tar();
    let mut bytes = fs::read(&tar).unwrap();
    let secret = bytes
        .windows(8)
        .position(|window| window == b"SECRET=1")
        .expect("the tar holds the bytes of .env");
    bytes[secret + 7] = b'2';
    #[expect(
        clippy::disallowed_methods,
        reason = "the test damages the tar as a failing disk would"
    )]
    fs::write(&tar, &bytes).unwrap();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::restore::ignored_damaged",
        "{document}"
    );
    let text = document.to_string();
    assert!(text.contains("app/.env"), "{document}");
    assert!(text.contains("mxv restore triage --force"), "{document}");
    assert!(!setup.workspace().exists(), "{document}");
    let (exit, restored) = setup.run(&["restore", "triage", "--yes", "--force"]);
    assert_eq!(exit, 0, "{restored}");
    assert_eq!(
        restored["data"]["lost"],
        serde_json::json!(["app/.env"]),
        "{restored}"
    );
    assert!(setup.checkout().is_dir(), "{restored}");
    assert!(!setup.checkout().join(".env").exists(), "{restored}");
}

#[test]
fn an_ignored_file_over_the_limit_refuses_the_archive_and_names_itself() {
    let setup = Setup::new();
    setup.ignore_env();
    write(
        &setup.root().join("config/mxv/config.toml"),
        "archive_max_file = \"4B\"\n",
    );
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::unkeepable", "{document}");
    assert!(
        document.to_string().contains(
            "app: the ignored file .env is 9 bytes, over archive_max_file (4 bytes). Move it out of the workspace or delete it."
        ),
        "{document}"
    );
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
fn removing_an_archive_of_a_name_that_no_archive_has_has_nothing_to_do() {
    let setup = Setup::new();
    let (exit, document) = setup.run(&["remove", "--archived", "other", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert!(
        document["data"]["pins"].as_array().unwrap().is_empty(),
        "{document}"
    );
}

#[test]
fn archiving_an_archived_name_has_nothing_to_do() {
    let setup = Setup::new();
    let archived = setup.archive();
    assert_eq!(archived["data"]["state"], "done", "{archived}");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert_eq!(
        document["data"]["pins"], archived["data"]["pins"],
        "{document}"
    );
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    let output = setup
        .command(&setup.root())
        .args(["archive", "-w", "triage", "--yes"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "The workspace triage is already archived, so there is nothing to do. `mxv restore triage` brings it back."
    );
    // The archive is untouched, so a restore still brings it back.
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "done", "{document}");
}

#[test]
fn restoring_a_live_workspace_whose_archive_is_gone_has_nothing_to_do() {
    let setup = Setup::new();
    setup.archive();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert_eq!(
        document["data"]["path"],
        setup.workspace().display().to_string(),
        "{document}"
    );
    assert!(
        document["data"]["stale_pins"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{document}"
    );
    let output = setup
        .command(&setup.root())
        .args(["restore", "triage", "--yes"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "A workspace named triage exists and no archive of it is left, so there is nothing to do."
    );
}

impl Setup {
    /// Restores the archive `triage` and writes its record and its first
    /// pin back, as a restore that stopped after it wrote the workspace's
    /// record and before it deleted the pins leaves them, and returns the
    /// record's path.
    fn stop_restore_before_its_deletion(&self) -> PathBuf {
        self.archive();
        let record = self.archive_record("triage");
        let saved = fs::read_to_string(&record).unwrap();
        let (exit, document) = self.run(&["restore", "triage", "--yes"]);
        assert_eq!(exit, 0, "{document}");
        write(&record, &saved);
        let value: Value = serde_json::from_str(&saved).unwrap();
        let checkout = &value["checkouts"][0];
        let mirror = PathBuf::from(checkout["mirror"].as_str().unwrap());
        let (pin, commit) = (checkout["pin"].as_str(), checkout["commit"].as_str());
        git(&mirror, &["update-ref", pin.unwrap(), commit.unwrap()]);
        record
    }

    /// Returns the pin of the archive `triage`'s first checkout, and the
    /// commit that the repository holding it resolves it to, or `None`.
    fn first_pin(&self) -> (String, Option<String>) {
        let record: Value =
            serde_json::from_str(&fs::read_to_string(self.archive_record("triage")).unwrap())
                .unwrap();
        let checkout = &record["checkouts"][0];
        let mirror = PathBuf::from(checkout["mirror"].as_str().unwrap());
        let pin = checkout["pin"].as_str().unwrap().to_owned();
        let commit = git(&mirror, &["for-each-ref", "--format=%(objectname)", &pin])
            .trim()
            .to_owned();
        (pin, Some(commit).filter(|commit| !commit.is_empty()))
    }
}

#[test]
fn a_dry_run_of_a_stopped_restore_lists_the_stale_pins_and_keeps_them() {
    let setup = Setup::new();
    let record = setup.stop_restore_before_its_deletion();
    let (pin, commit) = setup.first_pin();
    assert!(commit.is_some(), "the pin {pin} resolves before the run");
    let (exit, document) = setup.run(&["restore", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert_eq!(
        document["data"]["stale_pins"],
        serde_json::json!([pin]),
        "{document}"
    );
    assert!(record.exists(), "{document}");
    assert_eq!(setup.first_pin().1, commit, "{document}");
}

#[test]
fn a_stopped_restore_whose_workspace_directory_is_gone_keeps_its_archive() {
    let setup = Setup::new();
    let record = setup.stop_restore_before_its_deletion();
    let (pin, commit) = setup.first_pin();
    fs::remove_dir_all(setup.workspace()).unwrap();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::restore::name_taken", "{document}");
    assert!(record.exists(), "{document}");
    // `mxv repair` keeps it too, because the archive may hold the only
    // copies of the workspace's files.
    let (_, document) = setup.run(&["repair", "triage", "--yes"]);
    assert!(record.exists(), "{document}");
    assert_eq!(setup.first_pin(), (pin, commit), "{document}");
    // Removing what is left of the workspace lets the restore go ahead.
    let (exit, document) = setup.run(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "done", "{document}");
    assert!(setup.checkout().is_dir(), "{document}");
}

#[test]
fn a_workspace_record_written_back_beside_its_finished_archive_keeps_the_archive_through_repair() {
    let setup = Setup::new();
    let commit = setup.commit_locally();
    let workspace = setup.root().join("state/mxv/workspaces/triage.json");
    let saved = fs::read_to_string(&workspace).unwrap();
    setup.archive();
    let record = setup.archive_record("triage");
    let (pin, pinned) = setup.first_pin();
    assert_eq!(pinned.as_deref(), Some(commit.as_str()));
    // An older mxv's pin or up that waited for the lock while the archive
    // ran wrote the workspace's record back after the archive deleted it.
    write(&workspace, &saved);
    let (_, document) = setup.run(&["repair", "triage", "--yes"]);
    assert!(record.exists(), "{document}");
    assert_eq!(setup.first_pin(), (pin, pinned), "{document}");
    let (exit, document) = setup.run(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "done", "{document}");
    assert_eq!(setup.head(), commit, "{document}");
}

#[test]
fn a_restore_refuses_a_name_whose_workspace_and_archive_have_no_stable_id() {
    let setup = Setup::new();
    let record = setup.stop_restore_before_its_deletion();
    let workspace = setup.root().join("state/mxv/workspaces/triage.json");
    for (path, key) in [(&record, "workspace_id"), (&workspace, "id")] {
        let mut value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert!(
            value.as_object_mut().unwrap().remove(key).is_some(),
            "{key}"
        );
        write(path, &serde_json::to_string_pretty(&value).unwrap());
    }
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::restore::name_taken", "{document}");
    assert!(record.exists(), "{document}");
}

#[test]
fn a_restore_stopped_before_it_deleted_its_archive_is_finished_by_restoring_again() {
    let setup = Setup::new();
    setup.archive();
    let record = setup.archive_record("triage");
    let saved = fs::read_to_string(&record).unwrap();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    // The restore rebuilt the workspace with the archive's stable id; put
    // back the record it deleted, as a restore stopped just before that
    // deletion leaves it.
    write(&record, &saved);

    let (exit, document) = setup.run(&["restore", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert!(record.exists(), "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    let pin = document["data"]["stale_pins"][0].as_str().unwrap();
    assert!(pin.starts_with("refs/mxv/archives/"), "{document}");
    assert!(!record.exists(), "{document}");
    assert!(setup.workspace().join("app").is_dir(), "{document}");
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
        .replace("\"version\": 3", "\"version\": 99");
    write(&path, &record);
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::record::newer");
}

#[test]
fn an_unfinished_archive_is_a_degraded_row_that_names_the_command_that_finishes_it() {
    let setup = Setup::new();
    setup.archive();
    setup.mark_retired();
    for args in [&["list"][..], &["list", "--archived"][..]] {
        let (exit, document) = setup.run(args);
        assert_eq!(exit, 1, "{args:?}: {document}");
        assert_eq!(document["total"], 1, "{args:?}: {document}");
        let row = &document["data"][0];
        assert_eq!(row["name"], "triage");
        assert_eq!(row["problem"], "the archive stopped before it finished");
        assert_eq!(row["repair"], "mxv archive -w triage");
    }
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
fn an_archive_stopped_after_it_deleted_the_checkout_names_archive_and_not_remove() {
    let setup = Setup::new();
    let workspace = setup.root().join("state/mxv/workspaces/triage.json");
    let kept = fs::read_to_string(&workspace).unwrap();
    setup.archive();
    // A kill after the checkout was deleted and before the workspace record
    // was removed leaves both records, the archive's still retiring.
    write(&workspace, &kept);
    let path = setup.root().join("state/mxv/archives/triage/archive.json");
    let record = fs::read_to_string(&path)
        .unwrap()
        .replace("\"state\": \"archived\"", "\"state\": \"retiring\"");
    write(&path, &record);
    let (exit, document) = setup.run(&["list"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["total"], 1, "{document}");
    assert_eq!(
        document["data"][0]["path"],
        setup.workspace().display().to_string()
    );
    assert_eq!(document["data"][0]["repair"], "mxv archive -w triage");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["list"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 0, "{document}");
}

impl Setup {
    /// Returns the path of the archive record of `name`.
    fn archive_record(&self, name: &str) -> PathBuf {
        self.root()
            .join("state/mxv/archives")
            .join(name)
            .join("archive.json")
    }

    /// Marks the archive `triage` as retired, as a run that stopped after
    /// it deleted the workspace's record and before it marked the archive
    /// finished would leave it.
    fn mark_retired(&self) {
        self.mark("retired");
    }

    /// Marks the retired archive `triage` as archived again.
    fn mark_archived(&self) {
        let path = self.archive_record("triage");
        let record = fs::read_to_string(&path)
            .unwrap()
            .replace("\"state\": \"retired\"", "\"state\": \"archived\"");
        write(&path, &record);
    }

    /// Gives the archive `triage` the state `state`.
    fn mark(&self, state: &str) {
        let path = self.archive_record("triage");
        let record = fs::read_to_string(&path).unwrap().replace(
            "\"state\": \"archived\"",
            &format!("\"state\": \"{state}\""),
        );
        write(&path, &record);
    }

    /// Deletes every pin of the archive `triage` from its mirror, as a
    /// person who deleted the references by hand would.
    fn delete_pins(&self) {
        let record: Value =
            serde_json::from_str(&fs::read_to_string(self.archive_record("triage")).unwrap())
                .unwrap();
        let checkout = &record["checkouts"][0];
        let mirror = PathBuf::from(checkout["mirror"].as_str().unwrap());
        let pin = checkout["pin"].as_str().unwrap();
        git(&mirror, &["update-ref", "-d", pin]);
    }
}

#[test]
fn a_retiring_record_with_no_pins_is_never_marked_archived() {
    let setup = Setup::new();
    setup.archive();
    setup.mark_retired();
    setup.delete_pins();
    for args in [&["list"][..], &["list", "--archived"][..]] {
        let (exit, document) = setup.run(args);
        assert_eq!(exit, 1, "{args:?}: {document}");
        let row = &document["data"][0];
        assert_eq!(row["name"], "triage", "{args:?}: {document}");
        assert_eq!(
            row["problem"],
            "the archive stopped before it finished, and its pins are gone"
        );
        assert_eq!(row["repair"], "mxv repair triage");
    }
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::pins_gone", "{document}");
    assert!(setup.archive_record("triage").exists());

    let (exit, document) = setup.run(&["repair", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "would_move_aside", "{document}");
    assert!(setup.archive_record("triage").exists());
    let (exit, document) = setup.run(&["repair", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::confirmation::needs_yes",
        "{document}"
    );
    assert!(setup.archive_record("triage").exists());

    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "moved_aside", "{document}");
    let kept = PathBuf::from(document["data"]["kept"][0].as_str().unwrap());
    assert!(kept.join("archive.json").exists(), "{document}");
    assert!(!setup.archive_record("triage").exists());
    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 0, "{document}");
    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
}

#[test]
fn a_stale_retiring_archive_beside_a_newer_workspace_is_finished_under_a_dated_name() {
    let setup = Setup::new();
    let head = setup.head();
    setup.archive();
    setup.mark_retired();
    let (exit, document) = setup.run(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["list"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"][0]["problem"], "");
    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 1, "{document}");
    let row = &document["data"][0];
    assert_eq!(
        row["problem"],
        "an older archive beside a newer workspace triage"
    );
    assert_eq!(row["repair"], "mxv repair triage");

    let (exit, document) = setup.run(&["repair", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "would_finish", "{document}");
    assert!(setup.archive_record("triage").exists());

    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "finished", "{document}");
    let dated = document["data"]["archive"].as_str().unwrap().to_owned();
    let date = dated.strip_prefix("triage-").unwrap();
    assert_eq!(date.len(), "2026-11-02".len(), "{dated}");
    assert!(!setup.archive_record("triage").exists());

    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 1, "{document}");
    assert_eq!(document["data"][0]["name"], dated.as_str());
    let (exit, document) = setup.run(&["restore", &dated, "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let restored = setup.root().join("mxv").join(&dated).join("app");
    assert_eq!(git(&restored, &["rev-parse", "HEAD"]).trim(), head);
    assert!(setup.checkout().join(".git").exists());
}

impl Setup {
    /// Writes a copy of the record of the archive `triage` under its first
    /// dated name, with no other file, as a repair that stopped right after
    /// it wrote the renamed record leaves it, and returns the dated name.
    fn plant_dated_copy(&self) -> String {
        let mut record: Value =
            serde_json::from_str(&fs::read_to_string(self.archive_record("triage")).unwrap())
                .unwrap();
        let days = i64::try_from(record["time"].as_u64().unwrap() / 86_400).unwrap();
        let (year, month, day) = multiverse::phase::calendar::civil_from_days(days);
        let dated = format!("triage-{year:04}-{month:02}-{day:02}");
        let path = PathBuf::from(record["path"].as_str().unwrap()).with_file_name(&dated);
        record["name"] = dated.clone().into();
        record["path"] = path.display().to_string().into();
        for checkout in record["checkouts"].as_array_mut().unwrap() {
            let folder = checkout["folder"].as_str().unwrap().to_owned();
            checkout["path"] = path.join(folder).display().to_string().into();
        }
        let file = self.archive_record(&dated);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        write(&file, &serde_json::to_string_pretty(&record).unwrap());
        dated
    }
}

#[test]
fn a_stopped_rename_with_no_workspace_left_is_finished_by_mxv_repair_of_either_name() {
    for older in [true, false] {
        let setup = Setup::new();
        let head = setup.head();
        setup.archive();
        setup.mark_retired();
        let dated = setup.plant_dated_copy();
        for args in [&["list"][..], &["list", "--archived"][..]] {
            let (exit, document) = setup.run(args);
            assert_eq!(exit, 1, "{args:?}: {document}");
            assert_eq!(document["total"], 2, "{args:?}: {document}");
            for row in document["data"].as_array().unwrap() {
                assert_eq!(
                    row["problem"], "a repair stopped while it moved the archive to its dated name",
                    "{document}"
                );
                assert_eq!(row["repair"], "mxv repair triage", "{document}");
            }
        }
        for name in ["triage", dated.as_str()] {
            let (exit, document) = setup.run(&["archive", "-w", name, "--yes"]);
            assert_eq!(exit, 1, "{name}: {document}");
            assert_eq!(
                document["code"], "mxv::archive::rename_unfinished",
                "{name}: {document}"
            );
        }

        let given = if older { "triage" } else { dated.as_str() };
        let (exit, document) = setup.run(&["repair", given, "--yes"]);
        assert_eq!(exit, 0, "{given}: {document}");
        assert_eq!(document["data"]["state"], "finished", "{document}");
        assert_eq!(document["data"]["older"], "triage", "{document}");
        assert_eq!(document["data"]["archive"], dated.as_str(), "{document}");
        assert!(!setup.archive_record("triage").exists());

        let (exit, document) = setup.run(&["list", "--archived"]);
        assert_eq!(exit, 0, "{document}");
        assert_eq!(document["total"], 1, "{document}");
        assert_eq!(document["data"][0]["name"], dated.as_str());
        let (exit, document) = setup.run(&["restore", &dated, "--yes"]);
        assert_eq!(exit, 0, "{document}");
        let restored = setup.root().join("mxv").join(&dated).join("app");
        assert_eq!(git(&restored, &["rev-parse", "HEAD"]).trim(), head);
    }
}

#[test]
fn a_stopped_rename_beside_a_newer_workspace_is_finished_by_mxv_repair_of_the_dated_name() {
    let setup = Setup::new();
    let head = setup.head();
    setup.archive();
    setup.mark_retired();
    let dated = setup.plant_dated_copy();
    let (exit, document) = setup.run(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 1, "{document}");
    for row in document["data"].as_array().unwrap() {
        assert_eq!(row["repair"], "mxv repair triage", "{document}");
    }

    let (exit, document) = setup.run(&["repair", &dated, "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "finished", "{document}");
    assert_eq!(document["data"]["older"], "triage", "{document}");
    assert_eq!(document["data"]["archive"], dated.as_str(), "{document}");
    assert!(!setup.archive_record("triage").exists());
    assert!(setup.checkout().join(".git").exists());

    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 1, "{document}");
    let (exit, document) = setup.run(&["restore", &dated, "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let restored = setup.root().join("mxv").join(&dated).join("app");
    assert_eq!(git(&restored, &["rev-parse", "HEAD"]).trim(), head);
}

#[test]
fn repair_of_a_name_whose_archive_record_cannot_be_read_returns_the_read_error() {
    let setup = Setup::new();
    setup.archive();
    write(&setup.archive_record("triage"), "not a record\n");
    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::record::corrupt", "{document}");
    assert!(setup.archive_record("triage").exists());
}

#[test]
fn an_older_archive_whose_pins_are_gone_beside_a_newer_workspace_is_moved_aside() {
    let setup = Setup::new();
    setup.archive();
    setup.mark_retired();
    setup.delete_pins();
    let (exit, document) = setup.run(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "moved_aside", "{document}");
    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 0, "{document}");
    assert!(setup.checkout().join(".git").exists());
}

#[test]
fn a_restore_stopped_before_it_deleted_its_archive_is_finished_by_mxv_repair() {
    let setup = Setup::new();
    setup.archive();
    let path = setup.archive_record("triage");
    let record = fs::read_to_string(&path).unwrap();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    // A kill after the pins were deleted and before the record was leaves
    // the record of the finished archive beside the restored workspace.
    write(&path, &record);
    for args in [&["list"][..], &["list", "--archived"][..]] {
        let (exit, document) = setup.run(args);
        assert_eq!(exit, 1, "{args:?}: {document}");
        assert_eq!(document["total"], 1, "{args:?}: {document}");
        let row = &document["data"][0];
        assert_eq!(
            row["problem"],
            "the restore stopped before it deleted the archive"
        );
        assert_eq!(row["repair"], "mxv repair triage");
    }
    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "deleted", "{document}");
    assert!(!path.exists());
    assert!(setup.checkout().join(".git").exists());
    let (exit, document) = setup.run(&["list"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 1, "{document}");
}

#[test]
fn repair_of_a_name_with_nothing_broken_has_nothing_to_do() {
    let setup = Setup::new();
    for name in ["triage", "missing"] {
        let (exit, document) = setup.run(&["repair", name, "--yes"]);
        assert_eq!(exit, 0, "{name}: {document}");
        assert_eq!(document["data"]["state"], "unchanged", "{name}: {document}");
    }
}

#[test]
fn repair_of_an_unfinished_archive_names_the_command_that_finishes_it() {
    let setup = Setup::new();
    setup.archive();
    setup.mark_retired();
    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert_eq!(document["data"]["repair"], "mxv archive -w triage");
    assert!(setup.archive_record("triage").exists());
}

#[test]
fn archive_outside_every_workspace_names_the_flag_that_chooses_one() {
    let setup = Setup::new();
    let (exit, document) = setup.run(&["archive", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::not_current");
}

/// Writes a git configuration that gives git no identity and refuses to
/// guess one, as a machine with no name for its user does, and returns its
/// path.
fn no_identity(setup: &Setup) -> PathBuf {
    let path = setup.root().join("no-identity.gitconfig");
    write(&path, "[user]\n\tuseConfigOnly = true\n");
    path
}

#[test]
fn uncommitted_work_is_kept_with_the_stated_identity_when_git_has_none() {
    let setup = Setup::new();
    write(&setup.checkout().join("draft.txt"), "draft\n");
    let head = setup.head();
    let config = no_identity(&setup);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv with a git environment that has no identity"
    )]
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"))
        .args(["archive", "-w", "triage", "--yes", "--json"])
        .env_clear()
        .envs(
            setup
                .variables()
                .into_iter()
                .filter(|(name, _)| !name.starts_with("GIT_")),
        )
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(setup.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let archived: Value = serde_json::from_slice(&output.stdout).unwrap();
    let pin = &archived["data"]["pins"][0];
    let mirror = PathBuf::from(pin["repository"].as_str().unwrap());
    let wip = pin["reference"].as_str().unwrap().replace("/head", "/wip");
    assert_eq!(
        git(&mirror, &["log", "-1", "--format=%an <%ae>", &wip]).trim(),
        "mxv <mxv@localhost.invalid>"
    );
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(setup.head(), head, "the work comes back uncommitted");
    assert_eq!(
        fs::read_to_string(setup.checkout().join("draft.txt")).unwrap(),
        "draft\n"
    );
}

#[test]
fn a_refused_archive_leaves_the_processes_running() {
    let setup = Setup::new();
    write(
        &setup.checkout().join("mxv.toml"),
        "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"exec sleep 600\"]\n",
    );
    let (exit, document) = setup.run(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    // An archive cannot keep a socket, so it refuses.
    let socket = std::os::unix::net::UnixListener::bind(setup.workspace().join("socket")).unwrap();
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    drop(socket);
    let (_, status) = setup.run(&["status", "-w", "triage"]);
    let (_, _) = setup.run(&["down", "-w", "triage"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(status["data"][0]["state"], "running", "{document} {status}");
}

#[test]
fn removing_an_archive_with_delete_volumes_keeps_it_when_docker_does_not_answer() {
    let setup = Setup::new();
    setup.archive();
    let path = setup.root().join("state/mxv/archives/triage/archive.json");
    let record = fs::read_to_string(&path).unwrap().replacen(
        "\"state\": \"archived\",",
        "\"state\": \"archived\",\n  \"containers_ran\": true,",
        1,
    );
    write(&path, &record);
    let socket = format!("unix://{}", setup.root().join("none.sock").display());
    let output = setup
        .command(&setup.root())
        .args([
            "remove",
            "--archived",
            "triage",
            "--delete-volumes",
            "--yes",
            "--json",
        ])
        .env("DOCKER_HOST", socket)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let (_, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[test]
fn removing_an_archive_with_delete_volumes_refuses_while_a_live_workspace_has_its_id() {
    let setup = Setup::new();
    setup.archive();
    let archives = setup.root().join("state/mxv/archives");
    let path = archives.join("triage/archive.json");
    let record = fs::read_to_string(&path).unwrap();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    write(
        &path,
        &record.replacen(
            "\"state\": \"archived\",",
            "\"state\": \"archived\",\n  \"containers_ran\": true,",
            1,
        ),
    );
    fs::create_dir_all(archives.join("triage/root")).unwrap();
    let (exit, document) = setup.run(&[
        "remove",
        "--archived",
        "triage",
        "--delete-volumes",
        "--yes",
    ]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::archive::volumes_in_use",
        "{document}"
    );
}

#[test]
fn a_mirror_that_is_gone_refuses_with_its_code() {
    let setup = Setup::new();
    let (_, shown) = setup.run(&["show", "triage", "--full"]);
    let mirror = shown["data"][0]["checkouts"][0]["mirror"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_dir_all(&mirror).unwrap();
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::archive::mirror_missing",
        "{document}"
    );
}

#[test]
fn a_restore_without_its_mirror_refuses_and_from_origin_rebuilds_it() {
    let setup = Setup::new();
    let (_, shown) = setup.run(&["show", "triage", "--full"]);
    let mirror = shown["data"][0]["checkouts"][0]["mirror"]
        .as_str()
        .unwrap()
        .to_owned();
    let head = setup.head();
    setup.archive();
    fs::remove_dir_all(&mirror).unwrap();
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::restore::mirror_missing",
        "{document}"
    );
    assert!(
        document["recovery"]["text"]
            .as_str()
            .unwrap()
            .contains("--from-origin"),
        "{document}"
    );
    assert!(!Path::new(&mirror).exists(), "the refusal made a mirror");
    assert!(!setup.workspace().exists());
    let (exit, restored) = setup.run(&["restore", "triage", "--from-origin", "--yes"]);
    assert_eq!(exit, 0, "{restored}");
    assert_eq!(setup.head(), head);
    let (_, listed) = setup.run(&["list", "--archived"]);
    assert_eq!(listed["data"].as_array().unwrap().len(), 0);
}

#[test]
fn restore_from_origin_over_ssh_that_would_prompt_warns_and_restores_from_the_mirror() {
    let setup = Setup::new();
    let ssh = fake_ssh(&setup.root(), &setup.root().join("origin.git"));
    git(
        &setup.checkout(),
        &["remote", "set-url", "origin", SSH_ORIGIN],
    );
    let head = setup.head();
    setup.archive();
    let started = std::time::Instant::now();
    let (exit, restored) = Setup::finished(
        setup
            .command(&setup.root())
            .env("GIT_SSH_COMMAND", ssh.command("unknown")),
        &["restore", "triage", "--from-origin", "--yes"],
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(exit, 0, "{restored}\n{}", ssh.logged());
    let printed = restored.to_string();
    assert!(
        printed.contains("starts from what its mirror last fetched"),
        "{printed}"
    );
    assert!(printed.contains("example.invalid"), "{printed}");
    assert!(
        printed.contains("Host key verification failed."),
        "{printed}"
    );
    assert!(ssh.logged().contains("args: unknown "), "{}", ssh.logged());
    assert_eq!(setup.head(), head);
}

#[test]
fn an_unborn_head_refuses_with_the_code_of_a_checkout_that_cannot_be_read() {
    let setup = Setup::new();
    git(
        &setup.checkout(),
        &["switch", "--quiet", "--orphan", "empty"],
    );
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::archive::checkout_unusable",
        "{document}"
    );
}

#[test]
fn yes_alone_refuses_a_path_that_only_the_repository_manifest_makes_disposable() {
    let setup = Setup::new();
    setup.declare_manifest_disposable();
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::archive::manifest_disposable",
        "{document}"
    );
    assert!(setup.checkout().join("cache-dir/data").is_file());
}

#[test]
fn force_lists_a_path_that_only_the_repository_manifest_makes_disposable_as_lost() {
    let setup = Setup::new();
    setup.declare_manifest_disposable();
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    let lost = document["data"]["lost"].to_string();
    assert!(lost.contains("cache-dir"), "{document}");
}

#[test]
fn a_record_over_the_limit_refuses_with_its_code_before_anything_changes() {
    let setup = Setup::new();
    let many = setup.root().join("mxv/triage/many");
    for index in 0..12_000 {
        write(&many.join(format!("file-{index:05}.txt")), "x");
    }
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::archive::record_too_large",
        "{document}"
    );
    assert!(setup.checkout().join(".git").exists());
}

/// A run of `mxv archive` that waits at its question in a terminal, which
/// the `script` program provides, because mxv asks only a terminal.
struct Prompted {
    #[expect(
        clippy::disallowed_types,
        reason = "the test talks to the terminal program while it runs, which the snapshot harness cannot do"
    )]
    child: std::process::Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    /// What the terminal showed up to the end of the question.
    question: String,
}

impl Prompted {
    /// Starts `mxv archive -w triage` with `flags`, and returns once the
    /// question is on the terminal.
    fn start(setup: &Setup, flags: &[&str]) -> Self {
        let mut words = vec![env!("CARGO_BIN_EXE_mxv"), "archive", "-w", "triage"];
        words.extend(flags);
        words.push("--json");
        let line = words.join(" ");
        let args: Vec<&str> = if cfg!(target_os = "macos") {
            ["-q", "/dev/null"].into_iter().chain(words).collect()
        } else {
            vec!["-q", "-c", line.as_str(), "/dev/null"]
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "the test starts mxv in a terminal, which the snapshot harness cannot make"
        )]
        let mut child = std::process::Command::new("script")
            .args(args)
            .env_clear()
            .envs(setup.variables())
            .current_dir(setup.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut shown = Vec::new();
        let mut chunk = [0; 4096];
        while !String::from_utf8_lossy(&shown).contains("[y/N]") {
            let count = stdout.read(&mut chunk).unwrap();
            assert_ne!(
                count,
                0,
                "mxv ended before it asked: {}",
                String::from_utf8_lossy(&shown)
            );
            shown.extend_from_slice(&chunk[..count]);
        }
        Self {
            child,
            stdin,
            stdout,
            question: String::from_utf8_lossy(&shown).into_owned(),
        }
    }

    /// Answers the question, waits for the end of the run, and returns what
    /// the terminal showed after the question.
    fn answer(mut self, answer: &str) -> String {
        writeln!(self.stdin, "{answer}").unwrap();
        let mut shown = Vec::new();
        self.stdout.read_to_end(&mut shown).unwrap();
        drop(self.stdin);
        self.child.wait().unwrap();
        String::from_utf8_lossy(&shown).into_owned()
    }
}

#[test]
fn a_process_that_starts_while_the_question_waits_refuses_the_archive() {
    let setup = Setup::new();
    let prompted = Prompted::start(&setup, &[]);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts a process outside mxv whose working directory is in the workspace"
    )]
    let mut outsider = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(setup.checkout())
        .spawn()
        .unwrap();
    let shown = prompted.answer("y");
    outsider.kill().unwrap();
    outsider.wait().unwrap();
    assert!(shown.contains("mxv::archive::in_use"), "{shown}");
    assert!(setup.checkout().is_dir(), "{shown}");
}

#[test]
fn the_question_names_each_checkout_whose_uncommitted_work_it_keeps() {
    let setup = Setup::new();
    write(&setup.checkout().join("draft.txt"), "draft\n");
    let head = setup.head();
    let prompted = Prompted::start(&setup, &[]);
    let question = prompted.question.clone();
    prompted.answer("n");
    assert!(
        question.contains("It keeps the uncommitted work of app on pins"),
        "{question}"
    );
    assert_eq!(setup.head(), head, "a declined archive commits nothing");
    assert!(setup.checkout().join("draft.txt").is_file());
}

#[test]
fn the_question_names_the_ignored_files_that_the_archive_keeps() {
    let setup = Setup::new();
    setup.ignore_env();
    let prompted = Prompted::start(&setup, &[]);
    let question = prompted.question.clone();
    prompted.answer("n");
    assert!(
        question.contains(
            "It keeps 1 ignored file of app in a private file that only you can read, and restore writes them back."
        ),
        "{question}"
    );
    assert!(setup.checkout().join(".env").is_file());
}

#[test]
fn a_dry_run_names_what_the_archive_keeps_and_the_restore_names_what_it_put_back() {
    let setup = Setup::new();
    let checkout = setup.checkout();
    write(&checkout.join("stashed.txt"), "stash\n");
    git(&checkout, &["add", "stashed.txt"]);
    git(&checkout, &["stash", "--quiet"]);
    git(&checkout, &["branch", "spike-cache"]);
    write(&checkout.join("draft.txt"), "draft\n");
    setup.ignore_env();
    write(&checkout.join("target/debug/built"), "built\n");
    let output = setup
        .command(&setup.root())
        .args(["archive", "-w", "triage", "--dry-run"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    for line in [
        "mxv would keep the untracked file app/draft.txt with the uncommitted work.",
        "mxv would keep 1 stash of app.",
        "mxv would keep the local branch spike-cache of app.",
        "mxv would keep 1 ignored file of app, 9 B in all, in a private file that only you can read: app/.env.",
        "mxv would delete the disposable directory app/target/ (",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    setup.archive();
    let output = setup
        .command(&setup.root())
        .args(["restore", "triage", "--yes"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    for line in [
        "mxv put back the uncommitted work of app: 1 untracked file.",
        "mxv put back 1 stash of app.",
        "mxv put back the local branch spike-cache of app.",
        "mxv put back 1 ignored file of app: app/.env.",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
}

#[test]
fn the_pin_of_a_clone_is_written_with_its_reference_synced() {
    let setup = Setup::new();
    setup.commit_locally();
    let trace = setup.root().join("trace.json");
    let output = setup
        .command(&setup.root())
        .args(["archive", "-w", "triage", "--yes", "--json"])
        .env("GIT_TRACE2_EVENT", trace.display().to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let events: Vec<Value> = fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let writer = events
        .iter()
        .find(|event| {
            event["event"] == "start" && event["argv"].to_string().contains(":refs/mxv/archives/")
        })
        .expect("git's trace names the command that wrote the pin");
    assert!(
        events
            .iter()
            .any(|event| event["sid"] == writer["sid"] && event["category"] == "fsync"),
        "the command that wrote the pin synced nothing: {writer}"
    );
}

#[test]
fn the_objects_of_uncommitted_work_are_written_with_git_syncing_them() {
    let setup = Setup::new();
    write(&setup.checkout().join("draft.txt"), "draft\n");
    let trace = setup.root().join("trace.json");
    let output = setup
        .command(&setup.root())
        .args(["archive", "-w", "triage", "--yes", "--json"])
        .env("GIT_TRACE2_EVENT", trace.display().to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let writers: Vec<Value> = fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| {
            let argv = event["argv"].to_string();
            event["event"] == "start"
                && (argv.contains("\"write-tree\"")
                    || argv.contains("\"commit-tree\"")
                    || argv.contains("\"add\",\"--all\""))
        })
        .collect();
    assert!(
        writers
            .iter()
            .any(|event| event["argv"].to_string().contains("\"commit-tree\"")),
        "no commit-tree in the trace"
    );
    for event in &writers {
        assert!(
            event["argv"]
                .to_string()
                .contains("core.fsync=reference,loose-object"),
            "a command that writes objects runs without syncing them: {}",
            event["argv"]
        );
    }
}

#[test]
fn a_restore_replaces_a_taken_port_from_the_configured_range_in_a_dry_run_and_a_real_run() {
    let setup = Setup::new();
    setup.archive();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let held = listener.local_addr().unwrap().port();
    write(
        &setup.root().join("config/mxv/config.toml"),
        "ports_range = [31000, 31050]\n",
    );
    let path = setup.root().join("state/mxv/archives/triage/archive.json");
    let record = fs::read_to_string(&path).unwrap().replace(
        "\"version\": 3,",
        &format!("\"version\": 3,\n  \"ports\": {{ \"web\": {held} }},"),
    );
    write(&path, &record);
    for args in [
        ["restore", "triage", "--dry-run"].as_slice(),
        ["restore", "triage", "--yes"].as_slice(),
    ] {
        let (exit, document) = setup.run(args);
        assert_eq!(exit, 0, "{document}");
        let port = document["data"]["ports"][0]["to"]
            .as_u64()
            .unwrap_or_else(|| panic!("{document}"));
        assert!((31000..=31050).contains(&port), "{document}");
    }
}

#[test]
fn a_retiring_archive_that_meets_a_new_workspace_names_the_way_out() {
    let setup = Setup::new();
    setup.archive();
    let path = setup.root().join("state/mxv/archives/triage/archive.json");
    let record = fs::read_to_string(&path)
        .unwrap()
        .replace("\"state\": \"archived\"", "\"state\": \"retiring\"");
    write(&path, &record);
    let (exit, document) = setup.run(&["new", "triage", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::name_taken");
    let hint = document.to_string();
    assert!(hint.contains("mxv repair triage"), "{document}");
    assert!(!hint.contains("remove --archived"), "{document}");
}

impl Setup {
    /// Returns the data directory of the archive `triage`, which holds its
    /// ignored files.
    fn archive_data(&self) -> PathBuf {
        let record: Value =
            serde_json::from_str(&fs::read_to_string(self.archive_record("triage")).unwrap())
                .unwrap();
        let id = record["id"].as_str().unwrap();
        self.root().join("data/mxv/archives").join(id)
    }

    /// Moves the archive `triage` aside through `mxv repair`, as a repair
    /// of an archive whose pins are gone does, and returns the directory
    /// it moved to and its data directory.
    fn move_archive_aside(&self) -> (PathBuf, PathBuf) {
        self.ignore_env();
        self.archive();
        self.mark_retired();
        self.delete_pins();
        let data = self.archive_data();
        let (exit, document) = self.run(&["repair", "triage", "--yes"]);
        assert_eq!(exit, 0, "{document}");
        let aside = PathBuf::from(document["data"]["kept"][0].as_str().unwrap());
        assert!(aside.exists() && data.exists(), "{document}");
        (aside, data)
    }

    /// Writes a data directory of ignored files whose archive id no record
    /// names, as a person who deleted a moved-aside archive by hand leaves
    /// one, and returns it.
    fn plant_unnamed_data(&self) -> PathBuf {
        let data = self
            .root()
            .join("data/mxv/archives/0123456789abcdef0123456789abcdef");
        write(&data.join("ignored.tar"), "the only copy\n");
        data
    }
}

/// Returns the rows of `mxv repair --orphans` in `document`.
fn orphan_rows(document: &Value) -> &Vec<Value> {
    document["data"]["orphans"]
        .as_array()
        .unwrap_or_else(|| panic!("{document}"))
}

/// Returns the row of `document` whose kind is `kind`.
fn orphan_row<'a>(document: &'a Value, kind: &str) -> &'a Value {
    orphan_rows(document)
        .iter()
        .find(|row| row["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} row: {document}"))
}

/// Returns every path below `dir` with its contents, in path order.
fn tree(dir: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        if path.is_dir() && !path.is_symlink() {
            for entry in fs::read_dir(&path).unwrap() {
                stack.push(entry.unwrap().path());
            }
            found.push((path, None));
        } else {
            let bytes = fs::read(&path).ok();
            found.push((path, bytes));
        }
    }
    found.sort();
    found
}

#[test]
fn an_archive_moved_aside_by_repair_is_kept_by_orphans_until_delete_aside() {
    let setup = Setup::new();
    let (aside, data) = setup.move_archive_aside();

    for args in [
        &["repair", "--orphans"][..],
        &["repair", "--orphans", "--yes"],
    ] {
        let (exit, document) = setup.run(args);
        assert_eq!(exit, 0, "{document}");
        assert_eq!(document["data"]["state"], "unchanged", "{document}");
        let row = orphan_row(&document, "moved_aside");
        assert_eq!(row["state"], "kept", "{document}");
        assert_eq!(row["path"], aside.display().to_string());
        assert!(row["size"].as_u64().unwrap() > 0, "{document}");
        assert!(
            row["reasons"].to_string().contains("--delete-aside"),
            "{document}"
        );
        assert!(aside.exists() && data.exists());
    }

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "swept", "{document}");
    assert_eq!(orphan_row(&document, "moved_aside")["state"], "deleted");
    assert!(!aside.exists(), "{document}");
    assert!(!data.exists(), "{document}");

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert!(orphan_rows(&document).is_empty(), "{document}");
}

#[test]
fn an_ignored_tarball_with_no_record_is_deleted_by_orphans_with_delete_aside() {
    let setup = Setup::new();
    setup.ignore_env();
    setup.archive();
    let named = setup.archive_data();
    let data = setup.plant_unnamed_data();

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(orphan_rows(&document).len(), 1, "{document}");
    let row = orphan_row(&document, "ignored_files");
    assert_eq!(row["state"], "deleted", "{document}");
    assert_eq!(row["path"], data.display().to_string());
    assert!(!data.exists());
    assert!(named.join("ignored.tar").exists(), "{document}");
}

#[test]
fn repair_orphans_lists_and_dry_runs_without_writing_anything() {
    let setup = Setup::new();
    setup.move_archive_aside();
    setup.plant_unnamed_data();
    // mxv's own usage log in state/mxv grows with each command, so only the
    // archives and their data directories are compared.
    let archives = setup.root().join("state/mxv/archives");
    let before = tree(&archives);
    let data_before = tree(&setup.root().join("data"));

    let (exit, document) = setup.run(&["repair", "--orphans"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(orphan_rows(&document).len(), 2, "{document}");
    let (exit, document) = setup.run(&[
        "repair",
        "--orphans",
        "--delete-aside",
        "--dry-run",
        "--yes",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "would_sweep", "{document}");
    assert_eq!(orphan_rows(&document).len(), 2, "{document}");
    for row in orphan_rows(&document) {
        assert_eq!(row["state"], "would_delete", "{document}");
    }
    assert_eq!(tree(&archives), before);
    assert_eq!(tree(&setup.root().join("data")), data_before);
}

#[test]
fn repair_orphans_refuses_and_deletes_nothing_while_an_archive_record_cannot_be_read() {
    let setup = Setup::new();
    let (aside, data) = setup.move_archive_aside();
    let unnamed = setup.plant_unnamed_data();
    write(&setup.archive_record("other"), "not a record\n");

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::record::corrupt", "{document}");
    assert!(aside.exists() && data.exists() && unnamed.exists());
}

#[test]
fn repair_orphans_refuses_while_a_moved_aside_record_cannot_be_read() {
    let setup = Setup::new();
    let (aside, data) = setup.move_archive_aside();
    let unnamed = setup.plant_unnamed_data();
    write(&aside.join("archive.json"), "not a record\n");

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::record::corrupt", "{document}");
    assert!(aside.exists() && data.exists() && unnamed.exists());
}

#[test]
fn a_hidden_directory_without_its_own_record_is_kept_by_orphans_and_never_deleted() {
    let setup = Setup::new();
    let (aside, data) = setup.move_archive_aside();
    let archives = aside.parent().unwrap();
    let empty = archives.join(".notes.0123456789abcdef0123456789abcdef");
    write(&empty.join("notes.md"), "mine\n");
    let other = archives.join(".elsewhere.0123456789abcdef0123456789abcdef");
    write(
        &other.join("archive.json"),
        &fs::read_to_string(aside.join("archive.json")).unwrap(),
    );

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(orphan_rows(&document).len(), 3, "{document}");
    assert_eq!(
        orphan_row(&document, "moved_aside")["path"],
        aside.display().to_string()
    );
    let unrecorded: Vec<&Value> = orphan_rows(&document)
        .iter()
        .filter(|row| row["kind"] == "unrecorded")
        .collect();
    let paths: Vec<&str> = unrecorded
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [other.to_str().unwrap(), empty.to_str().unwrap()],
        "{document}"
    );
    for row in unrecorded {
        assert_eq!(row["state"], "kept", "{document}");
        let reason = row["reasons"][0].as_str().unwrap();
        assert!(reason.contains("mxv never deletes it"), "{document}");
    }
    assert!(!aside.exists() && !data.exists(), "{document}");
    assert!(empty.join("notes.md").exists(), "{document}");
    assert!(other.join("archive.json").exists(), "{document}");
}

#[test]
fn a_sweep_whose_deletion_fails_names_what_it_deleted_and_exits_with_the_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    let setup = Setup::new();
    let (aside, data) = setup.move_archive_aside();
    let unnamed = setup.plant_unnamed_data();
    // A directory without write permission keeps its file, so its deletion
    // fails after the moved-aside archive, which sorts first, is deleted.
    fs::set_permissions(&unnamed, fs::Permissions::from_mode(0o555)).unwrap();

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    fs::set_permissions(&unnamed, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["data"]["state"], "swept", "{document}");
    assert_eq!(orphan_row(&document, "moved_aside")["state"], "deleted");
    // The deletion began, so the row does not claim the item is whole.
    let row = orphan_row(&document, "ignored_files");
    assert_eq!(row["state"], "partly_deleted", "{document}");
    assert_eq!(row["deleted"], serde_json::json!([]), "{document}");
    assert_eq!(row["reasons"].as_array().unwrap().len(), 1, "{document}");
    let failures = document["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{document}");
    assert_eq!(failures[0]["code"], "mxv::file::unwritable", "{document}");
    assert!(!aside.exists() && !data.exists(), "{document}");
    assert!(unnamed.join("ignored.tar").exists(), "{document}");
}

#[test]
fn a_moved_aside_archive_whose_data_cannot_be_deleted_names_its_deleted_directory() {
    use std::os::unix::fs::PermissionsExt as _;

    let setup = Setup::new();
    let (aside, data) = setup.move_archive_aside();
    // The archive's directory is deleted first, and then its data
    // directory, which keeps its file.
    fs::set_permissions(&data, fs::Permissions::from_mode(0o555)).unwrap();

    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(exit, 1, "{document}");
    let row = orphan_row(&document, "moved_aside");
    assert_eq!(row["state"], "partly_deleted", "{document}");
    assert_eq!(
        row["deleted"],
        serde_json::json!([aside.display().to_string()]),
        "{document}"
    );
    assert_eq!(
        document["failures"][0]["code"], "mxv::file::unwritable",
        "{document}"
    );
    assert!(!aside.exists() && data.exists(), "{document}");

    // A second sweep lists the data directory as unnamed and deletes it.
    let (exit, document) = setup.run(&["repair", "--orphans", "--delete-aside", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(orphan_row(&document, "ignored_files")["state"], "deleted");
    assert!(!data.exists(), "{document}");
}

impl Setup {
    /// Starts a process in the workspace `triage` that writes its pid to
    /// `pid` in the checkout, and returns the workspace's stable id.
    fn start_process(&self) -> String {
        write(
            &self.checkout().join("mxv.toml"),
            "[processes.web]\nargv = [\"sh\", \"-c\", \"echo $$ > pid; echo ready; exec sleep 60\"]\nready = { log = \"^ready\", timeout_seconds = 20 }\n",
        );
        let (exit, document) = self.run(&["up", "-w", "triage"]);
        assert_eq!(exit, 0, "{document}");
        let record: Value =
            serde_json::from_str(&fs::read_to_string(self.workspace_record()).unwrap()).unwrap();
        record["id"].as_str().unwrap().to_owned()
    }

    /// Returns the path of the record of the workspace `triage`.
    fn workspace_record(&self) -> PathBuf {
        self.root().join("state/mxv/workspaces/triage.json")
    }
}

/// Reports whether a process with the pid `pid` runs.
fn runs(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .env("PATH", PATH)
        .output()
        .is_ok_and(|output| output.status.success())
}

#[test]
fn run_files_left_by_a_stopped_remove_are_forgotten_by_mxv_repair_orphans() {
    let setup = Setup::new();
    let id = setup.start_process();
    let pid = fs::read_to_string(setup.checkout().join("pid"))
        .unwrap()
        .trim()
        .to_owned();
    assert!(runs(&pid));
    let record = setup
        .root()
        .join("state/mxv/phase/runs")
        .join(format!("{id}.json"));
    let logs = setup.root().join("state/mxv/phase/logs").join(&id);
    let files = setup.root().join("state/mxv/runs/triage");
    assert!(record.exists() && logs.exists() && files.exists());
    // A removal killed after it deleted the record leaves the process, the
    // run record, the logs and the run files, with nothing that names
    // them.
    fs::remove_file(setup.workspace_record()).unwrap();

    let (exit, document) = setup.run(&["repair", "--orphans", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "would_sweep", "{document}");
    assert_eq!(orphan_row(&document, "run_files")["name"], "triage");
    assert_eq!(orphan_row(&document, "run_record")["id"], id.as_str());
    assert!(runs(&pid) && record.exists() && logs.exists() && files.exists());

    let (exit, document) = setup.run(&["repair", "--orphans", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "swept", "{document}");
    assert_eq!(orphan_row(&document, "run_files")["state"], "deleted");
    assert_eq!(orphan_row(&document, "run_record")["state"], "deleted");
    assert!(!runs(&pid), "{document}");
    assert!(!record.exists() && !logs.exists() && !files.exists());

    let (exit, document) = setup.run(&["repair", "--orphans", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "unchanged", "{document}");
    assert!(orphan_rows(&document).is_empty(), "{document}");
}

#[test]
fn delete_volumes_keeps_the_logs_and_deletes_nothing_when_docker_does_not_answer() {
    let setup = Setup::new();
    let id = "0123456789abcdef0123456789abcdef";
    let logs = setup.root().join("state/mxv/phase/logs").join(id);
    write(&logs.join("web.log"), "ready\n");
    let socket = format!("unix://{}", setup.root().join("none.sock").display());

    let output = setup
        .command(&setup.root())
        .args(["repair", "--orphans", "--delete-volumes", "--yes", "--json"])
        .env("DOCKER_HOST", socket)
        .output()
        .unwrap();
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{document}");
    // The listing asks docker for the volumes of ids that no record holds,
    // so it fails before the sweep deletes anything, and the logs keep the
    // id listed for the next sweep.
    let code = document["code"].as_str().unwrap();
    assert!(code.starts_with("mxv::docker::"), "{document}");
    assert!(logs.join("web.log").exists(), "{document}");

    let (exit, document) = setup.run(&["repair", "--orphans", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(orphan_row(&document, "run_record")["id"], id, "{document}");
}

#[test]
fn a_run_record_that_phase_keeps_is_kept_by_orphans_with_its_reason() {
    let setup = Setup::new();
    let id = setup.start_process();
    let pid = fs::read_to_string(setup.checkout().join("pid"))
        .unwrap()
        .trim()
        .to_owned();
    let record = setup
        .root()
        .join("state/mxv/phase/runs")
        .join(format!("{id}.json"));
    let logs = setup.root().join("state/mxv/phase/logs").join(&id);
    let original = fs::read_to_string(&record).unwrap();
    // phase never signals group 1, so it cannot stop an entry that names
    // it and keeps the entry in its record.
    let mut planted: Value = serde_json::from_str(&original).unwrap();
    set_groups(&mut planted, 1);
    write(&record, &serde_json::to_string_pretty(&planted).unwrap());
    fs::remove_file(setup.workspace_record()).unwrap();

    let (exit, document) = setup.run(&["repair", "--orphans", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let row = orphan_row(&document, "run_record");
    assert_eq!(row["state"], "kept", "{document}");
    let reasons = row["reasons"].to_string();
    assert!(reasons.contains("phase cannot tell whether"), "{document}");
    assert!(record.exists() && logs.exists(), "{document}");
    assert!(runs(&pid), "{document}");

    // The person stops the process, here by putting the record back so
    // that phase can, and the next sweep deletes the record and the logs.
    write(&record, &original);
    let (exit, document) = setup.run(&["repair", "--orphans", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(orphan_row(&document, "run_record")["state"], "deleted");
    assert!(
        !runs(&pid) && !record.exists() && !logs.exists(),
        "{document}"
    );
}

/// Sets every `group` field below `value` to `group`.
fn set_groups(value: &mut Value, group: u64) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if key == "group" && field.is_number() {
                    *field = Value::from(group);
                } else {
                    set_groups(field, group);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| set_groups(item, group)),
        _ => {}
    }
}

/// Sets the state of the record of the workspace `triage` to `state`, as
/// a `mxv new` that stopped before its last write leaves `creating`.
fn set_state(setup: &Setup, state: &str) {
    let path = setup.root().join("state/mxv/workspaces/triage.json");
    let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record["state"] = Value::from(state);
    write(&path, &serde_json::to_string_pretty(&record).unwrap());
}

#[test]
fn an_archive_refuses_a_workspace_that_mxv_new_did_not_finish_and_names_mxv_remove() {
    let setup = Setup::new();
    set_state(&setup, "creating");
    for args in [
        &["archive", "-w", "triage", "--yes"][..],
        &["archive", "-w", "triage", "--dry-run"][..],
    ] {
        let (exit, document) = setup.run(args);
        assert_eq!(exit, 1, "{args:?}: {document}");
        assert_eq!(document["code"], "mxv::workspace::creating", "{document}");
        assert!(
            document.to_string().contains("mxv remove triage"),
            "{document}"
        );
    }
    assert!(setup.checkout().exists());
}

#[test]
fn an_archive_of_a_live_workspace_runs() {
    let setup = Setup::new();
    set_state(&setup, "live");
    setup.archive();
    assert!(!setup.checkout().exists());
}

impl Setup {
    /// Archives the workspace `triage`, then puts back its record in the
    /// state `retiring` that names the archive, and gives the archive
    /// `state`, as an archive that stopped partway leaves the two. With
    /// `archived_record` false, the archive record is deleted instead, as
    /// a run that stopped right after the workspace's state leaves it.
    fn stopped_archive(&self, state: &str, archived_record: bool) {
        let path = self.root().join("state/mxv/workspaces/triage.json");
        let mut kept: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        self.archive();
        let archive: Value =
            serde_json::from_str(&fs::read_to_string(self.archive_record("triage")).unwrap())
                .unwrap();
        kept["state"] = serde_json::json!({ "retiring": { "archive": archive["id"] } });
        write(&path, &serde_json::to_string_pretty(&kept).unwrap());
        if archived_record {
            self.mark(state);
        } else {
            fs::remove_dir_all(self.archive_record("triage").parent().unwrap()).unwrap();
        }
    }
}

/// Returns the warnings of `document`, a result of `mxv remove`.
fn warnings(document: &Value) -> String {
    document["data"]["warnings"].to_string()
}

#[test]
fn a_workspace_removed_after_its_archive_stopped_partway_leaves_an_archive_that_only_repair_moves()
{
    let setup = Setup::new();
    setup.stopped_archive("retiring", true);
    let (exit, document) = setup.run(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let warned = warnings(&document);
    assert!(warned.contains("can no longer be resumed"), "{warned}");
    assert!(warned.contains("mxv repair triage"), "{warned}");

    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::workspace_removed");
    assert!(
        document.to_string().contains("mxv repair triage"),
        "{document}"
    );
    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 1, "{document}");
    let row = &document["data"][0];
    assert_eq!(
        row["problem"],
        "the archive stopped before it finished, and its workspace was removed"
    );
    assert_eq!(row["repair"], "mxv repair triage");
    let (exit, document) = setup.run(&["repair", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["state"], "moved_aside", "{document}");
    let (exit, document) = setup.run(&["list", "--archived"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["total"], 0, "{document}");
}

#[test]
fn a_workspace_removed_after_its_archive_retired_every_checkout_leaves_an_archive_that_archive_w_finishes()
 {
    let setup = Setup::new();
    let head = setup.head();
    setup.stopped_archive("retired", true);
    let (exit, document) = setup.run(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let warned = warnings(&document);
    assert!(warned.contains("mxv archive -w triage"), "{warned}");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(git(&setup.checkout(), &["rev-parse", "HEAD"]).trim(), head);
}

#[test]
fn removing_a_retiring_workspace_whose_archive_record_was_never_written_warns_of_no_archive() {
    let setup = Setup::new();
    setup.stopped_archive("retiring", false);
    let (exit, document) = setup.run(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let warned = warnings(&document);
    assert!(!warned.contains("mxv repair"), "{warned}");
    assert!(!warned.contains("mxv archive"), "{warned}");
}

#[test]
fn pin_on_a_retiring_workspace_keeps_its_state() {
    let setup = Setup::new();
    let path = setup.root().join("state/mxv/workspaces/triage.json");
    let mut record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let state = serde_json::json!({ "retiring": { "archive": "a".repeat(32) } });
    record["state"] = state.clone();
    write(&path, &serde_json::to_string_pretty(&record).unwrap());
    let (exit, document) = setup.run(&["pin", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let record: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["state"], state, "{record}");
    assert_eq!(record["pinned"], true, "{record}");
}

#[test]
fn removing_a_retired_archive_is_refused_and_an_archived_one_is_deleted() {
    let setup = Setup::new();
    setup.archive();
    setup.mark_retired();
    let (exit, document) = setup.run(&["remove", "--archived", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::archive::unfinished");
    assert!(setup.archive_record("triage").exists());
    setup.mark_archived();
    let (exit, document) = setup.run(&["remove", "--archived", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!setup.archive_record("triage").exists());
}
