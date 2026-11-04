use super::{BadName, WorkspaceName};

fn parse(text: &str) -> Result<WorkspaceName, BadName> {
    text.parse()
}

#[test]
fn a_name_of_letters_digits_dashes_underscores_and_dots_is_accepted() {
    assert_eq!(
        parse("reading-list_2.b").unwrap().as_str(),
        "reading-list_2.b"
    );
}

#[test]
fn an_empty_name_is_refused() {
    assert_eq!(parse(""), Err(BadName::Empty));
}

#[test]
fn a_name_longer_than_64_bytes_is_refused() {
    assert_eq!(parse(&"a".repeat(65)), Err(BadName::TooLong));
    assert!(parse(&"a".repeat(64)).is_ok());
}

#[test]
fn a_name_that_starts_with_a_dash_or_a_dot_is_refused() {
    assert_eq!(parse("-x"), Err(BadName::BadStart));
    assert_eq!(parse(".x"), Err(BadName::BadStart));
}

#[test]
fn a_name_with_a_slash_or_a_space_is_refused() {
    assert_eq!(parse("a/b"), Err(BadName::BadCharacter('/')));
    assert_eq!(parse("a b"), Err(BadName::BadCharacter(' ')));
}

#[test]
fn a_name_that_git_refuses_as_a_branch_is_refused() {
    assert_eq!(parse("a..b"), Err(BadName::BadDots));
    assert_eq!(parse("a."), Err(BadName::BadDots));
    assert_eq!(parse("a.lock"), Err(BadName::BadDots));
}

#[test]
fn a_name_reads_back_from_json_only_through_the_checks() {
    let name: WorkspaceName = serde_json::from_str("\"inbox\"").unwrap();
    assert_eq!(name.as_str(), "inbox");
    assert!(serde_json::from_str::<WorkspaceName>("\"a/b\"").is_err());
}
