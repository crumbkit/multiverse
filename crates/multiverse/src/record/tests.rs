//! The tests of the record form's host data and its version rule.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use super::{
    Checkout, HOST_RECORD_VERSION, HostData, HostKey, ID_RECORD_VERSION, MOST_HOST_KEY_BYTES, Made,
    Workspace,
};
use crate::host::HostName;
use crate::name::WorkspaceName;
use crate::store::Store;

fn workspace(name: &str) -> Workspace {
    let name: WorkspaceName = name.parse().unwrap();
    let path = PathBuf::from("/home/agent/work").join(name.as_str());
    let checkout = Checkout::new(
        "app".to_owned(),
        &PathBuf::from("/home/agent/src/app"),
        path.join("app"),
        name.to_string(),
        "refs/remotes/origin/main".to_owned(),
        Made::Worktree,
    );
    Workspace::new(name, path, vec![checkout]).of_worktrees()
}

fn data() -> HostData {
    let mut values = BTreeMap::new();
    values.insert(HostKey::new("runner").unwrap(), "local".to_owned());
    HostData::new(HostName::new("example").unwrap(), values)
}

#[test]
fn a_record_with_host_data_is_written_at_version_5_and_one_without_keeps_its_version() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let mut plain = workspace("plain");
    store.create(&mut plain).unwrap();
    let mut hosted = workspace("hosted");
    store.create(&mut hosted).unwrap();
    hosted.host = Some(data());
    store.update(&mut hosted).unwrap();
    store.update(&mut plain).unwrap();

    let plain = store.read(&"plain".parse().unwrap()).unwrap().unwrap();
    assert_eq!(plain.version, ID_RECORD_VERSION);
    assert_eq!(plain.host, None);
    let text = fs::read_to_string(dir.path().join("plain.json")).unwrap();
    assert!(!text.contains("\"host\""), "{text}");

    let hosted = store.read(&"hosted".parse().unwrap()).unwrap().unwrap();
    assert_eq!(hosted.version, HOST_RECORD_VERSION);
    assert_eq!(hosted.host, Some(data()));
    assert_eq!(
        hosted.owner().map(HostName::as_str),
        Some("example"),
        "the record names its owner"
    );
}

#[test]
fn a_host_key_holds_only_lowercase_letters_digits_and_three_marks() {
    assert!(HostKey::new("release.channel_2-a").is_ok());
    for bad in [
        "",
        "Runner",
        "a b",
        "a/b",
        &"k".repeat(MOST_HOST_KEY_BYTES + 1),
    ] {
        assert!(HostKey::new(bad).is_err(), "{bad:?}");
    }
    let refused: Result<HostData, _> =
        serde_json::from_str(r#"{"profile": "example", "values": {"Bad": "x"}}"#);
    assert!(refused.is_err());
    let refused: Result<HostData, _> = serde_json::from_str(r#"{"profile": "Not A Host"}"#);
    assert!(refused.is_err());
}
