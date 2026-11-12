use crumbkit::{Call, Failure as _, Recovery};

use super::{MxvFailure, RESTORE_CHANGED};

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
