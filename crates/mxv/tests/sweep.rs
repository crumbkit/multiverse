//! End-to-end tests of `mxv sweep`, and of the sweep that `mxv new` runs
//! first. Each test builds a repository in a temporary directory, merges a
//! workspace's branch into origin's `main`, and runs the built `mxv`
//! binary with a fake `board` program on its `PATH`.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the files and directories that they and the code under test write"
)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git, repository};

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

    /// Writes a fake `board` that appends its arguments to `board.log`
    /// and exits with `code`.
    fn fake_board(&self, code: i32) {
        let bin = self.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(self.root().join("board")).unwrap();
        let program = bin.join("board");
        let log = self.root().join("board.log");
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes a program that stands in for board"
        )]
        fs::write(
            &program,
            format!("#!/bin/sh\necho \"$@\" >> {}\nexit {code}\n", log.display()),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn board_log(&self) -> String {
        fs::read_to_string(self.root().join("board.log")).unwrap_or_default()
    }

    fn command(&self) -> Command {
        let at = |name: &str| self.root().join(name).display().to_string();
        let path = format!("{}:{PATH}", self.root().join("bin").display());
        let mut command = Command::new(env!("CARGO_BIN_EXE_mxv"))
            .current_dir(self.root())
            .env_clear()
            .env("HOME", at("home"))
            .env("PATH", path)
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

    fn text(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command().args(args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).arg("--json").output().unwrap();
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?}: {error}: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }

    fn checkout(&self, name: &str) -> PathBuf {
        self.root().join("mxv").join(name).join("app")
    }

    /// Makes the workspace `name` with a commit of its own, merged into
    /// origin's `main` when `merged` is true.
    fn workspace(&self, name: &str, merged: bool, extra: &[&str]) {
        let mut args = vec!["new", name, "--repo", "app", "--no-sweep"];
        args.extend_from_slice(extra);
        let (exit, _, stderr) = self.text(&args);
        assert_eq!(exit, 0, "{stderr}");
        let checkout = self.checkout(name);
        commit(&checkout, &format!("{name}.md"));
        if merged {
            git(
                &checkout,
                &["push", "--quiet", "origin", "HEAD", "HEAD:main"],
            );
        }
    }

    /// Puts a `gh` program that always fails on the `PATH`, as when
    /// GitHub cannot be reached.
    fn failing_gh(&self) {
        let bin = self.root().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let program = bin.join("gh");
        #[expect(
            clippy::disallowed_methods,
            reason = "the test writes a program that stands in for gh"
        )]
        fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn live(&self) -> Vec<String> {
        let list = self.json(&["list"]);
        list["data"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn commit(checkout: &Path, file: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would edit"
    )]
    fs::write(checkout.join(file), "text").unwrap();
    git(checkout, &["add", file]);
    git(checkout, &["commit", "--quiet", "-m", "Change"]);
}

fn row<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name}: {document}"))
}

#[test]
fn a_dry_run_names_the_merged_workspace_and_keeps_the_unmerged_one() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    setup.workspace("open", false, &[]);
    let document = setup.json(&["sweep", "--dry-run"]);
    assert_eq!(row(&document, "merged")["state"], "would_archive");
    assert_eq!(row(&document, "open")["state"], "kept");
    assert!(
        row(&document, "open")["reason"]
            .as_str()
            .unwrap()
            .contains("not merged")
    );
    assert_eq!(setup.live().len(), 2);
}

#[test]
fn sweep_archives_the_merged_workspace_and_reports_the_space_it_freed() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    setup.workspace("open", false, &[]);
    let (exit, stdout, stderr) = setup.text(&["sweep", "--yes"]);
    assert_eq!(exit, 0, "{stderr}");
    let printed = format!("{stdout}{stderr}");
    assert!(
        printed.contains("archived 1 workspace and freed"),
        "{printed}"
    );
    assert_eq!(setup.live(), vec!["open".to_owned()]);
    assert!(!setup.root().join("mxv/merged").exists());
}

#[test]
fn a_workspace_with_uncommitted_changes_is_kept() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would edit"
    )]
    fs::write(setup.checkout("merged").join("scratch.md"), "more").unwrap();
    let document = setup.json(&["sweep", "--yes"]);
    assert_eq!(row(&document, "merged")["state"], "kept");
    assert!(setup.root().join("mxv/merged").exists());
}

#[test]
fn sweep_updates_the_board_mount_that_mxv_new_recorded() {
    let setup = Setup::new();
    setup.fake_board(0);
    let mount = format!("{}:/repos/app", setup.root().join("board").display());
    setup.workspace("merged", true, &["--board-mount", &mount]);
    let (exit, _, stderr) = setup.text(&["sweep", "--yes"]);
    assert_eq!(exit, 0, "{stderr}");
    let board = setup.root().join("board");
    assert_eq!(
        setup.board_log().trim(),
        format!("-C {} mount update /repos/app", board.display())
    );
}

#[test]
fn a_failed_board_mount_update_warns_and_names_the_command_to_run_again() {
    let setup = Setup::new();
    setup.fake_board(1);
    let mount = format!("{}:/repos/app", setup.root().join("board").display());
    setup.workspace("merged", true, &["--board-mount", &mount]);
    let (exit, _, stderr) = setup.text(&["sweep", "--yes"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(stderr.contains("mount update /repos/app"), "{stderr}");
    assert!(stderr.contains("to retry"), "{stderr}");
    assert!(setup.live().is_empty());
}

#[test]
fn mxv_new_archives_a_merged_workspace_first_and_says_so() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    let (exit, _, stderr) = setup.text(&["new", "fresh", "--repo", "app"]);
    assert_eq!(exit, 0, "{stderr}");
    assert!(
        stderr.contains("archived 1 workspace and freed"),
        "{stderr}"
    );
    assert_eq!(setup.live(), vec!["fresh".to_owned()]);
}

#[test]
fn mxv_new_keeps_a_merged_workspace_of_the_name_it_was_given_and_refuses_the_name() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    let (exit, _, stderr) = setup.text(&["new", "merged", "--repo", "app"]);
    assert_ne!(exit, 0, "{stderr}");
    assert!(!stderr.contains("archived"), "{stderr}");
    assert_eq!(setup.live(), vec!["merged".to_owned()]);
}

#[test]
fn mxv_new_still_creates_the_workspace_when_gh_fails_for_an_unpublished_branch() {
    let setup = Setup::new();
    setup.failing_gh();
    setup.workspace("local", false, &[]);
    let (exit, _, stderr) = setup.text(&["new", "fresh", "--repo", "app"]);
    assert_eq!(exit, 0, "{stderr}");
    let mut live = setup.live();
    live.sort();
    assert_eq!(live, vec!["fresh".to_owned(), "local".to_owned()]);
}

#[test]
fn mxv_new_with_no_sweep_leaves_a_merged_workspace_alone() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    let (exit, _, stderr) = setup.text(&["new", "fresh", "--repo", "app", "--no-sweep"]);
    assert_eq!(exit, 0, "{stderr}");
    assert_eq!(setup.live().len(), 2);
}

#[test]
fn a_board_mount_without_a_path_is_refused() {
    let setup = Setup::new();
    let (exit, _, stderr) = setup.text(&["new", "x", "--repo", "app", "--board-mount", "nocolon"]);
    assert_ne!(exit, 0);
    assert!(stderr.contains("BOARD:PATH"), "{stderr}");
}

/// Sets the mode bits of `path`.
fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_failed_write_of_the_board_mounts_fails_mxv_new_and_leaves_the_workspace_without_mounts() {
    let setup = Setup::new();
    setup.fake_board(0);
    let records = setup.root().join("state/mxv/workspaces");
    // mxv new reads the ignored files of the source checkout after it has
    // written the live record and before it records the mounts. This git
    // makes the record directory read-only at that moment, so only the
    // write of the mounts fails.
    let bin = setup.root().join("bin");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a program that stands in for git"
    )]
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\ncase \"$*\" in *--ignored*) chmod 555 {} ;; esac\nfor dir in $(echo {PATH} | tr : ' '); do [ -x \"$dir/git\" ] && exec \"$dir/git\" \"$@\"; done\nexit 127\n",
            records.display()
        ),
    )
    .unwrap();
    set_mode(&bin.join("git"), 0o755);
    let mount = format!("{}:/repos/app", setup.root().join("board").display());
    let (exit, _, stderr) = setup.text(&[
        "new",
        "mounted",
        "--repo",
        "app",
        "--no-sweep",
        "--board-mount",
        &mount,
    ]);
    set_mode(&records, 0o755);
    fs::remove_file(bin.join("git")).unwrap();
    assert_ne!(exit, 0, "{stderr}");
    let record: Value =
        serde_json::from_str(&fs::read_to_string(records.join("mounted.json")).unwrap()).unwrap();
    assert_eq!(record["state"], "live", "{record}");
    assert!(record.get("board_mounts").is_none(), "{record}");
    assert_eq!(setup.live(), vec!["mounted".to_owned()]);
}

#[test]
fn mxv_new_warns_and_creates_the_workspace_when_the_sweep_itself_fails() {
    let setup = Setup::new();
    setup.workspace("merged", true, &[]);
    let records = setup.root().join("state/mxv/workspaces");
    // Without read permission the sweep cannot list the records, while
    // mxv new can still write and read a record by its name.
    set_mode(&records, 0o333);
    let (exit, _, stderr) = setup.text(&["new", "fresh", "--repo", "app"]);
    set_mode(&records, 0o755);
    assert_eq!(exit, 0, "{stderr}");
    assert!(
        stderr.contains("mxv could not sweep merged workspaces"),
        "{stderr}"
    );
    let mut live = setup.live();
    live.sort();
    assert_eq!(live, vec!["fresh".to_owned(), "merged".to_owned()]);
}
