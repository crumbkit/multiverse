//! The test of CLI-12 in the Rust standards: every prompt has a flag that
//! answers it, and a command that would prompt fails, without a terminal
//! or with `--no-input`, with a code that names the flag.
//!
//! [`ANSWERED`] lists each command that takes `--yes`, with the state that
//! makes it ask, and [`every_command_with_yes_is_listed`] checks the list
//! against the command tree, so a new command with `--yes` fails here
//! until it gets a row. One test runs every row twice: with its standard
//! input from a pipe, as a script or an agent runs it, and with
//! `--no-input` at a pseudo-terminal that the `script` program opens. Both
//! runs must fail with `mxv::confirmation::needs_yes`, name `--yes` in the
//! recovery and change nothing.
//!
//! The comparison with the command tree finds only the prompts that `--yes`
//! answers. Two questions have no flag that answers them, because only a
//! person may answer them, so this file lists them by hand: the grant of
//! wider permissions that `up` and `restart` ask for, and the trust of the
//! proxy's certificate authority, which also raises the system's password
//! prompt. Their tests expect each to fail with its own code and a
//! recovery that a person runs at a terminal. A new prompt without `--yes`
//! needs a test here that the tree comparison does not ask for. `mxv preflight` asks only at a terminal and never fails for
//! want of an answer, so it has no test here.

#![cfg(test)]
#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the files and directories that they and the code under test write"
)]

#[path = "support/git.rs"]
mod support;
#[path = "support/terminal.rs"]
mod terminal;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::CommandFactory as _;
use serde_json::Value;
use snapbox::cmd::Command;
use tempfile::TempDir;

use crate::support::{GIT_ENV, PATH, git, repository};
use crate::terminal::{document, script};

/// How long a run may take before the test fails, so a command that blocks
/// on a question fails the test that runs it instead of the whole job.
const DEADLINE: Duration = Duration::from_secs(60);

/// Writes `text` to the file at `path`, and creates its directory.
fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test writes a file that a person would write in a checkout"
    )]
    fs::write(path, text).unwrap();
}

/// A temporary directory with a repository `app` and a workspace `triage`
/// of it.
struct Setup {
    dir: TempDir,
}

impl Setup {
    fn new() -> Self {
        let setup = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        repository(&setup.root());
        let (exit, document) = setup.piped(&["new", "triage", "--repo", "app", "--no-sweep"]);
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

    fn archive_record(&self) -> PathBuf {
        self.root().join("state/mxv/archives/triage/archive.json")
    }

    /// Returns `command` with only the variables of a run: the workspace
    /// root, the XDG directories and git without the person's
    /// configuration.
    fn with_variables(&self, command: Command) -> Command {
        let root = self.root();
        let at = |name: &str| root.join(name).display().to_string();
        let mut command = command
            .current_dir(&root)
            .env_clear()
            .env("HOME", at("home"))
            .env("PATH", PATH)
            .env("MXV_ROOT", at("mxv"))
            .env("XDG_CONFIG_HOME", at("config"))
            .env("XDG_STATE_HOME", at("state"))
            .env("XDG_DATA_HOME", at("data"))
            .env("XDG_CACHE_HOME", at("cache"));
        for (name, value) in GIT_ENV {
            command = command.env(name, value);
        }
        command
    }

    /// Runs `mxv` with `args` and `--json`, with standard input from a
    /// pipe, and returns the exit code and the document that it printed.
    fn piped(&self, args: &[&str]) -> (i32, Value) {
        let output = self
            .with_variables(Command::new(env!("CARGO_BIN_EXE_mxv")))
            .args(args)
            .arg("--json")
            .timeout(DEADLINE)
            .output()
            .unwrap_or_else(|error| panic!("{args:?}: {error}"));
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{args:?}: {error}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    /// Runs `mxv` with `args`, `--no-input` and `--json` at a
    /// pseudo-terminal that the `script` program opens, so standard input
    /// and standard error are both terminals, and returns the exit code and
    /// the document that it printed.
    fn at_terminal(&self, args: &[&str]) -> (i32, Value) {
        let mut words = vec![env!("CARGO_BIN_EXE_mxv").to_owned()];
        words.extend(args.iter().map(|&arg| arg.to_owned()));
        words.extend(["--no-input".to_owned(), "--json".to_owned()]);
        let output = self
            .with_variables(script(&words))
            .timeout(DEADLINE)
            .output()
            .unwrap_or_else(|error| panic!("{args:?} at a terminal: {error}"));
        let text = String::from_utf8_lossy(&output.stdout);
        let document = document(&format!("{args:?}"), &text);
        (output.status.code().unwrap(), document)
    }
}

/// A command that asks before it changes anything and that `--yes`
/// answers.
struct Answered {
    /// The command's path in the command tree, such as `remove`.
    command: &'static str,
    /// The words that run it, after `mxv`.
    args: &'static [&'static str],
    /// Puts the setup into the state that makes the command ask.
    prepare: fn(&Setup),
    /// Returns the path that the command would delete or change if it went
    /// on, which must still exist afterwards.
    kept: fn(&Setup) -> PathBuf,
}

/// Archives the workspace `triage`.
fn archived(setup: &Setup) {
    let (exit, document) = setup.piped(&["archive", "-w", "triage", "--yes"]);
    assert_eq!(exit, 0, "{document}");
}

/// Archives the workspace `triage`, marks the archive as one that stopped
/// before it finished, and deletes its pin, which `mxv repair` moves aside.
fn retired_without_pins(setup: &Setup) {
    archived(setup);
    let path = setup.archive_record();
    let record = fs::read_to_string(&path).unwrap();
    write(
        &path,
        &record.replace("\"state\": \"archived\"", "\"state\": \"retired\""),
    );
    let record: Value = serde_json::from_str(&record).unwrap();
    let checkout = &record["checkouts"][0];
    let mirror = PathBuf::from(checkout["mirror"].as_str().unwrap());
    git(
        &mirror,
        &["update-ref", "-d", checkout["pin"].as_str().unwrap()],
    );
}

/// Gives the checkout an ignored build directory, which `mxv clean`
/// deletes.
fn with_build_output(setup: &Setup) {
    write(&setup.checkout().join(".git/info/exclude"), "target/\n");
    write(&setup.checkout().join("target/debug/app"), "built");
}

/// Commits in the checkout and pushes the commit to origin's `main`, so
/// `mxv sweep` finds the workspace merged.
fn merged(setup: &Setup) {
    let checkout = setup.checkout();
    write(&checkout.join("notes.md"), "notes\n");
    git(&checkout, &["add", "notes.md"]);
    git(&checkout, &["commit", "--quiet", "-m", "Notes"]);
    git(
        &checkout,
        &["push", "--quiet", "origin", "HEAD", "HEAD:main"],
    );
}

/// Leaves the run files of a name that no workspace has, which
/// `mxv repair --orphans` deletes.
fn orphaned_run_files(setup: &Setup) {
    write(&orphan(setup).join("notes"), "left behind");
}

fn orphan(setup: &Setup) -> PathBuf {
    setup.root().join("state/mxv/runs/gone")
}

fn nothing(_: &Setup) {}

/// Every command that `--yes` answers, each in a state that makes it ask.
const ANSWERED: &[Answered] = &[
    Answered {
        command: "remove",
        args: &["remove", "triage"],
        prepare: nothing,
        kept: Setup::workspace,
    },
    Answered {
        command: "remove",
        args: &["remove", "--archived", "triage"],
        prepare: archived,
        kept: Setup::archive_record,
    },
    Answered {
        command: "archive",
        args: &["archive", "-w", "triage"],
        prepare: nothing,
        kept: Setup::workspace,
    },
    Answered {
        command: "restore",
        args: &["restore", "triage"],
        prepare: archived,
        kept: Setup::archive_record,
    },
    Answered {
        command: "sweep",
        args: &["sweep"],
        prepare: merged,
        kept: Setup::workspace,
    },
    Answered {
        command: "clean",
        args: &["clean", "--all", "--idle", "0s"],
        prepare: with_build_output,
        kept: |setup| setup.checkout().join("target/debug/app"),
    },
    Answered {
        command: "repair",
        args: &["repair", "triage"],
        prepare: retired_without_pins,
        kept: Setup::archive_record,
    },
    Answered {
        command: "repair",
        args: &["repair", "--orphans"],
        prepare: orphaned_run_files,
        kept: orphan,
    },
];

/// Returns the path of each command below `command` that takes `--yes`,
/// with its names below the root joined by spaces, such as `repair`.
fn with_yes(command: &clap::Command, above: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for sub in command.get_subcommands() {
        let path = if above.is_empty() {
            sub.get_name().to_owned()
        } else {
            format!("{above} {}", sub.get_name())
        };
        if sub.get_arguments().any(|arg| arg.get_long() == Some("yes")) {
            found.insert(path.clone());
        }
        found.extend(with_yes(sub, &path));
    }
    found
}

#[test]
fn every_command_with_yes_is_listed() {
    let mut command = mxv::Cli::command();
    command.build();
    let listed: BTreeSet<String> = ANSWERED
        .iter()
        .map(|answered| answered.command.to_owned())
        .collect();
    assert_eq!(with_yes(&command, ""), listed);
}

#[test]
fn every_command_with_yes_asks_for_it_without_a_terminal() {
    for answered in ANSWERED {
        let args = answered.args;
        let setup = Setup::new();
        (answered.prepare)(&setup);
        let kept = (answered.kept)(&setup);
        assert!(kept.exists(), "{args:?}: {} is missing", kept.display());
        for (how, (exit, document)) in [
            ("from a pipe", setup.piped(args)),
            ("with --no-input at a terminal", setup.at_terminal(args)),
        ] {
            assert_eq!(exit, 1, "{args:?} {how}: {document}");
            assert_eq!(
                document["code"], "mxv::confirmation::needs_yes",
                "{args:?} {how}: {document}"
            );
            assert_eq!(
                document["recovery"]["kind"], "run",
                "{args:?} {how}: {document}"
            );
            let call = document["recovery"]["call"].as_str().unwrap();
            assert!(
                call.split(' ').any(|word| word == "--yes"),
                "{args:?} {how}: {document}"
            );
            assert!(kept.exists(), "{args:?} {how}: {} is gone", kept.display());
        }
    }
}

/// Expects the run of `args`, made `how`, which exited with `exit` and
/// printed `document`, to fail with `code` and a recovery that a person
/// runs at a terminal, because no flag answers the question.
fn refused_for_a_person(args: &[&str], how: &str, (exit, document): (i32, Value), code: &str) {
    assert_eq!(exit, 1, "{args:?} {how}: {document}");
    assert_eq!(document["code"], code, "{args:?} {how}: {document}");
    assert_eq!(
        document["recovery"]["kind"], "terminal",
        "{args:?} {how}: {document}"
    );
}

#[test]
fn up_and_restart_refuse_an_ungranted_unit_without_a_terminal() {
    let setup = Setup::new();
    write(
        &setup.checkout().join("mxv.toml"),
        "[processes.idle]\nargv = [\"sleep\", \"60\"]\n",
    );
    for verb in ["up", "restart"] {
        let args = [verb, "-w", "triage"];
        let code = "mxv::permissions::not_granted";
        refused_for_a_person(&args, "from a pipe", setup.piped(&args), code);
        refused_for_a_person(
            &args,
            "with --no-input at a terminal",
            setup.at_terminal(&args),
            code,
        );
    }
}

/// tests/proxy.rs runs `mxv proxy trust` from a pipe, and its real-Caddy
/// tests run `mxv proxy install` with `--no-input` at a terminal.
#[test]
fn proxy_trust_with_no_input_refuses_at_a_terminal() {
    let setup = Setup::new();
    let args = ["proxy", "trust"];
    refused_for_a_person(
        &args,
        "with --no-input at a terminal",
        setup.at_terminal(&args),
        "mxv::proxy::trust_needs_terminal",
    );
}
