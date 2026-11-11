use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use super::{Asked, Grant, GrantScope, Grants, Growth, grant, path, read};
use crate::error::PhaseError;
use crate::name::AppName;
use crate::plan::Permissions;

fn app(name: &str) -> AppName {
    name.parse().unwrap()
}

fn scope() -> GrantScope {
    GrantScope::new("multiverse", [PathBuf::from("/src/multiverse")])
}

fn sandboxed(write: &[&str], connect: &[&str]) -> Grant {
    Grant::Sandboxed(Permissions {
        write: write.iter().map(|path| (*path).to_owned()).collect(),
        connect: connect.iter().map(|target| (*target).to_owned()).collect(),
        ..Permissions::default()
    })
}

fn request(entries: &[(&str, Grant)]) -> BTreeMap<AppName, Grant> {
    entries
        .iter()
        .map(|(name, grant)| (app(name), grant.clone()))
        .collect()
}

fn granted(entries: &[(&str, Grant)]) -> Grants {
    Grants {
        apps: request(entries),
    }
}

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_new_entry_a_removed_sandbox_and_a_new_unit_each_count_as_growth() {
    let grants = granted(&[
        ("api", sandboxed(&["target"], &[])),
        ("web", sandboxed(&[], &[])),
    ]);

    let growth = grants.growth(&request(&[
        ("api", sandboxed(&["target", "/tmp/cache"], &[])),
        ("web", Grant::Unsandboxed),
        ("worker", sandboxed(&[], &["store"])),
    ]));

    let apps: BTreeMap<&AppName, &Asked> = growth.apps().collect();
    let more = Permissions {
        write: BTreeSet::from(["/tmp/cache".to_owned()]),
        ..Permissions::default()
    };
    assert_eq!(apps.get(&app("api")), Some(&&Asked::More(more)));
    assert_eq!(apps.get(&app("web")), Some(&&Asked::Unsandboxed));
    assert_eq!(
        apps.get(&app("worker")),
        Some(&&Asked::New(sandboxed(&[], &["store"])))
    );
}

#[test]
fn a_smaller_or_equal_request_asks_for_nothing() {
    let grants = granted(&[("api", sandboxed(&["target", "out"], &["store"]))]);

    assert!(
        grants
            .growth(&request(&[("api", sandboxed(&["target"], &["store"]))]))
            .is_empty()
    );
    assert!(
        grants
            .growth(&request(&[(
                "api",
                sandboxed(&["target", "out"], &["store"])
            )]))
            .is_empty()
    );
}

#[test]
fn an_unsandboxed_grant_covers_every_sandboxed_request() {
    let grants = granted(&[("api", Grant::Unsandboxed)]);

    assert!(
        grants
            .growth(&request(&[("api", sandboxed(&["/anywhere"], &["store"]))]))
            .is_empty()
    );
}

#[test]
fn a_state_write_path_is_left_out_of_a_request() {
    let permissions = Permissions {
        write: BTreeSet::from(["$STATE".to_owned()]),
        ..Permissions::default()
    };

    assert_eq!(
        Grant::of(Some(&permissions)),
        Grant::Sandboxed(Permissions::default())
    );
    assert_eq!(Grant::of(None), Grant::Unsandboxed);
}

#[test]
fn a_scope_without_a_file_has_no_grants_and_asks_about_every_unit() {
    let state = tempfile::tempdir().unwrap();

    let grants = read(state.path(), &scope()).unwrap();

    assert_eq!(grants, None);
    let growth = Grants::default().growth(&request(&[("api", Grant::Unsandboxed)]));
    assert_eq!(growth.apps().count(), 1);
}

#[test]
fn a_grant_is_written_and_read_back() {
    let state = tempfile::tempdir().unwrap();
    let growth = Grants::default().growth(&request(&[
        ("api", sandboxed(&["target"], &["store"])),
        ("worker", Grant::Unsandboxed),
    ]));

    grant(state.path(), &scope(), &growth, WAIT).unwrap();

    let grants = read(state.path(), &scope()).unwrap().unwrap();
    assert_eq!(
        grants.get(&app("api")),
        Some(&sandboxed(&["target"], &["store"]))
    );
    assert_eq!(grants.get(&app("worker")), Some(&Grant::Unsandboxed));
}

#[test]
fn a_grant_never_shrinks() {
    let state = tempfile::tempdir().unwrap();
    let wide = Grants::default().growth(&request(&[("api", sandboxed(&["target"], &["store"]))]));
    grant(state.path(), &scope(), &wide, WAIT).unwrap();
    let grants = read(state.path(), &scope()).unwrap().unwrap();
    let narrower = grants.growth(&request(&[("api", sandboxed(&["out"], &[]))]));

    grant(state.path(), &scope(), &narrower, WAIT).unwrap();

    let grants = read(state.path(), &scope()).unwrap().unwrap();
    assert_eq!(
        grants.get(&app("api")),
        Some(&sandboxed(&["out", "target"], &["store"]))
    );
}

#[test]
fn two_concurrent_grants_keep_both_additions() {
    let state = tempfile::tempdir().unwrap();
    let dir = state.path().to_path_buf();
    let threads: Vec<_> = ["api", "web"]
        .into_iter()
        .map(|name| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let growth = Grants::default().growth(&request(&[(name, Grant::Unsandboxed)]));
                grant(&dir, &scope(), &growth, WAIT).unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }

    let grants = read(&dir, &scope()).unwrap().unwrap();
    assert_eq!(grants.get(&app("api")), Some(&Grant::Unsandboxed));
    assert_eq!(grants.get(&app("web")), Some(&Grant::Unsandboxed));
}

#[test]
fn a_corrupt_file_fails_with_its_own_code() {
    let state = tempfile::tempdir().unwrap();
    let file = path(state.path(), &scope());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test plants a corrupt grant file"
    )]
    std::fs::write(&file, "{ not json").unwrap();

    let error = read(state.path(), &scope()).unwrap_err();

    assert_eq!(error.code(), "phase::grants::unreadable");
    assert!(error.to_string().contains(&file.display().to_string()));
}

#[test]
fn a_newer_file_fails_with_its_own_code() {
    let state = tempfile::tempdir().unwrap();
    let file = path(state.path(), &scope());
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test plants a grant file that a newer phase wrote"
    )]
    std::fs::write(&file, r#"{"schema":"phase/grants/v2","future":true}"#).unwrap();

    let error = read(state.path(), &scope()).unwrap_err();

    assert!(matches!(error, PhaseError::NewerGrants { .. }));
    assert_eq!(error.code(), "phase::grants::newer");
}

#[test]
fn the_file_is_named_after_the_label_and_a_hash_of_the_sorted_paths() {
    let one = GrantScope::new(
        "my repo",
        [
            PathBuf::from("/b"),
            PathBuf::from("/a"),
            PathBuf::from("/a"),
        ],
    );
    let two = GrantScope::new("my repo", [PathBuf::from("/a"), PathBuf::from("/b")]);
    let other = GrantScope::new("my repo", [PathBuf::from("/a")]);

    assert_eq!(one.stem(), two.stem());
    assert_ne!(one.stem(), other.stem());
    assert!(one.stem().starts_with("my-repo-"));
    assert_eq!(one.stem().len(), "my-repo-".len() + 16);
    assert_eq!(one.paths(), [PathBuf::from("/a"), PathBuf::from("/b")]);
}

#[test]
fn the_lines_name_each_unit_in_plain_words_and_hold_no_hash() {
    let api = Permissions {
        write: BTreeSet::from(["target".to_owned(), "$STATE".to_owned()]),
        listen: BTreeSet::from(["api".parse().unwrap()]),
        connect: BTreeSet::from(["store".to_owned()]),
        secrets: BTreeSet::from(["STRIPE_KEY".to_owned()]),
    };
    let web = Permissions {
        write: BTreeSet::from(["/tmp/cache".to_owned()]),
        ..Permissions::default()
    };
    let growth = Growth {
        apps: BTreeMap::from([
            (app("api"), Asked::New(Grant::Sandboxed(api))),
            (app("web"), Asked::More(web)),
            (app("worker"), Asked::New(Grant::Unsandboxed)),
        ]),
    };

    assert_eq!(
        growth.lines(),
        [
            "  api, a new unit:",
            "    write to target in its checkout",
            "    listen on the port api",
            "    connect to store",
            "    receive the secret STRIPE_KEY",
            "  web, more than it was granted:",
            "    write to /tmp/cache",
            "  worker, a new unit:",
            "    run outside the sandbox, with everything your account can reach",
        ]
    );
}
