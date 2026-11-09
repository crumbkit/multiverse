//! End-to-end tests of `mxv archive`, `mxv restore`, `mxv list --archived`
//! and `mxv remove --archived`. Each test builds a repository in a temporary
//! directory and runs the built `mxv` binary there.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Stdio};

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

/// Writes a git configuration that gives git no identity and refuses to
/// guess one, as a machine with no name for its user does, and returns its
/// path.
fn no_identity(setup: &Setup) -> PathBuf {
    let path = setup.root().join("no-identity.gitconfig");
    write(&path, "[user]\n\tuseConfigOnly = true\n");
    path
}

#[test]
fn wip_commits_with_the_stated_identity_when_git_has_none() {
    let setup = Setup::new();
    write(&setup.checkout().join("draft.txt"), "draft\n");
    let config = no_identity(&setup);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test runs mxv with a git environment that has no identity"
    )]
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"))
        .args(["archive", "-w", "triage", "--wip", "--yes", "--json"])
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
    let (exit, document) = setup.run(&["restore", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
    assert_eq!(
        git(&setup.checkout(), &["log", "-1", "--format=%an <%ae>"]).trim(),
        "mxv <mxv@localhost.invalid>"
    );
}

#[test]
fn a_wip_commit_that_cannot_run_fails_before_the_processes_stop() {
    let setup = Setup::new();
    write(
        &setup.checkout().join("mxv.toml"),
        "[processes.sleeper]\nargv = [\"sh\", \"-c\", \"exec sleep 600\"]\n",
    );
    let (exit, document) = setup.run(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    write(&setup.checkout().join(".git/index.lock"), "");
    let (exit, document) = setup.run(&["archive", "-w", "triage", "--wip", "--yes"]);
    fs::remove_file(setup.checkout().join(".git/index.lock")).unwrap();
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
fn the_question_names_each_path_that_wip_would_commit() {
    let setup = Setup::new();
    write(&setup.checkout().join("draft.txt"), "draft\n");
    let head = setup.head();
    let prompted = Prompted::start(&setup, &["--wip"]);
    let question = prompted.question.clone();
    prompted.answer("n");
    assert!(question.contains("draft.txt"), "{question}");
    assert_eq!(setup.head(), head, "a declined archive commits nothing");
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
        "\"version\": 1,",
        &format!("\"version\": 1,\n  \"ports\": {{ \"web\": {held} }},"),
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
    assert!(hint.contains("mxv remove triage"), "{document}");
    assert!(!hint.contains("remove --archived"), "{document}");
}
