use super::Cancel;

#[test]
fn a_new_cancel_is_not_cancelled() {
    assert!(!Cancel::new().is_cancelled());
}

#[test]
fn cancelling_a_clone_cancels_the_original() {
    let cancel = Cancel::new();
    let handler = cancel.clone();

    handler.cancel();

    assert!(cancel.is_cancelled());
}
