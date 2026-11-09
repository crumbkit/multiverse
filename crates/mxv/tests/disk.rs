//! End-to-end tests of the disk features: the sizes of `list` and `show`,
//! the disposable globs of the config file and the manifest, `clean`, and
//! the clones of ignored directories that `new` makes. Each test builds a
//! bare `origin` and a clone `app` of it in a temporary directory, and runs
//! the built `mxv` binary with its own state directory and workspace root
//! there.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git, repository};

/// The words that `new` uses in its warning when the file system cannot
/// clone.
const UNSUPPORTED: &str = "cannot clone files";

/// A temporary directory with the clone `app`, which ignores `.env`,
/// `target/`, `node_modules/` and `test-results/`, a workspace root and the
/// XDG directories.
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
        write_text(
            &clone.join(".git/info/exclude"),
            ".env\ntarget/\nnode_modules/\ntest-results/\n",
        );
        Self { dir, clone }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Returns the command `mxv` with `args`, a home directory, base
    /// directories and a workspace root of its own.
    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
    }

    /// Runs `mxv` with `args`, `--json` and `--full`, which prints every
    /// field of each row, and returns the exit code and the JSON document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self
            .command(args)
            .args(["--json", "--full"])
            .output()
            .unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Runs `mxv` with `args`, and returns the exit code.
    fn exit(&self, args: &[&str]) -> i32 {
        self.command(args).output().unwrap().status.code().unwrap()
    }

    /// Writes `text` as the config file `$XDG_CONFIG_HOME/mxv/config.toml`.
    fn config(&self, text: &str) {
        let dir = self.root().join("config").join("mxv");
        fs::create_dir_all(&dir).unwrap();
        write_text(&dir.join("config.toml"), text);
    }

    /// Creates the workspace `name`, and returns its checkout.
    fn created(&self, name: &str) -> PathBuf {
        let (exit, document) = self.json(&["new", name, "--repo", "app"]);
        assert_eq!(exit, 0, "{document}");
        self.checkout(name)
    }

    fn checkout(&self, name: &str) -> PathBuf {
        self.root().join("workspaces").join(name).join("app")
    }
}

/// Writes `text` to the file at `path`, as a person or a build would.
fn write_text(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person or a build would write"
    )]
    fs::write(path, text).unwrap();
}

/// Writes a file of `bytes` bytes, as a build would.
fn write_bytes(path: &Path, bytes: usize) {
    write_text(path, &"x".repeat(bytes));
}

/// Returns the bytes in the JSON value `value`, which a size prints as.
fn bytes(value: &Value) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("{value} is not a size"))
}

#[test]
fn list_measures_sizes_only_with_sizes_and_show_always_measures_them() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 200 * 1024);
    let (exit, listed) = setup.json(&["list"]);
    assert_eq!(exit, 0, "{listed}");
    assert_eq!(listed["data"][0]["size"], Value::Null);
    assert_eq!(listed["data"][0]["disposable"], Value::Null);
    let (exit, listed) = setup.json(&["list", "--sizes"]);
    assert_eq!(exit, 0, "{listed}");
    let disposable = bytes(&listed["data"][0]["disposable"]);
    assert!(disposable >= 200 * 1024, "{listed}");
    assert!(bytes(&listed["data"][0]["size"]) >= disposable, "{listed}");
    let (exit, shown) = setup.json(&["show", "reading-list"]);
    assert_eq!(exit, 0, "{shown}");
    assert_eq!(bytes(&shown["data"][0]["disposable"]), disposable);
    let path = &shown["data"][0]["disposable_paths"][0];
    assert_eq!(path["checkout"], "app", "{shown}");
    assert_eq!(path["path"], "target/", "{shown}");
    assert_eq!(bytes(&path["bytes"]), disposable, "{shown}");
}

#[test]
fn remove_deletes_a_directory_that_the_config_file_declares_disposable() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(&checkout.join("web/test-results/report.html"), "<p>ok</p>");
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::ignored_files");
    setup.config("disposable = [\"web/test-results/\"]\n");
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["disposable"][0], "app: web/test-results/");
    assert!(!checkout.exists());
}

#[test]
fn remove_with_force_deletes_a_directory_that_the_checkouts_manifest_declares_disposable() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(
        &checkout.join("mxv.toml"),
        "disposable = [\"web/test-results/\"]\n",
    );
    git(&checkout, &["add", "mxv.toml"]);
    git(
        &checkout,
        &["commit", "--quiet", "-m", "Declare test results"],
    );
    write_text(&checkout.join("web/test-results/report.html"), "<p>ok</p>");
    let (exit, document) = setup.json(&[
        "remove",
        "reading-list",
        "--force",
        "--yes",
        "--delete-unmerged",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["disposable"][0], "app: web/test-results/");
    assert!(!checkout.exists());
}

#[test]
fn an_absolute_disposable_glob_in_the_config_file_fails_with_config_invalid_and_its_line() {
    let setup = Setup::new();
    setup.created("reading-list");
    setup.config("root = \"/srv\"\ndisposable = [\"/var/cache/\"]\n");
    let (exit, document) = setup.json(&["show", "reading-list"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::config::invalid");
    assert_eq!(document["fields"]["line"], 2);
}

#[test]
fn clean_deletes_the_disposable_directories_of_an_idle_workspace_and_keeps_everything_else() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 100 * 1024);
    write_bytes(&checkout.join("web/node_modules/left-pad/index.js"), 4096);
    write_text(&checkout.join(".env"), "TOKEN=x");
    write_text(&checkout.join("notes.md"), "notes");
    let (exit, document) = setup.json(&["clean", "reading-list", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let row = &document["data"][0];
    assert_eq!(row["state"], "cleaned", "{document}");
    assert!(bytes(&row["freed"]) >= 100 * 1024, "{document}");
    assert_eq!(row["paths"][0], "app: target/");
    assert_eq!(row["paths"][1], "app: web/node_modules/");
    assert!(!checkout.join("target").exists());
    assert!(!checkout.join("web/node_modules").exists());
    assert!(checkout.join(".env").exists());
    assert!(checkout.join("notes.md").exists());
    assert!(checkout.join(".git").exists());
    assert_eq!(git(&checkout, &["status", "--porcelain"]), "?? notes.md\n");
}

#[test]
fn clean_keeps_a_workspace_that_changed_within_the_idle_age() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (exit, document) = setup.json(&["clean", "--all", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"][0]["state"], "kept");
    let reason = document["data"][0]["reason"].as_str().unwrap();
    assert!(reason.contains("within the idle age of 1w"), "{reason}");
    assert!(checkout.join("target").exists());
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"][0]["state"], "cleaned");
    assert!(!checkout.join("target").exists());
}

#[test]
fn the_config_file_sets_the_idle_age_and_unmodified_overrides_it() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    setup.config("clean_idle = \"0s\"\n");
    let (_, document) = setup.json(&["clean", "--all", "--unmodified", "4d", "--dry-run"]);
    assert_eq!(document["data"][0]["state"], "kept", "{document}");
    let (_, document) = setup.json(&["clean", "--all", "--dry-run"]);
    assert_eq!(document["data"][0]["state"], "would_clean", "{document}");
}

#[test]
fn a_dry_run_of_clean_deletes_nothing_and_reports_what_it_would_free() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 64 * 1024);
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["dry_run"], true);
    assert_eq!(document["data"][0]["state"], "would_clean");
    assert!(bytes(&document["data"][0]["freed"]) >= 64 * 1024);
    assert!(checkout.join("target/debug/app").exists());
}

#[test]
fn clean_without_yes_and_without_a_terminal_asks_for_yes() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::confirmation::needs_yes");
    assert!(checkout.join("target").exists());
}

#[test]
fn clean_of_a_name_that_no_workspace_has_fails_for_that_name_and_cleans_the_others() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (exit, document) = setup.json(&[
        "clean",
        "missing",
        "reading-list",
        "--unmodified",
        "0s",
        "--yes",
    ]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["data"][0]["name"], "reading-list", "{document}");
    assert_eq!(document["data"][0]["state"], "cleaned", "{document}");
    let failure = &document["failures"][0];
    assert_eq!(failure["code"], "mxv::workspace::not_found", "{document}");
    assert_eq!(failure["key"], "missing", "{document}");
    assert!(!checkout.join("target").exists());
}

/// Breaks the checkout of the workspace `name`, so git no longer knows it.
fn break_checkout(setup: &Setup, name: &str) {
    fs::remove_dir_all(setup.checkout(name).join(".git")).unwrap();
}

#[test]
fn clean_all_goes_on_past_a_workspace_that_it_cannot_measure() {
    let setup = Setup::new();
    setup.created("broken");
    break_checkout(&setup, "broken");
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["data"][0]["name"], "reading-list", "{document}");
    assert_eq!(document["data"][0]["state"], "cleaned", "{document}");
    assert_eq!(document["failures"][0]["key"], "broken", "{document}");
    assert!(!checkout.join("target").exists());
}

#[test]
fn list_with_sizes_goes_on_past_a_workspace_that_it_cannot_measure() {
    let setup = Setup::new();
    setup.created("broken");
    break_checkout(&setup, "broken");
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (_, listed) = setup.json(&["list", "--sizes"]);
    let row = |name: &str| {
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .cloned()
            .unwrap()
    };
    let broken = row("broken");
    assert!(
        broken["problem"].as_str().unwrap().contains("measure"),
        "{listed}"
    );
    assert!(
        bytes(&row("reading-list")["disposable"]) >= 1024,
        "{listed}"
    );
}

#[test]
fn a_manifest_that_declares_every_path_disposable_is_refused_and_nothing_else_is_deleted() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(&checkout.join("mxv.toml"), "disposable = [\"*\"]\n");
    git(&checkout, &["add", "mxv.toml"]);
    git(
        &checkout,
        &["commit", "--quiet", "-m", "Declare everything disposable"],
    );
    git(&checkout, &["push", "--quiet", "origin", "HEAD"]);
    write_text(&checkout.join(".env"), "TOKEN=x");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (_, shown) = setup.json(&["show", "reading-list"]);
    let warning = shown["data"][0]["warnings"].as_str().unwrap();
    assert!(warning.contains("mxv.toml"), "{shown}");
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"][0]["paths"][0], "app: target/",
        "{document}"
    );
    assert!(document["data"][0]["warnings"].is_string(), "{document}");
    assert!(checkout.join(".env").exists());
    assert!(checkout.join("mxv.toml").exists());
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::workspace::ignored_files",
        "{document}"
    );
    assert!(document["fields"]["warnings"][0].is_string(), "{document}");
}

#[test]
fn clean_keeps_a_disposable_directory_that_holds_a_nested_repository() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    git(&checkout, &["init", "--quiet", "node_modules/fork"]);
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let row = &document["data"][0];
    assert_eq!(row["paths"][0], "app: target/", "{document}");
    let skipped = row["skipped"][0].as_str().unwrap();
    assert!(skipped.contains("node_modules/fork/.git"), "{document}");
    assert!(
        row["reason"].as_str().unwrap().contains("kept"),
        "{document}"
    );
    assert!(checkout.join("node_modules/fork/.git").exists());
}

#[test]
fn clean_needs_names_all_or_over_limit() {
    let setup = Setup::new();
    assert_eq!(setup.exit(&["clean", "--yes"]), 2);
    assert_eq!(setup.exit(&["clean", "reading-list", "--all"]), 2);
}

#[test]
fn over_limit_cleans_the_oldest_idle_workspaces_until_the_total_is_under_the_limit() {
    let setup = Setup::new();
    for name in ["older", "newer", "newest"] {
        let checkout = setup.created(name);
        write_bytes(&checkout.join("target/debug/app"), 100 * 1024);
    }
    setup.config("max_disposable = \"250KiB\"\nclean_idle = \"0s\"\n");
    let (exit, document) = setup.json(&["clean", "--over-limit", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let row = |name: &str| {
        document["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(row("older")["state"], "cleaned", "{document}");
    assert_eq!(row("newer")["state"], "kept", "{document}");
    let reason = row("newer")["reason"].as_str().unwrap().to_owned();
    assert!(reason.contains("unchanged longer"), "{reason}");
    assert!(!setup.checkout("older").join("target").exists());
    assert!(setup.checkout("newer").join("target").exists());
    let (_, document) = setup.json(&["clean", "--over-limit", "--yes"]);
    let rows = document["data"].as_array().unwrap();
    assert!(rows.iter().all(|row| row["state"] == "kept"), "{document}");
    let reason = rows.iter().find(|row| row["name"] == "newer").unwrap()["reason"]
        .as_str()
        .unwrap();
    assert!(reason.contains("within the limit"), "{reason}");
}

#[test]
fn over_limit_without_a_limit_in_the_config_file_fails_with_no_limit() {
    let setup = Setup::new();
    setup.created("reading-list");
    let (exit, document) = setup.json(&["clean", "--over-limit", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::config::no_limit");
    assert!(
        document["recovery"].to_string().contains("max_disposable"),
        "{document}"
    );
}

/// Writes an ignored `node_modules/` and `target/` into the clone, as an
/// install and a build there would.
fn installed(setup: &Setup) {
    write_text(
        &setup.clone.join("node_modules/left-pad/index.js"),
        "module.exports = pad;",
    );
    write_bytes(&setup.clone.join("target/debug/app"), 1024);
}

#[test]
fn new_clones_the_repositorys_ignored_node_modules_into_the_checkout_and_never_target() {
    let setup = Setup::new();
    installed(&setup);
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    let checkout = setup.checkout("reading-list");
    assert!(!checkout.join("target").exists());
    let warning = document["data"]["clone_warning"]
        .as_str()
        .unwrap_or_default();
    if warning.contains(UNSUPPORTED) {
        // This file system cannot clone, so mxv copied nothing, and there is
        // no clone to check.
        assert!(!checkout.join("node_modules").exists());
        return;
    }
    assert_eq!(document["data"]["clone_warning"], Value::Null, "{document}");
    assert_eq!(document["data"]["cloned"][0], "app: node_modules/");
    assert_eq!(
        fs::read_to_string(checkout.join("node_modules/left-pad/index.js")).unwrap(),
        "module.exports = pad;"
    );
    assert_eq!(git(&checkout, &["status", "--porcelain"]), "");
}

#[test]
fn new_with_no_clone_clones_nothing() {
    let setup = Setup::new();
    installed(&setup);
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app", "--no-clone"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["cloned"], Value::Array(Vec::new()));
    assert!(!setup.checkout("reading-list").join("node_modules").exists());
}

#[test]
fn an_empty_clone_list_in_the_config_file_clones_nothing() {
    let setup = Setup::new();
    installed(&setup);
    setup.config("clone_on_new = []\n");
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["cloned"], Value::Array(Vec::new()));
    assert!(!setup.checkout("reading-list").join("node_modules").exists());
}

#[test]
fn a_dry_run_of_new_names_the_directories_it_would_clone() {
    let setup = Setup::new();
    installed(&setup);
    let (exit, document) = setup.json(&["new", "reading-list", "--repo", "app", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"]["cloned"][0], "app: node_modules/");
}

#[test]
fn a_path_that_cannot_be_deleted_in_full_fails_its_workspace_and_names_the_entry() {
    use std::os::unix::fs::PermissionsExt as _;
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/locked/app"), 1024);
    write_bytes(&checkout.join("web/node_modules/left-pad/index.js"), 1024);
    let locked = checkout.join("target/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(exit, 1, "{document}");
    let failure = &document["failures"][0];
    assert_eq!(failure["code"], "mxv::file::unwritable", "{document}");
    let path = failure["fields"]["path"].as_str().unwrap();
    assert!(path.ends_with("target/locked/app"), "{document}");
    let hint = failure["recovery"]["text"].as_str().unwrap();
    assert!(
        hint.contains(&format!("directory {} write", locked.display())),
        "{hint}"
    );
    assert_eq!(
        failure["fields"]["deleted"][0], "app: web/node_modules/",
        "{document}"
    );
    assert!(!checkout.join("web/node_modules").exists());
}

#[test]
fn remove_with_yes_refuses_a_path_that_only_the_manifest_declares_disposable() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(&checkout.join("mxv.toml"), "disposable = [\"data/\"]\n");
    git(&checkout, &["add", "mxv.toml"]);
    git(
        &checkout,
        &["commit", "--quiet", "-m", "Declare data disposable"],
    );
    git(&checkout, &["push", "--quiet", "origin", "HEAD"]);
    write_text(
        &checkout.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\ndata/\n",
    );
    write_text(&checkout.join("data/pg.db"), "rows");
    let (exit, document) = setup.json(&["remove", "reading-list", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["manifest_paths"][0], "app: data/",
        "{document}"
    );
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::workspace::ignored_files",
        "{document}"
    );
    assert_eq!(document["fields"]["paths"][0], "app: data/", "{document}");
    assert!(
        document["message"].as_str().unwrap().contains("mxv.toml"),
        "{document}"
    );
    assert!(checkout.join("data/pg.db").exists());
    let (exit, document) = setup.json(&["remove", "reading-list", "--force", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!checkout.exists());
}

#[test]
fn show_and_clean_mark_a_path_that_only_the_manifest_declares_disposable() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(&checkout.join("mxv.toml"), "disposable = [\"data/\"]\n");
    write_text(
        &checkout.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\ndata/\nmxv.toml\n",
    );
    write_text(&checkout.join("data/pg.db"), "rows");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (_, shown) = setup.json(&["show", "reading-list"]);
    let paths = shown["data"][0]["disposable_paths"].as_array().unwrap();
    let source = |path: &str| {
        paths
            .iter()
            .find(|found| found["path"] == path)
            .map(|found| found["source"].clone())
            .unwrap()
    };
    assert_eq!(source("data/"), "manifest", "{shown}");
    assert_eq!(source("target/"), "built_in", "{shown}");
    let (_, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--dry-run"]);
    assert_eq!(
        document["data"][0]["manifest_paths"][0], "app: data/",
        "{document}"
    );
    let output = setup
        .command(&["clean", "--all", "--unmodified", "0s"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(checkout.join("data/pg.db").exists(), "{stderr}");
}

#[test]
fn clean_with_yes_keeps_a_manifest_only_disposable_path_and_deletes_it_with_disposable() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(&checkout.join("mxv.toml"), "disposable = [\"data/\"]\n");
    write_text(
        &checkout.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\ndata/\nmxv.toml\n",
    );
    write_text(&checkout.join("data/pg.db"), "rows");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let row = &document["data"][0];
    assert_eq!(row["state"], "cleaned", "{document}");
    assert_eq!(
        row["paths"],
        serde_json::json!(["app: target/"]),
        "{document}"
    );
    let skipped = row["skipped"][0].as_str().unwrap();
    assert!(skipped.contains("app: data/"), "{skipped}");
    assert!(
        skipped.contains("clean <NAME> --yes --disposable"),
        "{skipped}"
    );
    assert!(checkout.join("data/pg.db").exists());
    assert!(!checkout.join("target").exists());

    let (exit, document) = setup.json(&[
        "clean",
        "--all",
        "--unmodified",
        "0s",
        "--yes",
        "--disposable",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"][0]["manifest_paths"][0], "app: data/",
        "{document}"
    );
    assert!(!checkout.join("data").exists());
}

#[test]
fn a_workspace_whose_every_disposable_path_is_kept_names_them_in_its_reason() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_text(&checkout.join("mxv.toml"), "disposable = [\"data/\"]\n");
    write_text(
        &checkout.join(".git/info/exclude"),
        ".env\ntarget/\nnode_modules/\ndata/\nmxv.toml\n",
    );
    write_text(&checkout.join("data/pg.db"), "rows");
    let (exit, document) = setup.json(&["clean", "--all", "--unmodified", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let row = &document["data"][0];
    assert_eq!(row["state"], "kept", "{document}");
    let reason = row["reason"].as_str().unwrap();
    assert!(reason.contains("app: data/"), "{reason}");
    assert!(
        !reason.contains("holds no disposable directory"),
        "{reason}"
    );
    assert!(checkout.join("data/pg.db").exists());
}
