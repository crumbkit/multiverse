//! The convention tests of the `mxv` command, which hold its command tree,
//! guide, help pages, examples and failures to crumbkit's conventions with
//! crumbkit-test (CLI-6).
//!
//! They live in an integration test so that the example check can run the
//! built binary, whose path cargo sets only for integration tests.
//! `SNAPSHOTS=overwrite cargo test` writes, rewrites and deletes the golden
//! files.
//!
//! Each example runs in a fresh copy of a directory that the test builds
//! with git: a bare `origin.git`, its clone `app`, a repository `plain`
//! without an origin, and a clone `fresh` of an origin without commits. Its
//! XDG state starts as a copy of `tests/fixtures/xdg`, which holds the
//! record of the workspace `reading-list`. That record names directories
//! under `/home/agent`, which do not exist, so the examples of `show`, `cd`
//! and `remove` read a record whose checkout is already gone. `MXV_ROOT` is
//! unset, and the fixture's config file sets `root = "/home/agent/mxv"`, so
//! the examples of `new` and `show` print a root whose length is the same on
//! every machine. The default root under the temporary `XDG_DATA_HOME`
//! would change the width of the table's columns, because crumbkit-test
//! measures them before it replaces that path with its placeholder. `new`
//! runs as a dry run, so the examples fetch nothing.
//!
//! The process verbs `up`, `restart` and `logs` need a live checkout with
//! an `mxv.toml` and start real processes, so their examples do not run
//! here, and `tests/processes.rs` runs them instead. `down` and `status`
//! of the fixture's workspace run, because they read the run files, which
//! the fixture does not hold, and need no checkout.
//!
//! The refusals of `remove` need live checkouts, which a record under
//! `/home/agent` cannot name. The recovery and twin checks therefore also
//! start from [`live_workspaces`], which creates real workspaces with the
//! built binary in a temporary directory and an XDG fixture that holds
//! their records. Each chain of those checks reads the records from a copy
//! of that fixture, and acts on the real checkouts, so each refusal has a
//! workspace of its own.
//!
//! `archive` and `restore` are declared before they are built. Each
//! one fails with `mxv::command::unbuilt`, so its examples cannot run and a
//! write verb has no document yet. The opt-outs below name each verb and
//! say so, and the pull request that builds a verb removes its opt-outs.

#![cfg(test)]

use clap::CommandFactory as _;
use crumbkit_test::{
    Allow, Budgets, Examples, Failing, Pages, PathBudgets, Recoveries, Rule, RuntimeFailing, Twin,
    Twins, Unprovoked,
};
#[path = "support/git.rs"]
mod support;

use crumbkit::codes::CONFIRM_NEEDS_YES;
use mxv::{
    BRANCH_EXISTS, CHECKOUT_LOCKED, CHECKOUT_UNOWNED, CHECKOUT_UNREACHABLE_COMMITS, CODES,
    CONFIG_INVALID, CONFIG_UNREADABLE, Cli, DIRECTORY_TAKEN, DOCKER_FAILED, DOCKER_NOT_FOUND,
    DOCKER_UNAVAILABLE, ENVIRONMENT_NO_DIRECTORY, ENVIRONMENT_RELATIVE_ROOT, FILE_UNWRITABLE,
    GIT_FAILED, GIT_NOT_FOUND, GIT_TIMED_OUT, GUIDE, MANIFEST_INVALID, MANIFEST_NEWER,
    MANIFEST_NOT_FOUND, MANIFEST_RESERVED, MANIFEST_UNKNOWN_KEY, MANIFEST_UNREADABLE,
    PATH_INACCESSIBLE, PORT_EXHAUSTED, PORT_NOT_FOUND, PORT_TAKEN, PROCESS_EXITED,
    PROCESS_NOT_FOUND, PROCESS_NOT_READY, PROCESS_TABLE_UNREADABLE, PROCESS_UNSTARTABLE,
    PROCESS_UNSTOPPABLE, RECORD_CORRUPT, RECORD_MISMATCHED, RECORD_NEWER, RECORD_UNREADABLE,
    REPOSITORY_EMPTY_ORIGIN, REPOSITORY_NO_DEFAULT_BRANCH, REPOSITORY_NO_ORIGIN,
    REPOSITORY_NOT_FOUND, RUN_CODES, UNBUILT, UNBUILT_CODES, WORKSPACE_BAD_NAME, WORKSPACE_BUSY,
    WORKSPACE_CHANGED, WORKSPACE_DIRTY, WORKSPACE_EXISTS, WORKSPACE_IGNORED_FILES,
    WORKSPACE_NOT_CURRENT, WORKSPACE_NOT_FOUND, WORKSPACE_RUNNING, WORKSPACE_VOLUMES,
};
use tempfile::TempDir;

use crate::support::{git, repository};

/// The reason that an unbuilt verb's examples do not run.
const NO_RUN: &str = "The verb is declared before it is built and fails with mxv::command::unbuilt, so its examples have no output to pin until the pull request that builds it.";

/// The reason that a code of the container verbs is unprovoked here.
const DOCKER: &str = "Only a live checkout whose mxv.toml declares containers, with a docker daemon that answers, provokes it; tests/containers.rs provokes it when docker runs.";

/// The reason that a process verb's examples do not run.
const NO_LIVE_CHECKOUT: &str = "The example starts or reads the processes of a live checkout with an mxv.toml, and the fixture's workspace names a checkout under /home/agent, which a test cannot create; tests/processes.rs runs the verb.";

/// The reason that `down` and `restart` ask no question.
const NO_QUESTION: &str = "The command stops only processes that mxv up started for the named workspace, and it deletes no file that the person wrote, so it asks no question first.";

/// The reason that an unbuilt write verb declares no document.
const NO_DOCUMENT: &str = "The verb is declared before it is built, and the pull request that builds it adds its document and the document's schema golden.";

/// The `mxv` command's opt-outs from crumbkit's conventions, which every
/// check below reads.
const ALLOW: &[Allow] = &[
    Allow::new(Rule::Examples, "up", NO_LIVE_CHECKOUT),
    Allow::new(Rule::Examples, "restart", NO_LIVE_CHECKOUT),
    Allow::new(Rule::Examples, "logs", NO_LIVE_CHECKOUT),
    Allow::new(Rule::Examples, "archive", NO_RUN),
    Allow::new(Rule::Examples, "restore", NO_RUN),
    Allow::new(
        Rule::Examples,
        "self usage",
        "The output names the day the report starts, which changes every day, and tests/usage.rs checks the usage log instead.",
    ),
    Allow::new(
        Rule::Examples,
        "self figures list",
        "The figures come from the usage log of the machine that runs the test, and tests/usage.rs checks the usage log instead.",
    ),
    Allow::new(Rule::GuideDocument, "archive", NO_DOCUMENT),
    Allow::new(Rule::GuideDocument, "restore", NO_DOCUMENT),
    Allow::new(Rule::Yes, "down", NO_QUESTION),
    Allow::new(Rule::Yes, "restart", NO_QUESTION),
    Allow::new(
        Rule::Yes,
        "ports reset",
        "The command replaces only host ports that mxv allocated, refuses while the workspace's processes run, and deletes no file that the person wrote, so it asks no question first.",
    ),
    Allow::new(Rule::Examples, "ports reset", NO_LIVE_CHECKOUT),
    Allow::untwinned(
        Failing::new(&UNBUILT, &["archive"]),
        "Every unbuilt verb fails with this code whatever its arguments, so no single word of the command line causes it.",
    ),
    Allow::untwinned(
        NEWER_RECORD,
        "The record that a newer mxv wrote causes the failure, and every word of the command line that names it reads the same record.",
    ),
    Allow::untwinned(
        CORRUPT_RECORD,
        "The corrupt record causes the failure, and every word of the command line that names it reads the same record.",
    ),
    Allow::untwinned(
        MISMATCHED_RECORD,
        "The record that holds another name causes the failure, and every word of the command line that names it reads the same record.",
    ),
    Allow::untwinned(
        BAD_CONFIG,
        "The config file causes the failure, and no word of the command line names the config file.",
    ),
    Allow::untwinned(
        UNREADABLE_CONFIG,
        "The directory at the config file's path causes the failure, and no word of the command line names the config file.",
    ),
];

/// `show` of a record that holds another name than its file.
const MISMATCHED_RECORD: Failing<'static> =
    Failing::new(&RECORD_MISMATCHED, &["show", "reading-list"])
        .xdg("tests/fixtures/xdg-mismatched");

/// `new` with a config file that sets an unknown key.
const BAD_CONFIG: Failing<'static> = Failing::new(
    &CONFIG_INVALID,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.xdg("tests/fixtures/xdg-bad-config");

/// `new` with a directory where the config file belongs.
const UNREADABLE_CONFIG: Failing<'static> = Failing::new(
    &CONFIG_UNREADABLE,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.xdg("tests/fixtures/xdg-config-directory");

/// `show` of a record that a newer mxv wrote.
const NEWER_RECORD: Failing<'static> =
    Failing::new(&RECORD_NEWER, &["show", "reading-list"]).xdg("tests/fixtures/xdg-newer");

/// `show` of a record that is not JSON.
const CORRUPT_RECORD: Failing<'static> =
    Failing::new(&RECORD_CORRUPT, &["show", "reading-list"]).xdg("tests/fixtures/xdg-corrupt");

/// The budgets of the `-h` pages, which are crumbkit's defaults.
const BUDGETS: Budgets = Budgets::new();

/// The budgets of the paths through the help, which are crumbkit's
/// defaults.
const PATH_BUDGETS: PathBudgets<'_> = PathBudgets::new("tests/golden/examples");
/// The variables of every run: a `PATH` that finds git, and git without
/// the person's configuration. `MXV_ROOT` is unset, so the workspace root
/// comes from the config file of `tests/fixtures/xdg`.
const ENV: &[(&str, &str)] = &[
    ("PATH", support::PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// The directory of repositories that each run starts in a fresh copy of,
/// which the test deletes when it drops.
struct Repositories {
    /// The temporary directory.
    _dir: TempDir,
    /// The directory's path, which [`Examples`] borrows.
    path: String,
}

/// Returns a new directory of the repositories that the module docs name.
/// The clone `app` also holds a branch named `taken`.
fn repositories() -> Repositories {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let app = repository(&root);
    git(&app, &["branch", "taken"]);
    let plain = root.join("plain");
    std::fs::create_dir(&plain).unwrap();
    git(&plain, &["init", "--quiet"]);
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "empty.git",
        ],
    );
    git(&root, &["clone", "--quiet", "empty.git", "fresh"]);
    git(
        &root.join("fresh"),
        &["remote", "set-url", "origin", "../empty.git"],
    );
    let path = root.to_str().unwrap().to_owned();
    Repositories { _dir: dir, path }
}

/// Returns the settings that run the built binary in a fresh copy of the
/// repositories at `path`, with the XDG state of `tests/fixtures/xdg`.
fn examples(path: &str) -> Examples<'_> {
    Examples::new(env!("CARGO_BIN_EXE_mxv"), "tests/golden/examples")
        .env(ENV)
        .xdg("tests/fixtures/xdg")
        .fresh_dir(path)
        .allow(ALLOW)
}

#[test]
fn every_command_and_argument_has_one_name() {
    crumbkit_test::check_names(&Cli::command(), Pages::rendered(&GUIDE), ALLOW);
}

#[test]
fn every_command_is_a_root_verb_or_a_resource_followed_by_its_verb() {
    crumbkit_test::check_grammar(
        &Cli::command(),
        Pages::rendered(&GUIDE),
        &["usage", "reset"],
        ALLOW,
    );
}

#[test]
fn every_command_declares_its_effects() {
    crumbkit_test::check_effects(&Cli::command(), &GUIDE, ALLOW);
}

#[test]
fn every_list_command_pages_and_every_show_command_takes_up_to_100_names() {
    crumbkit_test::check_listings(&Cli::command(), ALLOW);
}

#[test]
fn every_option_with_many_values_accepts_repeats_and_commas() {
    crumbkit_test::check_filters(&Cli::command(), ALLOW);
}

#[test]
fn every_command_leaf_and_topic_meets_the_guide_rules() {
    crumbkit_test::check_guide(&Cli::command(), &GUIDE, ALLOW);
}

#[test]
fn every_exit_code_meets_the_exit_code_rules() {
    crumbkit_test::check_exit_codes(&Cli::command(), &GUIDE, ALLOW);
}

#[test]
fn every_failure_code_is_declared_once_and_spelled_by_the_rules() {
    crumbkit_test::check_codes(&GUIDE, &[CODES, RUN_CODES, UNBUILT_CODES]);
}

#[test]
fn every_help_page_fits_its_size_budget() {
    crumbkit_test::check_help_sizes(&Cli::command(), Pages::rendered(&GUIDE), &BUDGETS);
}

#[test]
fn every_path_through_the_help_fits_its_weight_budget() {
    crumbkit_test::check_path_weights(&Cli::command(), Pages::rendered(&GUIDE), &PATH_BUDGETS);
}

#[test]
fn every_help_page_matches_its_golden_file() {
    crumbkit_test::check_help_goldens(
        &Cli::command(),
        Pages::rendered(&GUIDE),
        "tests/golden/help",
    );
}

#[test]
fn every_document_schema_matches_its_golden_file() {
    crumbkit_test::check_schema_goldens(&Cli::command(), &GUIDE, "tests/golden/schemas");
}

#[test]
fn every_document_is_named_after_the_path_of_the_command_that_prints_it() {
    crumbkit_test::check_document_ids(&Cli::command(), &GUIDE, ALLOW);
}

#[test]
fn every_crumbkit_command_sits_at_its_fixed_path() {
    crumbkit_test::check_mounts(&Cli::command(), &GUIDE);
}

#[test]
fn every_example_in_the_guide_runs_and_prints_its_golden_output() {
    let repositories = repositories();
    crumbkit_test::check_examples(&Cli::command(), &GUIDE, &examples(&repositories.path));
}

#[test]
fn every_json_example_cuts_or_refuses_a_result_past_its_output_budget() {
    let repositories = repositories();
    crumbkit_test::check_output_budget(&Cli::command(), &GUIDE, &examples(&repositories.path));
}

#[test]
fn every_command_matches_the_checked_in_command_reference() {
    crumbkit_test::check_command_reference(env!("CARGO_BIN_EXE_mxv"), "docs/commands.md");
}

#[test]
fn every_opt_out_matches_the_opt_out_report() {
    crumbkit_test::check_opt_outs(
        &Cli::command(),
        &GUIDE,
        ALLOW,
        &BUDGETS,
        Some(&PATH_BUDGETS),
        "tests/golden",
    );
}

/// `cd` of a name that no workspace has.
const CD_MISSING: Failing<'static> = Failing::new(&WORKSPACE_NOT_FOUND, &["cd", "missing"]);

/// `new` of the name of the workspace in the fixture.
const NEW_TAKEN_NAME: Failing<'static> = Failing::new(
    &WORKSPACE_EXISTS,
    &["new", "reading-list", "--repo", "app", "--dry-run"],
);

/// `new` of a name with a slash.
const NEW_BAD_NAME: Failing<'static> = Failing::new(
    &WORKSPACE_BAD_NAME,
    &["new", "a/b", "--repo", "app", "--dry-run"],
);

/// `new` of the name of a branch that the repository already has.
const NEW_TAKEN_BRANCH: Failing<'static> = Failing::new(
    &BRANCH_EXISTS,
    &["new", "taken", "--repo", "app", "--dry-run"],
);

/// `new` of a path that does not exist.
const NEW_MISSING_REPOSITORY: Failing<'static> = Failing::new(
    &REPOSITORY_NOT_FOUND,
    &["new", "triage", "--repo", "missing", "--dry-run"],
);

/// `new` of a repository without an origin.
const NEW_NO_ORIGIN: Failing<'static> = Failing::new(
    &REPOSITORY_NO_ORIGIN,
    &["new", "triage", "--repo", "plain", "--dry-run"],
);

/// `new` of a repository whose origin has no commits.
const NEW_EMPTY_ORIGIN: Failing<'static> = Failing::new(
    &REPOSITORY_EMPTY_ORIGIN,
    &["new", "triage", "--repo", "fresh", "--dry-run"],
);

/// `new` with a relative `MXV_ROOT`.
const NEW_RELATIVE_ROOT: Failing<'static> = Failing::new(
    &ENVIRONMENT_RELATIVE_ROOT,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.env(&[("MXV_ROOT", Some("work"))]);

/// `new` of a name whose directory already exists under the default root.
const NEW_TAKEN_DIRECTORY: Failing<'static> = Failing::new(
    &DIRECTORY_TAKEN,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.xdg("tests/fixtures/xdg-taken");

/// `new` with a `PATH` that finds no git.
const NEW_NO_GIT: Failing<'static> = Failing::new(
    &GIT_NOT_FOUND,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.env(&[("PATH", Some("/nonexistent"))]);

/// `status` outside every workspace, without a name.
const STATUS_OUTSIDE: Failing<'static> = Failing::new(&WORKSPACE_NOT_CURRENT, &["status"]);

/// `status` of a name that no workspace has.
const STATUS_MISSING: Failing<'static> =
    Failing::new(&WORKSPACE_NOT_FOUND, &["status", "-w", "missing"]);

/// The words of `new` that the fixture accepts, which each `new` line above
/// differs from in one word.
const NEW_TRIAGE: &[&str] = &["new", "triage", "--repo", "app", "--dry-run"];

/// The command lines that start the recovery chains, one for each code that
/// a command line provokes.
const STARTS: &[Failing<'static>] = &[
    CD_MISSING,
    NEW_TAKEN_NAME,
    NEW_BAD_NAME,
    NEW_TAKEN_BRANCH,
    NEW_MISSING_REPOSITORY,
    NEW_NO_ORIGIN,
    NEW_EMPTY_ORIGIN,
    NEW_RELATIVE_ROOT,
    NEW_TAKEN_DIRECTORY,
    NEW_NO_GIT,
    NEWER_RECORD,
    CORRUPT_RECORD,
    MISMATCHED_RECORD,
    BAD_CONFIG,
    UNREADABLE_CONFIG,
    Failing::new(&CONFIRM_NEEDS_YES, &["remove", "reading-list"]),
    Failing::new(&UNBUILT, &["archive"]),
    STATUS_OUTSIDE,
    STATUS_MISSING,
];

/// The declared codes that no command line in this test provokes, because
/// each needs a state that a fixed fixture cannot hold.
const UNPROVOKED: &[Unprovoked<'static>] = &[
    Unprovoked::new(
        &WORKSPACE_CHANGED,
        "Only a workspace that changes while mxv remove waits for the person's answer provokes it, and the check's runs have no terminal to wait at; the library's workspace tests provoke the change.",
    ),
    Unprovoked::new(
        &PATH_INACCESSIBLE,
        "Only a workspace path below a directory without search permission provokes it, and the check's fixture copies keep the permissions searchable; the library's workspace tests provoke it.",
    ),
    Unprovoked::new(
        &ENVIRONMENT_NO_DIRECTORY,
        "Only a run in which mxv finds no home directory provokes it, and etcetera reads the home directory from the user database when HOME is unset, so every test run has one.",
    ),
    Unprovoked::new(
        &CHECKOUT_UNOWNED,
        "Only a checkout directory that git does not list provokes it, and the fixture's workspace has no checkout directory.",
    ),
    Unprovoked::new(
        &REPOSITORY_NO_DEFAULT_BRANCH,
        "Only a clone whose origin has branches but no default branch provokes it; the library's git tests provoke it.",
    ),
    Unprovoked::new(
        &GIT_FAILED,
        "Only a git command that fails in a working repository provokes it, such as a fetch from an origin that is gone; the library's git tests provoke it.",
    ),
    Unprovoked::new(
        &GIT_TIMED_OUT,
        "Only a git command that runs past its deadline of 30, 120 or 600 seconds provokes it, which is longer than the check waits for a run.",
    ),
    Unprovoked::new(
        &RECORD_UNREADABLE,
        "Only a record directory without read permission provokes it, and the check's fixture copies keep the permissions readable.",
    ),
    Unprovoked::new(
        &FILE_UNWRITABLE,
        "Only a directory without write permission or a full disk provokes it, and the examples write nothing outside their fresh directories.",
    ),
    Unprovoked::new(&WORKSPACE_RUNNING, LIVE),
    Unprovoked::new(&WORKSPACE_VOLUMES, DOCKER),
    Unprovoked::new(
        &DOCKER_NOT_FOUND,
        "Only a live checkout whose mxv.toml declares containers, on a PATH without docker, provokes it; tests/containers.rs provokes it.",
    ),
    Unprovoked::new(
        &DOCKER_UNAVAILABLE,
        "Only a docker daemon that does not answer provokes it, which the fixture cannot hold; tests/containers.rs provokes it with a docker that cannot reach its daemon.",
    ),
    Unprovoked::new(&DOCKER_FAILED, DOCKER),
    Unprovoked::new(&MANIFEST_NOT_FOUND, LIVE),
    Unprovoked::new(&MANIFEST_UNKNOWN_KEY, LIVE),
    Unprovoked::new(&MANIFEST_NEWER, LIVE),
    Unprovoked::new(&PORT_NOT_FOUND, LIVE),
    Unprovoked::new(&MANIFEST_RESERVED, LIVE),
    Unprovoked::new(&MANIFEST_INVALID, LIVE),
    Unprovoked::new(
        &MANIFEST_UNREADABLE,
        "Only an mxv.toml without read permission provokes it, in a live checkout that the fixture cannot hold.",
    ),
    Unprovoked::new(&PROCESS_NOT_FOUND, LIVE),
    Unprovoked::new(&PROCESS_EXITED, LIVE),
    Unprovoked::new(&PROCESS_NOT_READY, LIVE),
    Unprovoked::new(
        &PROCESS_UNSTOPPABLE,
        "Only a process group that outlives SIGKILL provokes it, such as a process stuck in the kernel, which a test cannot make on demand.",
    ),
    Unprovoked::new(&PORT_TAKEN, LIVE),
    Unprovoked::new(
        &PROCESS_UNSTARTABLE,
        "Only a manifest process whose cwd is missing from a live checkout provokes it; the library's process tests provoke it.",
    ),
    Unprovoked::new(
        &PROCESS_TABLE_UNREADABLE,
        "Only a PATH without ps, during a run that has started processes, provokes it, which the fixture cannot hold.",
    ),
    Unprovoked::new(
        &PORT_EXHAUSTED,
        "Only a port range whose every port is held or taken provokes it.",
    ),
    Unprovoked::new(
        &WORKSPACE_BUSY,
        "Only a second mxv command that runs while the first holds the workspace's lock provokes it; tests/processes.rs provokes it.",
    ),
];

/// The reason that a code of the process verbs is unprovoked here.
const LIVE: &str = "Only a live checkout with an mxv.toml provokes it, and the fixture's workspace names a checkout under /home/agent, which a test cannot create; tests/processes.rs provokes it.";
/// The variables of the runs that create the live workspaces, beside the
/// XDG directories: git without the person's configuration, with an author
/// for the commits.
const LIVE_ENV: &[(&str, &str)] = &[
    ("PATH", support::PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
];

/// Real workspaces of the clone `app` in a temporary directory, which
/// [`live_workspaces`] creates, and the XDG fixture that holds their
/// records.
struct Live {
    /// The temporary directory, which the test deletes when it drops.
    _dir: TempDir,
    /// The XDG fixture, whose `state` directory holds the records.
    xdg: std::path::PathBuf,
}

/// Returns real workspaces, each in the state that one refusal of
/// `remove` needs: `changed` has an untracked file, `secret` an ignored
/// `.env`, `detached` a commit on a detached `HEAD`, `locked` a checkout
/// that git lists as locked, and `clean` nothing that stops a removal.
fn live_workspaces() -> Live {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let app = repository(&root);
    write_text(&app.join(".git/info/exclude"), ".env\n");
    let xdg = root.join("xdg");
    for name in ["changed", "secret", "detached", "locked", "clean"] {
        #[expect(
            clippy::disallowed_methods,
            reason = "the fixture runs the built binary to create real workspaces"
        )]
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(["new", name, "--repo"])
            .arg(&app)
            .env_clear()
            .envs(LIVE_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mxv new {name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let checkout = |name: &str| root.join("workspaces").join(name).join("app");
    write_text(&checkout("changed").join("notes.md"), "notes");
    write_text(&checkout("secret").join(".env"), "TOKEN=1");
    git(&checkout("detached"), &["switch", "--quiet", "--detach"]);
    git(
        &checkout("detached"),
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    git(
        &app,
        &[
            "worktree",
            "lock",
            "--reason",
            "in use",
            checkout("locked").to_str().unwrap(),
        ],
    );
    Live { _dir: dir, xdg }
}

/// Writes `text` to the file at `path`, as a person editing a file by hand
/// would.
fn write_text(path: &std::path::Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture writes a file that a person would write by hand"
    )]
    std::fs::write(path, text).unwrap();
}

impl Live {
    /// Returns the dry run of `remove` that fails with `code` on the
    /// workspace `name`, with the records of the live workspaces.
    fn line(&self, code: crumbkit::Code, name: &str) -> RuntimeFailing {
        RuntimeFailing::new(code, ["remove", name, "--dry-run"]).xdg(&self.xdg)
    }

    /// Returns the refusals of `remove` that the live workspaces provoke.
    fn refusals(&self) -> Vec<RuntimeFailing> {
        vec![
            self.line(WORKSPACE_DIRTY, "changed"),
            self.line(WORKSPACE_IGNORED_FILES, "secret"),
            self.line(CHECKOUT_UNREACHABLE_COMMITS, "detached"),
            self.line(CHECKOUT_LOCKED, "locked"),
        ]
    }
}

#[test]
fn every_recovery_leads_to_a_command_that_runs() {
    let repositories = repositories();
    let live = live_workspaces();
    let mut starts = STARTS.to_vec();
    starts.extend(
        live.refusals()
            .into_iter()
            .map(RuntimeFailing::into_failing),
    );
    let recoveries = Recoveries::new(
        examples(&repositories.path),
        &[CODES, RUN_CODES, UNBUILT_CODES],
    )
    .starts(&starts)
    .unprovoked(UNPROVOKED);
    crumbkit_test::check_recoveries(&Cli::command(), &GUIDE, &recoveries, "tests/golden");
}

/// One twin for each declared code that a command line provokes. Each twin
/// differs from its refused line in one word or one variable (TEST-1).
const PAIRS: &[Twin<'static>] = &[
    Twin::new(CD_MISSING, &["cd", "reading-list"]),
    Twin::new(NEW_TAKEN_NAME, NEW_TRIAGE),
    Twin::new(NEW_BAD_NAME, NEW_TRIAGE),
    Twin::new(NEW_TAKEN_BRANCH, NEW_TRIAGE),
    Twin::new(NEW_MISSING_REPOSITORY, NEW_TRIAGE),
    Twin::new(NEW_NO_ORIGIN, NEW_TRIAGE),
    Twin::new(NEW_EMPTY_ORIGIN, NEW_TRIAGE),
    Twin::variable(NEW_RELATIVE_ROOT, "MXV_ROOT"),
    Twin::new(
        NEW_TAKEN_DIRECTORY,
        &["new", "sorting", "--repo", "app", "--dry-run"],
    ),
    Twin::variable(NEW_NO_GIT, "PATH"),
    Twin::new(STATUS_OUTSIDE, &["status", "--workspace=reading-list"]),
];

#[test]
fn every_declared_code_has_a_twin_that_the_command_accepts() {
    let repositories = repositories();
    let live = live_workspaces();
    let mut pairs = PAIRS.to_vec();
    pairs.extend(
        live.refusals()
            .into_iter()
            .map(|refused| refused.twin(["remove", "clean", "--dry-run"])),
    );
    let twins = Twins::new(
        examples(&repositories.path),
        &[CODES, RUN_CODES, UNBUILT_CODES],
    )
    .pairs(&pairs)
    .unprovoked(UNPROVOKED);
    crumbkit_test::check_twins(&Cli::command(), &GUIDE, &twins);
}
