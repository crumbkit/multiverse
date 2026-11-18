use super::outside_clause;

#[test]
fn a_removal_question_names_the_process_outside_phase() {
    let clause = outside_clause(Some("process 42 outside phase works in it, in /w/app"));
    assert_eq!(
        clause,
        " A process outside phase works in it (process 42 outside phase works in it, in /w/app)."
    );
}

#[test]
fn a_removal_question_with_no_outside_process_adds_nothing() {
    assert_eq!(outside_clause(None), "");
}
