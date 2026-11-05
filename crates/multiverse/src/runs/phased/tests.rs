use phase::{
    AppName, AppOutcome, AppReport, Note, ProcessName, ProcessState as Phase, Report, RunnerKind,
    StatusRow,
};

use super::{notes_of, ready_of};
use crate::runs::ProcessNote;

fn row(state: Phase) -> StatusRow {
    StatusRow::new(
        AppName::try_from("api".to_owned()).unwrap(),
        ProcessName::try_from("main".to_owned()).unwrap(),
        RunnerKind::Host,
        None,
        state,
    )
}

#[test]
fn a_process_is_ready_once_phase_has_seen_it_ready_since_its_last_start() {
    assert_eq!(ready_of(&row(Phase::Ready)), Some(true));
    assert_eq!(ready_of(&row(Phase::Running)), Some(false));
    assert_eq!(ready_of(&row(Phase::Paused)), Some(false));
    assert_eq!(ready_of(&row(Phase::Starting)), Some(false));
    assert_eq!(ready_of(&row(Phase::Restarting)), Some(false));
}

#[test]
fn a_process_that_does_not_run_has_no_readiness() {
    assert_eq!(ready_of(&row(Phase::Stopped)), None);
    assert_eq!(ready_of(&row(Phase::Stopping)), None);
    assert_eq!(ready_of(&row(Phase::Completed)), None);
}

#[test]
fn the_notes_of_a_report_are_the_secret_notes_of_each_app() {
    let db = AppName::try_from("db".to_owned()).unwrap();
    let api = AppName::try_from("api".to_owned()).unwrap();
    let main = || ProcessName::try_from("main".to_owned()).unwrap();
    let report = Report::new(vec![
        AppReport::new(db.clone(), AppOutcome::AlreadyRunning).with_notes(vec![
            Note::secrets_rewritten(&db, main(), 2, 0),
            Note::not_ours(main()),
        ]),
        AppReport::new(api.clone(), AppOutcome::AlreadyRunning).with_notes(vec![
            Note::secrets_kept(&api, main(), "the source could not supply TOKEN"),
        ]),
        AppReport::new(
            AppName::try_from("web".to_owned()).unwrap(),
            AppOutcome::Started,
        ),
    ]);
    let notes = notes_of(&report);
    assert_eq!(
        notes.get("db"),
        Some(&vec![ProcessNote::SecretsRewritten {
            message: "rewrote 2 secret files of db/main".to_owned(),
        }])
    );
    assert_eq!(
        notes.get("api"),
        Some(&vec![ProcessNote::SecretsKept {
            message: "api/main still runs with its earlier secret files: the source could not supply TOKEN".to_owned(),
        }])
    );
    assert_eq!(notes.get("web"), None);
}

#[test]
fn a_restart_because_the_plan_changed_is_noted_with_phases_own_sentence() {
    let api = AppName::try_from("api".to_owned()).unwrap();
    let main = ProcessName::try_from("main".to_owned()).unwrap();
    let report = Report::new(vec![
        AppReport::new(api.clone(), AppOutcome::Started)
            .with_notes(vec![Note::plan_changed(&api, main)]),
    ]);

    let notes = notes_of(&report);

    let note = &notes["api"][0];
    assert!(!note.is_warning());
    assert_eq!(
        note,
        &ProcessNote::PlanChanged {
            message: "restarted api/main because its part of the plan changed".to_owned(),
        }
    );
}
