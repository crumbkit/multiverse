//! End-to-end tests of workspaces with several repositories: `new` with
//! more than one `--repo`, `add`, `cd` of a checkout, and the commands that
//! cover every checkout. Each test builds two repositories, `api` and
//! `web`, each with a bare origin of its own, in a temporary directory, and
//! runs the built `mxv` binary with its own state directory and workspace
//! root there.

#![cfg(test)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/git.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git, repository};

/// A temporary directory with the repositories `api` and `web`, a
/// workspace root and a state directory.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let setup = Self { dir };
        setup.repository("api");
        setup.repository("web");
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Makes the clone `<name>` of a bare origin `<name>.git` of its own,
    /// with one commit on `main`, below `parent` in the temporary
    /// directory, and returns the clone's path.
    fn repository_in(&self, parent: &str, name: &str) -> PathBuf {
        let root = self.root().join(parent);
        let seed = root.join(format!("{name}-seed"));
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "Start"],
        );
        let origin = format!("{name}.git");
        let seed_name = format!("{name}-seed");
        git(&root, &["clone", "--quiet", "--bare", &seed_name, &origin]);
        fs::remove_dir_all(&seed).unwrap();
        git(&root, &["clone", "--quiet", &origin, name]);
        let clone = root.join(name);
        git(
            &clone,
            &["remote", "set-url", "origin", &format!("../{origin}")],
        );
        clone
    }

    /// Makes the repository `name` at the top of the temporary directory.
    fn repository(&self, name: &str) -> PathBuf {
        self.repository_in(".", name)
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Runs `mxv` with `args`, and returns the exit code, standard output
    /// and standard error.
    fn text(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command(args).output().unwrap();
        (
            output.status.code().unwrap(),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// Returns the command `mxv` with `args`, a home directory, base
    /// directories and a workspace root of its own.
    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        grants::before(&root.join("state/mxv"), args);
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
            .env("MXV_ROOT", root.join("workspaces"))
    }

    fn workspace(&self, name: &str) -> PathBuf {
        self.root().join("workspaces").join(name)
    }

    /// Returns the text of the record of the workspace `name`.
    fn record(&self, name: &str) -> String {
        fs::read_to_string(
            self.root()
                .join("state")
                .join("mxv")
                .join("workspaces")
                .join(format!("{name}.json")),
        )
        .unwrap()
    }

    /// Creates the workspace `name` with a checkout of each of `repos`, and
    /// returns its directory.
    fn created(&self, name: &str, repos: &[&str]) -> PathBuf {
        let mut args = vec!["new", name];
        for repo in repos {
            args.extend(["--repo", repo]);
        }
        let (exit, document) = self.json(&args);
        assert_eq!(exit, 0, "{document}");
        self.workspace(name)
    }
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

/// Returns the branch that the checkout at `path` has checked out.
fn branch(path: &Path) -> String {
    git(path, &["branch", "--show-current"]).trim().to_owned()
}

#[test]
fn new_with_two_repositories_makes_a_clone_of_each_on_the_workspace_branch() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api", "web"]);
    for folder in ["api", "web"] {
        let checkout = workspace.join(folder);
        assert!(checkout.join(".git").is_dir(), "{folder}");
        assert_eq!(branch(&checkout), "triage");
    }
    assert!(setup.record("triage").contains("\"version\": 4"));
    let (exit, shown) = setup.json(&["show", "triage", "--full"]);
    assert_eq!(exit, 0, "{shown}");
    let names: Vec<&str> = shown["data"][0]["checkouts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|checkout| checkout["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api", "web"], "{shown}");
}

#[test]
fn new_accepts_the_repositories_as_a_comma_list() {
    let setup = Setup::new();
    let (exit, document) =
        setup.json(&["new", "triage", "--repo", "api,web", "--dry-run", "--full"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        document["data"]["checkouts"].as_array().unwrap().len(),
        2,
        "{document}"
    );
}

#[test]
fn agents_md_names_the_path_and_origin_of_every_checkout() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api", "web"]);
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    for folder in ["api", "web"] {
        assert!(
            agents.contains(&workspace.join(folder).display().to_string()),
            "{agents}"
        );
        assert!(agents.contains(&format!("{folder}.git")), "{agents}");
    }
}

#[test]
fn two_repositories_with_one_name_are_refused_unless_as_gives_one_another_folder() {
    let setup = Setup::new();
    let other = setup.repository_in("other", "api");
    let other = other.to_str().unwrap();
    let (exit, document) = setup.json(&["new", "triage", "--repo", "api", "--repo", other]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::name_taken", "{document}");
    assert!(document.to_string().contains("--as"), "{document}");
    assert!(!setup.workspace("triage").exists());
    let (exit, document) = setup.json(&[
        "new", "triage", "--repo", "api", "--repo", other, "--as", "api-v2",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert!(
        setup
            .workspace("triage")
            .join("api-v2")
            .join(".git")
            .is_dir()
    );
}

#[test]
fn an_as_before_every_repo_is_refused_as_a_usage_error() {
    let setup = Setup::new();
    let (exit, _, stderr) = setup.text(&["new", "triage", "--as", "x", "--repo", "api"]);
    assert_eq!(exit, 2, "{stderr}");
    assert!(stderr.contains("--as x"), "{stderr}");
}

#[test]
fn two_as_after_one_repo_are_refused_as_a_usage_error() {
    let setup = Setup::new();
    let (exit, _, stderr) = setup.text(&[
        "new", "triage", "--repo", "api", "--as", "x", "--as", "y", "--repo", "web",
    ]);
    assert_eq!(exit, 2, "{stderr}");
    assert!(stderr.contains("--as y"), "{stderr}");
}

#[test]
fn a_folder_that_holds_the_workspace_files_is_refused() {
    let setup = Setup::new();
    let (exit, document) = setup.json(&["new", "triage", "--repo", "api", "--as", "mxv.toml"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::checkout::invalid_name",
        "{document}"
    );
}

#[test]
fn cd_with_a_checkout_prints_its_directory_and_an_unknown_one_lists_the_folders() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api", "web"]);
    let (exit, stdout, _) = setup.text(&["cd", "triage", "web"]);
    assert_eq!(exit, 0);
    assert_eq!(stdout.trim(), workspace.join("web").display().to_string());
    let (exit, document) = setup.json(&["cd", "triage", "docs"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::not_found", "{document}");
    let text = document.to_string();
    assert!(text.contains("api") && text.contains("web"), "{document}");
}

#[test]
fn add_puts_another_repository_into_a_workspace_on_its_branch() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api"]);
    assert!(setup.record("triage").contains("\"version\": 3"));
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(branch(&workspace.join("web")), "triage");
    assert!(setup.record("triage").contains("\"version\": 4"));
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("web.git"), "{agents}");
}

#[test]
fn add_reads_the_workspace_from_the_current_directory() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api"]);
    // The shared fixture repository `app` has the origin `origin.git`.
    repository(&setup.root());
    let output = setup
        .command(&["add", "--repo", "../../../app", "--json"])
        .current_dir(workspace.join("api"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(workspace.join("app").join(".git").is_dir());
}

#[test]
fn a_dry_run_of_add_changes_nothing() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api"]);
    let before = setup.record("triage");
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.join("web").exists());
    assert_eq!(setup.record("triage"), before);
}

#[test]
fn add_of_a_folder_the_workspace_has_fails_with_name_taken() {
    let setup = Setup::new();
    setup.created("triage", &["api", "web"]);
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::name_taken", "{document}");
}

#[test]
fn add_to_a_workspace_with_a_workspace_manifest_needs_the_next_manifest_and_installs_it() {
    let setup = Setup::new();
    let file = setup.root().join("triage.toml");
    write_text(
        &file,
        "disposable = [\"data/cache/\"]\n\n[processes.server]\nargv = [\"true\"]\n",
    );
    let (exit, document) = setup.json(&[
        "new",
        "triage",
        "--repo",
        "api",
        "--manifest",
        "triage.toml",
    ]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::manifest::needed", "{document}");
    assert!(document.to_string().contains("--manifest"), "{document}");
    assert!(!setup.workspace("triage").join("web").exists());
    let next = setup.root().join("next.toml");
    let text = "disposable = [\"api/data/cache/\", \"web/coverage/\"]\n\n[processes.server]\nargv = [\"true\"]\ncwd = \"api\"\n";
    write_text(&next, text);
    let (exit, document) = setup.json(&[
        "add",
        "-w",
        "triage",
        "--repo",
        "web",
        "--manifest",
        "next.toml",
    ]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        fs::read_to_string(setup.workspace("triage").join("mxv.toml")).unwrap(),
        text
    );
    assert_eq!(fs::read_to_string(&next).unwrap(), text);
}

#[test]
fn add_refuses_a_manifest_that_refers_to_a_name_no_checkout_declares_and_keeps_the_workspace() {
    let setup = Setup::new();
    setup.created("triage", &["api"]);
    write_text(
        &setup.root().join("next.toml"),
        "[processes.proxy]\nargv = [\"true\"]\ndepends_on = [\"web-missing\"]\n",
    );
    let (exit, document) = setup.json(&[
        "add",
        "-w",
        "triage",
        "--repo",
        "web",
        "--manifest",
        "next.toml",
    ]);
    assert_eq!(exit, 1, "{document}");
    assert!(
        document["code"]
            .as_str()
            .is_some_and(|code| code.starts_with("mxv::manifest::")),
        "{document}"
    );
    assert!(!setup.workspace("triage").join("web").exists());
    assert!(!setup.workspace("triage").join("mxv.toml").exists());
}

#[test]
fn an_add_that_stopped_partway_is_resumed_by_the_same_add_and_refuses_any_other() {
    let setup = Setup::new();
    setup.repository("docs");
    let workspace = setup.created("triage", &["api", "web"]);
    // A run killed after it wrote the record and before it finished the
    // clone leaves the checkout without a commit and its directory partial.
    let path = setup.root().join("state/mxv/workspaces/triage.json");
    let mut record: Value = serde_json::from_str(&setup.record("triage")).unwrap();
    record["checkouts"][1]["commit"] = Value::Null;
    write_text(&path, &serde_json::to_string_pretty(&record).unwrap());
    fs::remove_dir_all(workspace.join("web")).unwrap();
    fs::create_dir_all(workspace.join("web/.git/objects")).unwrap();

    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "docs"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::checkout::incomplete", "{document}");
    let text = document.to_string();
    assert!(text.contains("--as web"), "{document}");
    assert!(!workspace.join("docs").exists());

    // The commands that act on the manifest refuse until the add resumes,
    // and the commands that only read or stop keep working.
    for args in [
        &["up", "-w", "triage"][..],
        &["restart", "-w", "triage"][..],
        &["clean", "triage", "--dry-run"][..],
    ] {
        let (exit, document) = setup.json(args);
        assert_eq!(exit, 1, "{args:?}: {document}");
        assert!(
            document.to_string().contains("mxv::checkout::incomplete"),
            "{args:?}: {document}"
        );
    }
    for args in [
        &["status", "-w", "triage"][..],
        &["show", "triage"][..],
        &["down", "-w", "triage"][..],
        &["remove", "triage", "--dry-run"][..],
    ] {
        let (exit, document) = setup.json(args);
        assert_eq!(exit, 0, "{args:?}: {document}");
    }

    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(branch(&workspace.join("web")), "triage");
    let record: Value = serde_json::from_str(&setup.record("triage")).unwrap();
    assert!(record["checkouts"][1]["commit"].is_string(), "{record}");
    let agents = fs::read_to_string(workspace.join("AGENTS.md")).unwrap();
    assert!(agents.contains("`web/`"), "{agents}");
}

#[test]
fn add_refuses_a_workspace_whose_one_checkout_has_a_folder_that_cannot_be_a_prefix() {
    let setup = Setup::new();
    setup.repository("_infra");
    setup.created("solo", &["_infra"]);
    let (exit, document) = setup.json(&["add", "-w", "solo", "--repo", "api"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::checkout::invalid_name",
        "{document}"
    );
    assert_eq!(document["fields"]["folder"], "_infra", "{document}");
    assert!(document.to_string().contains("mxv new"), "{document}");
    assert!(!setup.workspace("solo").join("api").exists());
}

#[test]
fn add_refuses_a_repository_whose_name_is_not_a_folder_name() {
    let setup = Setup::new();
    setup.repository("_infra");
    setup.created("triage", &["api"]);
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "_infra"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::checkout::invalid_name",
        "{document}"
    );
    assert!(document.to_string().contains("--as"), "{document}");
}

#[test]
fn a_merged_manifest_prefixes_each_checkouts_names_and_status_reads_them() {
    let setup = Setup::new();
    for folder in ["api", "web"] {
        write_text(
            &setup.root().join(folder).join("mxv.toml"),
            "ports = [\"http\"]\n\n[processes.server]\nargv = [\"true\"]\nports = { PORT = \"http\" }\n",
        );
        git(&setup.root().join(folder), &["add", "mxv.toml"]);
        git(
            &setup.root().join(folder),
            &["commit", "--quiet", "-m", "Manifest"],
        );
        git(
            &setup.root().join(folder),
            &["push", "--quiet", "origin", "main"],
        );
    }
    setup.created("triage", &["api", "web"]);
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    assert_eq!(status["summary"]["manifest"], "merged", "{status}");
    assert!(status["summary"]["manifest_error"].is_null(), "{status}");
    let names: Vec<&str> = status["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"api-server"), "{status}");
    assert!(names.contains(&"web-server"), "{status}");
}

#[test]
fn names_that_clash_once_prefixed_fail_with_name_clash() {
    let setup = Setup::new();
    let other = setup.repository_in("other", "api");
    for (repo, unit) in [(setup.root().join("api"), "b-c"), (other.clone(), "c")] {
        write_text(
            &repo.join("mxv.toml"),
            &format!("[processes.{unit}]\nargv = [\"true\"]\n"),
        );
        git(&repo, &["add", "mxv.toml"]);
        git(&repo, &["commit", "--quiet", "-m", "Manifest"]);
        git(&repo, &["push", "--quiet", "origin", "main"]);
    }
    let other = other.to_str().unwrap();
    let (exit, document) = setup.json(&[
        "new", "triage", "--repo", "api", "--as", "a", "--repo", other, "--as", "a-b",
    ]);
    assert_eq!(exit, 0, "{document}");
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    assert_eq!(
        status["summary"]["manifest_error_code"], "mxv::manifest::name_clash",
        "{status}"
    );
    assert!(
        status["summary"]["manifest_error"]
            .as_str()
            .unwrap()
            .contains("a-b-c"),
        "{status}"
    );
}

#[test]
fn show_warns_about_names_that_clash_once_prefixed() {
    let setup = Setup::new();
    let other = setup.repository_in("other", "api");
    for (repo, unit) in [(setup.root().join("api"), "b-c"), (other.clone(), "c")] {
        write_text(
            &repo.join("mxv.toml"),
            &format!("[processes.{unit}]\nargv = [\"true\"]\n"),
        );
        git(&repo, &["add", "mxv.toml"]);
        git(&repo, &["commit", "--quiet", "-m", "Manifest"]);
        git(&repo, &["push", "--quiet", "origin", "main"]);
    }
    let other = other.to_str().unwrap();
    let (exit, document) = setup.json(&[
        "new", "triage", "--repo", "api", "--as", "a", "--repo", other, "--as", "a-b",
    ]);
    assert_eq!(exit, 0, "{document}");
    let (exit, shown) = setup.json(&["show", "triage", "--full"]);
    assert_eq!(exit, 0, "{shown}");
    let warnings = shown["data"][0]["warnings"].as_str().unwrap_or_default();
    assert!(warnings.contains("a-b-c"), "{shown}");
}

#[test]
fn up_runs_each_checkouts_process_in_its_own_checkout_and_down_stops_them() {
    let setup = Setup::new();
    for folder in ["api", "web"] {
        let repo = setup.root().join(folder);
        write_text(
            &repo.join("mxv.toml"),
            "[processes.server]\nargv = [\"sh\", \"-c\", \"pwd > where.txt; exec sleep 60\"]\n",
        );
        git(&repo, &["add", "mxv.toml"]);
        git(&repo, &["commit", "--quiet", "-m", "Manifest"]);
        git(&repo, &["push", "--quiet", "origin", "main"]);
    }
    let workspace = setup.created("triage", &["api", "web"]);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    for folder in ["api", "web"] {
        let file = workspace.join(folder).join("where.txt");
        // Each run of `mxv status` takes long enough to pace the wait.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !file.is_file() && std::time::Instant::now() < deadline {
            setup.json(&["status", "-w", "triage"]);
        }
        let written = fs::read_to_string(&file).unwrap();
        assert_eq!(
            Path::new(written.trim()).canonicalize().unwrap(),
            workspace.join(folder).canonicalize().unwrap()
        );
    }
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    for name in ["api-server", "web-server"] {
        let row = status["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("no row {name}: {status}"));
        assert_eq!(row["state"], "running", "{status}");
    }
    let (exit, document) = setup.json(&["down", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
}

#[test]
fn status_reports_a_checkout_without_its_own_git_directory_as_a_problem() {
    let setup = Setup::new();
    // The workspace root sits inside a repository, so a checkout that
    // loses its `.git` would read that repository's branch.
    git(
        &setup.root(),
        &["init", "--quiet", "--initial-branch=outer"],
    );
    let workspace = setup.created("triage", &["api", "web"]);
    fs::remove_dir_all(workspace.join("web").join(".git")).unwrap();
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    let web = &status["summary"]["checkouts"][1];
    assert_eq!(web["name"], "web", "{status}");
    assert!(web["branch"].is_null(), "{status}");
    assert!(
        web["problem"]
            .as_str()
            .unwrap()
            .contains("no .git of its own"),
        "{status}"
    );
    assert_eq!(status["summary"]["checkouts"][0]["branch"], "triage");
}

#[test]
fn status_reports_the_branch_commit_and_changes_of_every_checkout() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api", "web"]);
    write_text(&workspace.join("web").join("notes.md"), "notes");
    let (exit, status) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    let checkouts = status["summary"]["checkouts"].as_array().unwrap();
    assert_eq!(checkouts.len(), 2, "{status}");
    for (checkout, folder, changes) in [(&checkouts[0], "api", 0), (&checkouts[1], "web", 1)] {
        assert_eq!(checkout["name"], folder, "{status}");
        assert_eq!(
            checkout["path"],
            workspace.join(folder).display().to_string(),
            "{status}"
        );
        assert_eq!(checkout["branch"], "triage", "{status}");
        let commit = git(&workspace.join(folder), &["rev-parse", "HEAD"]);
        assert_eq!(checkout["commit"], commit.trim(), "{status}");
        assert_eq!(checkout["changes"], changes, "{status}");
    }
    let (exit, text, _) = setup.text(&["status", "-w", "triage"]);
    assert_eq!(exit, 0);
    assert!(
        text.contains("with 1 uncommitted or untracked entries"),
        "{text}"
    );
}

#[test]
fn remove_refuses_unpushed_work_in_the_second_checkout_and_names_it() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api", "web"]);
    git(
        &workspace.join("web"),
        &["commit", "--quiet", "--allow-empty", "-m", "Unpushed"],
    );
    let (exit, document) = setup.json(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(
        document["code"], "mxv::checkout::unpushed_commits",
        "{document}"
    );
    assert!(document.to_string().contains("web"), "{document}");
    assert!(workspace.join("api").is_dir());
}

#[test]
fn remove_deletes_every_checkout_of_a_clean_workspace() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api", "web"]);
    let (exit, document) = setup.json(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

/// Rewrites the record of the workspace `name` in the shape that mxv
/// wrote before this change, at version 3 with one checkout, as a record
/// that origin/main's mxv wrote. It keeps the values that `new` wrote.
fn as_written_before(setup: &Setup, name: &str) {
    let path = setup
        .root()
        .join("state")
        .join("mxv")
        .join("workspaces")
        .join(format!("{name}.json"));
    let written: Value = serde_json::from_str(&setup.record(name)).unwrap();
    let checkout = &written["checkouts"][0];
    let before = format!(
        r#"{{
  "version": 3,
  "kind": "clone",
  "id": {id},
  "name": {name},
  "path": {path},
  "checkouts": [
    {{
      "name": "api",
      "backend": "git",
      "source": {source},
      "path": {checkout_path},
      "branch": {branch},
      "base": "refs/remotes/origin/main",
      "commit": {commit},
      "mirror": {mirror}
    }}
  ]
}}"#,
        id = written["id"],
        name = written["name"],
        path = written["path"],
        source = checkout["source"],
        checkout_path = checkout["path"],
        branch = checkout["branch"],
        commit = checkout["commit"],
        mirror = checkout["mirror"],
    );
    write_text(&path, &before);
}

#[test]
fn a_record_that_the_previous_mxv_wrote_still_reads_takes_a_checkout_and_is_removed() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api"]);
    as_written_before(&setup, "triage");
    let (exit, shown) = setup.json(&["show", "triage"]);
    assert_eq!(exit, 0, "{shown}");
    let (exit, stdout, _) = setup.text(&["cd", "triage", "api"]);
    assert_eq!(exit, 0);
    assert_eq!(stdout.trim(), workspace.join("api").display().to_string());
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 0, "{document}");
    assert!(setup.record("triage").contains("\"version\": 4"));
    let (exit, document) = setup.json(&["remove", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert!(!workspace.exists());
}

/// Sets the state of the record of the workspace `name` to `state`, as a
/// `mxv new` that stopped before its last write leaves `creating`.
fn set_state(setup: &Setup, name: &str, state: &str) {
    let path = setup
        .root()
        .join("state/mxv/workspaces")
        .join(format!("{name}.json"));
    let mut record: Value = serde_json::from_str(&setup.record(name)).unwrap();
    record["state"] = Value::from(state);
    write_text(&path, &serde_json::to_string_pretty(&record).unwrap());
}

#[test]
fn add_and_clean_refuse_a_workspace_that_mxv_new_did_not_finish() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api"]);
    set_state(&setup, "triage", "creating");
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 1, "{document}");
    assert_eq!(document["code"], "mxv::workspace::creating", "{document}");
    assert!(!workspace.join("web").exists());
    let (exit, document) = setup.json(&["clean", "triage", "--dry-run"]);
    assert_eq!(exit, 1, "{document}");
    assert!(
        document.to_string().contains("mxv::workspace::creating"),
        "{document}"
    );
}

#[test]
fn add_and_clean_run_on_a_live_workspace() {
    let setup = Setup::new();
    let workspace = setup.created("triage", &["api"]);
    set_state(&setup, "triage", "live");
    let (exit, document) = setup.json(&["clean", "triage", "--dry-run"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, document) = setup.json(&["add", "-w", "triage", "--repo", "web"]);
    assert_eq!(exit, 0, "{document}");
    assert!(workspace.join("web").exists());
}
