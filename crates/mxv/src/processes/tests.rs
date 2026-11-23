use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use multiverse::phase::{Grant, Grants, Growth, Permissions};
use multiverse::{Manifest, Reclaimed, Workspace};

use super::{answer, question, selection};

/// Returns what a new sandboxed unit `api` that writes `target`, and a new
/// unit `worker` that runs outside the sandbox, ask for.
fn growth() -> Growth {
    let mut api = Permissions::default();
    api.write.insert("target".to_owned());
    Grants::default().growth(&BTreeMap::from([
        ("api".parse().unwrap(), Grant::Sandboxed(api)),
        ("worker".parse().unwrap(), Grant::Unsandboxed),
    ]))
}

/// Returns what `answer` reports for the reply `reply`, and what it wrote.
fn answered(reply: &str) -> (bool, String) {
    let mut written = Vec::new();
    let granted = answer("Grant them? [y/N] ", &mut written, &mut reply.as_bytes());
    (granted, String::from_utf8(written).unwrap())
}

#[test]
fn the_question_names_the_repository_and_each_unit_in_plain_words_and_holds_no_hash() {
    let text = question("multiverse", &[PathBuf::from("/src/multiverse")], &growth());

    assert_eq!(
        text,
        "Repository multiverse (/src/multiverse) asks for these permissions:\n\
         \x20 api, a new unit:\n\
         \x20   write to target in its checkout\n\
         \x20 worker, a new unit:\n\
         \x20   run outside the sandbox, with everything your account can reach\n\
         Grant them? [y/N] "
    );
    assert!(
        !text
            .split_whitespace()
            .any(|word| word.len() >= 16 && word.chars().all(|c| c.is_ascii_hexdigit())),
        "{text}"
    );
}

#[test]
fn yes_in_any_case_grants_and_writes_the_question() {
    assert_eq!(answered("y\n"), (true, "Grant them? [y/N] ".to_owned()));
    assert!(answered("  YES \n").0);
}

#[test]
fn any_other_reply_or_no_reply_grants_nothing() {
    assert!(!answered("n\n").0);
    assert!(!answered("\n").0);
    assert!(!answered("").0);
    assert!(!answered("sure\n").0);
}

/// Returns a workspace that `mxv reclaim` marked as running `reclaimed`,
/// and a manifest that declares the units `store`, `api` and `web`.
fn reclaimed(reclaimed: &[&str]) -> (Workspace, Manifest) {
    let mut workspace = Workspace::new(
        "triage".parse().unwrap(),
        PathBuf::from("/nowhere/triage"),
        Vec::new(),
    );
    workspace.reclaimed = Some(Reclaimed::new(
        reclaimed.iter().map(|name| (*name).to_owned()).collect(),
    ));
    let manifest = Manifest::parse(
        "[processes.store]\nargv = [\"true\"]\n\n\
         [processes.api]\nargv = [\"true\"]\n\n\
         [processes.web]\nargv = [\"true\"]\n",
        Path::new("mxv.toml"),
    )
    .unwrap();
    (workspace, manifest)
}

#[test]
fn an_empty_selection_after_reclaim_is_the_marked_units_that_the_manifest_still_declares() {
    let (workspace, manifest) = reclaimed(&["api", "gone"]);

    assert_eq!(selection(&workspace, &manifest, &[]), ["api"]);
}

#[test]
fn named_units_are_the_selection_whatever_the_reclaim_mark_holds() {
    let (workspace, manifest) = reclaimed(&["api"]);

    assert_eq!(
        selection(&workspace, &manifest, &["web".to_owned()]),
        ["web"]
    );
}

#[test]
fn an_empty_selection_without_a_reclaim_mark_stays_empty_so_it_means_every_unit() {
    let (mut workspace, manifest) = reclaimed(&[]);
    workspace.reclaimed = None;

    assert!(selection(&workspace, &manifest, &[]).is_empty());
}
