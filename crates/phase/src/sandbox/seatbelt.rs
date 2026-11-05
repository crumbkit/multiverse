//! phase's own Seatbelt rules, which the macOS capability set adds after
//! nono's. The rules are plain text, so this module builds on every
//! platform and its ordering tests run on the Linux pull-request lane too.

use std::path::Path;

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

/// Returns phase's own Seatbelt rules for `spec`, in the order they apply.
///
/// The filesystem rules deny the spec's protected paths, deny writes to
/// the paths it may only read, and then allow again the grants that lie
/// inside a denied path. The Mach rules deny [`DENIED_SERVICES`] and
/// sending Apple events. The network rules allow the loopback ports, and
/// each starts with [`NETWORK_RULE`].
pub(crate) fn rules(spec: &SandboxSpec) -> Vec<String> {
    let quoted = |path: &Path| format!("(subpath \"{}\")", canonical(path).display());
    let mut rules = Vec::new();
    for path in &spec.deny {
        rules.push(format!(
            "(deny file-read-data file-write* {})",
            quoted(path)
        ));
    }
    for path in &spec.read_only {
        rules.push(format!("(deny file-write* {})", quoted(path)));
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
    for service in DENIED_SERVICES {
        rules.push(format!("(deny mach-lookup (global-name \"{service}\"))"));
    }
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
