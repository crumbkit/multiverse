use super::{BadHost, Host, HostName, MOST_HOST_NAME_BYTES};
use crate::archive::{KEPT_NAMESPACE, WIP_MESSAGE};
use crate::manifest::MANIFEST_FILE;
use crate::proxy::DOMAIN;
use crate::record::{Checkout, Workspace, agents_text};
use crate::service::RESERVED_PREFIX;
use crate::testing::unmade_clone;

#[test]
fn the_mxv_profile_holds_every_value_that_0_5_0_hard_coded() {
    let host = Host::mxv();
    assert_eq!(host.name().as_str(), "mxv");
    assert_eq!(host.manifest_file(), MANIFEST_FILE);
    assert_eq!(host.manifest_file(), "mxv.toml");
    assert_eq!(host.unit_prefix(), RESERVED_PREFIX);
    assert_eq!(host.unit_prefix(), "mxv-");
    assert_eq!(host.ref_namespace(), "refs/mxv/");
    assert_eq!(host.kept_namespace(), KEPT_NAMESPACE);
    assert_eq!(host.pin_namespace(), "refs/mxv/archives/");
    assert_eq!(host.origin_namespace(), "refs/mxv/origin/");
    assert_eq!(host.labels().as_str(), "mxv.");
    assert_eq!(host.proxy_domain(), DOMAIN);
    assert_eq!(host.proxy_domain(), "mxv.localhost");
    assert_eq!(
        host.wip_author(),
        ("mxv", "mxv@localhost.invalid".to_owned())
    );
    assert_eq!(host.wip_message(), WIP_MESSAGE);
    assert_eq!(host.wip_message(), "wip: archived by mxv");
    assert_eq!(host.aside_suffix(), ".mxv-removing");
    assert_eq!(host.quarantine_suffix(), ".mxv-archiving");
    let workspace = sample();
    assert_eq!(
        host.agents_text(&workspace, &[]),
        agents_text(&workspace, &[])
    );
}

#[test]
fn a_named_profile_derives_every_value_from_its_name() {
    let host = Host::named("example").unwrap();
    assert_eq!(host.name().as_str(), "example");
    assert_eq!(host.manifest_file(), "example.toml");
    assert_eq!(host.unit_prefix(), "example-");
    assert_eq!(host.ref_namespace(), "refs/example/");
    assert_eq!(host.kept_namespace(), "refs/example/kept/");
    assert_eq!(host.pin_namespace(), "refs/example/archives/");
    assert_eq!(host.origin_namespace(), "refs/example/origin/");
    assert_eq!(host.labels().as_str(), "example.");
    assert_eq!(host.proxy_domain(), "example.localhost");
    assert_eq!(
        host.wip_author(),
        ("example", "example@localhost.invalid".to_owned())
    );
    assert_eq!(host.wip_message(), "wip: archived by example");
    assert_eq!(host.aside_suffix(), ".example-removing");
    assert_eq!(host.quarantine_suffix(), ".example-archiving");
    let workspace = sample();
    let text = host.agents_text(&workspace, &[]);
    assert!(!text.contains("mxv"), "{text}");
    assert!(text.contains(workspace.name.as_str()), "{text}");

    let mxv = Host::named("mxv").unwrap();
    assert_eq!(mxv.manifest_file(), Host::mxv().manifest_file());
    assert_eq!(mxv.ref_namespace(), Host::mxv().ref_namespace());
    assert_ne!(
        mxv.agents_text(&workspace, &[]),
        Host::mxv().agents_text(&workspace, &[])
    );

    let kept = host
        .with_manifest_file("tasks.toml")
        .unwrap()
        .with_proxy_domain("apps.example.localhost")
        .unwrap()
        .with_agents_text(|_, _| "own text\n".to_owned());
    assert_eq!(kept.manifest_file(), "tasks.toml");
    assert_eq!(kept.proxy_domain(), "apps.example.localhost");
    assert_eq!(kept.agents_text(&workspace, &[]), "own text\n");
    assert_eq!(kept.ref_namespace(), "refs/example/");
}

#[test]
fn a_profile_refuses_a_name_with_a_capital_letter_a_slash_or_more_than_32_bytes() {
    let longest = "a".repeat(MOST_HOST_NAME_BYTES);
    assert!(Host::named(&longest).is_ok());
    for bad in [
        String::new(),
        "Example".to_owned(),
        "ex/ample".to_owned(),
        "ex.ample".to_owned(),
        "2example".to_owned(),
        "-example".to_owned(),
        "a".repeat(MOST_HOST_NAME_BYTES + 1),
    ] {
        let refused = Host::named(&bad).unwrap_err();
        assert_eq!(refused.field(), "name", "{bad:?}");
        assert!(HostName::new(&bad).is_err(), "{bad:?}");
    }
    let refused: BadHost = Host::named("Example").unwrap_err();
    assert!(refused.to_string().contains("Example"), "{refused}");
}

#[test]
fn a_profile_refuses_a_proxy_domain_outside_localhost() {
    let host = Host::named("example").unwrap();
    assert!(host.clone().with_proxy_domain("example.localhost").is_ok());
    for bad in [
        "localhost",
        "example.com",
        "example.localhost.com",
        ".localhost",
        "a..localhost",
        "Example.localhost",
        "ex_ample.localhost",
        "-a.localhost",
        &format!("{}.localhost", "a".repeat(64)),
    ] {
        let refused = host.clone().with_proxy_domain(bad).unwrap_err();
        assert_eq!(refused.field(), "proxy domain", "{bad:?}");
    }
}

#[test]
fn a_profile_refuses_a_manifest_file_that_is_a_path() {
    let host = Host::named("example").unwrap();
    assert!(host.clone().with_manifest_file("tasks.toml").is_ok());
    for bad in [
        "dir/tasks.toml",
        "../tasks.toml",
        "/tasks.toml",
        "tasks.json",
        ".toml",
        "tasks\\x.toml",
        "ta sks.toml",
        &format!("{}.toml", "a".repeat(60)),
    ] {
        let refused = host.clone().with_manifest_file(bad).unwrap_err();
        assert_eq!(refused.field(), "manifest file", "{bad:?}");
    }
}

/// Returns a workspace record named `reading-list` with one checkout.
fn sample() -> Workspace {
    let checkout = Checkout::new(
        "app".to_owned(),
        std::path::Path::new("/source/app"),
        "/work/reading-list/app".into(),
        "reading-list".to_owned(),
        "refs/remotes/origin/main".to_owned(),
        unmade_clone(),
    );
    Workspace::new(
        "reading-list".parse().unwrap(),
        "/work/reading-list".into(),
        vec![checkout],
    )
}
