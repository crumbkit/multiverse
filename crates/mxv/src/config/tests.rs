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

#[test]
fn a_file_without_a_ports_range_sets_none() {
    assert_eq!(parsed("root = \"/srv\"\n").unwrap().ports_range, None);
}

#[test]
fn a_ports_range_of_two_ports_is_the_range_from_the_first_to_the_last() {
    let config = parsed("ports_range = [23000, 23999]\n").unwrap();
    assert_eq!(config.ports_range, Some(23000..=23999));
}

#[test]
fn a_ports_range_that_is_not_two_ports_in_order_is_refused_with_its_line() {
    for text in [
        "\nports_range = [23999, 23000]\n",
        "\nports_range = [23000]\n",
        "\nports_range = \"23000-23999\"\n",
        "\nports_range = [23000, 70000]\n",
    ] {
        let (line, reason) = invalid(text);
        assert_eq!(line, Some(2), "{text}");
        assert!(reason.contains("ports_range"), "{reason}");
    }
}

#[test]
fn a_ports_range_that_reaches_a_privileged_or_ephemeral_port_is_refused() {
    let (_, reason) = invalid("ports_range = [80, 2000]\n");
    assert!(reason.contains("1024"), "{reason}");
    let (_, reason) = invalid("ports_range = [30000, 40000]\n");
    assert!(reason.contains("32767"), "{reason}");
}

#[test]
fn a_root_with_a_nul_character_is_refused_with_its_line() {
    let (line, reason) = invalid("\nroot = \"/srv/a\\u0000b\"\n");
    assert_eq!(line, Some(2));
    assert!(reason.contains("NUL"), "{reason}");
}

#[test]
fn the_file_declares_disposable_globs_and_directories_to_clone() {
    let config = parsed(
        "disposable = [\"web/test-results/\", \".venv/\"]\nclone_on_new = [\"node_modules/\", \"web/.yarn/\"]\n",
    )
    .unwrap();
    let disposable: Vec<&str> = config.disposable.iter().map(|glob| glob.as_str()).collect();
    assert_eq!(disposable, ["web/test-results/", ".venv/"]);
    let clone_on_new = config.clone_on_new.unwrap();
    let cloned: Vec<&str> = clone_on_new.iter().map(|glob| glob.as_str()).collect();
    assert_eq!(cloned, ["node_modules/", "web/.yarn/"]);
}

#[test]
fn an_empty_clone_list_clones_nothing_and_a_missing_one_keeps_the_default() {
    assert_eq!(
        parsed("clone_on_new = []\n").unwrap().clone_on_new,
        Some(Vec::new())
    );
    assert_eq!(parsed("").unwrap().clone_on_new, None);
}

#[test]
fn an_absolute_disposable_glob_is_refused_with_the_line_of_the_glob() {
    let (line, reason) = invalid("disposable = [\n  \"web/\",\n  \"/var/cache/\",\n]\n");
    assert_eq!(line, Some(3));
    assert!(reason.contains("/var/cache/"), "{reason}");
    assert!(reason.contains("absolute"), "{reason}");
}

#[test]
fn a_disposable_glob_with_a_parent_component_is_refused_with_its_line() {
    let (line, reason) = invalid("\ndisposable = [\"web/../../x/\"]\n");
    assert_eq!(line, Some(2));
    assert!(reason.contains(".."), "{reason}");
}

#[test]
fn a_clone_glob_that_names_target_is_refused_with_its_line() {
    let (line, reason) = invalid("clone_on_new = [\"target/\"]\n");
    assert_eq!(line, Some(1));
    assert!(reason.contains("target"), "{reason}");
}

#[test]
fn a_disposable_list_that_is_not_an_array_of_strings_is_refused() {
    let (line, _) = invalid("disposable = \"target/\"\n");
    assert_eq!(line, Some(1));
    let (line, _) = invalid("disposable = [1]\n");
    assert_eq!(line, Some(1));
}

#[test]
fn the_idle_age_and_the_disposable_limit_read_their_units() {
    let config = parsed("clean_idle = \"3d\"\nmax_disposable = \"50GiB\"\n").unwrap();
    assert_eq!(
        config.clean_idle,
        Some(std::time::Duration::from_secs(3 * 24 * 60 * 60))
    );
    assert_eq!(config.max_disposable, Some(50 * 1024 * 1024 * 1024));
}

#[test]
fn an_idle_age_or_a_limit_without_a_unit_is_refused_with_its_line() {
    let (line, reason) = invalid("clean_idle = \"7\"\n");
    assert_eq!(line, Some(1));
    assert!(reason.contains("clean_idle"), "{reason}");
    let (line, reason) = invalid("\nmax_disposable = 50\n");
    assert_eq!(line, Some(2));
    assert!(reason.contains("max_disposable"), "{reason}");
}

#[test]
fn the_config_file_may_declare_file_globs_and_wildcard_names_as_disposable() {
    let config = parsed("disposable = [\"*.log\", \"web/cov*/\"]\n").unwrap();
    assert_eq!(config.disposable.len(), 2);
}

#[test]
fn a_clone_glob_in_the_config_file_must_name_a_directory_by_a_literal_name() {
    let (line, reason) = invalid("\nclone_on_new = [\"*/\"]\n");
    assert_eq!(line, Some(2));
    assert!(reason.contains("directory"), "{reason}");
    assert!(!reason.contains("only the config file"), "{reason}");
}
