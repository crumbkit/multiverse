//! End-to-end tests of the secret variables of a process's starmap file.
//! Each test builds the repository `dispatch`, with a bare origin of its
//! own, in a temporary directory, and runs the built `mxv` binary with its
//! own state directory and workspace root there.
//!
//! The tests read no real store. A fake `op` program, first on `PATH`,
//! answers each 1Password read with a fake value or fails, and logs each
//! run, so a test can tell that mxv never ran it. No test declares a
//! generated variable, which would write to the login Keychain. One test
//! declares a secret without a bind, which mxv only reads from the
//! Keychain, under the service of a new workspace that holds no item.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the files and directories that they and the code under test write"
)]

#[path = "support/grants.rs"]
mod grants;
#[path = "support/git.rs"]
#[expect(
    dead_code,
    reason = "these tests make their repository with its files in one commit, and the helper also makes an empty one"
)]
mod support;
#[path = "support/ports.rs"]
mod test_ports;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use multiverse::{Runs, Store};
use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git};
use crate::test_ports::PortBlock;

/// The reference of the dispatcher's client secret.
const REFERENCE: &str = "op://acme-staging/auth-client-dispatcher/credential";

/// The fake value that the fake `op` gives for [`REFERENCE`].
const FAKE_VALUE: &str = "fake-client-secret-0042";

/// The manifest: `dispatcher` reads `dispatcher/starmap.toml`, which no
/// other unit reads, and writes its
/// environment to `seen.txt`, `web` depends on it, and `notes` depends on
/// nothing. Each writes a file once it starts and is ready once it prints
/// `ready`. The `[secrets.client_secret]` table checks the client secret's
/// reference and names how to sign in.
const MANIFEST: &str = r#"[processes.dispatcher]
argv = ["sh", "-c", "env > seen.txt; echo ready; exec sleep 60"]
ready = { log = "^ready$" }
starmap = "dispatcher/starmap.toml"

[processes.web]
argv = ["sh", "-c", "touch web-started; echo ready; exec sleep 60"]
ready = { log = "^ready$" }
depends_on = ["dispatcher"]

[processes.notes]
argv = ["sh", "-c", "touch notes-started; echo ready; exec sleep 60"]
ready = { log = "^ready$" }

[secrets.client_secret]
check = ["op", "read", "op://acme-staging/auth-client-dispatcher/credential"]
sign_in = "op signin --account acme.1password.com"
"#;

/// The dispatcher's starmap file: a required client secret from
/// 1Password.
const SPEC: &str = r#"[variables.CLIENT_SECRET]
purpose = "The client secret that the dispatcher signs in with."
type = "string"
unset = "required"
show = "withheld"
source = "secret"
bind = { one-password = "op://acme-staging/auth-client-dispatcher/credential" }
"#;

/// A temporary directory with the repository `dispatch`, a fake `op`, a
/// workspace root, a state directory and a block of host ports.
struct Setup {
    /// The temporary directory.
    dir: TempDir,
    /// The host ports that `mxv up` allocates from.
    ports: PortBlock,
}

impl Setup {
    /// Returns the setup, whose fake `op` gives [`FAKE_VALUE`] when `works`
    /// and otherwise fails as an `op` that is not signed in does.
    fn new(works: bool) -> Self {
        Self::with_spec(works, SPEC)
    }

    /// Returns the setup of [`Setup::new`] whose dispatcher's starmap file
    /// is `spec`.
    fn with_spec(works: bool, spec: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let setup = Self {
            dir,
            ports: PortBlock::claim(),
        };
        let root = setup.root();
        let config = root.join("config/mxv/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        write_text(&config, &setup.ports.config_line());
        setup.fake_op(works);
        setup.repository(spec);
        let (exit, document) = setup.json(&["new", "triage", "--repo", "dispatch"]);
        assert_eq!(exit, 0, "{document}");
        setup
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// The directory of the fake `op`, which comes first on `PATH`.
    fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }

    /// The file that the fake `op` appends its arguments to each time it
    /// runs.
    fn op_log(&self) -> PathBuf {
        self.root().join("op.log")
    }

    /// Writes the fake `op`. Each run appends its first argument to
    /// [`Setup::op_log`]. `op inject` replaces the client secret's
    /// placeholder in its standard input with [`FAKE_VALUE`], and `op
    /// read` prints it; a failing `op` prints that it is not signed in and
    /// exits 1.
    fn fake_op(&self, works: bool) {
        let log = self.op_log();
        let body = if works {
            format!(
                "case \"$1\" in\n  inject) sed -e 's#{{{{ {REFERENCE} }}}}#{FAKE_VALUE}#g' ;;\n  read) printf '%s' '{FAKE_VALUE}' ;;\n  *) exit 2 ;;\nesac\n"
            )
        } else {
            "echo '[ERROR] you are not currently signed in' >&2\nexit 1\n".to_owned()
        };
        let script = format!("#!/bin/sh\necho \"$1\" >> '{}'\n{body}", log.display());
        let op = self.bin().join("op");
        fs::create_dir_all(op.parent().unwrap()).unwrap();
        write_text(&op, &script);
        fs::set_permissions(&op, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Makes the clone `dispatch` of a bare origin `dispatch.git`, whose
    /// one commit on `main` holds the manifest and the dispatcher's starmap
    /// file `spec`.
    fn repository(&self, spec: &str) {
        let root = self.root();
        let seed = root.join("dispatch-seed");
        fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "--quiet", "--initial-branch=main"]);
        for (path, text) in [("mxv.toml", MANIFEST), ("dispatcher/starmap.toml", spec)] {
            fs::create_dir_all(seed.join(path).parent().unwrap()).unwrap();
            write_text(&seed.join(path), text);
            git(&seed, &["add", path]);
        }
        git(&seed, &["commit", "--quiet", "-m", "Start"]);
        git(
            &root,
            &[
                "clone",
                "--quiet",
                "--bare",
                "dispatch-seed",
                "dispatch.git",
            ],
        );
        fs::remove_dir_all(&seed).unwrap();
        git(&root, &["clone", "--quiet", "dispatch.git", "dispatch"]);
        git(
            &root.join("dispatch"),
            &["remote", "set-url", "origin", "../dispatch.git"],
        );
    }

    /// The workspace's checkout.
    fn checkout(&self) -> PathBuf {
        self.root().join("workspaces/triage/dispatch")
    }

    /// Returns the command `mxv` with `args`, a home directory, base
    /// directories, a workspace root of its own, and the fake `op` first
    /// on `PATH`.
    fn command(&self, args: &[&str]) -> Command {
        let root = self.root();
        grants::before(&root.join("state/mxv"), args);
        Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(args)
            .current_dir(&root)
            .env_clear()
            .env("PATH", format!("{}:{PATH}", self.bin().display()))
            .envs(GIT_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("MXV_ROOT", root.join("workspaces"))
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

    /// Grants every secret reference that the workspace's units read, as a
    /// person at a terminal would by answering yes to `mxv up`.
    fn grant_secrets(&self) {
        let state = self.root().join("state/mxv");
        let store = Store::new(state.join("workspaces"));
        let runs = Runs::new(&state);
        let name = multiverse::WorkspaceName::try_from("triage".to_owned()).unwrap();
        let workspace = store.read(&name).unwrap().unwrap();
        let id = match &workspace.id {
            Some(id) => id.clone(),
            None => {
                let lock = runs.lock(&name).unwrap();
                runs.stable_id(&lock, &store, &name).unwrap()
            }
        };
        let workspace = store.read(&name).unwrap().unwrap();
        let manifest = workspace.manifest().unwrap().unwrap();
        let ask = runs
            .secret_granting(&id, &workspace, &manifest, &[])
            .expect("the dispatcher reads a reference that no person granted");
        assert_eq!(ask.secrets().len(), 1, "{:?}", ask.secrets());
        runs.grant_secrets(&ask).unwrap();
        assert!(
            runs.secret_granting(&id, &workspace, &manifest, &[])
                .is_none(),
            "the grant covers the reference"
        );
    }

    /// Returns each file under the state and data directories whose text
    /// holds [`FAKE_VALUE`].
    fn files_holding_the_value(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for dir in ["state", "data"] {
            holding(&self.root().join(dir), &mut found);
        }
        found
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.json(&["down", "-w", "triage"]);
    }
}

/// Adds each file under `dir` whose bytes hold [`FAKE_VALUE`] to `found`.
fn holding(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            holding(&path, found);
        } else if kind.is_file()
            && fs::read(&path).is_ok_and(|bytes| {
                bytes
                    .windows(FAKE_VALUE.len())
                    .any(|window| window == FAKE_VALUE.as_bytes())
            })
        {
            found.push(path);
        }
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

/// Returns the row of `document` for the unit `name`.
fn row<'a>(document: &'a Value, name: &str) -> &'a Value {
    document["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name} in {document}"))
}

/// Returns the one failure of `document`.
fn failure(document: &Value) -> &Value {
    let failures = document["failures"].as_array().unwrap();
    assert_eq!(failures.len(), 1, "{document}");
    &failures[0]
}

#[test]
fn a_granted_secret_reaches_the_process_and_no_file_of_mxv_or_phase_holds_its_value() {
    let setup = Setup::new(true);
    setup.grant_secrets();

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["failures"], Value::Array(Vec::new()), "{document}");
    let seen = fs::read_to_string(setup.checkout().join("seen.txt")).unwrap();
    assert!(
        seen.lines()
            .any(|line| line == format!("CLIENT_SECRET={FAKE_VALUE}")),
        "the dispatcher did not receive the secret"
    );
    assert_eq!(setup.files_holding_the_value(), Vec::<PathBuf>::new());
    assert!(!document.to_string().contains(FAKE_VALUE));
}

#[test]
fn an_ungranted_secret_fails_only_its_unit_and_its_dependents_say_why_without_running_op() {
    let setup = Setup::new(true);

    let (exit, document) = setup.json(&["up", "-w", "triage", "--no-input"]);

    assert_eq!(exit, 1, "{document}");
    let failure = failure(&document);
    assert_eq!(failure["code"], "mxv::secret::not_granted", "{document}");
    assert_eq!(failure["key"], "dispatcher", "{document}");
    assert_eq!(failure["fields"]["unit"], "dispatcher", "{document}");
    assert_eq!(failure["fields"]["variable"], "CLIENT_SECRET", "{document}");
    assert_eq!(failure["fields"]["store"], "1password", "{document}");
    assert_eq!(failure["fields"]["reference"], REFERENCE, "{document}");
    assert_eq!(row(&document, "notes")["state"], "running", "{document}");
    let web = row(&document, "web");
    assert_ne!(web["state"], "running", "{document}");
    assert!(
        web["notes"].to_string().contains("depends on dispatcher"),
        "{document}"
    );
    assert!(setup.checkout().join("notes-started").exists());
    assert!(!setup.checkout().join("web-started").exists());
    assert!(!setup.checkout().join("seen.txt").exists());
    assert!(
        !setup.op_log().exists(),
        "mxv ran op for an ungranted reference"
    );
}

#[test]
fn a_store_that_cannot_read_a_granted_secret_fails_its_unit_with_the_sign_in_of_its_table() {
    let setup = Setup::new(false);
    setup.grant_secrets();

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{document}");
    let failure = failure(&document);
    assert_eq!(failure["code"], "mxv::secret::unavailable", "{document}");
    assert_eq!(failure["fields"]["unit"], "dispatcher", "{document}");
    assert_eq!(
        failure["fields"]["sign_in"], "op signin --account acme.1password.com",
        "{document}"
    );
    assert!(
        failure["recovery"]
            .to_string()
            .contains("op signin --account acme.1password.com"),
        "{document}"
    );
    assert_eq!(row(&document, "notes")["state"], "running", "{document}");
    assert!(setup.op_log().exists(), "mxv did not run op");
}

#[test]
fn a_repeat_up_reads_no_secret_of_a_unit_that_runs_and_reports_it_running() {
    let setup = Setup::new(true);
    setup.grant_secrets();
    let (exit, document) = setup.json(&["up", "-w", "triage"]);
    assert_eq!(exit, 0, "{document}");
    // The store signs out, and the log starts again, so a second read
    // would show and would fail.
    setup.fake_op(false);
    fs::remove_file(setup.op_log()).unwrap();

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 0, "{document}");
    assert_eq!(document["failures"], Value::Array(Vec::new()), "{document}");
    for name in ["dispatcher", "web", "notes"] {
        let row = row(&document, name);
        assert_eq!(row["state"], "running", "{document}");
        assert!(
            !row["notes"].to_string().contains("did not start"),
            "{document}"
        );
    }
    assert!(
        !setup.op_log().exists(),
        "mxv ran op for a unit that already runs"
    );
}

/// A dispatcher's starmap file whose one variable is a required secret
/// without a bind, which only the login Keychain can hold.
const SAVED_SPEC: &str = r#"[variables.API_TOKEN]
purpose = "The token that the dispatcher calls its partner with."
type = "string"
unset = "required"
show = "withheld"
source = "secret"
"#;

#[test]
fn a_saved_secret_that_the_keychain_lacks_names_the_security_command_that_saves_it() {
    // The workspace's stable id is new, so the Keychain holds no item of
    // its service, and the test only reads.
    let setup = Setup::with_spec(true, SAVED_SPEC);

    let (exit, document) = setup.json(&["up", "-w", "triage"]);

    assert_eq!(exit, 1, "{document}");
    let failure = failure(&document);
    assert_eq!(failure["code"], "mxv::secret::unavailable", "{document}");
    assert_eq!(failure["fields"]["store"], "keychain", "{document}");
    let service = failure["fields"]["keychain_service"].as_str().unwrap();
    assert!(service.starts_with("starmap.mxv-"), "{document}");
    assert_eq!(
        failure["fields"]["keychain_account"], "dispatcher/API_TOKEN",
        "{document}"
    );
    let save = format!("security add-generic-password -U -s {service} -a dispatcher/API_TOKEN -w");
    assert!(
        failure["recovery"].to_string().contains(&save),
        "{document}"
    );
    assert!(
        !failure["message"].to_string().contains("starmap run"),
        "{document}"
    );
    assert_eq!(row(&document, "notes")["state"], "running", "{document}");
    assert!(!setup.op_log().exists(), "mxv ran op for a saved secret");
}
