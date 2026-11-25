//! The test of CLI-12 in the Rust standards: every prompt has a flag that
//! answers it, and a command that would prompt fails, without a terminal
//! or with `--no-input`, with a code that names the flag.
//!
//! [`ANSWERED`] lists each command that takes `--yes`, with the state that
//! makes it ask, and [`every_command_with_yes_is_listed`] checks the list
//! against the command tree, so a new command with `--yes` fails here
//! until it gets a row. Each row's test runs the command twice: with its
//! standard input from a pipe, as a script or an agent runs it, and with
//! `--no-input` at a pseudo-terminal that the `script` program opens. Both
//! runs must fail with `mxv::confirmation::needs_yes`, name `--yes` in the
//! recovery and change nothing.
//!
//! Two questions have no flag that answers them, because only a person may
//! answer them: the grant of wider permissions that `up` and `restart` ask
//! for, and the trust of the proxy's certificate authority, which also
//! raises the system's password prompt. Their tests expect each to fail in
//! both runs with its own code and a recovery that a person runs at a
//! terminal. `mxv preflight` asks only at a terminal and never fails for
//! want of an answer, so it has no test here.

#![cfg(test)]

#[path = "support/git.rs"]
mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use clap::CommandFactory as _;
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
            .output()
            .unwrap();
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
        let output = self.with_variables(script(&words)).output().unwrap();
        // The terminal joins standard output and standard error, ends lines
        // with a carriage return, and echoes the end of input that `script`
        // passes on, so the document is the text between the first brace and
        // the last.
        let text = String::from_utf8_lossy(&output.stdout);
        let start = text.find('{');
        let end = text.rfind('}');
        let document = match (start, end) {
            (Some(start), Some(end)) if start < end => serde_json::from_str(&text[start..=end]),
            _ => serde_json::from_str(""),
        }
        .unwrap_or_else(|error| {
            panic!(
                "{args:?} at a terminal: {error}: {text} {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }
}

/// Returns a `script` command that runs `words` at a pseudo-terminal,
/// prints nothing of its own and exits with the code of the command.
#[cfg(target_os = "macos")]
fn script(words: &[String]) -> Command {
    Command::new("script")
        .args(["-q", "-e", "/dev/null"])
        .args(words)
}

/// Returns a `script` command that runs `words` at a pseudo-terminal,
/// prints nothing of its own and exits with the code of the command.
/// util-linux's `script` takes the command as one line of shell, so each
/// word is quoted.
#[cfg(not(target_os = "macos"))]
fn script(words: &[String]) -> Command {
    let line = words
        .iter()
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ");
    Command::new("script").args(["-q", "-e", "-c", &line, "/dev/null"])
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
/// with its names below the root joined by spaces.
fn with_yes(command: &clap::Command, above: &str, found: &mut BTreeSet<String>) {
    for sub in command.get_subcommands() {
        let path = if above.is_empty() {
            sub.get_name().to_owned()
        } else {
            format!("{above} {}", sub.get_name())
        };
        if sub.get_arguments().any(|arg| arg.get_long() == Some("yes")) {
            found.insert(path.clone());
        }
        with_yes(sub, &path, found);
    }
}

#[test]
fn every_command_with_yes_is_listed() {
    let mut command = mxv::Cli::command();
    command.build();
    let mut found = BTreeSet::new();
    with_yes(&command, "", &mut found);
    let listed: BTreeSet<String> = ANSWERED
        .iter()
        .map(|answered| answered.command.to_owned())
        .collect();
    assert_eq!(found, listed);
}

/// Runs the row of [`ANSWERED`] whose words are `args` from a pipe and with
/// `--no-input` at a terminal, and expects each run to ask for `--yes` and
/// change nothing.
fn asks_for_yes(args: &[&str]) {
    let answered = ANSWERED
        .iter()
        .find(|answered| answered.args == args)
        .unwrap_or_else(|| panic!("no row runs {args:?}"));
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

#[test]
fn remove_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["remove", "triage"]);
}

#[test]
fn remove_of_an_archive_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["remove", "--archived", "triage"]);
}

#[test]
fn archive_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["archive", "-w", "triage"]);
}

#[test]
fn restore_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["restore", "triage"]);
}

#[test]
fn sweep_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["sweep"]);
}

#[test]
fn clean_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["clean", "--all", "--idle", "0s"]);
}

#[test]
fn repair_of_a_name_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["repair", "triage"]);
}

#[test]
fn repair_of_orphans_asks_for_yes_without_a_terminal() {
    asks_for_yes(&["repair", "--orphans"]);
}

/// Runs `args` from a pipe and with `--no-input` at a terminal, and expects
/// each run to fail with `code` and a recovery that a person runs at a
/// terminal, because no flag answers the question.
fn refuses_for_a_person(setup: &Setup, args: &[&str], code: &str) {
    for (how, (exit, document)) in [
        ("from a pipe", setup.piped(args)),
        ("with --no-input at a terminal", setup.at_terminal(args)),
    ] {
        assert_eq!(exit, 1, "{args:?} {how}: {document}");
        assert_eq!(document["code"], code, "{args:?} {how}: {document}");
        assert_eq!(
            document["recovery"]["kind"], "terminal",
            "{args:?} {how}: {document}"
        );
    }
}

#[test]
fn up_and_restart_refuse_an_ungranted_unit_without_a_terminal() {
    let setup = Setup::new();
    write(
        &setup.checkout().join("mxv.toml"),
        "[processes.idle]\nargv = [\"sleep\", \"60\"]\n",
    );
    for verb in ["up", "restart"] {
        refuses_for_a_person(
            &setup,
            &[verb, "-w", "triage"],
            "mxv::permissions::not_granted",
        );
    }
}

#[test]
fn proxy_trust_refuses_without_a_terminal() {
    let setup = Setup::new();
    refuses_for_a_person(
        &setup,
        &["proxy", "trust"],
        "mxv::proxy::trust_needs_terminal",
    );
}
