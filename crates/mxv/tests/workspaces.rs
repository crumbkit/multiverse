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
    /// The clone `app`.
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

    /// Runs `mxv` with `args` and `--json` in `dir`, and returns the exit
    /// code and the JSON document.
    fn json_in(&self, dir: &Path, args: &[&str]) -> (i32, Value) {
        Self::run_json(self.command(args).current_dir(dir))
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
    let (exit, document) = setup.json(&["new", name, "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    setup.workspace(name)
}

/// Makes the workspace `name` as mxv made workspaces before clones: its
/// checkout `app` is a git worktree of the clone `app` on a new branch
/// `name` from `origin/main`, and its record has version 1 and no `kind`.
/// The workspaces `inbox-ingest` and `auth-tenants-users` of a person who
/// used that mxv are such records. Returns the workspace's directory.
fn worktrees(setup: &Setup, name: &str) -> PathBuf {
    let workspace = setup.workspace(name);
    let checkout = workspace.join("app");
    fs::create_dir_all(&workspace).unwrap();
    git(&setup.clone, &["branch", "--no-track", name, "origin/main"]);
    git(
        &setup.clone,
        &[
            "worktree",
            "add",
            "--quiet",
            checkout.to_str().unwrap(),
            name,
        ],
    );
    let commit = git(&checkout, &["rev-parse", "HEAD"]);
    let record = serde_json::json!({
        "version": 1,
        "name": name,
        "path": workspace,
        "checkouts": [{
            "name": "app",
            "backend": "git",
            "source": setup.clone,
            "path": checkout,
            "branch": name,
            "base": "refs/remotes/origin/main",
            "commit": commit.trim(),
        }],
    });
    let records = setup.root().join("state").join("mxv").join("workspaces");
    fs::create_dir_all(&records).unwrap();
    write_text(&records.join(format!("{name}.json")), &record.to_string());
    workspace
}

#[test]
fn new_checks_out_the_repository_inside_the_workspace_directory_on_a_branch_of_the_workspaces_name()
{
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["schema"], "mxv/new/v1");
    let workspace = setup.workspace("reading-list");
    let checkout = workspace.join("app");
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
fn mxv_new_writes_an_empty_deadband_workspace_marker_in_the_workspace_directory() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let metadata = fs::symlink_metadata(workspace.join(".deadband-workspace")).unwrap();
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), 0);
    assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
}

#[test]
fn new_makes_and_reuses_a_mirror_when_git_refuses_to_discover_bare_repositories() {
    let setup = Setup::new();
    let config = setup.root().join("explicit.gitconfig");
    write_text(&config, "[safe]\n\tbareRepository = explicit\n");
    for name in ["reading-list", "triage"] {
        let command = setup
            .command(&["new", name, "--repo", "app"])
            .env("GIT_CONFIG_GLOBAL", &config);
        let (exit, document) = Setup::run_json(command);
        assert_eq!(exit, 0, "{document}");
        let checkout = setup.workspace(name).join("app");
        assert_eq!(git(&checkout, &["branch", "--show-current"]).trim(), name);
    }
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
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app", "--dry-run"]);
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
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::exists");
}

#[test]
fn a_name_with_a_slash_fails_with_bad_name() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "a/b", "--repo", "app"]);
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
    write(&workspace.join("app").join("notes.md"));
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::dirty");
    assert_eq!(document["fields"]["changes"][0], "app: ?? notes.md");
    assert!(workspace.join("app").join("notes.md").exists());
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--force"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

#[test]
fn remove_refuses_a_workspace_with_an_uncommitted_change() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("app");
    write(&checkout.join("notes.md"));
    git(&checkout, &["add", "notes.md"]);
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1);
    assert_eq!(document["code"], "mxv::workspace::dirty");
}

#[test]
fn remove_keeps_a_branch_with_unmerged_commits_unless_forced() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    git(
        &workspace.join("app"),
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
    let workspace = worktrees(&setup, "reading-list");
    let checkout = workspace.join("app");
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
    assert_eq!(document["data"]["discarded"][0], "app: ?? notes.md");
    assert!(!workspace.exists());
    assert!(!git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

#[test]
fn delete_unmerged_deletes_a_branch_with_unmerged_commits_and_counts_them() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    for _ in 0..2 {
        git(
            &workspace.join("app"),
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
    let checkout = workspace.join("app");
    write(&checkout.join(".env"));
    fs::create_dir_all(checkout.join("target")).unwrap();
    write(&checkout.join("target").join("mxv"));
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::ignored_files");
    assert_eq!(document["fields"]["paths"][0], "app: .env");
    assert!(checkout.join(".env").exists());

    fs::remove_file(checkout.join(".env")).unwrap();
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["disposable"][0], "app: target/");
    assert!(!workspace.exists());
}

#[test]
fn remove_refuses_commits_on_a_detached_head_unless_unmerged_commits_may_be_deleted() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    let checkout = workspace.join("app");
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
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "app"]));
    assert_eq!(exit, 0, "{document}");
    let root = setup.root().join("data/mxv/workspaces");
    assert_eq!(document["data"]["root"], root.display().to_string());
    assert_eq!(document["data"]["root_source"], "default");
    assert!(root.join("reading-list").join("app").exists());
}

#[test]
fn the_config_file_root_names_the_root_and_show_reports_it() {
    let setup = Setup::new();
    setup.config("root = \"~/work\"\n");
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "app"]));
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
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["root_source"], "env");
    assert!(setup.workspace("reading-list").exists());
}

#[test]
fn a_config_file_with_an_unknown_key_fails_with_config_invalid_naming_the_file_and_line() {
    let setup = Setup::new();
    setup.config("root = \"/srv\"\nroots = \"/srv\"\n");
    let (exit, document) =
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "app"]));
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
        Setup::run_json(setup.without_mxv_root(&["new", "reading-list", "--repo", "app"]));
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::config::invalid");
    assert_eq!(document["fields"]["line"], 1);
}

#[test]
fn a_relative_mxv_root_fails_with_relative_root() {
    let setup = Setup::new();
    let (exit, document) = Setup::run_json(
        setup
            .without_mxv_root(&["new", "reading-list", "--repo", "app"])
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
        Setup::run_json(setup.without_mxv_root(&["new", "triage", "--repo", "app"]));
    assert_eq!(exit, 0, "{document}");
    assert!(setup.root().join("home/elsewhere/triage/app").exists());
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
    assert_eq!(listed["data"][0]["name"], "broken");
    assert_ne!(listed["data"][0]["problem"], "");
    let record = setup.root().join("state/mxv/workspaces/broken.json");
    assert_eq!(
        listed["data"][0]["repair"],
        format!(
            "move {} aside by hand; mxv never deletes a record it cannot read",
            record.display()
        )
    );
    assert_eq!(listed["data"][1]["name"], "reading-list");
    assert_eq!(listed["data"][1]["problem"], "");
    assert_eq!(listed["data"][1]["repair"], "");
}

#[test]
fn a_workspace_whose_checkouts_are_all_gone_names_remove_which_removes_it() {
    let setup = Setup::new();
    created(&setup, "reading-list");
    let workspace = created(&setup, "triage");
    fs::remove_dir_all(&workspace).unwrap();
    let (exit, listed) = setup.json(&["list"]);
    assert_eq!(exit, 1, "{listed}");
    let triage = &listed["data"][0];
    assert_eq!(triage["name"], "triage", "{listed}");
    assert_eq!(triage["problem"], "every checkout directory is gone");
    assert_eq!(triage["repair"], "mxv remove triage");
    assert_eq!(listed["data"][1]["name"], "reading-list", "{listed}");
    let (exit, document) = setup.json(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, listed) = setup.json(&["list"]);
    assert_eq!(exit, 0, "{listed}");
    assert_eq!(listed["total"], 1, "{listed}");
}

#[test]
fn a_dry_run_of_remove_changes_nothing() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let (exit, text) = setup.text(&["remove", "reading-list", "--dry-run"]);
    assert_eq!(exit, 0);
    assert!(text.contains("would remove the checkout"), "{text}");
    assert!(workspace.join("app").exists());
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

#[test]
fn remove_refuses_a_checkout_locked_for_another_reason_and_names_git_worktree_unlock() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    let checkout = workspace.join("app");
    git(
        &setup.clone,
        &[
            "worktree",
            "lock",
            "--reason",
            "in use",
            checkout.to_str().unwrap(),
        ],
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--force"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::locked");
    assert_eq!(document["fields"]["reason"], "in use");
    let hint =
        document["hint"].as_str().unwrap_or_default().to_owned() + document.to_string().as_str();
    assert!(hint.contains("worktree unlock"), "{document}");
    assert!(checkout.exists());
}

#[test]
fn remove_keeps_a_branch_whose_base_is_gone_and_reports_an_unknown_count() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    git(
        &setup.clone,
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["checkouts"][0]["branch_fate"],
        "kept_unmerged"
    );
    assert!(document["data"]["checkouts"][0]["unmerged_commits"].is_null());
    assert!(!workspace.exists());
    assert!(!git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

/// Returns the words after `mxv` of `line`, a command line that mxv
/// printed, such as `mxv remove notes --force --yes`, and fails the test
/// when the line does not start with `mxv`.
fn words(line: &str) -> Vec<&str> {
    let mut words = line.split_whitespace();
    assert_eq!(words.next(), Some("mxv"), "{line}");
    words.collect()
}

/// Returns the text between the first pair of backticks in `text` that
/// starts with `start`.
fn quoted<'a>(text: &'a str, start: &str) -> &'a str {
    text.split('`')
        .skip(1)
        .step_by(2)
        .find(|quoted| quoted.starts_with(start))
        .unwrap_or_else(|| panic!("no `{start}...` in {text}"))
}

/// Runs the first alternative of the failure `document`, as written and
/// without a terminal, and fails the test unless it exits with code 0.
fn run_alternative(setup: &Setup, document: &Value) {
    let call = document["alternatives"][0]["call"]
        .as_str()
        .unwrap_or_else(|| panic!("no alternative call in {document}"));
    let (exit, result) = setup.json(&words(call));
    assert_eq!(exit, 0, "{call}: {result}");
}

#[test]
fn the_steps_that_a_dirty_workspace_suggests_run_as_written_without_a_terminal() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    write(&workspace.join("app").join("notes.md"));
    let (exit, document) = setup.json(&["remove", "reading-list"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::dirty");
    let hint = document["recovery"]["text"].as_str().unwrap();
    assert_eq!(quoted(hint, "mxv"), "mxv remove reading-list --yes");
    assert_eq!(
        document["alternatives"][0]["call"],
        "mxv remove reading-list --force --yes"
    );
    run_alternative(&setup, &document);
    assert!(!workspace.exists());
}

#[test]
fn the_steps_that_an_ignored_file_suggests_run_as_written_without_a_terminal() {
    let setup = Setup::new();
    write_text(&setup.clone.join(".git/info/exclude"), ".env\n");
    let workspace = created(&setup, "reading-list");
    write(&workspace.join("app").join(".env"));
    let (exit, document) = setup.json(&["remove", "reading-list"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::ignored_files");
    let hint = document["recovery"]["text"].as_str().unwrap();
    assert_eq!(quoted(hint, "mxv"), "mxv remove reading-list --yes");
    assert_eq!(
        document["alternatives"][0]["call"],
        "mxv remove reading-list --force --yes"
    );
    run_alternative(&setup, &document);
    assert!(!workspace.exists());
}

#[test]
fn the_steps_that_unreachable_commits_suggest_keep_the_flags_given_and_run_as_written() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    let checkout = workspace.join("app");
    git(&checkout, &["switch", "--quiet", "--detach"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    write(&checkout.join("notes.md"));
    let (exit, document) = setup.json(&["remove", "reading-list", "--force"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::unreachable_commits");
    let hint = document["recovery"]["text"].as_str().unwrap();
    assert_eq!(quoted(hint, "mxv"), "mxv remove reading-list --force --yes");
    assert_eq!(
        document["alternatives"][0]["call"],
        "mxv remove reading-list --force --delete-unmerged --yes"
    );
    run_alternative(&setup, &document);
    assert!(!workspace.exists());
}

#[test]
fn the_step_that_a_locked_checkout_suggests_after_unlocking_has_yes() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    let checkout = workspace.join("app");
    git(
        &setup.clone,
        &[
            "worktree",
            "lock",
            "--reason",
            "in use",
            checkout.to_str().unwrap(),
        ],
    );
    let (exit, document) = setup.json(&["remove", "reading-list"]);
    assert_eq!(exit, 1, "{document}");
    let hint = document["recovery"]["text"].as_str().unwrap();
    git(
        &setup.clone,
        &["worktree", "unlock", checkout.to_str().unwrap()],
    );
    let line = quoted(hint, "mxv");
    assert_eq!(line, "mxv remove reading-list --yes");
    let (exit, result) = setup.json(&words(line));
    assert_eq!(exit, 0, "{result}");
}

#[test]
fn a_dry_run_names_the_removal_that_deletes_an_unmerged_branch_and_it_runs_as_written() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    git(
        &workspace.join("app"),
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    let (exit, text) = setup.text(&["remove", "reading-list", "--dry-run"]);
    assert_eq!(exit, 0, "{text}");
    let line = quoted(&text, "mxv");
    assert_eq!(line, "mxv remove reading-list --delete-unmerged --yes");
    let (exit, result) = setup.json(&words(line));
    assert_eq!(exit, 0, "{result}");
    assert_eq!(
        result["data"]["checkouts"][0]["branch_fate"],
        "deleted_unmerged"
    );
    assert!(git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

#[test]
fn a_removal_that_keeps_an_unmerged_branch_names_the_git_command_that_deletes_it() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "reading-list");
    git(
        &workspace.join("app"),
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    let (exit, text) = setup.text(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{text}");
    let line = quoted(&text, "git");
    assert_eq!(
        line,
        format!("git -C {} branch -D reading-list", setup.clone.display())
    );
    let words: Vec<&str> = line.split_whitespace().skip(1).collect();
    git(&setup.root(), &words);
    assert!(git(&setup.clone, &["branch", "--list", "reading-list"]).is_empty());
}

/// Returns every reference and worktree of the clone `app`, which a
/// workspace of clones never changes.
fn repository_state(setup: &Setup) -> String {
    let refs = git(&setup.clone, &["for-each-ref"]);
    let worktrees = git(&setup.clone, &["worktree", "list", "--porcelain"]);
    format!("{refs}\n{worktrees}")
}

#[test]
fn new_makes_a_clone_with_its_own_git_directory_whose_origin_is_the_repositorys_origin() {
    let setup = Setup::new();
    let before = repository_state(&setup);
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("app");
    assert!(checkout.join(".git").is_dir());
    let origin = git(&checkout, &["remote", "get-url", "origin"]);
    assert_eq!(
        Path::new(origin.trim()),
        setup.root().join("origin.git").as_path()
    );
    assert_eq!(repository_state(&setup), before);
    let (exit, shown) = setup.json(&["show", "reading-list", "--full"]);
    assert_eq!(exit, 0, "{shown}");
    assert_eq!(shown["data"][0]["kind"], "clone", "{shown}");
    let mirror = shown["data"][0]["checkouts"][0]["mirror"].as_str().unwrap();
    let mirrors = setup.root().join("data").join("mxv").join("mirrors");
    assert!(Path::new(mirror).starts_with(&mirrors), "{shown}");
    assert!(Path::new(mirror).join("HEAD").is_file());
}

#[test]
fn removing_a_clone_keeps_its_mirror_and_leaves_the_repository_as_it_was() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let before = repository_state(&setup);
    let mirrors = setup.root().join("data").join("mxv").join("mirrors");
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["checkouts"][0]["branch_fate"], "deleted");
    assert!(!workspace.exists());
    assert!(fs::read_dir(&mirrors).unwrap().next().is_some());
    assert_eq!(repository_state(&setup), before);
}

#[test]
fn remove_refuses_a_clone_with_unpushed_commits_and_its_alternative_deletes_them() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    git(
        &workspace.join("app"),
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::unpushed_commits");
    assert_eq!(document["fields"]["commits"], 1);
    assert!(workspace.exists());
    assert_eq!(
        document["alternatives"][0]["call"],
        "mxv remove reading-list --delete-unmerged --yes"
    );
    run_alternative(&setup, &document);
    assert!(!workspace.exists());
}

#[test]
fn remove_deletes_a_clone_whose_commits_are_pushed() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("app");
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    git(&checkout, &["push", "--quiet", "origin", "reading-list"]);
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

#[test]
fn remove_refuses_a_clone_with_a_stash_and_its_alternative_discards_it() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    let checkout = workspace.join("app");
    write(&checkout.join("notes.md"));
    git(
        &checkout,
        &["stash", "push", "--quiet", "--include-untracked"],
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::stashes");
    assert_eq!(document["fields"]["stashes"], 1);
    assert_eq!(
        document["alternatives"][0]["call"],
        "mxv remove reading-list --force --yes"
    );
    let (exit, text) = setup.text(&["remove", "reading-list", "--force", "--dry-run"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains("mxv would delete 1 stash entry from the checkout"),
        "{text}"
    );
    run_alternative(&setup, &document);
    assert!(!workspace.exists());
}

#[test]
fn new_fails_with_mirror_fetch_failed_when_the_origin_is_gone_and_leaves_nothing() {
    let setup = Setup::new();
    fs::remove_dir_all(setup.root().join("origin.git")).unwrap();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::mirror::fetch_failed");
    assert!(
        document["fields"]["origin"]
            .as_str()
            .unwrap()
            .ends_with("origin.git")
    );
    assert!(!setup.workspace("reading-list").exists());
    let (_, listed) = setup.json(&["list"]);
    assert_eq!(listed["total"], 0, "{listed}");
}

#[test]
fn remove_fails_as_origin_unreachable_offline_and_delete_unmerged_skips_the_fetch() {
    let setup = Setup::new();
    let workspace = created(&setup, "reading-list");
    fs::remove_dir_all(setup.root().join("origin.git")).unwrap();
    let (exit, document) = setup.json(&["remove", "reading-list", "--dry-run"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::origin_unreachable");
    assert!(
        document["recovery"]["text"]
            .as_str()
            .unwrap()
            .contains("--delete-unmerged"),
        "{document}"
    );
    assert!(workspace.exists());
    let (exit, document) = setup.json(&["remove", "reading-list", "--delete-unmerged", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

#[test]
fn a_failed_fetch_never_prints_the_credentials_of_the_origin_url() {
    let setup = Setup::new();
    let token = "s3cr3t-token";
    let url = format!("http://someone:{token}@127.0.0.1:9/app.git");
    git(
        &setup.root().join("app"),
        &["remote", "set-url", "origin", &url],
    );
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::mirror::fetch_failed");
    assert_eq!(document["fields"]["origin"], "http://127.0.0.1:9/app.git");
    assert!(!document.to_string().contains(token), "{document}");
    let output = setup
        .command(&["new", "reading-list", "--repo", "app"])
        .output()
        .unwrap();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains("127.0.0.1:9"), "{printed}");
    assert!(!printed.contains(token), "{printed}");
}

#[test]
fn list_and_show_read_a_workspace_of_worktrees_beside_a_clone() {
    let setup = Setup::new();
    worktrees(&setup, "inbox-ingest");
    created(&setup, "reading-list");
    let (exit, listed) = setup.json(&["list", "--long"]);
    assert_eq!(exit, 0, "{listed}");
    let kinds: Vec<&Value> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["kind"])
        .collect();
    assert_eq!(kinds, ["worktree", "clone"], "{listed}");
    let (exit, shown) = setup.json(&["show", "inbox-ingest", "--full"]);
    assert_eq!(exit, 0, "{shown}");
    assert_eq!(shown["data"][0]["kind"], "worktree", "{shown}");
    assert!(shown["data"][0]["checkouts"][0]["mirror"].is_null());
}

#[test]
fn remove_of_a_workspace_of_worktrees_removes_the_worktree_and_its_merged_branch() {
    let setup = Setup::new();
    let workspace = worktrees(&setup, "auth-tenants-users");
    let (exit, document) = setup.json(&["remove", "auth-tenants-users", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["checkouts"][0]["branch_fate"], "deleted");
    assert!(!workspace.exists());
    assert!(git(&setup.clone, &["branch", "--list", "auth-tenants-users"]).is_empty());
    assert!(!git(&setup.clone, &["worktree", "list"]).contains("auth-tenants-users"));
}

/// Writes `text` as the template `name` in `$XDG_CONFIG_HOME/mxv/templates`.
fn template(setup: &Setup, name: &str, text: &str) -> PathBuf {
    let dir = setup.root().join("config").join("mxv").join("templates");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    write_text(&path, text);
    path
}

/// Returns a template for the fixture's origin, with `body` before its
/// `[template]` table and a table of another tool after it.
fn app_template(setup: &Setup, body: &str) -> String {
    format!(
        "{body}\n[template]\norigins = [\"{}\"]\n\n[app]\nname = \"app\"\n",
        setup.root().join("origin.git").display()
    )
}

#[test]
fn new_copies_the_manifest_file_it_is_given_above_the_checkout() {
    let setup = Setup::new();
    let file = setup.root().join("reading-list.toml");
    write_text(&file, "ports = [\"web\"]\n\n[app]\nname = \"app\"\n");
    let (exit, document) = setup.json(&[
        "new",
        "reading-list",
        "--repo",
        "app",
        "--manifest",
        "reading-list.toml",
        "--long",
    ]);
    assert_eq!(exit, 0, "{document}");
    let workspace = setup.workspace("reading-list");
    assert_eq!(document["data"]["manifest"], "workspace", "{document}");
    assert_eq!(
        document["data"]["manifest_path"],
        workspace.join("mxv.toml").display().to_string()
    );
    assert_eq!(
        document["data"]["manifest_from"],
        file.display().to_string()
    );
    assert_eq!(
        fs::read_to_string(workspace.join("mxv.toml")).unwrap(),
        "ports = [\"web\"]\n\n[app]\nname = \"app\"\n"
    );
}

#[test]
fn new_copies_the_template_of_the_origin_without_its_template_table() {
    let setup = Setup::new();
    let path = template(
        &setup,
        "app.toml",
        &app_template(&setup, "ports = [\"web\"]\n"),
    );
    template(
        &setup,
        "other.toml",
        "[template]\norigins = [\"github.com/acme/other\"]\n",
    );
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["manifest"], "workspace", "{document}");
    assert_eq!(
        document["data"]["manifest_from"],
        path.display().to_string()
    );
    let copied = fs::read_to_string(setup.workspace("reading-list").join("mxv.toml")).unwrap();
    assert!(!copied.contains("[template]"), "{copied}");
    assert!(copied.contains("ports = [\"web\"]"), "{copied}");
    assert!(copied.contains("[app]\nname = \"app\"\n"), "{copied}");
    let (exit, shown) = setup.json(&["show", "reading-list", "--long"]);
    assert_eq!(exit, 0, "{shown}");
    assert_eq!(shown["data"][0]["manifest"], "workspace", "{shown}");
    assert_eq!(shown["data"][0]["ports"], serde_json::json!({}), "{shown}");
}

#[test]
fn a_dry_run_names_the_template_it_would_copy_and_writes_nothing() {
    let setup = Setup::new();
    let path = template(&setup, "app.toml", &app_template(&setup, ""));
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["manifest"], "workspace", "{document}");
    assert_eq!(
        document["data"]["manifest_from"],
        path.display().to_string()
    );
    assert!(!setup.workspace("reading-list").exists());
}

#[test]
fn list_and_show_name_the_manifest_that_each_workspace_reads() {
    let setup = Setup::new();
    created(&setup, "bare");
    write_text(&setup.clone.join("mxv.toml"), "ports = [\"api\"]\n");
    git(&setup.clone, &["add", "mxv.toml"]);
    git(&setup.clone, &["commit", "--quiet", "-m", "Add a manifest"]);
    git(&setup.clone, &["push", "--quiet", "origin", "HEAD"]);
    created(&setup, "repository");
    let file = setup.root().join("own.toml");
    write_text(&file, "ports = [\"web\"]\n");
    let (exit, document) = setup.json(&["new", "own", "--repo", "app", "--manifest", "own.toml"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, listed) = setup.json(&["list", "--long"]);
    assert_eq!(exit, 0, "{listed}");
    let rows: Vec<(String, String, Value)> = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap().to_owned(),
                row["manifest"].as_str().unwrap().to_owned(),
                row["manifest_path"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("bare".to_owned(), "none".to_owned(), Value::Null),
            (
                "own".to_owned(),
                "workspace".to_owned(),
                Value::from(
                    setup
                        .workspace("own")
                        .join("mxv.toml")
                        .display()
                        .to_string()
                )
            ),
            (
                "repository".to_owned(),
                "repository".to_owned(),
                Value::from(
                    setup
                        .workspace("repository")
                        .join("app/mxv.toml")
                        .display()
                        .to_string()
                )
            ),
        ],
        "{listed}"
    );
    let (exit, text) = setup.text(&["status", "-w", "own"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains("The manifest is the workspace manifest"),
        "{text}"
    );
    let (exit, status) = setup.json(&["status", "-w", "repository"]);
    assert_eq!(exit, 0, "{status}");
    assert!(
        status.to_string().contains("\"manifest\":\"repository\""),
        "{status}"
    );
}

#[test]
fn new_fails_when_two_templates_list_the_origin_and_creates_nothing() {
    let setup = Setup::new();
    let first = template(&setup, "a.toml", &app_template(&setup, ""));
    let second = template(&setup, "b.toml", &app_template(&setup, ""));
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::template::ambiguous", "{document}");
    assert_eq!(
        document["fields"]["templates"],
        serde_json::json!([first.display().to_string(), second.display().to_string()])
    );
    assert!(!setup.workspace("reading-list").exists());
}

#[test]
fn new_fails_on_a_malformed_template_of_the_origin_and_names_it() {
    let setup = Setup::new();
    let text = format!(
        "[template]\norigins = [\"{}\"]\nports = [\"web\"]\n",
        setup.root().join("origin.git").display()
    );
    let path = template(&setup, "broken.toml", &text);
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::template::invalid", "{document}");
    assert_eq!(document["fields"]["path"], path.display().to_string());
    assert!(!setup.workspace("reading-list").exists());
}

#[test]
fn new_skips_a_malformed_template_of_no_known_origin_and_warns_about_it() {
    let setup = Setup::new();
    let path = template(&setup, "broken.toml", "ports = [\"web\"]\n");
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["manifest"], "none", "{document}");
    let warnings = document["data"]["warnings"].to_string();
    assert!(
        warnings.contains(&format!("mxv skipped the template {}", path.display())),
        "{document}"
    );
}

#[test]
fn new_never_copies_a_template_that_is_a_symbolic_link() {
    let setup = Setup::new();
    let real = setup.root().join("real.toml");
    write_text(&real, &app_template(&setup, "disposable = [\"data/\"]\n"));
    let dir = setup.root().join("config").join("mxv").join("templates");
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("app.toml")).unwrap();
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::template::invalid", "{document}");
    assert!(document.to_string().contains("symbolic link"), "{document}");
    assert!(!setup.workspace("reading-list").exists());
}

#[test]
fn new_refuses_a_manifest_file_from_a_repository_or_from_another_workspace() {
    let setup = Setup::new();
    write_text(&setup.clone.join("mxv.toml"), "disposable = [\"data/\"]\n");
    let (exit, document) = setup.json_in(
        &setup.clone,
        &["new", "m1", "--repo", ".", "--manifest", "mxv.toml"],
    );
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::untrusted", "{document}");
    assert!(
        document["fields"]["reason"]
            .as_str()
            .unwrap()
            .contains("inside the git work tree"),
        "{document}"
    );
    assert!(!setup.workspace("m1").exists());
    created(&setup, "other");
    let planted = setup.workspace("other").join("app/planted.toml");
    write_text(&planted, "disposable = [\"data/\"]\n");
    let (exit, document) = setup.json(&[
        "new",
        "m2",
        "--repo",
        "app",
        "--manifest",
        planted.to_str().unwrap(),
    ]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::untrusted", "{document}");
    assert!(!setup.workspace("m2").exists());
}

#[test]
fn a_workspace_manifest_that_links_to_the_repositorys_never_deletes_without_a_confirmation() {
    let setup = Setup::new();
    write_text(&setup.clone.join(".gitignore"), "data/\n");
    write_text(&setup.clone.join("mxv.toml"), "disposable = [\"data/\"]\n");
    git(&setup.clone, &["add", ".gitignore", "mxv.toml"]);
    git(
        &setup.clone,
        &["commit", "--quiet", "-m", "Declare data/ disposable"],
    );
    git(&setup.clone, &["push", "--quiet", "origin", "HEAD"]);
    created(&setup, "s");
    let workspace = setup.workspace("s");
    fs::create_dir_all(workspace.join("app/data")).unwrap();
    write_text(&workspace.join("app/data/file"), "");
    std::os::unix::fs::symlink("app/mxv.toml", workspace.join("mxv.toml")).unwrap();
    let (exit, shown) = setup.json(&["show", "s", "--long"]);
    assert_eq!(exit, 0, "{shown}");
    let paths = &shown["data"][0]["disposable_paths"];
    assert_eq!(paths[0]["path"], "data/", "{shown}");
    assert_eq!(paths[0]["source"], "manifest", "{shown}");
    let (exit, document) = setup.json(&["remove", "s", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::workspace::ignored_files",
        "{document}"
    );
    assert!(workspace.join("app/data/file").exists());
}

#[test]
fn new_fails_on_a_manifest_file_that_is_invalid_or_missing() {
    let setup = Setup::new();
    write_text(
        &setup.root().join("bad.toml"),
        "[processes.web]\nargv = \"npm\"\n",
    );
    let (exit, document) = setup.json(&[
        "new",
        "reading-list",
        "--repo",
        "app",
        "--manifest",
        "bad.toml",
    ]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::invalid", "{document}");
    let (exit, document) = setup.json(&[
        "new",
        "reading-list",
        "--repo",
        "app",
        "--manifest",
        "missing.toml",
    ]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::unreadable", "{document}");
    assert!(!setup.workspace("reading-list").exists());
}

#[test]
fn remove_deletes_the_workspace_manifest_with_the_workspace() {
    let setup = Setup::new();
    write_text(&setup.root().join("own.toml"), "ports = [\"web\"]\n");
    let (exit, document) = setup.json(&["new", "own", "--repo", "app", "--manifest", "own.toml"]);
    assert_eq!(exit, 0, "{document}");
    let manifest = setup.workspace("own").join("mxv.toml");
    let (exit, text) = setup.text(&["remove", "own", "--dry-run"]);
    assert_eq!(exit, 0, "{text}");
    assert!(
        text.contains(&format!(
            "mxv would delete the workspace manifest {}.",
            manifest.display()
        )),
        "{text}"
    );
    let (exit, document) = setup.json(&["remove", "own", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["manifest_deleted"],
        manifest.display().to_string(),
        "{document}"
    );
    assert!(!setup.workspace("own").exists());
}
