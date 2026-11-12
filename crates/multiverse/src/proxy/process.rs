//! The programs the proxy runs itself: `caddy version`, to check the
//! program it found, and `caddy trust`, which the person runs through
//! `mxv proxy trust`. Caddy's server process is phase's, not this module's.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt as _;

/// How long `caddy version` may take.
const VERSION_WAIT: Duration = Duration::from_secs(10);

/// The oldest Caddy that mxv drives: the first that `caddy trust` can send
/// to an admin socket and that keeps the JSON mxv renders.
pub(super) const OLDEST: (u32, u32) = (2, 8);

/// Returns the first file named `caddy` in `dirs` that is executable.
pub(super) fn find(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join("caddy"))
        .find(|candidate| is_executable(candidate))
}

/// Reports whether `path` is a file that its owner can run.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// What `caddy version` said, or why it said nothing.
pub(super) enum Version {
    /// The program printed a version, such as `2.10.0`.
    Found(String),
    /// The program could not run, or printed nothing.
    Unusable(String),
}

/// Runs `caddy version` and returns the version it prints. A version
/// prints as `v2.10.0 h1:...`, and the result drops the `v` and the hash.
pub(super) fn version(caddy: &Path) -> Version {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/proxy/process.rs runs caddy to read its version and owns the deadline (PROC-1)"
    )]
    let spawned = Command::new(caddy)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/proxy/process.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = match spawned {
        Ok(child) => child,
        Err(error) => return Version::Unusable(error.to_string()),
    };
    let finished = child.wait_timeout(VERSION_WAIT);
    let ended = matches!(finished, Ok(Some(status)) if status.success());
    if !ended {
        let _ = child.kill();
        let _ = child.wait();
        let mut said = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut said);
        }
        let said = said.lines().next().unwrap_or("it did not print a version");
        return Version::Unusable(said.trim().to_owned());
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    out.split_whitespace()
        .next()
        .map(|word| word.trim_start_matches('v').to_owned())
        .filter(|word| !word.is_empty())
        .map_or_else(
            || Version::Unusable("it did not print a version".to_owned()),
            Version::Found,
        )
}

/// Returns the major and minor numbers of the version `text`, such as
/// `2.10.0`, or `None` when it has none.
pub(super) fn major_minor(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split(['.', '-', '+']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Runs `caddy trust --address unix//<socket>` with the terminal of the
/// caller, so the system can ask for a password.
pub(super) fn trust(caddy: &Path, socket: &Path) -> std::io::Result<ExitStatus> {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/proxy/process.rs runs caddy trust for the person at a terminal (PROC-1)"
    )]
    Command::new(caddy)
        .arg("trust")
        .arg("--address")
        .arg(format!("unix/{}", socket.display()))
        .status()
}

/// Reports whether this machine trusts the root certificate at
/// `certificate`: `Some` on macOS, which `security verify-cert` answers,
/// and `None` elsewhere.
pub(super) fn trusted(certificate: &Path) -> Option<bool> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/proxy/process.rs asks the system whether it trusts the root (PROC-1)"
    )]
    let spawned = Command::new("/usr/bin/security")
        .args(["verify-cert", "-L", "-c"])
        .arg(certificate)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/proxy/process.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = spawned.ok()?;
    match child.wait_timeout(VERSION_WAIT) {
        Ok(Some(status)) => Some(status.success()),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}
