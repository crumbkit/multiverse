//! Writes a `Duration` in JSON as a whole number of milliseconds, which a
//! field names with an `_ms` suffix. Reading and writing drop any part of a
//! millisecond.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serializer};

/// Writes `duration` as whole milliseconds.
///
/// # Errors
///
/// Returns the serializer's error when the duration holds more
/// milliseconds than a `u64` holds.
pub(crate) fn serialize<S: Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let millis = u64::try_from(duration.as_millis()).map_err(serde::ser::Error::custom)?;
    serializer.serialize_u64(millis)
}

/// Reads whole milliseconds as a `Duration`.
///
/// # Errors
///
/// Returns the deserializer's error when the value is not a whole number
/// that a `u64` holds.
pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    u64::deserialize(deserializer).map(Duration::from_millis)
}
