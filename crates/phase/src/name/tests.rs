use super::{
    AppName, BadName, MOST_NAME_BYTES, PortName, ProcessName, VolumeName, WorkspaceId,
    WorkspaceName,
};

#[test]
fn a_workspace_name_accepts_letters_digits_dashes_underscores_and_dots() {
    let name: WorkspaceName = "reading-list_2.0".parse().unwrap();
    assert_eq!(name.as_str(), "reading-list_2.0");
}

#[test]
fn a_workspace_name_refuses_what_git_refuses_in_a_branch_name() {
    assert_eq!("a..b".parse::<WorkspaceName>(), Err(BadName::BadDots));
    assert_eq!("main.".parse::<WorkspaceName>(), Err(BadName::BadDots));
    assert_eq!("main.lock".parse::<WorkspaceName>(), Err(BadName::BadDots));
}

#[test]
fn every_name_refuses_empty_text() {
    assert_eq!("".parse::<WorkspaceName>(), Err(BadName::Empty));
    assert_eq!("".parse::<AppName>(), Err(BadName::Empty));
    assert_eq!("".parse::<ProcessName>(), Err(BadName::Empty));
    assert_eq!("".parse::<PortName>(), Err(BadName::Empty));
}

#[test]
fn every_name_refuses_text_longer_than_the_limit() {
    let long = "a".repeat(MOST_NAME_BYTES + 1);
    assert_eq!(long.parse::<AppName>(), Err(BadName::TooLong));
    assert_eq!(long.parse::<ProcessName>(), Err(BadName::TooLong));
    let longest = "a".repeat(MOST_NAME_BYTES);
    assert!(longest.parse::<AppName>().is_ok());
}

#[test]
fn every_name_starts_with_a_letter_or_a_digit() {
    assert_eq!("-api".parse::<AppName>(), Err(BadName::BadStart));
    assert_eq!("_server".parse::<ProcessName>(), Err(BadName::BadStart));
}

#[test]
fn an_app_name_is_lowercase_because_it_names_a_compose_project() {
    assert_eq!("Api".parse::<AppName>(), Err(BadName::Uppercase('A')));
}

#[test]
fn an_app_name_holds_no_dot() {
    assert_eq!("api.v2".parse::<AppName>(), Err(BadName::BadCharacter('.')));
}

#[test]
fn a_process_name_and_a_port_name_may_hold_uppercase_letters() {
    assert!("Server".parse::<ProcessName>().is_ok());
    assert!("HTTP".parse::<PortName>().is_ok());
}

#[test]
fn a_name_reads_from_json_only_through_its_check() {
    let refused = serde_json::from_str::<AppName>("\"Bad Name\"");
    assert!(refused.is_err());
    let accepted: AppName = serde_json::from_str("\"api\"").unwrap();
    assert_eq!(serde_json::to_string(&accepted).unwrap(), "\"api\"");
}

#[test]
fn a_workspace_id_accepts_a_lowercase_uuid() {
    let id: WorkspaceId = "0b9f3c8e-6a1d-4f7b-9c2e-5d8a7b6c4e21".parse().unwrap();
    assert_eq!(id.as_str(), "0b9f3c8e-6a1d-4f7b-9c2e-5d8a7b6c4e21");
}

#[test]
fn a_workspace_id_holds_no_underscore_or_dot() {
    assert_eq!(
        "a_b".parse::<WorkspaceId>(),
        Err(BadName::BadCharacter('_'))
    );
    assert_eq!(
        "a.b".parse::<WorkspaceId>(),
        Err(BadName::BadCharacter('.'))
    );
}

#[test]
fn a_volume_name_follows_the_rule_of_a_process_name() {
    assert!("cargo-cache".parse::<VolumeName>().is_ok());
    assert_eq!("a/b".parse::<VolumeName>(), Err(BadName::BadCharacter('/')));
}
