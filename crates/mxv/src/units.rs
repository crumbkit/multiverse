//! The sizes and ages that `mxv clean` and the config file read, and the
//! size that rows print.
//!
//! A size is a whole number followed by a unit: `B`, the binary units
//! `KiB`, `MiB`, `GiB` and `TiB`, which count in powers of 1024, or the
//! decimal units `KB`, `MB`, `GB` and `TB`, which count in powers of 1000,
//! such as `50GiB`. An age is a whole number followed by `w`, `d`, `h`, `m`
//! or `s`, for weeks, days, hours, minutes and seconds, such as `7d`. A
//! space may stand between the number and the unit.

use std::fmt;
use std::time::Duration;

use crumbkit::Shown;
use crumbkit::schemars::{JsonSchema as _, Schema, SchemaGenerator};
use crumbkit::serde::Serializer;

#[cfg(test)]
mod tests;

/// The units of a size, each with the bytes it holds.
const SIZE_UNITS: &[(&str, u64)] = &[
    ("B", 1),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
    ("KB", 1_000),
    ("MB", 1_000_000),
    ("GB", 1_000_000_000),
    ("TB", 1_000_000_000_000),
];

/// The units of an age, each with the seconds it holds.
const AGE_UNITS: &[(&str, u64)] = &[
    ("w", 7 * 24 * 60 * 60),
    ("d", 24 * 60 * 60),
    ("h", 60 * 60),
    ("m", 60),
    ("s", 1),
];

/// The binary units that a size prints in, from the kibibyte up.
const PRINTED_UNITS: &[&str] = &["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// Returns the number and the unit of `text`, a whole number followed by a
/// unit of `units`, or `None`.
fn number_and_unit(text: &str, units: &[(&str, u64)]) -> Option<(u64, u64)> {
    let text = text.trim();
    let split = text.find(|char: char| !char.is_ascii_digit())?;
    let (digits, unit) = text.split_at(split);
    let number: u64 = digits.parse().ok()?;
    let unit = unit.trim_start();
    units
        .iter()
        .find(|(name, _)| *name == unit)
        .map(|(_, scale)| (number, *scale))
}

/// Returns the bytes that `text` names, a size as the module docs
/// describe.
///
/// # Errors
///
/// Returns the reason that `text` is not a size.
pub(crate) fn parse_size(text: &str) -> Result<u64, String> {
    let (number, scale) = number_and_unit(text, SIZE_UNITS).ok_or_else(|| {
        format!(
            "{text:?} is not a size, which is a whole number and a unit, such as 50GiB or 500MB"
        )
    })?;
    number
        .checked_mul(scale)
        .ok_or_else(|| format!("{text:?} is more bytes than mxv can count"))
}

/// Returns the length of time that `text` names, an age as the module
/// docs describe.
///
/// # Errors
///
/// Returns the reason that `text` is not an age.
pub(crate) fn parse_age(text: &str) -> Result<Duration, String> {
    let (number, scale) = number_and_unit(text, AGE_UNITS).ok_or_else(|| {
        format!(
            "{text:?} is not a length of time, which is a whole number and one of w, d, h, m or s, such as 7d"
        )
    })?;
    number
        .checked_mul(scale)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{text:?} is longer than mxv can count"))
}

/// A number of bytes on disk, which prints as `1.5 GiB` in text and as the
/// number of bytes in JSON.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Size(pub(crate) u64);

impl fmt::Display for Size {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1024 {
            return write!(formatter, "{} B", self.0);
        }
        let bytes = u128::from(self.0);
        let mut unit: u128 = 1;
        for name in PRINTED_UNITS {
            unit *= 1024;
            let tenths = (bytes * 10 + unit / 2) / unit;
            if tenths < 10 * 1024 || Some(name) == PRINTED_UNITS.last() {
                return write!(formatter, "{}.{} {name}", tenths / 10, tenths % 10);
            }
        }
        write!(formatter, "{} B", self.0)
    }
}

impl Shown for Size {
    fn json<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }

    fn schema(generator: &mut SchemaGenerator) -> Schema {
        u64::json_schema(generator)
    }
}

crumbkit::shown!(Size);
