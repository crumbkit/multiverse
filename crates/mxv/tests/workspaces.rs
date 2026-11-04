//! End-to-end tests of the workspace verbs `new`, `list`, `show`, `cd` and
//! `remove`. Each test builds a bare `origin` and a clone of it in a
//! temporary directory, and runs the built `mxv` binary with its own state
//! directory and workspace root there.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git, repository};

/// A temporary directory with a repository, a workspace root and a state
/// directory.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The clone `inbox`.
    clone: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clone = repository(&dir.path().canonicalize().unwrap());
        Self { dir, clone }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
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

    /// Runs `mxv` with `args`, and returns the exit code and standard
    /// output.
    fn text(&self, args: &[&str]) -> (i32, String) {
        let output = self.command(args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
        )
    }

    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.root().join("workspaces").join(name)
    }
}

fn created(setup: &Setup, name: &str) -> PathBuf {
    let (exit, document) = setup.json(&["new", name, "--repo", "inbox"]);
    assert_eq!(exit, 0, "{document}");
    setup.workspace(name)
}

#[test]
fn new_checks_out_the_repository_inside_the_workspace_directory_on_a_branch_of_the_workspaces_name()
{
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "inbox"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["schema"], "mxv/new/v1");
    let workspace = setup.workspace("reading-list");
    let checkout = workspace.join("inbox");
    assert_eq!(document["data"]["path"], workspace.display().to_string());
    assert_eq!(document["data"]["branch"], "reading-list");
    assert_eq!(
        git(&checkout, &["branch", "--show-current"]).trim(),
        "reading-list"
    );
    assert_eq!(
        git(&checkout, &["rev-parse", "HEAD"]),
        git(&setup.clone, &["rev-parse", "origin/main"])
    );
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("mxv remove reading-list"), "{agents}");
}

#[test]
fn list_show_and_cd_report_the_new_workspace() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let (exit, listed) = setup.json(&["list"]);
    assert_eq!(exit, 0);
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["data"][0]["name"], "reading-list");
    let (exit, shown) = setup.json(&["show", "reading-list"]);
    assert_eq!(exit, 0);
    assert_eq!(shown["data"][0]["path"], workspace.display().to_string());
    let (exit, path) = setup.text(&["cd", "reading-list"]);
    assert_eq!(exit, 0);
    assert_eq!(path, format!("{}\n", workspace.display()));
}

#[test]
fn a_dry_run_of_new_creates_nothing() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "inbox", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["dry_run"], true);
    assert!(!setup.workspace("reading-list").exists());
    assert_eq!(setup.json(&["list"]).1["total"], 0);
}

#[test]
fn new_of_a_path_that_does_not_exist_fails_with_repository_not_found() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "missing"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::repository::not_found");
}

#[test]
fn new_of_a_directory_outside_any_repository_fails_with_repository_not_found() {
    let setup = Setup::new();
    fs::create_dir(setup.root().join("plain")).unwrap();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "plain"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::repository::not_found");
}

#[test]
fn new_of_a_repository_whose_origin_has_no_commits_fails_with_empty_origin_and_says_to_push_one() {
    let setup = Setup::new();
    empty_repository(&setup.root());
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "fresh"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::repository::empty_origin");
    let hint = document["recovery"].to_string();
    assert!(hint.contains("push an initial commit"), "{hint}");
    assert!(!setup.workspace("reading-list").exists());
    assert_eq!(setup.json(&["list"]).1["total"], 0);
}

#[test]
fn a_second_workspace_with_the_same_name_fails_with_exists() {
    let setup = Setup::new();
    created(&setup, "reading-list");
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "inbox"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::exists");
}

#[test]
fn a_name_with_a_slash_fails_with_bad_name() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "a/b", "--repo", "inbox"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::bad_name");
}

#[test]
fn show_prints_the_workspaces_it_finds_and_a_failure_for_each_name_it_does_not() {
    let setup = Setup::new();
    created(&setup, "reading-list");
    let (exit, document) = setup.json(&["show", "reading-list", "missing"]);
    assert_eq!(exit, 1);
    assert_eq!(document["data"][0]["name"], "reading-list");
    assert_eq!(document["failures"][0]["code"], "mxv::workspace::not_found");
}

#[test]
fn cd_of_a_name_that_no_workspace_has_fails_with_not_found() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["cd", "missing"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::not_found");
}

#[test]
fn remove_deletes_a_clean_workspace_its_merged_branch_and_its_record() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["checkouts"][0]["branch_fate"], "deleted");
    assert!(!workspace.exists());
    assert!(git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
    assert_eq!(setup.json(&["list"]).1["total"], 0);
}

#[test]
fn remove_without_yes_and_without_a_terminal_asks_for_yes() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let (exit, document) = setup.json(&["remove", "reading-list"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::confirmation::needs_yes");
    assert!(workspace.exists());
}

#[test]
fn remove_refuses_a_workspace_with_an_untracked_file_unless_forced() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    write(&workspace.join("inbox").join("notes.md"));
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::dirty");
    assert_eq!(document["fields"]["changes"][0], "inbox: ?? notes.md");
    assert!(workspace.join("inbox").join("notes.md").exists());
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

#[test]
fn remove_refuses_a_workspace_with_an_uncommitted_change() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("inbox");
    write(&checkout.join("notes.md"));
    git(&checkout, &["add", "notes.md"]);
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::dirty");
}

#[test]
fn remove_keeps_a_branch_with_unmerged_commits_unless_forced() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    git(
        &workspace.join("inbox"),
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["checkouts"][0]["branch_fate"], "kept");
    assert!(!git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

#[test]
fn a_dry_run_of_remove_changes_nothing() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let (exit, text) = setup.text(&["remove", "reading-list", "--dry-run"]);
    assert_eq!(exit, 0);
    assert!(text.contains("would remove the checkout"), "{text}");
    assert!(workspace.join("inbox").exists());
    assert_eq!(setup.json(&["list"]).1["total"], 1);
}

#[test]
fn remove_of_a_name_that_no_workspace_has_fails_with_not_found() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["remove", "missing", "--yes"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::not_found");
}

/// Writes a small file at `path`, as a person working in a checkout would.
fn write(path: &Path) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, "notes").unwrap();
}

/// Creates, in `root`, a bare repository `empty.git` with no commits and a
/// clone of it named `fresh`, as a brand-new repository has. Returns the
/// clone's path.
fn empty_repository(root: &Path) -> PathBuf {
    git(
        root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "empty.git",
        ],
    );
    git(root, &["clone", "--quiet", "empty.git", "fresh"]);
    root.join("fresh")
}
