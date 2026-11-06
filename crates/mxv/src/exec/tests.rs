use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;

use clap::Parser as _;
use crumbkit::{Failure as _, Recovery};
use multiverse::ExecEnd;

use super::{
    EXEC_SANDBOXED_TERMINAL, EXEC_UNSTARTABLE, ExecFailure, PROCESS_NOT_RUNNING, ended, exit_number,
};
use crate::cli::{Cli, ExecArgs, Verb};

/// Parses `words` after `mxv` and returns the arguments of `exec`.
fn parsed(words: &[&str]) -> Result<ExecArgs, clap::Error> {
    let cli = Cli::try_parse_from(std::iter::once("mxv").chain(words.iter().copied()))?;
    match cli.verb {
        Verb::Exec(args) => Ok(args),
        other => panic!("parsed {other:?}"),
    }
}

#[test]
fn exec_takes_the_workspace_the_process_and_every_word_after_the_double_dash() {
    let args = parsed(&[
        "exec",
        "-w",
        "triage",
        "web",
        "--",
        "sh",
        "-c",
        "echo $PORT",
    ])
    .unwrap();

    assert_eq!(args.workspace.as_deref(), Some("triage"));
    assert_eq!(args.process, "web");
    assert_eq!(args.command, ["sh", "-c", "echo $PORT"]);
}

#[test]
fn exec_passes_the_commands_own_flags_after_the_double_dash_to_the_command() {
    let args = parsed(&["exec", "web", "--", "ls", "-la", "--json", "-w", "x"]).unwrap();

    assert_eq!(args.workspace, None);
    assert_eq!(args.command, ["ls", "-la", "--json", "-w", "x"]);
}

#[test]
fn exec_refuses_a_command_line_without_a_command_after_the_double_dash() {
    assert!(parsed(&["exec", "web"]).is_err());
    assert!(parsed(&["exec", "web", "--"]).is_err());
}

#[test]
fn exec_refuses_a_command_that_is_not_after_a_double_dash() {
    assert!(parsed(&["exec", "web", "env"]).is_err());
}

#[test]
fn mxv_exits_with_the_commands_exit_code() {
    assert_eq!(exit_number(ExitStatus::from_raw(0)), 0);
    assert_eq!(exit_number(ExitStatus::from_raw(3 << 8)), 3);
    assert_eq!(exit_number(ExitStatus::from_raw(255 << 8)), 255);
}

#[test]
fn mxv_exits_with_128_plus_the_signal_that_ended_the_command() {
    assert_eq!(exit_number(ExitStatus::from_raw(2)), 130);
    assert_eq!(exit_number(ExitStatus::from_raw(9)), 137);
    assert_eq!(exit_number(ExitStatus::from_raw(15)), 143);
}

#[test]
fn a_command_that_ran_ends_with_its_status() {
    let status = ExitStatus::from_raw(7 << 8);

    let end = ended("triage", "web", ExecEnd::Exited(status)).unwrap();

    assert_eq!(end, status);
}

#[test]
fn a_container_that_is_not_running_fails_with_not_running_and_offers_up() {
    let failure = ended("triage", "db", ExecEnd::NotRunning).unwrap_err();

    assert_eq!(failure.code(), &PROCESS_NOT_RUNNING);
    let Recovery::Run(call) = failure.recovery() else {
        panic!("{failure:?} offers no call");
    };
    assert_eq!(
        format!("{call:?}"),
        format!(
            "{:?}",
            crumbkit::Call::new(&["up"])
                .value("workspace", "triage")
                .value("processes", "db")
        )
    );
}

#[test]
fn a_command_that_cannot_start_fails_with_unstartable_and_phases_reason() {
    let end = ExecEnd::Unstartable {
        reason: "no such program".to_owned(),
    };

    let failure = ended("triage", "web", end).unwrap_err();

    assert_eq!(failure.code(), &EXEC_UNSTARTABLE);
    assert_eq!(
        failure.to_string(),
        "the command could not start in the context of web: no such program"
    );
}

#[test]
fn a_sandboxed_command_on_a_terminal_fails_with_sandboxed_terminal_and_says_how_to_redirect() {
    let failure = ended("triage", "writer", ExecEnd::NeedsTerminal).unwrap_err();

    assert_eq!(failure.code(), &EXEC_SANDBOXED_TERMINAL);
    let Recovery::Hint(hint) = failure.recovery() else {
        panic!("{failure:?} offers no hint");
    };
    assert!(
        hint.contains("mxv exec -w triage writer -- COMMAND </dev/null 2>&1 | cat"),
        "{hint}"
    );
    assert!(hint.contains("no permissions table"), "{hint}");
}

#[test]
fn json_is_refused_as_a_usage_failure() {
    assert_eq!(ExecFailure::JsonRefused.code(), &crumbkit::codes::CLI_USAGE);
}
