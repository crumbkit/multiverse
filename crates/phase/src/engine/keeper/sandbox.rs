//! The hook where the keeper will apply the operating system's sandbox to
//! its app: Seatbelt on macOS and Landlock on Linux, generated from the
//! app's permissions. A later release fills it in, and until then the app
//! runs with the keeper's own access, which the design's Sandbox section
//! describes as the target.

use std::process::Command;

/// Applies the sandbox to the app's `command` before the keeper starts it.
/// It changes nothing yet, so the app runs unsandboxed.
pub(crate) fn apply_sandbox(command: &mut Command) {
    let _ = command;
}
