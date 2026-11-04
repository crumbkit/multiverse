use std::time::Duration;

use super::{Size, parse_age, parse_size};

const DAY: u64 = 24 * 60 * 60;

#[test]
fn a_size_reads_in_binary_or_decimal_units() {
    assert_eq!(parse_size("50GiB"), Ok(50 * 1024 * 1024 * 1024));
    assert_eq!(parse_size("512 MiB"), Ok(512 * 1024 * 1024));
    assert_eq!(parse_size("2KiB"), Ok(2048));
    assert_eq!(parse_size("1TiB"), Ok(1024_u64.pow(4)));
    assert_eq!(parse_size("3GB"), Ok(3_000_000_000));
    assert_eq!(parse_size("10MB"), Ok(10_000_000));
    assert_eq!(parse_size("700B"), Ok(700));
}

#[test]
fn a_size_without_a_unit_or_with_an_unknown_unit_is_refused() {
    assert!(parse_size("50").is_err());
    assert!(parse_size("50GB2").is_err());
    assert!(parse_size("50gigs").is_err());
    assert!(parse_size("GiB").is_err());
    assert!(parse_size("-1GiB").is_err());
    assert!(parse_size("1.5GiB").is_err());
}

#[test]
fn a_size_too_large_for_64_bits_is_refused() {
    assert!(parse_size("99999999999TiB").is_err());
}

#[test]
fn an_age_reads_in_weeks_days_hours_minutes_or_seconds() {
    assert_eq!(parse_age("7d"), Ok(Duration::from_secs(7 * DAY)));
    assert_eq!(parse_age("2w"), Ok(Duration::from_secs(14 * DAY)));
    assert_eq!(parse_age("36h"), Ok(Duration::from_secs(36 * 60 * 60)));
    assert_eq!(parse_age("90m"), Ok(Duration::from_secs(90 * 60)));
    assert_eq!(parse_age("0s"), Ok(Duration::ZERO));
}

#[test]
fn an_age_without_a_unit_or_with_an_unknown_unit_is_refused() {
    assert!(parse_age("7").is_err());
    assert!(parse_age("7y").is_err());
    assert!(parse_age("d").is_err());
    assert!(parse_age("1d12h").is_err());
}

#[test]
fn a_size_prints_in_the_largest_binary_unit_with_one_decimal() {
    assert_eq!(Size(0).to_string(), "0 B");
    assert_eq!(Size(1023).to_string(), "1023 B");
    assert_eq!(Size(1024).to_string(), "1.0 KiB");
    assert_eq!(Size(1536).to_string(), "1.5 KiB");
    assert_eq!(
        Size(5 * 1024 * 1024 * 1024 + 300 * 1024 * 1024).to_string(),
        "5.3 GiB"
    );
    assert_eq!(Size(u64::MAX).to_string(), "16.0 EiB");
}

#[test]
fn a_size_is_its_number_of_bytes_in_json() {
    assert_eq!(serde_json::to_string(&Size(1536)).unwrap(), "1536");
}

#[test]
fn a_size_just_under_a_unit_rounds_up_into_that_unit() {
    assert_eq!(Size(1024 * 1024 - 1).to_string(), "1.0 MiB");
}
