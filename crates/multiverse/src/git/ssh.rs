//! The environment that keeps git, and the ssh that git starts, from asking
//! a question, and the reading of an ssh refusal from git's standard error.
//!
//! mxv runs git for a person or an agent who may not be at the terminal,
//! so no git command may wait for an answer. `GIT_TERMINAL_PROMPT=0` stops
//! git's own prompt for an HTTPS user name or password. ssh opens the
//! terminal itself, so mxv also sets `SSH_ASKPASS_REQUIRE=force`, which
//! sends every question that ssh would ask, such as a key's passphrase or
//! whether to trust an unknown host key, to the program in `SSH_ASKPASS`,
//! and sets that program to `false`, which answers no question. ssh then
//! fails at once with exit status 255 where it would have waited.
//!
//! The variables leave the SSH command that the person chose in
//! `GIT_SSH_COMMAND`, `core.sshCommand` or `GIT_SSH` as they wrote it, and
//! they reach every ssh that git starts, including one that a
//! `ProxyCommand` starts. ssh has read `SSH_ASKPASS_REQUIRE` since
//! OpenSSH 8.4. An older ssh ignores it and may still wait, until the
//! fetch deadline ends git.

use std::path::Path;
use std::process::Command;

use super::GIT;
use crate::error::{Error, SshRefusal};
use crate::mirror::{host_of, recorded_origin, redacted, scp_like};

#[cfg(test)]
mod tests;

/// The variables that every git command gets, so that neither git nor ssh
/// waits for an answer, and git's messages are in English.
const QUIET: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("SSH_ASKPASS_REQUIRE", "force"),
    ("SSH_ASKPASS", "false"),
    ("LC_ALL", "C"),
];

/// Returns the command `git -C <dir>`, with the variables of [`QUIET`]
/// added to the caller's environment. Every git command of the backend
/// starts here.
pub(super) fn git_command(dir: &Path) -> Command {
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/git/ssh.rs belongs to the module that starts processes (PROC-1)"
    )]
    let mut command = Command::new(GIT);
    command.arg("-C").arg(dir).envs(QUIET);
    command
}

/// The lines that OpenSSH prints when it refuses, each with its refusal,
/// in the order that [`refused`] looks for them. ssh prints both the
/// warning about a changed key and the line about a failed verification
/// when a host key changed, so the warning comes first.
const REFUSALS: [(&str, SshRefusal); 3] = [
    (
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        SshRefusal::HostKeyChanged,
    ),
    (
        "Host key verification failed.",
        SshRefusal::HostKeyUnverified,
    ),
    ("Permission denied (", SshRefusal::AuthenticationFailed),
];

/// The user, host and port of an SSH URL.
struct Target {
    /// The user name, if the URL names one.
    user: Option<String>,
    /// The host name in lower case.
    host: String,
    /// The port, when the URL names one other than 22.
    port: Option<u16>,
}

/// Returns the user, host and port of `url` when git reaches it over ssh:
/// a URL with the scheme `ssh`, `git+ssh` or `ssh+git`, or git's scp-like
/// form, such as `git@github.com:acme/app.git`. Returns `None` for any
/// other URL, such as `https://` or a local path.
fn target(url: &str) -> Option<Target> {
    let authority = if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "ssh" | "git+ssh" | "ssh+git") {
            return None;
        }
        rest.split_once('/')
            .map_or(rest, |(authority, _)| authority)
    } else {
        scp_like(url)?.0
    };
    let user = authority
        .rsplit_once('@')
        .map(|(credential, _)| credential.split(':').next().unwrap_or_default())
        .filter(|user| !user.is_empty())
        .map(str::to_owned);
    let host = host_of(authority, Some("22"));
    // A port follows the last `:` outside the brackets of an IPv6 address.
    let (host, port) = match host.rsplit_once(':') {
        Some((name, port)) if !name.is_empty() && !port.contains(']') => {
            (name.to_owned(), port.parse().ok())
        }
        _ => (host, None),
    };
    Some(Target { user, host, port })
}

/// Returns the refusal that ssh reported in `stderr`, the standard error of
/// a failed fetch from `url`, or `None` when `url` is not an SSH URL or
/// `stderr` holds none of the lines of [`REFUSALS`]. A failure that ssh
/// words differently, such as one from an older ssh, keeps git's own
/// failure.
pub(super) fn refused(url: &str, stderr: &str) -> Option<Error> {
    let Target { user, host, port } = target(url)?;
    let (line, refusal) = REFUSALS.iter().find_map(|(marker, refusal)| {
        stderr
            .lines()
            .find(|line| line.contains(marker))
            .map(|line| (line, *refusal))
    })?;
    // The warning about a changed key sits inside a frame of `@`.
    let message = line.trim().trim_matches('@').trim().to_owned();
    let login = user.map_or_else(|| host.clone(), |user| format!("{user}@{host}"));
    Some(Error::SshRefused {
        remote: recorded_origin(url),
        login,
        port,
        refusal,
        message: redacted(&message),
    })
}
