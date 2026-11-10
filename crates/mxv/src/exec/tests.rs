use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use clap::Parser as _;
use crumbkit::{Failure as _, Recovery};
use multiverse::{ExecEnd, TaskHome};

use super::{
    EXEC_SANDBOXED_TERMINAL, EXEC_UNSTARTABLE, ExecFailure, PROCESS_NOT_RUNNING, Place, ended,
    exit_number, place_of,
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
    assert_eq!(args.process.as_deref(), Some("web"));
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

#[test]
fn exec_takes_no_unit_and_every_word_after_the_double_dash() {
    let args = parsed(&["exec", "-w", "triage", "--", "cargo", "test", "--all"]).unwrap();

    assert_eq!(args.process, None);
    assert_eq!(args.command, ["cargo", "test", "--all"]);
    assert_eq!(args.repo, None);
    assert!(!args.no_gate);
}

#[test]
fn exec_with_no_unit_takes_a_checkout_and_no_gate() {
    let args = parsed(&["exec", "--repo", "api", "--no-gate", "--", "env"]).unwrap();

    assert_eq!(args.repo.as_deref(), Some("api"));
    assert!(args.no_gate);
}

#[test]
fn exec_with_no_unit_refuses_a_command_line_without_a_command() {
    assert!(parsed(&["exec"]).is_err());
    assert!(parsed(&["exec", "--"]).is_err());
}

#[test]
fn exec_refuses_repo_and_no_gate_beside_a_unit() {
    for words in [
        &["exec", "--repo", "api", "web", "--", "env"][..],
        &["exec", "--no-gate", "web", "--", "env"][..],
    ] {
        let error = parsed(words).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{words:?}"
        );
    }
}

#[test]
fn a_missing_program_with_no_unit_says_it_could_not_start_in_the_workspace() {
    let failure = ExecFailure::UnstartableInWorkspace {
        workspace: "triage".to_owned(),
        reason: "no such program".to_owned(),
    };

    assert_eq!(failure.code(), &EXEC_UNSTARTABLE);
    assert_eq!(
        failure.to_string(),
        "the command could not start in the workspace triage: no such program"
    );
}

/// Returns the home of the checkout `name` at `root`, whose port `web` is
/// named in the merged manifest with the checkout's prefix.
fn checkout_home(name: &str, root: &Path) -> TaskHome {
    let mut home = TaskHome::new(Some(name.to_owned()), root.to_path_buf(), BTreeMap::new());
    home.own_ports = BTreeMap::from([("web".to_owned(), format!("{name}-web"))]);
    home
}

/// A workspace directory that holds the checkouts `app` and `second`, and
/// the directory `app/sub`.
fn two_checkouts() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap().join("multi");
    fs::create_dir_all(workspace.join("app/sub")).unwrap();
    fs::create_dir_all(workspace.join("second")).unwrap();
    (dir, workspace)
}

#[test]
fn a_directory_inside_a_checkout_runs_there_with_that_checkouts_short_ports() {
    let (_dir, workspace) = two_checkouts();
    let app = workspace.join("app");
    let second = workspace.join("second");
    let homes = [checkout_home("app", &app), checkout_home("second", &second)];

    let place = place_of(&homes, &workspace, Some(&app.join("sub")));

    assert_eq!(
        place,
        Place {
            root: app,
            dir: PathBuf::from("sub"),
            own_ports: BTreeMap::from([("web".to_owned(), "app-web".to_owned())]),
        }
    );
}

#[test]
fn a_checkout_whose_name_starts_with_another_checkouts_name_is_told_apart() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap().join("multi");
    fs::create_dir_all(workspace.join("app")).unwrap();
    fs::create_dir_all(workspace.join("app2/sub")).unwrap();
    let app2 = workspace.join("app2");
    let homes = [
        checkout_home("app", &workspace.join("app")),
        checkout_home("app2", &app2),
    ];

    let place = place_of(&homes, &workspace, Some(&app2.join("sub")));

    assert_eq!(place.root, app2);
    assert_eq!(place.dir, PathBuf::from("sub"));
}

#[test]
fn the_top_of_a_checkout_runs_in_the_directory_dot() {
    let (_dir, workspace) = two_checkouts();
    let app = workspace.join("app");
    let homes = [
        checkout_home("app", &app),
        checkout_home("second", &workspace.join("second")),
    ];

    let place = place_of(&homes, &workspace, Some(&app));

    assert_eq!(place.dir, PathBuf::from("."));
}

#[test]
fn a_current_directory_reached_through_a_symlink_counts_as_inside_the_checkout() {
    let (dir, workspace) = two_checkouts();
    let link = dir.path().canonicalize().unwrap().join("link");
    std::os::unix::fs::symlink(&workspace, &link).unwrap();
    let recorded = link.join("app");
    let homes = [
        checkout_home("app", &recorded),
        checkout_home("second", &link.join("second")),
    ];

    let place = place_of(&homes, &link, Some(&workspace.join("app/sub")));

    assert_eq!(place.root, recorded);
    assert_eq!(place.dir, PathBuf::from("sub"));
}

#[test]
fn a_directory_outside_with_one_checkout_runs_at_the_top_of_it() {
    let (dir, workspace) = two_checkouts();
    let app = workspace.join("app");
    let homes = [TaskHome::new(
        Some("app".to_owned()),
        app.clone(),
        BTreeMap::new(),
    )];

    let place = place_of(&homes, &workspace, Some(dir.path()));

    assert_eq!(place.root, app);
    assert_eq!(place.dir, PathBuf::from("."));
}

#[test]
fn a_directory_outside_with_several_checkouts_runs_in_the_workspace_directory() {
    let (dir, workspace) = two_checkouts();
    let homes = [
        checkout_home("app", &workspace.join("app")),
        checkout_home("second", &workspace.join("second")),
    ];

    let place = place_of(&homes, &workspace, Some(dir.path()));

    assert_eq!(place.root, workspace);
    assert_eq!(place.dir, PathBuf::from("."));
    assert!(place.own_ports.is_empty());
}

#[test]
fn a_workspace_manifest_does_not_count_the_workspace_directory_as_a_checkout() {
    let (_dir, workspace) = two_checkouts();
    let shared = TaskHome::new(None, workspace.clone(), BTreeMap::new());
    let homes = [
        shared,
        checkout_home("app", &workspace.join("app")),
        checkout_home("second", &workspace.join("second")),
    ];

    let place = place_of(&homes, &workspace, Some(&workspace));

    assert_eq!(place.root, workspace);
    assert!(place.own_ports.is_empty());
}

#[test]
fn a_current_directory_that_cannot_be_read_counts_as_outside_the_workspace() {
    let (_dir, workspace) = two_checkouts();
    let homes = [
        checkout_home("app", &workspace.join("app")),
        checkout_home("second", &workspace.join("second")),
    ];

    let gone = workspace.join("app/gone");
    assert_eq!(place_of(&homes, &workspace, Some(&gone)).root, workspace);
    assert_eq!(place_of(&homes, &workspace, None).root, workspace);
}
