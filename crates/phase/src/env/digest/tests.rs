use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::process::Command;
use std::time::SystemTime;

use super::{DigestKey, KEY_FILE, load_key};
use crate::env::AppliedEnvironment;

/// An environment that holds its values in a map, and writes each value
/// when asked.
struct Values(BTreeMap<String, String>);

impl Values {
    fn of(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        )
    }
}

impl AppliedEnvironment for Values {
    fn names(&self) -> BTreeSet<String> {
        self.0.keys().cloned().collect()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}

    fn write_value(&self, name: &str, out: &mut dyn io::Write) -> io::Result<()> {
        let value = self.0.get(name).ok_or(io::ErrorKind::NotFound)?;
        out.write_all(value.as_bytes())
    }
}

/// An environment that predates `write_value`.
struct Unwritable;

impl AppliedEnvironment for Unwritable {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::from(["MODE".to_owned()])
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}
}

fn key(byte: u8) -> DigestKey {
    DigestKey::from_bytes([byte; 32])
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_changed_value_changes_the_digest_and_an_unchanged_value_keeps_it() {
    let wanted = names(&["SERVICES"]);
    let s3 = Values::of(&[("SERVICES", "s3")]);
    let again = Values::of(&[("SERVICES", "s3")]);
    let sqs = Values::of(&[("SERVICES", "s3,sqs")]);

    let before = s3.digest_of(&wanted, &key(1)).unwrap();

    assert_eq!(again.digest_of(&wanted, &key(1)).unwrap(), before);
    assert_ne!(sqs.digest_of(&wanted, &key(1)).unwrap(), before);
}

#[test]
fn the_value_of_a_variable_that_the_names_leave_out_never_changes_the_digest() {
    let wanted = names(&["SERVICES"]);
    let one = Values::of(&[("SERVICES", "s3"), ("TOKEN", "first secret")]);
    let other = Values::of(&[("SERVICES", "s3"), ("TOKEN", "second secret")]);

    assert_eq!(
        one.digest_of(&wanted, &key(1)).unwrap(),
        other.digest_of(&wanted, &key(1)).unwrap()
    );
}

#[test]
fn another_key_gives_another_digest_of_the_same_values() {
    let wanted = names(&["SERVICES"]);
    let values = Values::of(&[("SERVICES", "s3")]);

    assert_ne!(
        values.digest_of(&wanted, &key(1)).unwrap(),
        values.digest_of(&wanted, &key(2)).unwrap()
    );
}

#[test]
fn the_encoding_tells_a_name_from_its_value_wherever_their_bytes_divide() {
    let one = Values::of(&[("a", "bc")]);
    let other = Values::of(&[("ab", "c")]);

    assert_ne!(
        one.digest_of(&names(&["a"]), &key(1)).unwrap(),
        other.digest_of(&names(&["ab"]), &key(1)).unwrap()
    );
    let two = Values::of(&[("A", "x"), ("B", "")]);
    let joined = Values::of(&[("A", "x\0B"), ("B", "")]);
    assert_ne!(
        two.digest_of(&names(&["A", "B"]), &key(1)).unwrap(),
        joined.digest_of(&names(&["A", "B"]), &key(1)).unwrap()
    );
}

#[test]
fn an_absent_variable_gives_another_digest_than_an_empty_one() {
    let wanted = names(&["LANG"]);
    let absent = Values::of(&[]);
    let empty = Values::of(&[("LANG", "")]);

    assert_ne!(
        absent.digest_of(&wanted, &key(1)).unwrap(),
        empty.digest_of(&wanted, &key(1)).unwrap()
    );
}

#[test]
fn the_digest_is_hmac_sha256_over_the_documented_encoding() {
    use hmac::{Hmac, KeyInit as _, Mac as _};
    let values = Values::of(&[("A", "x")]);
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&[7; 32]).unwrap();
    mac.update(b"phase/env-digest/v1\0");
    mac.update(&1_u64.to_be_bytes());
    mac.update(&1_u64.to_be_bytes());
    mac.update(b"A");
    mac.update(&[1]);
    mac.update(&1_u64.to_be_bytes());
    mac.update(b"x");
    let expected: [u8; 32] = mac.finalize().into_bytes().into();

    let digest = values.digest_of(&names(&["A"]), &key(7)).unwrap();

    assert_eq!(digest.bytes(), &expected);
}

#[test]
fn an_environment_that_cannot_write_a_value_gives_no_digest() {
    let error = Unwritable
        .digest_of(&names(&["MODE"]), &key(1))
        .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("MODE"), "{error}");
}

#[test]
fn a_key_and_a_digest_print_none_of_their_bytes() {
    let printed = format!("{:?}", key(0xab));
    assert!(!printed.contains("ab"), "{printed}");
    assert!(!printed.contains("171"), "{printed}");
}

#[test]
fn the_first_load_creates_a_key_file_that_only_its_owner_reads_and_later_loads_read_it_back() {
    let state = tempfile::tempdir().unwrap();

    let first = load_key(state.path()).unwrap();
    let second = load_key(state.path()).unwrap();

    let metadata = std::fs::metadata(state.path().join(KEY_FILE)).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.len(), 32);
    let values = Values::of(&[("A", "x")]);
    assert_eq!(
        values.digest_of(&names(&["A"]), &first).unwrap(),
        values.digest_of(&names(&["A"]), &second).unwrap()
    );
    let leftovers: Vec<_> = std::fs::read_dir(state.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from(KEY_FILE)]);
}

#[test]
fn two_state_directories_get_different_keys() {
    let one = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let values = Values::of(&[("A", "x")]);

    assert_ne!(
        values
            .digest_of(&names(&["A"]), &load_key(one.path()).unwrap())
            .unwrap(),
        values
            .digest_of(&names(&["A"]), &load_key(other.path()).unwrap())
            .unwrap()
    );
}

#[test]
fn a_key_file_that_others_can_read_is_refused_and_left_alone() {
    let state = tempfile::tempdir().unwrap();
    load_key(state.path()).unwrap();
    let path = state.path().join(KEY_FILE);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let error = load_key(state.path()).unwrap_err();

    let reason = std::error::Error::source(&error).unwrap().to_string();
    assert!(reason.contains("0644"), "{reason}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn a_key_file_that_is_a_symbolic_link_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    load_key(elsewhere.path()).unwrap();
    symlink(elsewhere.path().join(KEY_FILE), state.path().join(KEY_FILE)).unwrap();

    let error = load_key(state.path()).unwrap_err();

    let reason = std::error::Error::source(&error).unwrap().to_string();
    assert!(reason.contains("symbolic link"), "{reason}");
}

#[test]
fn a_key_file_of_the_wrong_length_is_refused() {
    let state = tempfile::tempdir().unwrap();
    load_key(state.path()).unwrap();
    let path = state.path().join(KEY_FILE);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test lengthens a key file of its own temporary state directory"
    )]
    let file = std::fs::OpenOptions::new().append(true).open(&path);
    io::Write::write_all(&mut file.unwrap(), b"more").unwrap();

    let error = load_key(state.path()).unwrap_err();

    let reason = std::error::Error::source(&error).unwrap().to_string();
    assert!(reason.contains("36 bytes"), "{reason}");
}

#[test]
fn a_key_file_owned_by_another_user_is_refused() {
    let uid = rustix::process::geteuid().as_raw();
    let state = tempfile::tempdir().unwrap();
    load_key(state.path()).unwrap();
    let metadata = std::fs::metadata(state.path().join(KEY_FILE)).unwrap();

    let refusal = super::check_key_file(&metadata, uid + 1).unwrap_err();

    assert!(refusal.to_string().contains("belongs to"), "{refusal}");
    super::check_key_file(&metadata, uid).unwrap();
}
