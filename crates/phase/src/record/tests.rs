use super::{
    ContainerId, DockerName, Handle, ObjectKind, Role, SweepOutcome, Swept, SweptItem, SweptObject,
};

#[test]
fn a_sweep_result_survives_a_trip_through_json() {
    let id: ContainerId = "ab".repeat(32).try_into().unwrap();
    let swept = Swept::new(
        vec![
            SweptItem::new(
                "api".parse().unwrap(),
                "server".parse().unwrap(),
                Role::Keeper,
                Some(Handle::pid(4242, 1_793_000_000_000_000)),
                SweepOutcome::PidReused,
            ),
            SweptItem::new(
                "db".parse().unwrap(),
                "server".parse().unwrap(),
                Role::Process,
                Some(Handle::container(id)),
                SweepOutcome::Stopped,
            ),
        ],
        vec![SweptObject::new(
            ObjectKind::Volume,
            "mxv-0b9f3c8e-data".parse().unwrap(),
            SweepOutcome::Kept,
        )],
    );

    let json = serde_json::to_string(&swept).unwrap();

    assert_eq!(serde_json::from_str::<Swept>(&json).unwrap(), swept);
}

#[test]
fn a_container_id_is_64_lowercase_hexadecimal_digits() {
    assert!(ContainerId::try_from("ab".repeat(32)).is_ok());
    assert!(ContainerId::try_from("AB".repeat(32)).is_err());
    assert!(ContainerId::try_from("ab".repeat(31)).is_err());
}

#[test]
fn a_docker_name_follows_dockers_rule() {
    assert!("mxv-0b9f3c8e_db.1".parse::<DockerName>().is_ok());
    assert!("".parse::<DockerName>().is_err());
    assert!("-db".parse::<DockerName>().is_err());
    assert!("db/server".parse::<DockerName>().is_err());
    assert!("a".repeat(129).parse::<DockerName>().is_err());
}

#[test]
fn a_linux_start_time_is_field_22_of_stat_counted_after_the_command_name() {
    let stat =
        "4242 (my (odd) app) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10";

    assert_eq!(super::process::stat_start_ticks(stat), Some(98_765));
    assert_eq!(super::process::stat_start_ticks("4242 (cut"), None);
}

#[test]
fn the_parent_and_group_of_a_linux_stat_are_fields_4_and_5_counted_after_the_command_name() {
    let line = "4242 (my (odd) app) S 1 4200 4200 0 -1 4194560\n";
    assert_eq!(super::process::stat_field::<u32>(line, 4), Some(1));
    assert_eq!(super::process::stat_field::<u32>(line, 5), Some(4200));
    assert_eq!(super::process::stat_field::<u32>("4242 (short) S", 5), None);
    assert_eq!(super::process::stat_field::<u32>(line, 2), None);
}

#[test]
fn a_bsd_info_gives_its_start_as_seconds_and_microseconds() {
    let mut info = [0_u8; 136];
    info[120..128].copy_from_slice(&1_793_809_711_u64.to_ne_bytes());
    info[128..136].copy_from_slice(&5_u64.to_ne_bytes());
    assert_eq!(
        super::process::bsd_info_since(&info),
        Some(super::process::RawStart::Since {
            seconds: 1_793_809_711,
            microseconds: 5
        })
    );
    assert_eq!(super::process::bsd_info_since(&info[..130]), None);
}

#[test]
fn the_raw_start_of_this_process_is_readable_and_a_free_pid_has_none() {
    let me = super::process::raw_start(std::process::id()).unwrap();
    assert!(
        matches!(
            me,
            super::process::RawStart::Ticks(_) | super::process::RawStart::Since { .. }
        ),
        "{me:?}"
    );
    assert_eq!(
        super::process::raw_start(0x7fff_fff0).unwrap(),
        super::process::RawStart::Absent
    );
}

#[test]
fn a_process_table_failure_keeps_its_message_and_an_unsupported_struct_names_its_sizes() {
    use super::process::ProcessError;
    let table = ProcessError::table("cannot read /proc: denied".to_owned());
    assert_eq!(table.to_string(), "cannot read /proc: denied");
    let unsupported = ProcessError::Unsupported {
        call: "proc_pidinfo(PROC_PIDTBSDINFO)",
        pid: 42,
        written: 120,
        expected: 136,
    };
    assert_eq!(
        unsupported.to_string(),
        "proc_pidinfo(PROC_PIDTBSDINFO) for the process 42 wrote 120 bytes, and phase reads a struct of 136"
    );
    assert!(matches!(
        crate::error::PhaseError::from(unsupported),
        crate::error::PhaseError::ProcessTable { message } if message.contains("wrote 120 bytes")
    ));
}

#[test]
fn a_signal_to_a_pid_that_names_no_process_or_group_fails_and_no_such_group_exists() {
    use rustix::process::Signal;
    for pid in [0, u32::MAX] {
        assert_eq!(
            super::process::signal_group(pid, Signal::TERM)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            super::process::signal_pid(pid, Signal::TERM)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!super::process::group_exists(pid));
    }
    assert!(super::process::signal_pid(0x7fff_fff0, Signal::TERM).is_err());
    assert!(!super::process::group_exists(0x7fff_fff0));
    assert!(super::process::group_exists(
        u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap()
    ));
}

#[test]
fn clock_ticks_become_microseconds() {
    assert_eq!(
        super::process::ticks_to_micros(98_765, 100),
        Some(987_650_000)
    );
    assert_eq!(super::process::ticks_to_micros(1, 0), None);
    assert_eq!(super::process::ticks_to_micros(u64::MAX, 100), None);
}

#[test]
fn a_macos_start_time_joins_the_seconds_and_microseconds_of_proc_bsdinfo() {
    let mut info = [0_u8; 136];
    info[120..128].copy_from_slice(&1_793_809_711_u64.to_ne_bytes());
    info[128..136].copy_from_slice(&139_785_u64.to_ne_bytes());

    assert_eq!(
        super::process::bsd_info_start(&info),
        Some(1_793_809_711_139_785)
    );
    assert_eq!(super::process::bsd_info_start(&info[..130]), None);
}

#[test]
fn the_start_time_of_this_process_is_readable_and_a_free_pid_has_none() {
    let me = std::process::id();

    assert!(matches!(
        super::process::start_time(me).unwrap(),
        super::process::Start::At(_)
    ));
    assert_eq!(
        super::process::start_time(0x7fff_fff0).unwrap(),
        super::process::Start::Absent
    );
}
