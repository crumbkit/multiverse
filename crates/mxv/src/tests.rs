use std::ffi::OsString;

use super::{
    KEEPER_WORDS, PROVISION_WORDS, SERVICES_WORDS, is_keeper, is_provisioner, is_services,
};

/// A predicate that reports whether a whole command line starts one of
/// mxv's own commands.
type Predicate = fn(&[OsString]) -> bool;

/// Each predicate of mxv's own command lines, with the two words that
/// follow the program on its command line.
const PREDICATES: [(Predicate, [&str; 2]); 3] = [
    (is_keeper, KEEPER_WORDS),
    (is_provisioner, PROVISION_WORDS),
    (is_services, SERVICES_WORDS),
];

fn line(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

#[test]
fn each_predicate_accepts_the_program_followed_by_its_two_words() {
    for (predicate, [first, second]) in PREDICATES {
        assert!(
            predicate(&line(&["mxv", first, second])),
            "{first} {second}"
        );
    }
}

#[test]
fn each_predicate_accepts_its_words_followed_by_more_arguments() {
    for (predicate, [first, second]) in PREDICATES {
        assert!(
            predicate(&line(&["mxv", first, second, "x", "--y"])),
            "{first} {second}"
        );
    }
}

#[test]
fn each_predicate_refuses_an_empty_line_and_the_program_alone() {
    for (predicate, words) in PREDICATES {
        assert!(!predicate(&[]), "{words:?}");
        assert!(!predicate(&line(&["mxv"])), "{words:?}");
    }
}

#[test]
fn each_predicate_refuses_its_first_word_alone() {
    for (predicate, [first, _]) in PREDICATES {
        assert!(!predicate(&line(&["mxv", first])), "{first}");
    }
}

#[test]
fn each_predicate_refuses_its_words_in_the_wrong_order() {
    for (predicate, [first, second]) in PREDICATES {
        assert!(
            !predicate(&line(&["mxv", second, first])),
            "{first} {second}"
        );
    }
}

#[test]
fn each_predicate_refuses_its_words_after_an_extra_leading_word() {
    for (predicate, [first, second]) in PREDICATES {
        assert!(
            !predicate(&line(&["mxv", "extra", first, second])),
            "{first} {second}"
        );
    }
}

#[test]
fn each_predicate_refuses_a_different_second_word() {
    for (predicate, [first, second]) in PREDICATES {
        assert!(
            !predicate(&line(&["mxv", first, "other"])),
            "{first} {second}"
        );
    }
}

#[test]
fn each_predicate_refuses_the_words_of_the_other_two() {
    for (index, (predicate, words)) in PREDICATES.iter().enumerate() {
        for (other, (_, [first, second])) in PREDICATES.iter().enumerate() {
            if other != index {
                assert!(
                    !predicate(&line(&["mxv", first, second])),
                    "{words:?} accepted {first} {second}"
                );
            }
        }
    }
}

#[test]
fn each_predicate_ignores_the_program_and_refuses_a_second_word_that_is_not_utf_8() {
    use std::os::unix::ffi::OsStringExt as _;

    for (predicate, [first, second]) in PREDICATES {
        let program = OsString::from_vec(b"/bin/\xffmxv".to_vec());
        assert!(
            predicate(&[program.clone(), first.into(), second.into()]),
            "{first} {second}"
        );
        let word = OsString::from_vec(b"\xff".to_vec());
        assert!(!predicate(&[program, first.into(), word]), "{first}");
    }
}
