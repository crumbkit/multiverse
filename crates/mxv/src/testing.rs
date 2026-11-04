//! Helpers for the unit tests.

use std::path::Path;

/// Writes `text` to the file at `path`, as a person editing a config file
/// by hand would.
pub(crate) fn write(path: impl AsRef<Path>, text: impl AsRef<[u8]>) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the tests write files that a person would edit by hand"
    )]
    std::fs::write(path, text).unwrap();
}
