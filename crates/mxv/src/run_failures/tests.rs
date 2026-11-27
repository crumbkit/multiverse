use super::{change_binds, joined};

#[test]
fn joined_lists_one_two_and_three_names_as_a_person_writes_them() {
    assert_eq!(joined(&[]), "");
    assert_eq!(joined(&["AUTH_URL"]), "AUTH_URL");
    assert_eq!(
        joined(&["AUTH_URL", "LEDGER_URL"]),
        "AUTH_URL and LEDGER_URL"
    );
    assert_eq!(
        joined(&["AUTH_URL", "LEDGER_URL", "SHELF_URL"]),
        "AUTH_URL, LEDGER_URL and SHELF_URL"
    );
}

#[test]
fn the_recovery_of_one_bind_names_its_variable_and_its_starmap_file() {
    assert_eq!(
        change_binds(&["AUTH_URL"]),
        "change the bind of AUTH_URL in the starmap file"
    );
}

#[test]
fn the_recovery_of_several_binds_names_each_variable_in_order() {
    assert_eq!(
        change_binds(&["LEDGER_URL", "AUTH_URL"]),
        "change the binds of LEDGER_URL and AUTH_URL in their starmap files"
    );
}

#[test]
fn the_recovery_names_a_variable_that_several_processes_bind_once() {
    assert_eq!(
        change_binds(&["AUTH_URL", "AUTH_URL", "AUTH_URL"]),
        "change the binds of AUTH_URL in their starmap files"
    );
    assert_eq!(
        change_binds(&["AUTH_URL", "LEDGER_URL", "AUTH_URL"]),
        "change the binds of AUTH_URL and LEDGER_URL in their starmap files"
    );
}
