//! The programs the proxy runs itself: `caddy version`, to check the
//! program it found, and `caddy trust`, which the person runs through
//! `mxv proxy trust`. Caddy's server process is phase's, not this module's.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::Duration;

use crumbkit_process::{Capture, OverLimit, RunError, RunOptions, Scope};

use crate::process::{deadline_in, ran_to_its_end};

/// How long `caddy version` and `security verify-cert` may take.
const VERSION_WAIT: Duration = Duration::from_secs(10);

/// The most bytes of `caddy version`'s standard output that mxv keeps.
const MOST_VERSION_BYTES: usize = 64 * 1024;

/// The most bytes of `caddy version`'s standard error that mxv keeps, of
/// which a failure quotes the first line.
const MOST_MESSAGE_BYTES: usize = 4 * 1024;

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

/// A Caddy version that has a numeric major and minor, such as `2.10.0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CaddyVersion {
    /// The major number.
    pub(super) major: u32,
    /// The minor number.
    pub(super) minor: u32,
    /// The version as Caddy printed it, without the `v` and the hash.
    text: String,
}

impl CaddyVersion {
    /// Parses `word`, such as `2.10.0` or `2.8.0-beta.1`, or returns `None`
    /// when it has no numeric major and minor.
    pub(super) fn parse(word: &str) -> Option<Self> {
        let mut parts = word.split(['.', '-', '+']);
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some(Self {
            major,
            minor,
            text: word.to_owned(),
        })
    }

    /// Returns the major and minor numbers.
    pub(super) fn major_minor(&self) -> (u32, u32) {
        (self.major, self.minor)
    }
}

impl std::fmt::Display for CaddyVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// What `caddy version` said, or why it said nothing usable.
#[derive(Debug)]
pub(super) enum Version {
    /// The program printed a version with a major and minor number.
    Found(CaddyVersion),
    /// The program could not run, printed nothing, or printed a version
    /// without a major and minor number.
    Unusable(String),
}

/// Runs `caddy version` and returns the version it prints. A version
/// prints as `v2.10.0 h1:...`, and the result drops the `v` and the hash.
///
/// Caddy stays in mxv's process group and is killed alone when it misses
/// [`VERSION_WAIT`]. A process that it leaves holding its output holds mxv
/// for at most [`crumbkit_process::DRAIN_GRACE`] after it exits.
pub(super) fn version(caddy: &Path) -> Version {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/proxy/process.rs runs caddy to read its version and owns the deadline (PROC-1)"
    )]
    let mut command = Command::new(caddy);
    command.arg("version");
    let options = RunOptions::new()
        .stdout(Capture::whole(MOST_VERSION_BYTES).over(OverLimit::Sink))
        .stderr(Capture::whole(MOST_MESSAGE_BYTES).over(OverLimit::Sink))
        .scope(Scope::Child)
        .deadline(deadline_in(VERSION_WAIT));
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/proxy/process.rs runs caddy to read its version and owns the deadline (PROC-1)"
    )]
    let finished = match crumbkit_process::run(command, &options) {
        Ok(finished) => finished,
        Err(RunError::Start(error)) => return Version::Unusable(error.to_string()),
        Err(_) => return Version::Unusable("it did not print a version".to_owned()),
    };
    let ended = ran_to_its_end(&finished).is_some_and(|status| status.success());
    let (stdout, stderr) = finished.into_output();
    if !ended {
        let said = String::from_utf8_lossy(stderr.bytes());
        let said = said.lines().next().unwrap_or("it did not print a version");
        return Version::Unusable(said.trim().to_owned());
    }
    let out = String::from_utf8_lossy(stdout.bytes());
    let Some(word) = out
        .split_whitespace()
        .next()
        .map(|word| word.trim_start_matches('v'))
        .filter(|word| !word.is_empty())
    else {
        return Version::Unusable("it did not print a version".to_owned());
    };
    CaddyVersion::parse(word).map_or_else(
        || {
            Version::Unusable(format!(
                "it printed `{word}`, which has no major and minor version"
            ))
        },
        Version::Found,
    )
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
    let mut command = Command::new("/usr/bin/security");
    command.args(["verify-cert", "-L", "-c"]).arg(certificate);
    let options = RunOptions::new()
        .scope(Scope::Child)
        .deadline(deadline_in(VERSION_WAIT));
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/proxy/process.rs asks the system whether it trusts the root (PROC-1)"
    )]
    let finished = crumbkit_process::run(command, &options).ok()?;
    ran_to_its_end(&finished).map(|status| status.success())
}
