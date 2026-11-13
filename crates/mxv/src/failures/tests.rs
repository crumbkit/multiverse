use crumbkit::{Call, Failure as _, Recovery};

use super::{MxvFailure, RESTORE_CHANGED, WORKSPACE_NOT_FOUND};

#[test]
fn a_record_that_is_gone_at_a_write_reads_as_a_workspace_that_is_not_found() {
    let failure = MxvFailure::from(multiverse::Error::RecordMissing {
        name: "triage".parse().unwrap(),
    });

    assert_eq!(failure.code(), &WORKSPACE_NOT_FOUND);
    assert_eq!(failure.to_string(), "no workspace is named triage");
    let Recovery::Run(call) = failure.recovery() else {
        panic!("{failure:?} offers no call");
    };
    let expected = Call::new(&["list"]);
    assert_eq!(format!("{call:?}"), format!("{expected:?}"));
}

#[test]
fn a_restore_whose_records_changed_after_the_question_offers_its_dry_run() {
    let failure = MxvFailure::RestoreChanged {
        name: "triage".to_owned(),
    };

    assert_eq!(failure.code(), &RESTORE_CHANGED);
    assert_eq!(
        failure.to_string(),
        "the records of triage changed after mxv asked, so mxv deleted nothing"
    );
    let Recovery::Run(call) = failure.recovery() else {
        panic!("{failure:?} offers no call");
    };
    let expected = Call::typed(vec![
        "restore".to_owned(),
        "triage".to_owned(),
        "--dry-run".to_owned(),
    ]);
    assert_eq!(format!("{call:?}"), format!("{expected:?}"));
}
