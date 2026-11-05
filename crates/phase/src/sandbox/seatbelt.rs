//! phase's own Seatbelt rules, which the macOS capability set adds after
//! nono's. The rules are plain text, so this module builds on every
//! platform and its ordering tests run on the Linux pull-request lane too.

use std::path::{Path, PathBuf};

use super::{SandboxSpec, canonical};

/// The prefix that makes nono write a platform rule after its
/// `(deny network*)`. nono 0.81 sorts a platform rule that starts with
/// `(allow network` or `(deny network` into its network section, after
/// the deny, and writes every other platform rule after its filesystem
/// grants. Seatbelt applies the last rule that matches, so a port rule
/// without this prefix would come before the deny and do nothing.
pub(crate) const NETWORK_RULE: &str = "(allow network";

/// The Mach services that a sandboxed process may not look up, which
/// nono's profile allows: the pasteboard, LaunchServices, which `open`
/// asks to start an app, and the launchd domains that `launchctl` submits
/// jobs to.
pub(crate) const DENIED_SERVICES: &[&str] = &[
    "com.apple.pasteboard.1",
    "com.apple.coreservices.launchservicesd",
    "com.apple.lsd.mapdb",
    "com.apple.lsd.modifydb",
    "com.apple.coreservices.quarantine-resolver",
];

/// The symbolic links at the root whose own metadata a sandboxed process
/// may read. Seatbelt checks the metadata of every name of a path as the
/// kernel resolves it, so a process that reads
/// `/var/select/developer_dir`, as `xcode-select` and `xcrun` do, needs the
/// `/var` link itself as well as the `/private/var/select` that it leads
/// to. nono grants a link's metadata only when a granted path or a `PATH`
/// entry is spelt through it.
const ROOT_LINKS: &[&str] = &["/var"];

/// The terminal devices of macOS that no sandboxed process may open or
/// use: each pseudo-terminal's slave, `/dev/ttysNNN`, and the old BSD
/// pseudo-terminals. `/dev/tty`, which names only the process's own
/// controlling terminal, and `/dev/ptmx`, which opens a new pseudo-terminal
/// of the process's own, stay allowed.
pub(crate) const TERMINAL_DEVICES: &str = "^/dev/(ttys[0-9]+|pty[a-z][0-9a-f]+|tty[p-w][0-9a-f])$";

/// Returns phase's own Seatbelt rules for `spec`, in the order they apply.
///
/// The filesystem rules deny the spec's protected paths, deny writes to
/// the paths it may only read, allow again the grants that lie inside a
/// denied path, and allow the metadata of [`ROOT_LINKS`] and of the spec's
/// `metadata` paths. A denied path
/// that lies inside a grant that the rules allow again, such as the
/// `private` directory of an OpenSSL directory inside `/usr/local/etc`, is
/// denied after that allow, because Seatbelt applies the last rule that
/// matches. The terminal devices of [`TERMINAL_DEVICES`] are denied
/// after every filesystem allow. The Mach rules deny [`DENIED_SERVICES`] and sending Apple
/// events. The network rules allow the loopback ports, and each starts
/// with [`NETWORK_RULE`].
pub(crate) fn rules(spec: &SandboxSpec) -> Vec<String> {
    let quoted = |path: &Path| format!("(subpath \"{}\")", canonical(path).display());
    let deny_rule = |path: &Path| format!("(deny file-read-data file-write* {})", quoted(path));
    let inside_deny = |path: &Path| {
        let path = canonical(path);
        spec.deny.iter().any(|deny| path.starts_with(deny))
    };
    let allowed_again: Vec<PathBuf> = spec
        .read
        .iter()
        .chain(&spec.write)
        .filter(|path| inside_deny(path))
        .map(|path| canonical(path))
        .collect();
    let (nested, outer): (Vec<&PathBuf>, Vec<&PathBuf>) = spec.deny.iter().partition(|path| {
        let path = canonical(path);
        allowed_again
            .iter()
            .any(|grant| path.starts_with(grant) && path != *grant)
    });
    let mut rules: Vec<String> = outer.into_iter().map(|path| deny_rule(path)).collect();
    for path in &spec.read_only {
        rules.push(format!("(deny file-write* {})", quoted(path)));
    }
    for path in spec.read.iter().filter(|path| inside_deny(path)) {
        rules.push(format!("(allow file-read-data {})", quoted(path)));
    }
    for path in spec.write.iter().filter(|path| inside_deny(path)) {
        rules.push(format!(
            "(allow file-read-data file-write* {})",
            quoted(path)
        ));
    }
    rules.extend(nested.into_iter().map(|path| deny_rule(path)));
    for link in ROOT_LINKS {
        rules.push(format!("(allow file-read-metadata (literal \"{link}\"))"));
    }
    for path in &spec.metadata {
        rules.push(format!(
            "(allow file-read-metadata (literal \"{}\"))",
            path.display()
        ));
    }
    for service in DENIED_SERVICES {
        rules.push(format!("(deny mach-lookup (global-name \"{service}\"))"));
    }
    // nono's base profile allows `ioctl` on every terminal device, and a
    // later grant could reach `/dev`, so the profile denies the terminals
    // of other sessions outright: a process that opened one could read
    // what the user types there, or push input into it.
    rules.push(format!(
        "(deny file-read* file-write* file-ioctl (regex #\"{TERMINAL_DEVICES}\"))"
    ));
    rules.push("(deny appleevent-send)".to_owned());
    rules.push("(deny lsopen)".to_owned());
    if !spec.listen.is_empty() || !spec.connect.is_empty() {
        rules
            .push("(allow system-socket (socket-domain AF_INET) (socket-type SOCK_STREAM))".into());
        rules.push(
            "(allow system-socket (socket-domain AF_INET6) (socket-type SOCK_STREAM))".into(),
        );
    }
    for port in &spec.listen {
        rules.push(format!(
            "{NETWORK_RULE}-bind (local tcp \"localhost:{port}\"))"
        ));
        // Seatbelt's `localhost` matches every address of this machine,
        // and an inbound rule that names the remote side also refuses the
        // listen, so the rule names only the local port. A process that
        // listens on every address therefore accepts any peer, which
        // `SEATBELT_PARTIAL` says.
        rules.push(format!(
            "{NETWORK_RULE}-inbound (local tcp \"localhost:{port}\"))"
        ));
    }
    for port in &spec.connect {
        rules.push(format!(
            "{NETWORK_RULE}-outbound (remote tcp \"localhost:{port}\"))"
        ));
    }
    rules
}
