use std::path::{Path, PathBuf};

use super::{Config, parse};
use crate::failures::MxvFailure;

fn parsed(text: &str) -> Result<Config, MxvFailure> {
    parse(
        Path::new("/home/agent/.config/mxv/config.toml"),
        text,
        Path::new("/home/agent"),
    )
}

/// Returns the line and the reason of a `mxv::config::invalid` failure.
fn invalid(text: &str) -> (Option<usize>, String) {
    match parsed(text) {
        Err(MxvFailure::BadConfig { line, reason, .. }) => (line, reason),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_empty_file_sets_no_root() {
    assert_eq!(parsed("").unwrap().root, None);
    assert_eq!(parsed("# a comment\n\n").unwrap().root, None);
}

#[test]
fn an_absolute_root_is_the_root() {
    let config = parsed("root = \"/srv/workspaces\"\n").unwrap();
    assert_eq!(config.root, Some(PathBuf::from("/srv/workspaces")));
}

#[test]
fn a_root_that_starts_with_a_tilde_starts_at_the_home_directory() {
    let config = parsed("root = \"~/work/mxv\"\n").unwrap();
    assert_eq!(config.root, Some(PathBuf::from("/home/agent/work/mxv")));
    let config = parsed("root = \"~\"\n").unwrap();
    assert_eq!(config.root, Some(PathBuf::from("/home/agent")));
}

#[test]
fn a_relative_root_is_refused_with_its_line() {
    let (line, reason) = invalid("# workspaces\nroot = \"work\"\n");
    assert_eq!(line, Some(2));
    assert!(reason.contains("absolute"), "{reason}");
}

#[test]
fn a_tilde_followed_by_a_user_name_is_refused() {
    let (line, _) = invalid("root = \"~agent/mxv\"\n");
    assert_eq!(line, Some(1));
}

#[test]
fn an_unknown_key_is_refused_with_its_line() {
    let (line, reason) = invalid("root = \"/srv\"\n\nroots = \"/srv\"\n");
    assert_eq!(line, Some(3));
    assert!(reason.contains("roots"), "{reason}");
}

#[test]
fn a_root_that_is_not_a_string_is_refused_with_its_line() {
    let (line, _) = invalid("\nroot = 7\n");
    assert_eq!(line, Some(2));
}

#[test]
fn text_that_is_not_toml_is_refused_with_its_line() {
    let (line, _) = invalid("root = \"/srv\"\nroot =\n");
    assert_eq!(line, Some(2));
}
