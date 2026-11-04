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
use std::time::{Duration, SystemTime};

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

    /// Sets the modification time of every file of the workspace `name`
    /// and of its checkout's git directory `days` days back, as if nobody
    /// had touched it since.
    fn backdate(&self, name: &str, days: u64) {
        let time = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
        backdate(&self.root().join("workspaces").join(name), time);
        let link = fs::read_to_string(self.checkout(name).join(".git")).unwrap();
        let git_dir = link.trim().strip_prefix("gitdir: ").unwrap();
        backdate(Path::new(git_dir), time);
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

/// Sets the modification time of every file below `dir` to `time`.
fn backdate(dir: &Path, time: SystemTime) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            backdate(&path, time);
        } else {
            fs::File::open(&path).unwrap().set_modified(time).unwrap();
        }
    }
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
    assert_eq!(shown["data"][0]["disposable_paths"][0], "app: target/");
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
fn remove_deletes_a_directory_that_the_checkouts_manifest_declares_disposable() {
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
    let (exit, document) = setup.json(&["remove", "reading-list", "--yes", "--delete-unmerged"]);
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
    setup.backdate("reading-list", 8);
    let (exit, document) = setup.json(&["clean", "reading-list", "--yes"]);
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
    let (exit, document) = setup.json(&["clean", "--all", "--idle", "0s", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["data"][0]["state"], "cleaned");
    assert!(!checkout.join("target").exists());
}

#[test]
fn the_config_file_sets_the_idle_age_and_idle_overrides_it() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    setup.backdate("reading-list", 3);
    setup.config("clean_idle = \"2d\"\n");
    let (_, document) = setup.json(&["clean", "--all", "--idle", "4d", "--dry-run"]);
    assert_eq!(document["data"][0]["state"], "kept", "{document}");
    let (_, document) = setup.json(&["clean", "--all", "--dry-run"]);
    assert_eq!(document["data"][0]["state"], "would_clean", "{document}");
}

#[test]
fn a_dry_run_of_clean_deletes_nothing_and_reports_what_it_would_free() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 64 * 1024);
    let (exit, document) = setup.json(&["clean", "--all", "--idle", "0s", "--dry-run"]);
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
    let (exit, document) = setup.json(&["clean", "--all", "--idle", "0s"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::confirmation::needs_yes");
    assert!(checkout.join("target").exists());
}

#[test]
fn clean_of_a_name_that_no_workspace_has_fails_with_not_found_and_deletes_nothing() {
    let setup = Setup::new();
    let checkout = setup.created("reading-list");
    write_bytes(&checkout.join("target/debug/app"), 1024);
    let (exit, document) =
        setup.json(&["clean", "reading-list", "missing", "--idle", "0s", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::not_found");
    assert!(checkout.join("target").exists());
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
    for (name, days) in [("older", 30), ("newer", 10), ("busy", 1)] {
        let checkout = setup.created(name);
        write_bytes(&checkout.join("target/debug/app"), 100 * 1024);
        setup.backdate(name, days);
    }
    setup.config("max_disposable = \"250KiB\"\n");
    let (exit, document) = setup.json(&["clean", "--over-limit", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    let state = |name: &str| {
        document["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .map(|row| row["state"].clone())
            .unwrap()
    };
    assert_eq!(state("older"), "cleaned", "{document}");
    assert_eq!(state("newer"), "kept", "{document}");
    assert_eq!(state("busy"), "kept", "{document}");
    assert!(!setup.checkout("older").join("target").exists());
    assert!(setup.checkout("newer").join("target").exists());
    let (_, document) = setup.json(&["clean", "--over-limit", "--yes"]);
    assert!(
        document["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["state"] == "kept"),
        "{document}"
    );
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
