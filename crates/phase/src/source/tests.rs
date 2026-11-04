use super::{BadSourceText, GithubRepository, ImageReference, Version};

#[test]
fn a_version_accepts_a_release_tag() {
    let version: Version = "v1.2.0-rc.1+build_7".parse().unwrap();
    assert_eq!(version.as_str(), "v1.2.0-rc.1+build_7");
}

#[test]
fn a_version_refuses_whitespace_and_slashes() {
    assert_eq!(
        "v1 2".parse::<Version>(),
        Err(BadSourceText::BadCharacter(' '))
    );
    assert_eq!(
        "v1/2".parse::<Version>(),
        Err(BadSourceText::BadCharacter('/'))
    );
    assert_eq!("".parse::<Version>(), Err(BadSourceText::Empty));
}

#[test]
fn a_github_repository_is_an_owner_and_a_name() {
    let repository: GithubRepository = "crumbkit/example-api".parse().unwrap();
    assert_eq!(repository.owner(), "crumbkit");
    assert_eq!(repository.name(), "example-api");
}

#[test]
fn a_github_repository_refuses_text_without_exactly_one_slash() {
    assert_eq!(
        "crumbkit".parse::<GithubRepository>(),
        Err(BadSourceText::NotOwnerAndName)
    );
    assert_eq!(
        "a/b/c".parse::<GithubRepository>(),
        Err(BadSourceText::NotOwnerAndName)
    );
    assert_eq!(
        "/b".parse::<GithubRepository>(),
        Err(BadSourceText::NotOwnerAndName)
    );
}

#[test]
fn an_image_reference_accepts_a_pinned_tag_or_a_digest() {
    assert!("postgres:17".parse::<ImageReference>().is_ok());
    assert!(
        "registry.example:5000/team/db:1.2"
            .parse::<ImageReference>()
            .is_ok()
    );
    assert!(
        "postgres@sha256:0000000000000000000000000000000000000000000000000000000000000000"
            .parse::<ImageReference>()
            .is_ok()
    );
}

#[test]
fn an_image_reference_refuses_an_image_without_a_tag_or_tagged_latest() {
    assert_eq!(
        "postgres".parse::<ImageReference>(),
        Err(BadSourceText::Unpinned)
    );
    assert_eq!(
        "postgres:latest".parse::<ImageReference>(),
        Err(BadSourceText::Unpinned)
    );
    assert_eq!(
        "registry.example:5000/db".parse::<ImageReference>(),
        Err(BadSourceText::Unpinned)
    );
}
