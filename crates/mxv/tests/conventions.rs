//! The convention tests of the `mxv` command, which hold its command tree,
//! guide, help pages, examples and failures to crumbkit's conventions with
//! crumbkit-test (CLI-6).
//!
//! They live in an integration test so that the example check can run the
//! built binary, whose path cargo sets only for integration tests.
//! `SNAPSHOTS=overwrite cargo test` writes, rewrites and deletes the golden
//! files.
//!
//! The workspace verbs are declared before they are built. Each one fails
//! with `mxv::command::unbuilt`, so its examples cannot run and a write verb
//! has no document yet. The opt-outs below name each verb and say so, and
//! the pull request that builds a verb removes its opt-outs. The opt-out
//! report in `tests/golden/opt-outs.txt` therefore lists the verbs that are
//! still to be built.

#![cfg(test)]

use clap::CommandFactory as _;
use crumbkit_test::{
    Allow, Budgets, Examples, Failing, Pages, PathBudgets, Recoveries, Rule, Twins,
};
use mxv::{CODES, Cli, GUIDE, UNBUILT};

/// The reason that an unbuilt verb's examples do not run.
const NO_RUN: &str = "The verb is declared before it is built and fails with mxv::command::unbuilt, so its examples have no output to pin until the pull request that builds it.";

/// The reason that an unbuilt write verb declares no document.
const NO_DOCUMENT: &str = "The verb is declared before it is built, and the pull request that builds it adds its document and the document's schema golden.";

/// The `mxv` command's opt-outs from crumbkit's conventions, which every
/// check below reads.
const ALLOW: &[Allow] = &[
    Allow::new(Rule::Examples, "new", NO_RUN),
    Allow::new(Rule::Examples, "list", NO_RUN),
    Allow::new(Rule::Examples, "show", NO_RUN),
    Allow::new(Rule::Examples, "cd", NO_RUN),
    Allow::new(Rule::Examples, "run", NO_RUN),
    Allow::new(Rule::Examples, "archive", NO_RUN),
    Allow::new(Rule::Examples, "restore", NO_RUN),
    Allow::new(Rule::Examples, "remove", NO_RUN),
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
    Allow::new(Rule::GuideDocument, "new", NO_DOCUMENT),
    Allow::new(Rule::GuideDocument, "archive", NO_DOCUMENT),
    Allow::new(Rule::GuideDocument, "restore", NO_DOCUMENT),
    Allow::new(Rule::GuideDocument, "remove", NO_DOCUMENT),
    Allow::new(
        Rule::Yes,
        "run",
        "run starts only the app that the person names and stops it on Ctrl-C, so it asks no question first.",
    ),
    Allow::untwinned(
        Failing::new(&UNBUILT, &["new"]),
        "Every unbuilt verb fails with this code whatever its arguments, so no single word of the command line causes it.",
    ),
];

/// The budgets of the `-h` pages, which are crumbkit's defaults.
const BUDGETS: Budgets = Budgets::new();

/// The budgets of the paths through the help, which are crumbkit's
/// defaults.
const PATH_BUDGETS: PathBudgets<'_> = PathBudgets::new("tests/golden/examples");

/// The built binary and the directory that holds the example golden files.
/// Each example runs in a fresh empty directory, because no command reads
/// a workspace yet.
const EXAMPLES: Examples<'_> =
    Examples::new(env!("CARGO_BIN_EXE_mxv"), "tests/golden/examples").allow(ALLOW);

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
    crumbkit_test::check_codes(&GUIDE, &[CODES]);
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
    crumbkit_test::check_examples(&Cli::command(), &GUIDE, &EXAMPLES);
}

#[test]
fn every_json_example_cuts_or_refuses_a_result_past_its_output_budget() {
    crumbkit_test::check_output_budget(&Cli::command(), &GUIDE, &EXAMPLES);
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

/// The command lines that fail, one for each declared code.
const STARTS: &[Failing<'static>] = &[Failing::new(&UNBUILT, &["new"])];

/// The settings of the recovery check.
const RECOVERIES: Recoveries<'_> = Recoveries::new(EXAMPLES, &[CODES]).starts(STARTS);

#[test]
fn every_recovery_leads_to_a_command_that_runs() {
    crumbkit_test::check_recoveries(&Cli::command(), &GUIDE, &RECOVERIES, "tests/golden");
}

/// The settings of the twin check. The one code has no twin, and `ALLOW`
/// says why.
const TWINS: Twins<'_> = Twins::new(EXAMPLES, &[CODES]);

#[test]
fn every_declared_code_has_a_twin_that_the_command_accepts() {
    crumbkit_test::check_twins(&Cli::command(), &GUIDE, &TWINS);
}
