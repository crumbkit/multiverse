use std::collections::BTreeMap;
use std::path::Path;

use clap::Parser as _;
use multiverse::{Manifest, MiseListing, MiseTaskName, TaskHome, TaskName};

use super::{Home, TaskFailure, TaskRef, names, resolve};
use crate::cli::{Cli, RunArgs, Verb};
use crate::failures::rendered;

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

#[test]
fn run_keeps_flags_after_the_task_for_the_task() {
    let args = parsed(&["run", "-w", "triage", "test", "--json", "-x"]);
    assert_eq!(args.task().map(String::as_str), Some("test"));
    assert_eq!(args.task_words(), ["--json", "-x"]);
    assert_eq!(args.workspace.as_deref(), Some("triage"));
}

#[test]
fn a_double_dash_before_the_task_leaves_the_task_named() {
    let args = parsed(&["run", "--", "test", "--all"]);
    assert_eq!(args.task().map(String::as_str), Some("test"));
    assert_eq!(args.task_words(), ["--all"]);
}

#[test]
fn run_takes_no_task_to_list_them() {
    assert_eq!(parsed(&["run"]).task(), None);
}

#[test]
fn a_declared_task_wins_over_a_mise_task_of_its_home_with_a_note() {
    let homes = [home(Some("app"), CHECK, &["check"])];
    let found = resolve("triage", &homes, "check").unwrap();
    let TaskRef::Declared(name, _) = &found.reference else {
        panic!("{:?}", found.reference);
    };
    assert_eq!(name, &TaskName::new("check").unwrap());
    assert!(found.note.is_some());
}

#[test]
fn a_name_that_only_mise_lists_may_hold_a_colon() {
    let homes = [home(Some("app"), "", &["lint:fix"])];
    let found = resolve("triage", &homes, "lint:fix").unwrap();
    assert_eq!(
        found.reference,
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

/// What each case of the test below gave when `TaskFailure` had a
/// hand-written `Failure` impl, recorded from that impl.
const TASK_FAILURES: &str = r#"mxv::task::unknown exits 1
  recovery Run(Call { target: Command { path: ["run"], args: [Value { id: "workspace", value: "triage" }] }, slot: None })
  field workspace = Text("triage")
  field task = Text("test")
  field names = List([Text("check"), Text("lint")])
mxv::task::ambiguous exits 1
  recovery Hint("choose a checkout with --repo, such as `mxv run -w triage --repo app test`")
  field workspace = Text("triage")
  field task = Text("test")
  field checkouts = List([Text("app"), Text("api")])
mxv::task::ambiguous exits 1
  recovery Hint("choose a checkout with --repo, such as `mxv run -w triage --repo CHECKOUT test`")
  field workspace = Text("triage")
  field task = Text("test")
  field checkouts = List([])
crumbkit::cli::usage exits 2
  recovery Hint("run the command without --json to read the task's output")
  field argument = Text("--json")
"#;

#[test]
fn task_failures_give_the_code_exit_recovery_and_fields_recorded_from_their_hand_written_impl() {
    let ambiguous = |checkouts: &[&str]| TaskFailure::Ambiguous {
        workspace: "triage".to_owned(),
        task: "test".to_owned(),
        checkouts: checkouts.iter().map(|&folder| folder.to_owned()).collect(),
    };
    let cases = [
        TaskFailure::Unknown {
            workspace: "triage".to_owned(),
            task: "test".to_owned(),
            names: vec!["check".to_owned(), "lint".to_owned()],
            mise_unasked: true,
        },
        ambiguous(&["app", "api"]),
        ambiguous(&[]),
        TaskFailure::JsonRefused,
    ];

    let text: String = cases.iter().map(|case| rendered(case)).collect();

    assert_eq!(text, TASK_FAILURES, "\n{text}");
}
