use super::{BadGlob, DisposableSet, DisposableSource, PathGlob};

fn glob(text: &str) -> PathGlob {
    PathGlob::parse(text).unwrap()
}

#[test]
fn a_glob_with_a_trailing_slash_matches_only_a_directory() {
    let results = glob("test-results/");
    assert!(results.matches("test-results/"));
    assert!(!results.matches("test-results"));
}

#[test]
fn a_glob_without_a_slash_in_the_middle_matches_its_name_at_any_depth() {
    let modules = glob("node_modules/");
    assert!(modules.matches("node_modules/"));
    assert!(modules.matches("web/node_modules/"));
    assert!(modules.matches("packages/ui/node_modules/"));
    assert!(!modules.matches("web/node_modules.txt"));
}

#[test]
fn a_glob_with_a_slash_in_the_middle_is_anchored_at_the_repository_root() {
    let results = glob("web/test-results/");
    assert!(results.matches("web/test-results/"));
    assert!(!results.matches("test-results/"));
    assert!(!results.matches("app/web/test-results/"));
}

#[test]
fn a_star_matches_within_one_component_and_a_question_mark_matches_one_character() {
    let coverage = glob("web/coverage-*/");
    assert!(coverage.matches("web/coverage-unit/"));
    assert!(!coverage.matches("web/coverage-unit/html/"));
    assert!(!coverage.matches("web/coverage/"));
    let logs = glob("log?.txt");
    assert!(logs.matches("log1.txt"));
    assert!(logs.matches("tools/log2.txt"));
    assert!(!logs.matches("log12.txt"));
}

#[test]
fn a_double_star_matches_any_number_of_directories() {
    let cache = glob("packages/**/.turbo/");
    assert!(cache.matches("packages/.turbo/"));
    assert!(cache.matches("packages/ui/.turbo/"));
    assert!(cache.matches("packages/ui/forms/.turbo/"));
    assert!(!cache.matches("apps/ui/.turbo/"));
}

#[test]
fn a_glob_never_matches_a_path_inside_the_directory_it_names() {
    let results = glob("web/test-results/");
    assert!(!results.matches("web/test-results/report.html"));
}

#[test]
fn an_absolute_glob_is_refused() {
    assert_eq!(PathGlob::parse("/tmp/cache/"), Err(BadGlob::Absolute));
}

#[test]
fn a_glob_that_names_git_in_any_case_is_refused() {
    assert_eq!(PathGlob::parse(".git"), Err(BadGlob::Git));
    assert_eq!(PathGlob::parse(".git/"), Err(BadGlob::Git));
    assert_eq!(PathGlob::parse("**/.git/"), Err(BadGlob::Git));
    assert_eq!(PathGlob::parse("vendor/.GIT/objects/"), Err(BadGlob::Git));
    assert_eq!(PathGlob::parse_directory("**/.git/"), Err(BadGlob::Git));
    assert!(PathGlob::parse(".github/").is_ok());
}

#[test]
fn a_glob_that_leaves_the_repository_is_refused() {
    assert_eq!(PathGlob::parse("../other/target/"), Err(BadGlob::Parent));
    assert_eq!(PathGlob::parse("web/../../x/"), Err(BadGlob::Parent));
}

#[test]
fn an_empty_glob_or_one_with_an_empty_or_dot_component_is_refused() {
    assert_eq!(PathGlob::parse(""), Err(BadGlob::Empty));
    assert_eq!(PathGlob::parse("/"), Err(BadGlob::Absolute));
    assert_eq!(PathGlob::parse("web//x/"), Err(BadGlob::EmptyComponent));
    assert_eq!(PathGlob::parse("./web/"), Err(BadGlob::Dot));
    assert_eq!(PathGlob::parse("a\0b"), Err(BadGlob::Nul));
}

#[test]
fn the_built_in_set_holds_the_build_directories_at_any_depth() {
    let set = DisposableSet::built_in();
    for path in [
        "target/",
        "web/node_modules/",
        "dist/",
        "tools/build/",
        ".cache/",
    ] {
        assert!(set.matches(path), "{path}");
    }
    assert!(!set.matches("build"), "an ignored file named build");
    assert!(!set.matches(".env"));
    assert!(!set.matches("tools/dist/prod.secret"));
}

#[test]
fn a_declared_glob_extends_the_built_in_set() {
    let set =
        DisposableSet::built_in().with(&[glob("web/test-results/")], DisposableSource::Config);
    assert!(set.matches("web/test-results/"));
    assert!(set.matches("target/"));
    assert!(!set.matches("test-results/"));
}

#[test]
fn a_glob_reads_back_as_written() {
    assert_eq!(glob("web/test-results/").as_str(), "web/test-results/");
    assert_eq!(glob("web/test-results/").to_string(), "web/test-results/");
}

#[test]
fn a_glob_of_directories_to_clone_refuses_target_and_accepts_node_modules() {
    assert_eq!(PathGlob::parse_clone("target/"), Err(BadGlob::Target));
    assert_eq!(PathGlob::parse_clone("web/target/"), Err(BadGlob::Target));
    assert_eq!(
        PathGlob::parse_clone("/node_modules/"),
        Err(BadGlob::Absolute)
    );
    assert!(PathGlob::parse_clone("node_modules/").is_ok());
}

#[test]
fn a_repository_glob_must_name_a_directory_by_a_literal_last_name() {
    for text in [
        "*",
        "**",
        "**/*",
        "*/",
        "?*",
        "**/",
        ".env",
        "web/*.log",
        "web/cov*/",
    ] {
        assert!(
            PathGlob::parse_directory(text).is_err(),
            "{text} should be refused"
        );
    }
    for text in [
        "web/test-results/",
        "**/coverage/",
        "coverage/",
        "web/*/out/",
    ] {
        assert!(
            PathGlob::parse_directory(text).is_ok(),
            "{text} should be accepted"
        );
    }
}

#[test]
fn a_repository_glob_without_a_trailing_slash_is_refused_as_not_a_directory() {
    assert_eq!(
        PathGlob::parse_directory("local.db"),
        Err(BadGlob::NotDirectory)
    );
}

#[test]
fn a_repository_glob_whose_last_name_holds_a_wildcard_is_refused() {
    assert_eq!(PathGlob::parse_directory("*/"), Err(BadGlob::Wildcard));
    assert_eq!(PathGlob::parse_directory("**/"), Err(BadGlob::Wildcard));
}

#[test]
fn a_clone_glob_follows_the_directory_rule() {
    assert_eq!(PathGlob::parse_clone("*/"), Err(BadGlob::Wildcard));
    assert_eq!(
        PathGlob::parse_clone("node_modules"),
        Err(BadGlob::NotDirectory)
    );
    assert!(PathGlob::parse_clone("web/node_modules/").is_ok());
}

#[test]
fn a_path_that_only_the_manifest_declares_comes_from_the_manifest() {
    let set = DisposableSet::built_in()
        .with(
            &[glob("data/"), glob("coverage/")],
            DisposableSource::Manifest,
        )
        .with(&[glob("coverage/")], DisposableSource::Config);
    assert_eq!(set.source("data/"), Some(DisposableSource::Manifest));
    assert_eq!(set.source("coverage/"), Some(DisposableSource::Config));
    assert_eq!(
        set.source("web/node_modules/"),
        Some(DisposableSource::BuiltIn)
    );
    assert_eq!(set.source(".env"), None);
}
