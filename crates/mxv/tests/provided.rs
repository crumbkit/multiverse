//! End-to-end tests of the provided variables of a process's starmap
//! file. Each test builds two repositories, `catalogue` and `shelf`, each
//! with a bare origin of its own, in a temporary directory, and runs the
//! built `mxv` binary with its own state directory and workspace root
//! there. The catalogue's process `api` listens on its port `http`, and the
//! shelf's `starmap.toml` binds variables to that port.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the real clock and the files and directories that they and the code under test write"
)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/git.rs"]
#[expect(
    dead_code,
    reason = "these tests make their repositories with their files in one commit, and the helper also makes an empty one"
)]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git};
use crate::test_ports::PortBlock;

/// The catalogue's manifest: `api` listens on `http` and is ready once it
/// prints `ready`.
const CATALOGUE: &str = "ports = [\"http\"]\n\n[processes.api]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nports = { PORT = \"http\" }\nlisten = \"http\"\nready = { log = \"^ready$\" }\n";

/// The shelf's manifest: `web` writes its environment to `seen.txt` and is
/// ready once it prints `ready`.
const SHELF: &str = "[processes.web]\nargv = [\"sh\", \"-c\", \"env > seen.txt; echo ready; exec sleep 60\"]\nready = { log = \"^ready$\" }\n";

/// A temporary directory with the repositories `catalogue` and `shelf`, a
/// workspace root, a state directory and a block of host ports.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The host ports that `mxv up` allocates from.
    ports: PortBlock,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let setup = Self {
            dir,
            ports: PortBlock::claim(),
        };
        let config = setup.root().join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write_text(&config, &setup.ports.config_line());
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Makes the clone `<name>` of a bare origin `<name>.git` of its own,
    /// whose one commit on `main` holds `files`, each a path and its text.
    fn repository(&self, name: &str, files: &[(&str, &str)]) {
        let root = self.root();
        let seed = root.join(format!("{name}-seed"));
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        for (path, text) in files {
            write_text(&seed.join(path), text);
            git(&seed, &["add", path]);
        }
        git(
            &seed,
            &["commit", "--quiet", "--allow-empty", "-m", "Start"],
        );
        let origin = format!("{name}.git");
        let seed_name = format!("{name}-seed");
        git(&root, &["clone", "--quiet", "--bare", &seed_name, &origin]);
        fs::remove_dir_all(&seed).unwrap();
        git(&root, &["clone", "--quiet", &origin, name]);
        git(
            &root.join(name),
            &["remote", "set-url", "origin", &format!("../{origin}")],
        );
    }

    /// Runs `mxv` with `args` and `--json`, and returns the exit code and
    /// the JSON document.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let root = self.root();
        grants::before(&root.join("state/mxv"), args);
        let output = Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .arg("--json")
            .current_dir(&root)
            .env_clear()
            .env("PATH", PATH)
            .envs(GIT_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
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

    /// Creates the workspace `triage` with a checkout of `catalogue` and
    /// one of `shelf`, whose `starmap.toml` holds `spec`, and returns the
    /// workspace's directory.
    fn workspace(&self, spec: &str) -> PathBuf {
        self.repository("catalogue", &[("mxv.toml", CATALOGUE)]);
        self.repository("shelf", &[("mxv.toml", SHELF), ("starmap.toml", spec)]);
        let (exit, document) =
            self.json(&["new", "triage", "--repo", "catalogue", "--repo", "shelf"]);
        assert_eq!(exit, 0, "{document}");
        self.root().join("workspaces/triage")
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.json(&["down", "-w", "triage"]);
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

/// Returns a starmap file entry that declares the required variable `name`
/// as provided with `bind`.
fn provided(name: &str, bind: &str) -> String {
    format!(
        "[variables.{name}]\npurpose = \"A value of the catalogue.\"\ntype = \"string\"\nunset = \"required\"\nshow = \"shown\"\nsource = \"provided\"\nbind = {bind}\n\n"
    )
}

/// Returns the variables of the environment that a process wrote to
/// `file`. The process writes the file before it prints the line that
/// makes it ready, so the file is complete once `mxv up` returns.
fn seen(file: &Path) -> Vec<(String, String)> {
    fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

#[test]
fn up_gives_a_process_the_url_and_port_of_another_checkouts_unit_that_its_starmap_file_binds() {
    let setup = Setup::new();
    let spec = provided(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-api" }"#,
    ) + &provided(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", process = "catalogue-api" }"#,
    );
    let workspace = setup.workspace(&spec);
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    let (exit, shown) = setup.json(&["show", "triage"]);
    assert_eq!(exit, 0, "{shown}");
    let port = shown["data"][0]["ports"]["catalogue-http"]
        .as_u64()
        .unwrap_or_else(|| panic!("no port catalogue-http: {shown}"));
    let vars = seen(&workspace.join("shelf/seen.txt"));
    let value = |name: &str| {
        vars.iter()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("no {name}: {vars:?}"))
    };
    assert_eq!(value("CATALOGUE_PORT"), port.to_string());
    assert_eq!(value("CATALOGUE_URL"), format!("http://127.0.0.1:{port}"));
}

#[test]
fn up_refuses_a_bind_to_a_unit_that_does_not_exist_before_any_process_starts() {
    let setup = Setup::new();
    let workspace = setup.workspace(&provided(
        "CATALOGUE_URL",
        r#"{ provided = "multiverse/url", app = "catalogue-search" }"#,
    ));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::provided_unknown",
        "{document}"
    );
    assert_eq!(document["fields"]["unit"], "shelf-web", "{document}");
    assert_eq!(
        document["fields"]["variable"], "CATALOGUE_URL",
        "{document}"
    );
    assert_eq!(document["fields"]["app"], "catalogue-search", "{document}");
    assert_eq!(document["fields"]["port"], "listen", "{document}");
    let message = document["message"].as_str().unwrap();
    assert!(message.contains("`catalogue-search`"), "{document}");
    assert!(message.contains("`listen`"), "{document}");
    assert!(!workspace.join("shelf/seen.txt").exists());
}

#[test]
fn up_refuses_a_bind_to_a_port_that_the_unit_does_not_have_and_names_both() {
    let setup = Setup::new();
    setup.workspace(&provided(
        "CATALOGUE_PORT",
        r#"{ provided = "multiverse/port", app = "catalogue-api", port = "catalogue-grpc" }"#,
    ));
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::provided_unknown",
        "{document}"
    );
    assert_eq!(document["fields"]["app"], "catalogue-api", "{document}");
    assert_eq!(document["fields"]["port"], "catalogue-grpc", "{document}");
    let message = document["message"].as_str().unwrap();
    assert!(message.contains("`catalogue-api`"), "{document}");
    assert!(message.contains("`catalogue-grpc`"), "{document}");
}

#[test]
fn up_refuses_a_starmap_file_that_does_not_parse() {
    let setup = Setup::new();
    setup.workspace("[variables.BROKEN]\npurpose = 3\n");
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(
        document["code"], "mxv::manifest::starmap_invalid",
        "{document}"
    );
    assert!(
        document["fields"]["file"]
            .as_str()
            .unwrap()
            .ends_with("shelf/starmap.toml"),
        "{document}"
    );
}
