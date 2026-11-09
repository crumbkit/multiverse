use std::collections::BTreeMap;
use std::path::Path;

use clap::Parser as _;
use multiverse::{Manifest, MiseListing, MiseTaskName, TaskHome, TaskName};

use super::{Found, Home, TaskFailure, TaskRef, names, resolve};
use crate::cli::{Cli, RunArgs, Verb};

/// Parses `words` after `mxv` and returns the arguments of `run`.
fn parsed(words: &[&str]) -> RunArgs {
    let cli = Cli::try_parse_from(std::iter::once("mxv").chain(words.iter().copied())).unwrap();
    match cli.verb {
        Verb::Run(args) => args,
        other => panic!("parsed {other:?}"),
    }
}

/// Returns the home of `checkout` whose manifest is `manifest` and whose
/// mise lists `listed`.
fn home(checkout: Option<&str>, manifest: &str, listed: &[&str]) -> Home {
    let tasks: BTreeMap<_, _> = Manifest::parse(manifest, Path::new("/repo/mxv.toml"))
        .unwrap()
        .tasks;
    Home {
        tasks: TaskHome::new(checkout.map(str::to_owned), "/repo".into(), tasks),
        mise: MiseListing::Listed(
            listed
                .iter()
                .map(|name| MiseTaskName::new(name).unwrap())
                .collect(),
        ),
    }
}

const CHECK: &str = "[tasks.check]\nargv = [\"true\"]\n";

fn reference(found: &Found<'_>) -> TaskRef {
    found.reference.clone()
}

#[test]
fn run_keeps_flags_after_the_task_for_the_task() {
    let args = parsed(&["run", "-w", "triage", "test", "--json", "-x"]);
    assert_eq!(args.task().map(String::as_str), Some("test"));
    assert_eq!(args.task_words(), ["--json", "-x"]);
    assert_eq!(args.workspace.as_deref(), Some("triage"));
}

#[test]
fn run_takes_no_task_to_list_them() {
    assert_eq!(parsed(&["run"]).task(), None);
}

#[test]
fn a_declared_task_wins_over_a_mise_task_of_its_home_with_a_note() {
    let homes = [home(Some("app"), CHECK, &["check"])];
    let found = resolve("triage", &homes, "check").unwrap();
    assert_eq!(
        reference(&found),
        TaskRef::Declared(TaskName::new("check").unwrap())
    );
    assert!(found.note.is_some());
}

#[test]
fn a_name_that_only_mise_lists_may_hold_a_colon() {
    let homes = [home(Some("app"), "", &["lint:fix"])];
    let found = resolve("triage", &homes, "lint:fix").unwrap();
    assert_eq!(
        reference(&found),
        TaskRef::Mise(MiseTaskName::new("lint:fix").unwrap())
    );
    assert_eq!(found.note, None);
}

#[test]
fn a_name_in_two_checkouts_is_refused_and_names_them() {
    let homes = [
        home(Some("api"), CHECK, &[]),
        home(Some("web"), "", &["check"]),
    ];
    let error = resolve("triage", &homes, "check").unwrap_err();
    assert!(matches!(
        error,
        TaskFailure::Ambiguous { checkouts, .. } if checkouts == ["api", "web"]
    ));
}

#[test]
fn a_task_of_the_workspace_manifest_wins_over_those_of_the_checkouts() {
    let homes = [home(None, CHECK, &[]), home(Some("api"), CHECK, &[])];
    let found = resolve("triage", &homes, "check").unwrap();
    assert_eq!(found.home.tasks.checkout, None);
}

#[test]
fn a_name_that_nothing_holds_is_unknown_and_lists_each_name_once() {
    let homes = [
        home(Some("api"), CHECK, &["lint"]),
        home(Some("web"), CHECK, &[]),
    ];
    assert_eq!(names(&homes), ["check", "lint"]);
    let error = resolve("triage", &homes, "other").unwrap_err();
    assert!(matches!(
        error,
        TaskFailure::Unknown { names, .. } if names == ["check", "lint"]
    ));
}

#[test]
fn a_mise_name_is_not_found_when_mise_refused() {
    let mut refused = home(Some("api"), "", &[]);
    refused.mise = MiseListing::Refused {
        message: "no".to_owned(),
    };
    assert!(matches!(
        resolve("triage", &[refused], "lint").unwrap_err(),
        TaskFailure::Unknown { .. }
    ));
}
