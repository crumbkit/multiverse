use super::{BadBrace, Piece, split};

/// Accepts the placeholder `{x}` and `{y.<anything>}`.
fn letter(inside: &str) -> Option<String> {
    (inside == "x" || inside.starts_with("y.")).then(|| inside.to_owned())
}

fn text(literal: &str) -> Piece<String> {
    Piece::Text(literal.to_owned())
}

fn placeholder(inside: &str) -> Piece<String> {
    Piece::Placeholder(inside.to_owned())
}

#[test]
fn text_without_braces_is_one_literal_piece() {
    assert_eq!(split("plain text", letter), Ok(vec![text("plain text")]));
}

#[test]
fn empty_text_has_no_pieces() {
    assert_eq!(split("", letter), Ok(vec![]));
}

#[test]
fn doubled_braces_are_single_literal_braces() {
    assert_eq!(split("a{{b}}c", letter), Ok(vec![text("a{b}c")]));
    assert_eq!(split("{{x}}", letter), Ok(vec![text("{x}")]));
    assert_eq!(split("{{}}", letter), Ok(vec![text("{}")]));
}

#[test]
fn a_placeholder_splits_the_text_at_the_start_the_middle_and_the_end() {
    assert_eq!(
        split("{x}tail", letter),
        Ok(vec![placeholder("x"), text("tail")])
    );
    assert_eq!(
        split("a{x}b", letter),
        Ok(vec![text("a"), placeholder("x"), text("b")])
    );
    assert_eq!(
        split("head{x}", letter),
        Ok(vec![text("head"), placeholder("x")])
    );
}

#[test]
fn two_placeholders_next_to_each_other_are_two_pieces() {
    assert_eq!(
        split("{x}{y.z}", letter),
        Ok(vec![placeholder("x"), placeholder("y.z")])
    );
}

#[test]
fn doubled_braces_around_a_placeholder_join_the_literal_text_beside_it() {
    assert_eq!(
        split("{{{x}}}", letter),
        Ok(vec![text("{"), placeholder("x"), text("}")])
    );
}

#[test]
fn the_closure_receives_the_text_up_to_the_first_closing_brace() {
    assert_eq!(
        split("{y.{a}}}", letter),
        Ok(vec![placeholder("y.{a"), text("}")])
    );
    assert_eq!(split("{y.{a}}", letter), Err(BadBrace("}")));
}

#[test]
fn an_unclosed_brace_is_bad_from_that_brace_on() {
    assert_eq!(split("open {", letter), Err(BadBrace("{")));
    assert_eq!(split("{x", letter), Err(BadBrace("{x")));
    assert_eq!(split("{{{", letter), Err(BadBrace("{")));
}

#[test]
fn a_lone_closing_brace_is_bad_from_that_brace_on() {
    assert_eq!(split("a}b", letter), Err(BadBrace("}b")));
    assert_eq!(split("}{", letter), Err(BadBrace("}{")));
    assert_eq!(split("{x}}", letter), Err(BadBrace("}")));
}

#[test]
fn a_placeholder_the_closure_refuses_is_bad_from_its_opening_brace_on() {
    assert_eq!(split("a {nope} b", letter), Err(BadBrace("{nope} b")));
    assert_eq!(split("{}", letter), Err(BadBrace("{}")));
    assert_eq!(split("{nope}{x}", letter), Err(BadBrace("{nope}{x}")));
}
