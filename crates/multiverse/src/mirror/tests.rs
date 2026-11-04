use std::path::Path;

use super::Origin;

fn key(url: &str) -> String {
    Origin::parse(url, Path::new("/home/someone/app"))
        .unwrap()
        .key
        .display()
        .to_string()
}

#[test]
fn an_https_url_and_an_ssh_url_of_one_repository_share_a_mirror() {
    assert_eq!(
        key("https://github.com/acme/app.git"),
        "github.com/acme/app.git"
    );
    assert_eq!(key("git@github.com:acme/app"), "github.com/acme/app.git");
    assert_eq!(
        key("ssh://git@github.com/acme/app.git/"),
        "github.com/acme/app.git"
    );
    assert_eq!(
        key("https://GitHub.com/acme/app"),
        "github.com/acme/app.git"
    );
}

#[test]
fn a_port_and_a_user_name_keep_the_host_apart_only_by_the_port() {
    assert_eq!(
        key("ssh://deploy@git.example.com:2222/team/app.git"),
        "git.example.com%3A2222/team/app.git"
    );
}

#[test]
fn a_local_path_is_resolved_against_the_repository_and_kept_under_local() {
    let origin = Origin::parse("../origin.git", Path::new("/srv/work/app")).unwrap();
    assert_eq!(origin.url, "/srv/work/origin.git");
    assert_eq!(
        origin.key.display().to_string(),
        "local/srv/work/origin.git"
    );
    let origin = Origin::parse("file:///srv/repos/app", Path::new("/x")).unwrap();
    assert_eq!(origin.key.display().to_string(), "local/srv/repos/app.git");
}

#[test]
fn a_component_that_could_leave_the_mirror_directory_is_encoded() {
    assert_eq!(
        key("https://example.com/a/%2e%2e/b"),
        "example.com/a/%252e%252e/b.git"
    );
    assert_eq!(
        key("https://example.com/./.hidden/b"),
        "example.com/%2Ehidden/b.git"
    );
    assert_eq!(key("https://example.com/a b/c"), "example.com/a%20b/c.git");
}

#[test]
fn two_urls_that_differ_never_share_a_mirror_by_encoding() {
    assert_ne!(
        key("https://example.com/a-b/c"),
        key("https://example.com/a/b-c")
    );
    assert_ne!(
        key("https://example.com/a%2Fb"),
        key("https://example.com/a/b")
    );
}

#[test]
fn a_url_without_a_path_is_refused() {
    assert!(Origin::parse("https://example.com", Path::new("/x")).is_none());
    assert!(Origin::parse("", Path::new("/x")).is_none());
}
