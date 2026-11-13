use std::path::Path;

use super::{Origin, redacted};

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
        "local/srv/work/origin.git.git"
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

#[test]
fn a_local_path_with_git_and_one_without_have_mirrors_of_their_own() {
    assert_ne!(key("/srv/repos/app"), key("/srv/repos/app.git"));
}

#[test]
fn a_local_path_through_a_link_shares_the_mirror_of_its_target() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().canonicalize().unwrap().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let through_link = Origin::parse(link.to_str().unwrap(), Path::new("/x")).unwrap();
    let direct = Origin::parse(real.to_str().unwrap(), Path::new("/x")).unwrap();
    assert_eq!(through_link.key, direct.key);
}

#[test]
fn the_default_port_of_a_scheme_is_left_out_of_the_key() {
    assert_eq!(
        key("ssh://git@github.com:22/acme/app"),
        key("git@github.com:acme/app")
    );
    assert_eq!(
        key("https://github.com:443/acme/app"),
        key("https://github.com/acme/app")
    );
}

#[test]
fn the_case_of_a_path_is_kept_because_a_host_may_tell_it_apart() {
    assert_eq!(
        key("https://example.com/Acme/App"),
        "example.com/Acme/App.git"
    );
}

#[test]
fn a_directory_of_one_mirror_never_holds_another_mirror_or_its_lock() {
    assert_eq!(
        key("https://example.com/a.git/b"),
        "example.com/a%2Egit/b.git"
    );
    assert_eq!(
        key("https://example.com/a.git.lock/b"),
        "example.com/a.git%2Elock/b.git"
    );
}

#[test]
fn credentials_in_a_url_are_left_out_wherever_it_is_shown() {
    assert_eq!(
        redacted("https://someone:token@example.com/acme/app.git"),
        "https://example.com/acme/app.git"
    );
    assert_eq!(
        redacted("fatal: unable to access 'https://someone:token@example.com/a/': timed out"),
        "fatal: unable to access 'https://example.com/a/': timed out"
    );
    assert_eq!(
        redacted("git@github.com:acme/app"),
        "git@github.com:acme/app"
    );
    assert_eq!(redacted("/srv/repos/app"), "/srv/repos/app");
}

#[test]
fn the_host_of_an_ssh_url_and_of_an_scp_like_url_is_the_name_after_the_user() {
    assert_eq!(
        super::host_of("deploy:secret@Git.Example.com:22", Some("22")),
        "git.example.com"
    );
    assert_eq!(
        super::host_of("git@git.example.com:2222", Some("22")),
        "git.example.com:2222"
    );
    let (authority, path) = super::scp_like("git@GitHub.com:acme/app.git").unwrap();
    assert_eq!(super::host_of(authority, None), "github.com");
    assert_eq!(path, "acme/app.git");
}
