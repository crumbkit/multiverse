//! Turns a [`SandboxSpec`] into nono's capability set, which the launcher
//! applies.

use std::path::Path;

use nono::{AccessMode, CapabilitySet};

use super::SandboxSpec;

/// The files under `/dev` that a sandboxed process may also write.
const DEV_WRITE: &[&str] = &["/dev/null", "/dev/zero", "/dev/tty"];

/// Returns the capability set of `spec`: read access to its read paths,
/// read and write access to its write paths, loopback TCP to its ports, and
/// no other network. On macOS it also denies the spec's protected paths
/// after the grants, allows again the grants that lie inside them, and
/// blocks the host's DNS resolver, so the process reaches no other host by
/// name.
///
/// # Errors
///
/// Returns nono's message when a path cannot be granted or a rule is
/// refused.
pub(crate) fn capabilities(spec: &SandboxSpec) -> Result<CapabilitySet, String> {
    let fail = |error: nono::NonoError| error.to_string();
    let mut caps = CapabilitySet::new().block_network();
    for path in &spec.read {
        caps = grant(caps, path, AccessMode::Read).map_err(fail)?;
    }
    for path in &spec.write {
        caps = grant(caps, path, AccessMode::ReadWrite).map_err(fail)?;
    }
    for path in DEV_WRITE.iter().map(Path::new).filter(|path| path.exists()) {
        caps = caps.allow_file(path, AccessMode::ReadWrite).map_err(fail)?;
    }
    platform(caps, spec)
}

/// Grants `access` to `path`, a directory or a file.
fn grant(caps: CapabilitySet, path: &Path, access: AccessMode) -> nono::Result<CapabilitySet> {
    if path.is_dir() {
        caps.allow_path(path, access)
    } else {
        caps.allow_file(path, access)
    }
}

/// Adds the rules of macOS: the `PATH` lookups, the explicit denies and
/// the loopback ports, which phase writes itself because nono's port
/// rules allow binding any port. Every network rule starts with
/// `(allow network`, so nono writes it after its `(deny network*)`.
#[cfg(target_os = "macos")]
fn platform(caps: CapabilitySet, spec: &SandboxSpec) -> Result<CapabilitySet, String> {
    let mut caps = caps.block_dns();
    for dir in &spec.path_dirs {
        caps.add_path_metadata_dir(dir.clone());
    }
    for rule in seatbelt_rules(spec) {
        caps.add_platform_rule(rule)
            .map_err(|error| error.to_string())?;
    }
    Ok(caps)
}

/// Returns phase's own Seatbelt rules for `spec`, in the order they apply.
#[cfg(target_os = "macos")]
pub(crate) fn seatbelt_rules(spec: &SandboxSpec) -> Vec<String> {
    let quoted = |path: &Path| format!("(subpath \"{}\")", canonical(path).display());
    let mut rules = Vec::new();
    for path in &spec.deny {
        rules.push(format!(
            "(deny file-read-data file-write* {})",
            quoted(path)
        ));
    }
    let inside_deny = |path: &Path| {
        let path = canonical(path);
        spec.deny.iter().any(|deny| path.starts_with(deny))
    };
    for path in spec.read.iter().filter(|path| inside_deny(path)) {
        rules.push(format!("(allow file-read-data {})", quoted(path)));
    }
    for path in spec.write.iter().filter(|path| inside_deny(path)) {
        rules.push(format!(
            "(allow file-read-data file-write* {})",
            quoted(path)
        ));
    }
    if !spec.listen.is_empty() || !spec.connect.is_empty() {
        rules
            .push("(allow system-socket (socket-domain AF_INET) (socket-type SOCK_STREAM))".into());
        rules.push(
            "(allow system-socket (socket-domain AF_INET6) (socket-type SOCK_STREAM))".into(),
        );
    }
    for port in &spec.listen {
        rules.push(format!(
            "(allow network-bind (local tcp \"localhost:{port}\"))"
        ));
        rules.push(format!(
            "(allow network-inbound (local tcp \"localhost:{port}\"))"
        ));
    }
    for port in &spec.connect {
        rules.push(format!(
            "(allow network-outbound (remote tcp \"localhost:{port}\"))"
        ));
    }
    rules
}

/// Returns `path` with its symbolic links resolved, as Seatbelt matches
/// it, or `path` when it does not exist.
#[cfg(target_os = "macos")]
fn canonical(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Adds the rules of Linux: Landlock's TCP port rules. Landlock restricts
/// a port on every address, so a process that may connect to a loopback
/// port may also connect to that port on another host, and it does not
/// restrict UDP. Landlock cannot deny a path inside a granted one, so the
/// spec never grants one that reaches a protected path.
#[cfg(target_os = "linux")]
fn platform(caps: CapabilitySet, spec: &SandboxSpec) -> Result<CapabilitySet, String> {
    let mut caps = caps;
    for port in &spec.listen {
        caps.add_tcp_bind_port(*port);
    }
    for port in &spec.connect {
        caps.add_tcp_connect_port(*port);
    }
    Ok(caps)
}
