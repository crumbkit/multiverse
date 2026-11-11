use std::collections::BTreeMap;
use std::path::PathBuf;

use multiverse::phase::{Grant, Grants, Growth, Permissions};

use super::{answer, question};

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
