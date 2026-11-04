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
        Self::run_json(self.command(args))
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

    /// Returns the command `mxv` with `args`, with `MXV_ROOT` set to the
    /// directory `workspaces`.
    fn command(&self, args: &[&str]) -> Command {
        self.without_mxv_root(args)
            .env("MXV_ROOT", self.root().join("workspaces"))
    }

    /// Returns the command `mxv` with `args`, a home directory and base
    /// directories of its own, and no `MXV_ROOT`.
    fn without_mxv_root(&self, args: &[&str]) -> Command {
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
    }

    /// Runs `command` with `--json`, and returns the exit code and the JSON
    /// document.
    fn run_json(command: Command) -> (i32, Value) {
        let output = command.arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Writes `text` as the config file `$XDG_CONFIG_HOME/mxv/config.toml`.
    fn config(&self, text: &str) {
        let dir = self.root().join("config").join("mxv");
        fs::create_dir_all(&dir).unwrap();
        write_text(&dir.join("config.toml"), text);
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
    assert_eq!(
        document["data"]["checkouts"][0]["branch_fate"],
        "kept_unmerged"
    );
    assert_eq!(document["data"]["checkouts"][0]["unmerged_commits"], 1);
    assert!(!git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

#[test]
fn force_discards_changes_but_keeps_an_unmerged_branch() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("inbox");
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    write(&checkout.join("notes.md"));
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["checkouts"][0]["branch_fate"],
        "kept_unmerged"
    );
    assert_eq!(document["data"]["discarded"][0], "inbox: ?? notes.md");
    assert!(!workspace.exists());
    assert!(!git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

#[test]
fn delete_unmerged_deletes_a_branch_with_unmerged_commits_and_counts_them() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    for _ in 0..2 {
        git(
            &workspace.join("inbox"),
            &["commit", "--quiet", "--allow-empty", "-m", "Work"],
        );
    }
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--delete-unmerged"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["checkouts"][0]["branch_fate"],
        "deleted_unmerged"
    );
    assert_eq!(document["data"]["checkouts"][0]["unmerged_commits"], 2);
    assert!(git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

#[test]
fn remove_refuses_an_ignored_env_file_but_deletes_an_ignored_target_directory() {
    let setup = Setup::new();
    write_text(&setup.clone.join(".git/info/exclude"), ".env\ntarget/\n");
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("inbox");
    write(&checkout.join(".env"));
    fs::create_dir_all(checkout.join("target")).unwrap();
    write(&checkout.join("target").join("mxv"));
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::ignored_files");
    assert_eq!(document["fields"]["paths"][0], "inbox: .env");
    assert!(checkout.join(".env").exists());

    fs::remove_file(checkout.join(".env")).unwrap();
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["disposable"][0], "inbox: target/");
    assert!(!workspace.exists());
}

#[test]
fn remove_refuses_commits_on_a_detached_head_unless_unmerged_commits_may_be_deleted() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("inbox");
    git(&checkout, &["switch", "--quiet", "--detach"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--force"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::unreachable_commits");
    assert_eq!(document["fields"]["commits"], 1);
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--delete-unmerged"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["checkouts"][0]["stray_commits"], 1);
}

#[test]
fn without_mxv_root_or_a_config_file_new_puts_the_workspace_in_the_data_directory() {
    let setup = Setup::new();
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "inbox"]));
    assert_eq!(exit, 0, "{document}");
    let root = setup.root().join("data/mxv/workspaces");
    assert_eq!(document["data"]["root"], root.display().to_string());
    assert_eq!(document["data"]["root_source"], "default");
    assert!(root.join("reading-list").join("inbox").exists());
}

#[test]
fn the_config_file_root_names_the_root_and_show_reports_it() {
    let setup = Setup::new();
    setup.config("root = \"~/work\"\n");
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "inbox"]));
    assert_eq!(exit, 0, "{document}");
    let root = setup.root().join("home/work");
    assert_eq!(document["data"]["root"], root.display().to_string());
    assert_eq!(document["data"]["root_source"], "config");
    let (exit, shown) = Setup::run_json(setup.without_mxv_root(&["show", "reading-list"]));
    assert_eq!(exit, 0, "{shown}");
    assert_eq!(shown["data"][0]["root_source"], "config");
    assert_eq!(
        shown["data"][0]["path"],
        root.join("reading-list").display().to_string()
    );
}

#[test]
fn mxv_root_wins_over_the_config_file() {
    let setup = Setup::new();
    setup.config("root = \"~/work\"\n");
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "inbox"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["root_source"], "env");
    assert!(setup.workspace("reading-list").exists());
}

#[test]
fn a_config_file_with_an_unknown_key_fails_with_config_invalid_naming_the_file_and_line() {
    let setup = Setup::new();
    setup.config("root = \"/srv\"\nroots = \"/srv\"\n");
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "inbox"]));
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::config::invalid");
    assert_eq!(document["fields"]["line"], 2);
    assert_eq!(
        document["fields"]["path"],
        setup
            .root()
            .join("config/mxv/config.toml")
            .display()
            .to_string()
    );
}

#[test]
fn a_relative_root_in_the_config_file_fails_with_config_invalid() {
    let setup = Setup::new();
    setup.config("root = \"work\"\n");
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "inbox"]));
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::config::invalid");
    assert_eq!(document["fields"]["line"], 1);
}

#[test]
fn a_relative_mxv_root_fails_with_relative_root() {
    let setup = Setup::new();
    let (exit, document) = Setup::run_json(
        setup
            .without_mxv_root(&["new", "reading-list", "--repo", "inbox"])
            .env("MXV_ROOT", "work"),
    );
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::environment::relative_root");
}

#[test]
fn changing_the_root_later_leaves_existing_workspaces_where_they_are() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    setup.config("root = \"~/elsewhere\"\n");
    let (exit, shown) = Setup::run_json(setup.without_mxv_root(&["show", "reading-list"]));
    assert_eq!(exit, 0, "{shown}");
    assert_eq!(shown["data"][0]["path"], workspace.display().to_string());
    let (exit, listed) = Setup::run_json(setup.without_mxv_root(&["list"]));
    assert_eq!(exit, 0, "{listed}");
    assert_eq!(listed["data"][0]["path"], workspace.display().to_string());
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "triage", "--repo", "inbox"]));
    assert_eq!(exit, 0, "{document}");
    assert!(setup.root().join("home/elsewhere/triage/inbox").exists());
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["remove", "reading-list", "--yes"]));
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

#[test]
fn list_reports_a_corrupt_record_and_still_lists_the_other_workspaces() {
    let setup = Setup::new();
    created(&setup, "reading-list");
    write_text(
        &setup.root().join("state/mxv/workspaces/broken.json"),
        "not json",
    );
    let (exit, listed) = setup.json(&["list"]);
    assert_eq!(exit, 1, "{listed}");
    assert_eq!(listed["total"], 2);
    assert_eq!(listed["data"][0]["name"], "broken.json");
    assert_ne!(listed["data"][0]["problem"], "");
    assert_eq!(listed["data"][1]["name"], "reading-list");
    assert_eq!(listed["data"][1]["problem"], "");
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
    write_text(path, "notes");
}

/// Writes `text` to the file at `path`, as a person editing a file by hand
/// would.
fn write_text(path: &Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write by hand"
    )]
    fs::write(path, text).unwrap();
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
