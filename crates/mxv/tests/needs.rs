//! End-to-end tests of the `[needs.*]` and `[provides.*]` tables of
//! `mxv.toml` and of the checks of a start's dependency plan. Each test
//! builds repositories, each with a bare origin of its own, in a temporary
//! directory, and runs the built `mxv` binary with its own state directory
//! and workspace root there. The repositories are those of case A of the
//! example company: a harness, a dispatcher, an agent and an auth service.

#![cfg(test)]

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

/// A temporary directory with repositories, a workspace root, a state
/// directory and a block of host ports.
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

    /// Runs `mxv` with `args` and `--json`, and returns the exit code, the
    /// JSON document and standard error.
    fn json(&self, args: &[&str]) -> (i32, Value, String) {
        let output = self.command(args).arg("--json").output().unwrap();
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "printed no JSON ({error}): {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (
            output.status.code().unwrap(),
            document,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    /// Creates the workspace `triage` with a checkout of each of
    /// `checkouts`, and returns its directory.
    fn workspace(&self, checkouts: &[&str]) -> PathBuf {
        let mut args = vec!["new", "triage"];
        for checkout in checkouts {
            args.extend(["--repo", checkout]);
        }
        let (exit, document, _) = self.json(&args);
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

/// Returns the manifest of an app whose one process `unit` listens on
/// `port`, is ready once it prints `ready`, and declares `needs`, the text
/// of its `[needs.*]` tables.
fn app(unit: &str, port: &str, variable: &str, needs: &str) -> String {
    format!(
        "version = 1\nports = [\"{port}\"]\n\n[processes.{unit}]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nports = {{ {variable} = \"{port}\" }}\nlisten = \"{port}\"\nready = {{ log = \"^ready$\" }}\n\n{needs}"
    )
}

/// Returns a `[needs.<name>]` table that names `repo` and `from`.
fn need(name: &str, repo: &str, from: &str) -> String {
    format!("[needs.{name}]\nrepo = \"github.com/acme/{repo}\"\nfrom = \"{from}\"\n\n")
}

/// Returns a starmap file entry that declares `name` as provided with the
/// bind `bind`, required unless `optional`.
fn provided(name: &str, bind: &str, optional: bool) -> String {
    let unset = if optional { "optional" } else { "required" };
    format!(
        "[variables.{name}]\npurpose = \"A value of another service.\"\ntype = \"string\"\nunset = \"{unset}\"\nshow = \"shown\"\nsource = \"provided\"\nbind = {bind}\n\n"
    )
}

/// Returns a starmap entry whose bind is the URL of `app`.
fn url(name: &str, app: &str, optional: bool) -> String {
    provided(
        name,
        &format!(r#"{{ provided = "multiverse/url", app = "{app}" }}"#),
        optional,
    )
}

/// Returns a starmap entry whose bind is the port of the own process
/// `process`.
fn own_port(name: &str, process: &str) -> String {
    provided(
        name,
        &format!(r#"{{ provided = "multiverse/port", process = "{process}" }}"#),
        false,
    )
}

impl Setup {
    /// Makes the four repositories of case A, each with its `mxv.toml`,
    /// whose needs name only their providers, and its `starmap.toml`.
    fn case_a(&self) {
        let harness = app(
            "runner",
            "http",
            "HARNESS_PORT",
            &(need("dispatcher-http", "dispatcher", "http") + &need("agent-http", "agent", "http")),
        );
        let harness_spec = own_port("HARNESS_PORT", "runner")
            + &url("DISPATCHER_URL", "dispatcher/api", false)
            + &url("AGENT_URL", "agent/worker", false);
        self.repository(
            "harness",
            &[("mxv.toml", &harness), ("starmap.toml", &harness_spec)],
        );
        let dispatcher = app(
            "api",
            "http",
            "DISPATCHER_PORT",
            &(need("agent-http", "agent", "http")
                + &need("auth-api", "auth", "api")
                + &need("harness-events", "harness", "http")),
        );
        let dispatcher_spec = own_port("DISPATCHER_PORT", "api")
            + &url("AGENT_URL", "agent/worker", false)
            + &url("AUTH_API_URL", "auth/server", false)
            + &url("HARNESS_EVENTS_URL", "harness/runner", true);
        self.repository(
            "dispatcher",
            &[
                ("mxv.toml", &dispatcher),
                ("starmap.toml", &dispatcher_spec),
            ],
        );
        let agent = app(
            "worker",
            "http",
            "AGENT_PORT",
            &(need("dispatcher-http", "dispatcher", "http")
                + &need("auth-api", "auth", "api")
                + &need("billing-api", "billing", "api")),
        );
        let agent_spec = own_port("AGENT_PORT", "worker")
            + &url("DISPATCHER_URL", "dispatcher/api", false)
            + &url("AUTH_API_URL", "auth/server", false)
            + &url("BILLING_URL", "billing/api", true);
        self.repository(
            "agent",
            &[("mxv.toml", &agent), ("starmap.toml", &agent_spec)],
        );
        let auth = app("server", "api", "AUTH_PORT", "");
        self.repository(
            "auth",
            &[
                ("mxv.toml", &auth),
                ("starmap.toml", &own_port("AUTH_PORT", "server")),
            ],
        );
    }
}

#[test]
fn up_of_case_as_four_checkouts_passes_every_check_and_starts_every_unit() {
    let setup = Setup::new();
    setup.case_a();
    setup.workspace(&["harness", "dispatcher", "agent", "auth"]);
    let (exit, document, stderr) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}{stderr}");
    let rows = document["data"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "{document}");
    let text = document.to_string() + &stderr;
    assert!(!text.contains("mxv::needs::"), "{text}");
    assert!(!text.contains("mxv::plan::check"), "{text}");
    let (exit, status, _) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    assert!(!status.to_string().contains("mxv::needs::"), "{status}");
}

/// The manifest of one app whose process `web` is ready once it prints
/// `ready`, with `extra` after it.
fn single(extra: &str) -> String {
    format!(
        "[processes.web]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nready = {{ log = \"^ready$\" }}\n\n{extra}"
    )
}

#[test]
fn a_refused_needs_table_warns_in_status_and_show_and_up_still_starts() {
    let setup = Setup::new();
    setup.repository(
        "shelf",
        &[(
            "mxv.toml",
            &single(
                "[needs.agent-http]\nrepo = \"github.com/acme/agent\"\nfrom = \"http\"\nas = \"AGENT_URL\"\n",
            ),
        )],
    );
    setup.workspace(&["shelf"]);
    let (exit, document, stderr) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}{stderr}");
    assert!(stderr.contains("mxv::needs::moved"), "{stderr}");
    let (exit, status, _) = setup.json(&["status", "-w", "triage"]);
    assert_eq!(exit, 0, "{status}");
    let status = status.to_string();
    assert!(status.contains("mxv::needs::moved"), "{status}");
    assert!(status.contains("mxv.toml:5"), "{status}");
    let (exit, shown, _) = setup.json(&["show", "triage"]);
    assert_eq!(exit, 0, "{shown}");
    assert!(shown.to_string().contains("mxv::needs::moved"), "{shown}");
}

#[test]
fn a_need_for_a_shared_service_warns_with_service_keys_and_up_still_starts() {
    let setup = Setup::new();
    setup.repository(
        "shelf",
        &[("mxv.toml", &single("[needs.db]\nkind = \"postgres\"\n"))],
    );
    setup.workspace(&["shelf"]);
    let (exit, document, stderr) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}{stderr}");
    assert!(stderr.contains("mxv::needs::service_keys"), "{stderr}");
    let (_, status, _) = setup.json(&["status", "-w", "triage"]);
    assert!(
        status.to_string().contains("mxv::needs::service_keys"),
        "{status}"
    );
}

#[test]
fn up_refuses_a_manifest_value_for_a_secret_of_the_starmap_file_before_any_build() {
    let setup = Setup::new();
    let manifest = "[processes.web]\nargv = [\"sh\", \"-c\", \"echo ready; exec sleep 60\"]\nbuild = [\"sh\", \"-c\", \"touch built\"]\nenv = { API_TOKEN = \"hunter2-literal\" }\nready = { log = \"^ready$\" }\n";
    let spec = "[variables.API_TOKEN]\npurpose = \"The token the shelf sends.\"\ntype = \"string\"\nunset = \"required\"\nshow = \"withheld\"\nsource = \"secret\"\n";
    setup.repository("shelf", &[("mxv.toml", manifest), ("starmap.toml", spec)]);
    let workspace = setup.workspace(&["shelf"]);
    let (exit, document, stderr) = setup.json(&["up", "-w", "triage"]);
    assert_ne!(exit, 0, "{document}");
    assert_eq!(document["code"], "mxv::plan::check", "{document}");
    assert_eq!(document["fields"]["unit"], "web", "{document}");
    assert_eq!(document["fields"]["check"], "no-secret-value", "{document}");
    assert_eq!(document["fields"]["variable"], "API_TOKEN", "{document}");
    let failures = document["fields"]["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{document}");
    assert!(
        !(document.to_string() + &stderr).contains("hunter2-literal"),
        "{document}{stderr}"
    );
    assert!(!workspace.join("shelf/built").exists());
}
