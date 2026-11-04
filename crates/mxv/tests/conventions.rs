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
//! with git: a bare `origin.git`, its clone `inbox`, a repository `plain`
//! without an origin, and a clone `fresh` of an origin without commits. Its
//! XDG state starts as a copy of `tests/fixtures/xdg`, which holds the
//! record of the workspace `reading-list`. That record names directories
//! under `/home/agent`, which do not exist, so the examples of `show`, `cd`
//! and `remove` read a record whose checkout is already gone. `new` runs
//! as a dry run, because `MXV_ROOT` names `/home/agent/mxv`, which a test
//! cannot create.
//!
//! `run`, `archive` and `restore` are declared before they are built. Each
//! one fails with `mxv::command::unbuilt`, so its examples cannot run and a
//! write verb has no document yet. The opt-outs below name each verb and
//! say so, and the pull request that builds a verb removes its opt-outs.

#![cfg(test)]

use clap::CommandFactory as _;
use crumbkit_test::{
    Allow, Budgets, Examples, Failing, Pages, PathBudgets, Recoveries, Rule, Twin, Twins,
    Unprovoked,
};
#[path = "support/git.rs"]
mod support;

use crumbkit::codes::CONFIRM_NEEDS_YES;
use mxv::{
    BRANCH_EXISTS, CHECKOUT_UNOWNED, CODES, Cli, DIRECTORY_TAKEN, ENVIRONMENT_NO_DIRECTORY,
    FILE_UNWRITABLE, GIT_FAILED, GIT_NOT_FOUND, GIT_TIMED_OUT, GUIDE, RECORD_CORRUPT, RECORD_NEWER,
    RECORD_UNREADABLE, REPOSITORY_EMPTY_ORIGIN, REPOSITORY_NO_DEFAULT_BRANCH, REPOSITORY_NO_ORIGIN,
    REPOSITORY_NOT_FOUND, UNBUILT, UNBUILT_CODES, WORKSPACE_BAD_NAME, WORKSPACE_DIRTY,
    WORKSPACE_EXISTS, WORKSPACE_NOT_FOUND,
};
use tempfile::TempDir;

use crate::support::{git, repository};

/// The reason that an unbuilt verb's examples do not run.
const NO_RUN: &str = "The verb is declared before it is built and fails with mxv::command::unbuilt, so its examples have no output to pin until the pull request that builds it.";

/// The reason that an unbuilt write verb declares no document.
const NO_DOCUMENT: &str = "The verb is declared before it is built, and the pull request that builds it adds its document and the document's schema golden.";

/// The `mxv` command's opt-outs from crumbkit's conventions, which every
/// check below reads.
const ALLOW: &[Allow] = &[
    Allow::new(Rule::Examples, "run", NO_RUN),
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
    Allow::new(
        Rule::Yes,
        "run",
        "run starts only the app that the person names and stops it on Ctrl-C, so it asks no question first.",
    ),
    Allow::untwinned(
        Failing::new(&UNBUILT, &["run"]),
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
];

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
/// The variables of every run: a `PATH` that finds git, git without the
/// person's configuration, and a workspace root that does not exist.
const ENV: &[(&str, &str)] = &[
    ("PATH", support::PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("MXV_ROOT", "/home/agent/mxv"),
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
/// The clone `inbox` also holds a branch named `taken`.
fn repositories() -> Repositories {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let inbox = repository(&root);
    git(&inbox, &["branch", "taken"]);
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
    crumbkit_test::check_grammar(&Cli::command(), Pages::rendered(&GUIDE), &["usage"], ALLOW);
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
    crumbkit_test::check_codes(&GUIDE, &[CODES, UNBUILT_CODES]);
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
    &["new", "reading-list", "--repo", "inbox", "--dry-run"],
);

/// `new` of a name with a slash.
const NEW_BAD_NAME: Failing<'static> = Failing::new(
    &WORKSPACE_BAD_NAME,
    &["new", "a/b", "--repo", "inbox", "--dry-run"],
);

/// `new` of the name of a branch that the repository already has.
const NEW_TAKEN_BRANCH: Failing<'static> = Failing::new(
    &BRANCH_EXISTS,
    &["new", "taken", "--repo", "inbox", "--dry-run"],
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

/// `new` without `MXV_ROOT` and without `HOME`.
const NEW_NO_ROOT: Failing<'static> = Failing::new(
    &ENVIRONMENT_NO_DIRECTORY,
    &["new", "triage", "--repo", "inbox", "--dry-run"],
)
.env(&[("MXV_ROOT", None)]);

/// `new` with a `PATH` that finds no git.
const NEW_NO_GIT: Failing<'static> = Failing::new(
    &GIT_NOT_FOUND,
    &["new", "triage", "--repo", "inbox", "--dry-run"],
)
.env(&[("PATH", Some("/nonexistent"))]);

/// The words of `new` that the fixture accepts, which each `new` line above
/// differs from in one word.
const NEW_TRIAGE: &[&str] = &["new", "triage", "--repo", "inbox", "--dry-run"];

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
    NEW_NO_ROOT,
    NEW_NO_GIT,
    NEWER_RECORD,
    CORRUPT_RECORD,
    Failing::new(&CONFIRM_NEEDS_YES, &["remove", "reading-list"]),
    Failing::new(&UNBUILT, &["run"]),
];

/// The declared codes that no command line in this test provokes, because
/// each needs a state that a fixed fixture cannot hold.
/// `tests/workspaces.rs` provokes the dirty refusal with a real checkout.
const UNPROVOKED: &[Unprovoked<'static>] = &[
    Unprovoked::new(
        &WORKSPACE_DIRTY,
        "Only a live checkout with changes provokes it, and the fixture's workspace has no checkout; tests/workspaces.rs provokes it.",
    ),
    Unprovoked::new(
        &DIRECTORY_TAKEN,
        "Only an existing directory under the workspace root provokes it, and the root of the examples is /home/agent/mxv, which a test cannot create.",
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
        "Only a git command that runs past its deadline of 30 or 120 seconds provokes it, which is longer than the check waits for a run.",
    ),
    Unprovoked::new(
        &RECORD_UNREADABLE,
        "Only a record directory without read permission provokes it, and the check's fixture copies keep the permissions readable.",
    ),
    Unprovoked::new(
        &FILE_UNWRITABLE,
        "Only a directory without write permission or a full disk provokes it, and the examples write nothing outside their fresh directories.",
    ),
];

#[test]
fn every_recovery_leads_to_a_command_that_runs() {
    let repositories = repositories();
    let recoveries = Recoveries::new(examples(&repositories.path), &[CODES, UNBUILT_CODES])
        .starts(STARTS)
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
    Twin::variable(NEW_NO_ROOT, "MXV_ROOT"),
    Twin::variable(NEW_NO_GIT, "PATH"),
];

#[test]
fn every_declared_code_has_a_twin_that_the_command_accepts() {
    let repositories = repositories();
    let twins = Twins::new(examples(&repositories.path), &[CODES, UNBUILT_CODES])
        .pairs(PAIRS)
        .unprovoked(UNPROVOKED);
    crumbkit_test::check_twins(&Cli::command(), &GUIDE, &twins);
}
