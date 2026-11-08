use crate::engine::container::volumes::app_volume_name;
use crate::name::WorkspaceId;
use crate::phase::Phase;

#[test]
fn the_data_volumes_of_a_workspaces_apps_are_named_after_its_stable_id() {
    let phase = Phase::new("/state/phase".into());
    let id = WorkspaceId::try_from("ws-1".to_owned()).unwrap();
    let key = crate::engine::container::names::VolumeKey::new("data").unwrap();
    let name = app_volume_name(&phase, &id, &key).unwrap();
    assert!(
        name.starts_with("phase-") && name.ends_with(".data"),
        "{name}"
    );
    assert_eq!(name.len(), "phase-".len() + 16 + ".data".len());
}

#[test]
fn a_docker_stats_line_gives_the_container_and_its_resident_bytes() {
    assert_eq!(
        super::stats_line("0123abcd\t12.5MiB / 7.6GiB"),
        Some(("0123abcd".to_owned(), 13_107_200))
    );
    assert_eq!(
        super::stats_line("0123abcd\t512KiB / 1GiB"),
        Some(("0123abcd".to_owned(), 524_288))
    );
    assert_eq!(
        super::stats_line("0123abcd\t1.5GB / 8GB"),
        Some(("0123abcd".to_owned(), 1_500_000_000))
    );
    assert_eq!(
        super::stats_line("0123abcd\t0B / 0B"),
        Some(("0123abcd".to_owned(), 0))
    );
    assert_eq!(super::stats_line("0123abcd\t-- / --"), None);
    assert_eq!(super::stats_line(""), None);
}
