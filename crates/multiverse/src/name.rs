//! The name of a workspace, which is also the name of its directory and of
//! the branch that each of its checkouts is on.
//!
//! A name holds 1 to [`MOST_NAME_BYTES`] lowercase ASCII letters, digits,
//! `-`, `_` and `.`. It starts with a letter or a digit, holds no `..`, and
//! does not end with `.` or `.lock`. These rules keep the name usable both
//! as a directory name and as a git branch name, so the same text names the
//! workspace, its directory and its branches. Names are lowercase, so two
//! names never refer to the same record on a file system that ignores case,
//! such as the default one on macOS, and a name is never `HEAD`. The type
//! and its rule are phase's, which names the run directories and docker
//! objects of a workspace by it.

pub use phase::{BadName, MOST_NAME_BYTES, WorkspaceName};

#[cfg(test)]
mod tests;

/// Reports whether `name` is a valid name of a manifest's process,
/// container, volume or port: 1 to [`MOST_NAME_BYTES`] ASCII letters,
/// digits, `-` and `_`, starting with a letter or a digit. Such a name
/// never holds a `.`. The rule is phase's, whose processes, ports and
/// volumes take the same names.
pub(crate) fn is_unit_name(name: &str) -> bool {
    phase::is_unit_name(name)
}
