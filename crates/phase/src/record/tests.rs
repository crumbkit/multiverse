use super::{ContainerId, Handle, SweepOutcome, Swept, SweptItem};

#[test]
fn a_sweep_result_survives_a_trip_through_json() {
    let id: ContainerId = "ab".repeat(32).try_into().unwrap();
    let swept = Swept::new(vec![
        SweptItem::new(
            "api".parse().unwrap(),
            "server".parse().unwrap(),
            Handle::pid(4242, 1_793_000_000_000_000),
            SweepOutcome::PidReused,
        ),
        SweptItem::new(
            "db".parse().unwrap(),
            "server".parse().unwrap(),
            Handle::container(id),
            SweepOutcome::Stopped,
        ),
    ]);

    let json = serde_json::to_string(&swept).unwrap();

    assert_eq!(serde_json::from_str::<Swept>(&json).unwrap(), swept);
}

#[test]
fn a_container_id_is_64_lowercase_hexadecimal_digits() {
    assert!(ContainerId::try_from("ab".repeat(32)).is_ok());
    assert!(ContainerId::try_from("AB".repeat(32)).is_err());
    assert!(ContainerId::try_from("ab".repeat(31)).is_err());
}
