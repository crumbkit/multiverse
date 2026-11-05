//! Random ids from the operating system's cryptographically secure random
//! number generator, which macOS and Linux serve at `/dev/urandom`.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::Path;

/// The device that serves the operating system's random bytes.
pub const RANDOM_DEVICE: &str = "/dev/urandom";

/// The number of random bytes in an id. Each byte is two hex digits.
const ID_BYTES: usize = 16;

/// Returns 32 lowercase hex digits from [`RANDOM_DEVICE`].
///
/// # Errors
///
/// Returns the error of opening or reading the device.
pub(crate) fn hex_id() -> io::Result<String> {
    let mut bytes = [0_u8; ID_BYTES];
    File::open(Path::new(RANDOM_DEVICE))?.read_exact(&mut bytes)?;
    let mut text = String::with_capacity(ID_BYTES * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    Ok(text)
}

/// Returns `N` bytes from [`RANDOM_DEVICE`].
///
/// # Errors
///
/// Returns the error of opening or reading the device.
pub(crate) fn bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    File::open(Path::new(RANDOM_DEVICE))?.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::hex_id;

    #[test]
    fn a_random_id_is_32_lowercase_hex_digits_and_two_ids_differ() {
        let first = hex_id().unwrap();
        let second = hex_id().unwrap();
        assert_eq!(first.len(), 32);
        assert!(
            first
                .chars()
                .all(|digit| matches!(digit, '0'..='9' | 'a'..='f'))
        );
        assert_ne!(first, second);
    }
}
