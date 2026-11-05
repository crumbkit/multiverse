//! The operating system's sandbox around a host process (decision P9).
//!
//! A host app that declares `[permissions]` runs each of its processes
//! under the sandbox, through the `nono` library: Seatbelt on macOS and
//! Landlock on Linux. An app without permissions runs unsandboxed, and the
//! plan records that, because it writes the app's permissions as `null`.
//! On a platform with no sandbox that phase supports, or a Linux kernel
//! whose Landlock cannot restrict TCP ports, an app with permissions runs
//! unsandboxed too, and `status` shows why. Landlock leaves Unix sockets
//! open, so phase still applies it on Linux but `status` reports the
//! process as unsandboxed ([`LANDLOCK_UNSANDBOXED`]). Seatbelt is reported
//! as a partial sandbox ([`SEATBELT_PARTIAL`]).
//!
//! The engine turns the plan into a [`SandboxSpec`]: the paths the process
//! may read and write, the paths it may only read, the ports it may listen
//! on and connect to, and the paths it is always denied. The spec holds
//! canonical paths and ports, never an environment value. The keeper
//! starts the app through the launcher, the program that links phase run
//! as `<keeper words> --sandboxed <spec> -- <app command>`, which checks
//! the spec again, applies the sandbox to itself and then replaces itself
//! with the app. So every start of the app, including each restart by its
//! keeper, runs under the sandbox, and the app's pid is the launcher's.
//!
//! The spec allows reading the system's own directories, the toolchain
//! directories that `PATH` names (Cargo, rustup, mise and the program
//! directories of Homebrew) and the app's checkout. It allows writing the
//! app's own state directory, `<phase state>/apps/<workspace id>/<app>/state`,
//! its temporary directory `.../tmp`, which becomes the app's `TMPDIR`, and
//! the paths its permissions add. phase refuses to build a spec whose
//! allowed paths cover or lie inside a protected path: the credentials in
//! the home directory, the Keychains, the per-user temporary directory
//! that holds the agent's socket, phase's own state directory, the paths
//! the host program protects, and the workspace's own directory outside the
//! app's checkout. It also refuses a spec whose write paths reach a path
//! the app may only read: the launcher program and its directory, the
//! `PATH` directories, the toolchain directories and the checkout's
//! `.git`. On macOS the profile also denies those paths explicitly.
//!
//! The network is loopback only until phase runs its allowlist proxy: the
//! app may listen on the ports its permissions name and connect to the
//! ports of the apps it depends on. The plan refuses a `connect` that
//! names an external host. On macOS phase writes its own Seatbelt rules for
//! those ports, because nono's port rules allow binding any port. Seatbelt
//! applies the last rule that matches, so the rules work only because nono
//! writes them after its `(deny network*)`, which it does for every rule
//! that starts with `(allow network`. The exact pin of nono in Cargo.toml,
//! a unit test of the rules' prefix and the macOS test of a sandboxed
//! process's ports guard that order.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::name::{AppName, WorkspaceId};
use crate::plan::{AppPlan, Permissions, ProcessPlan, RunnerKind, STATE};
use crate::source::Source;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod capabilities;
mod launch;
#[cfg(any(target_os = "macos", test))]
mod seatbelt;

#[cfg(test)]
mod tests;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use capabilities::capabilities;
pub(crate) use launch::launcher_command;
pub use launch::{SANDBOX_REFUSED, launch};

/// Whether a host process runs under the sandbox, as `status` shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Sandboxing {
    /// The process runs under macOS's Seatbelt, which confines it only in
    /// part: [`SEATBELT_PARTIAL`] says what Seatbelt leaves open.
    #[non_exhaustive]
    Seatbelt {
        /// What Seatbelt leaves unrestricted.
        partial: String,
    },
    /// The process runs without a sandbox, for this reason. On Linux phase
    /// still applies Landlock, which [`LANDLOCK_UNSANDBOXED`] explains.
    #[non_exhaustive]
    Unsandboxed {
        /// Why, which names no environment value.
        reason: String,
    },
}

impl Sandboxing {
    /// Returns the sandbox of macOS, which [`SEATBELT_PARTIAL`] describes.
    #[must_use]
    pub fn seatbelt() -> Self {
        Self::Seatbelt {
            partial: SEATBELT_PARTIAL.to_owned(),
        }
    }

    /// Returns how Linux reports a process that runs under Landlock: as
    /// unsandboxed, for the reason [`LANDLOCK_UNSANDBOXED`] gives.
    #[must_use]
    pub fn landlock() -> Self {
        Self::Unsandboxed {
            reason: LANDLOCK_UNSANDBOXED.to_owned(),
        }
    }
}

impl fmt::Display for Sandboxing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seatbelt { partial } => write!(formatter, "seatbelt (partial: {partial})"),
            Self::Unsandboxed { reason } => write!(formatter, "unsandboxed ({reason})"),
        }
    }
}

/// What Seatbelt leaves open on macOS. Seatbelt restricts a bind by port
/// and never by address, and its `localhost` matches every address of the
/// machine, so a process that may listen on a port can listen on it on
/// every address and accept a peer on any of them.
pub const SEATBELT_PARTIAL: &str =
    "its listening ports accept connections from other machines on the network";

/// Why Linux reports a process under Landlock as unsandboxed (Luke's
/// choice, 5 November 2026). Before its ABI 9 Landlock cannot restrict
/// `connect` to a Unix socket, and nono 0.81 does not enforce
/// `ResolveUnix`, so a process can reach any Unix socket it can name, such
/// as Docker's, the user's D-Bus bus or an ssh or gpg agent, and leave the
/// sandbox through it. phase still applies Landlock, which confines files
/// and TCP ports.
pub const LANDLOCK_UNSANDBOXED: &str = "Landlock cannot restrict Unix sockets (nono does not yet enforce ResolveUnix), so a process could leave the sandbox through Docker or D-Bus; phase still applies Landlock to files and TCP ports";

/// The reason of a process whose app declares no permissions.
pub(crate) const UNDECLARED: &str = "its app declares no permissions";

/// The sandbox that this machine offers, or why it offers none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    /// phase applies this machine's sandbox, Seatbelt or Landlock, and
    /// reports a process under it as this value: [`Sandboxing::Seatbelt`]
    /// on macOS, and [`Sandboxing::Unsandboxed`] on Linux, because
    /// Landlock leaves Unix sockets open.
    Available(Sandboxing),
    /// This machine has no sandbox that phase supports, for this reason.
    Unavailable(String),
}

/// Returns the sandbox that this machine offers.
#[cfg(target_os = "macos")]
pub(crate) fn platform() -> Platform {
    if nono::Sandbox::is_supported() {
        Platform::Available(Sandboxing::seatbelt())
    } else {
        Platform::Unavailable(nono::Sandbox::support_info().details)
    }
}

/// Returns the sandbox that this machine offers. Landlock counts only when
/// it can restrict TCP ports, which needs its ABI 4. phase applies it, and
/// reports each process under it as unsandboxed ([`LANDLOCK_UNSANDBOXED`]).
#[cfg(target_os = "linux")]
pub(crate) fn platform() -> Platform {
    match nono::Sandbox::detect_abi() {
        Ok(abi) if abi.has_network() => Platform::Available(Sandboxing::landlock()),
        Ok(abi) => Platform::Unavailable(format!(
            "the kernel's Landlock ABI {} cannot restrict TCP ports, which needs ABI 4",
            abi.version_string()
        )),
        Err(error) => Platform::Unavailable(format!("the kernel offers no Landlock: {error}")),
    }
}

/// Returns the sandbox that this machine offers, which is none.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn platform() -> Platform {
    Platform::Unavailable(format!("phase has no sandbox for {}", std::env::consts::OS))
}

/// Returns how the processes of the host `app` run on a machine that offers
/// `platform`, or `None` for a container app, which runs in Docker.
pub(crate) fn sandboxing(app: &AppPlan, platform: &Platform) -> Option<Sandboxing> {
    if app.runner != RunnerKind::Host {
        return None;
    }
    Some(match (&app.permissions, platform) {
        (None, _) => Sandboxing::Unsandboxed {
            reason: UNDECLARED.to_owned(),
        },
        (Some(_), Platform::Available(sandboxing)) => sandboxing.clone(),
        (Some(_), Platform::Unavailable(reason)) => Sandboxing::Unsandboxed {
            reason: reason.clone(),
        },
    })
}

/// The program that links phase and the words of its hidden keeper
/// subcommand, which the keeper runs as the launcher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launcher {
    /// The program's path.
    pub(crate) program: PathBuf,
    /// The words before the launcher's arguments, such as `phase keep`.
    pub(crate) words: Vec<String>,
}

/// What a sandboxed process may do, as the keeper passes it to the
/// launcher. It holds paths and ports, never an environment value. Its
/// read and write paths are canonical, so the launcher can tell that a
/// path moved before it applies the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SandboxSpec {
    /// The directories and files that the process may read.
    pub(crate) read: BTreeSet<PathBuf>,
    /// The directories that the process may read and write. The launcher
    /// creates one that does not exist yet.
    pub(crate) write: BTreeSet<PathBuf>,
    /// The directories of `PATH`, which the process may look a program up
    /// in.
    pub(crate) path_dirs: Vec<PathBuf>,
    /// The paths that the process may neither read nor write, apart from
    /// each one's exception.
    pub(crate) protected: Vec<Protected>,
    /// The paths that the process may read but never write: the launcher
    /// program and its directory, the `PATH` directories, the toolchain
    /// directories and the checkout's `.git`.
    pub(crate) read_only: BTreeSet<PathBuf>,
    /// The paths that the process is always denied, which the macOS
    /// profile names explicitly: the protected paths, and on macOS the
    /// directories inside a system directory that it must not read.
    pub(crate) deny: BTreeSet<PathBuf>,
    /// The loopback ports that the process may listen on.
    pub(crate) listen: BTreeSet<u16>,
    /// The loopback ports that the process may connect to.
    pub(crate) connect: BTreeSet<u16>,
    /// The process's temporary directory, which becomes its `TMPDIR`.
    pub(crate) tmp: PathBuf,
    /// The program that applies the sandbox and then starts the app.
    pub(crate) launcher: Launcher,
}

/// Why phase cannot build a process's sandbox. It names paths, never an
/// environment value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum SandboxError {
    /// The app runs from a release, which has no sandbox yet.
    Release,
    /// The app's checkout is the workspace's own directory, which holds
    /// the workspace manifest and every other checkout.
    WorkspaceRoot(PathBuf),
    /// An allowed path covers or lies inside a path that the sandbox always
    /// denies.
    Protected {
        /// The allowed path.
        allowed: PathBuf,
        /// The protected path.
        protected: PathBuf,
    },
    /// A writable path covers or lies inside a path that the app may only
    /// read.
    ReadOnly {
        /// The writable path.
        allowed: PathBuf,
        /// The path that the app may only read.
        read_only: PathBuf,
    },
    /// A path resolves somewhere else than when phase built the sandbox,
    /// because a symbolic link changed.
    Moved {
        /// The path as phase built the sandbox.
        path: PathBuf,
        /// Where it resolves now.
        now: PathBuf,
    },
    /// A writable path lies inside a sticky, group-writable or
    /// world-writable directory, such as `/tmp`, where another process
    /// could replace a name in it with a symbolic link.
    SharedDirectory {
        /// The writable path.
        path: PathBuf,
        /// The shared directory.
        dir: PathBuf,
    },
    /// A writable path resolves outside the app's checkout and its own
    /// directories, for example through a symbolic link in the checkout.
    /// Writes of two workspaces cannot be compared yet, so the first stage
    /// allows no write elsewhere.
    OutsideCheckout {
        /// The writable path.
        path: PathBuf,
        /// The app's checkout.
        checkout: PathBuf,
    },
    /// A path that the sandbox needs cannot be read or created.
    Unusable {
        /// The path.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// A path holds a quote, a backslash or a control character, which a
    /// Seatbelt profile cannot hold, or is not UTF-8.
    Unwritable(PathBuf),
    /// The keeper program cannot be named as the launcher.
    Launcher(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Release => formatter.write_str(
                "the app runs from a release, and phase sandboxes only an app that runs from its checkout",
            ),
            Self::WorkspaceRoot(path) => write!(
                formatter,
                "the app runs in {}, the workspace's own directory, which holds the workspace manifest and every checkout, so phase sandboxes only an app that runs in one checkout",
                path.display()
            ),
            Self::Protected { allowed, protected } => write!(
                formatter,
                "the sandbox would allow {}, which overlaps {}, a path that the sandbox always denies",
                allowed.display(),
                protected.display()
            ),
            Self::ReadOnly { allowed, read_only } => write!(
                formatter,
                "the sandbox would let the app write {}, which overlaps {}, a path that the app may only read",
                allowed.display(),
                read_only.display()
            ),
            Self::Moved { path, now } => write!(
                formatter,
                "the sandbox allows {}, which now resolves to {}",
                path.display(),
                now.display()
            ),
            Self::SharedDirectory { path, dir } => write!(
                formatter,
                "the sandbox would let the app write {}, which lies inside {}, a directory that other users or processes may write",
                path.display(),
                dir.display()
            ),
            Self::OutsideCheckout { path, checkout } => write!(
                formatter,
                "the sandbox would let the app write {}, which lies outside its checkout {} and its own state and temporary directories",
                path.display(),
                checkout.display()
            ),
            Self::Unusable { path, reason } => write!(
                formatter,
                "the sandbox needs {}, which phase cannot use: {reason}",
                path.display()
            ),
            Self::Unwritable(path) => write!(
                formatter,
                "the path {} holds a character that a sandbox profile cannot hold",
                path.display()
            ),
            Self::Launcher(reason) => write!(
                formatter,
                "the keeper program cannot launch the sandbox: {reason}"
            ),
        }
    }
}

impl std::error::Error for SandboxError {}

/// A path that the sandbox always denies, apart from the directories inside
/// it that the app may use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Protected {
    /// The path, canonical.
    path: PathBuf,
    /// The directories inside it that the app may use, canonical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    except: Vec<PathBuf>,
}

impl Protected {
    /// Returns the protection of `path`.
    pub(crate) fn path(path: &Path) -> Self {
        Self {
            path: canonical(path),
            except: Vec::new(),
        }
    }

    /// Returns the protection of `path` apart from each of `except`.
    fn except(path: &Path, except: &[&Path]) -> Self {
        Self {
            path: canonical(path),
            except: except.iter().map(|path| canonical(path)).collect(),
        }
    }

    /// Reports whether allowing `allowed`, a canonical path, would reach
    /// the protected path: it covers the protected path, or lies inside it
    /// and outside its exceptions.
    fn reached_by(&self, allowed: &Path) -> bool {
        let covers = self.path.starts_with(allowed);
        let inside = allowed.starts_with(&self.path)
            && !self.except.iter().any(|except| allowed.starts_with(except));
        covers || inside
    }
}

/// Returns `path` with its symbolic links resolved. A path that does not
/// exist resolves its nearest existing ancestor and keeps the rest of its
/// names, so it compares equal to the path once it is created.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    let mut names = Vec::new();
    let mut current = path;
    loop {
        if let Ok(resolved) = current.canonicalize() {
            return names
                .iter()
                .rev()
                .fold(resolved, |resolved, name| resolved.join(name));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                names.push(name.to_os_string());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The paths of one workspace that the host program gives the plan: the
/// workspace's own directory, which holds every checkout, and the paths of
/// the workspace that a sandboxed app may never read or write, such as its
/// manifest. They are not part of the plan's digest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkspacePaths {
    /// The workspace's own directory, or `None` when the host program gave
    /// none, and phase takes the parent of each checkout instead.
    pub(crate) dir: Option<PathBuf>,
    /// The workspace's protected paths.
    pub(crate) protected: Vec<PathBuf>,
}

/// What the engine knows when it builds a process's sandbox, besides the
/// plan.
#[derive(Clone, Debug)]
pub(crate) struct Context<'a> {
    /// phase's state directory.
    pub(crate) state_dir: &'a Path,
    /// The workspace's stable id.
    pub(crate) workspace: &'a WorkspaceId,
    /// The workspace's own directory and protected paths.
    pub(crate) workspace_paths: &'a WorkspacePaths,
    /// The value of `PATH` that keepers and apps inherit, or `None`.
    pub(crate) path: Option<&'a str>,
    /// The value of `HOME` that keepers and apps inherit, or `None`.
    pub(crate) home: Option<&'a Path>,
    /// The paths that the host program protects, such as its own state and
    /// data directories.
    pub(crate) protected: &'a [PathBuf],
    /// The loopback ports of the apps that the app depends on.
    pub(crate) connect: BTreeSet<u16>,
    /// The program that runs the launcher.
    pub(crate) launcher: Launcher,
}

/// The directories that an app may always write, under phase's state
/// directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AppDirs {
    /// The directory that holds both, `apps/<workspace>/<app>`.
    pub(crate) base: PathBuf,
    /// The app's state directory, which `$STATE` names.
    pub(crate) state: PathBuf,
    /// The app's temporary directory.
    pub(crate) tmp: PathBuf,
}

impl AppDirs {
    /// Returns the directories of `app` in `workspace` under `state_dir`:
    /// `apps/<workspace>/<app>/state` and `.../tmp`. `state_dir` is
    /// resolved, so no name of the paths below it is a symbolic link that
    /// phase made.
    pub(crate) fn of(state_dir: &Path, workspace: &WorkspaceId, app: &AppName) -> Self {
        let base = canonical(state_dir)
            .join("apps")
            .join(workspace.to_string())
            .join(app.as_str());
        Self {
            state: base.join("state"),
            tmp: base.join("tmp"),
            base,
        }
    }

    /// Creates both directories and sets them readable only by the user,
    /// also when they exist already. It refuses a directory when any name
    /// of its path is a symbolic link, and never changes the mode of a
    /// link's target.
    pub(crate) fn create(&self) -> Result<(), SandboxError> {
        for dir in [&self.state, &self.tmp] {
            let unusable = |error: std::io::Error| SandboxError::Unusable {
                path: dir.clone(),
                reason: error.to_string(),
            };
            let opened = create_dir_nofollow(dir).map_err(unusable)?;
            rustix::fs::fchmod(&opened, rustix::fs::Mode::from_raw_mode(0o700))
                .map_err(|errno| unusable(errno.into()))?;
        }
        Ok(())
    }
}

/// The system directories that every sandboxed process may read on macOS,
/// and the one file of `/Library/Preferences` that records whether the
/// Xcode license was accepted, which `xcodebuild` reads before `xcrun`
/// runs a tool. `/etc`, `/var` and `/tmp` are symbolic links into `/private`, and the
/// profile allows the resolved paths and the metadata of the `/var` link. Of `/Library` it allows only what
/// toolchains read.
#[cfg(target_os = "macos")]
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library/Developer",
    "/Library/Frameworks",
    "/Library/Apple",
    "/private/etc",
    "/private/var/db/timezone",
    "/private/var/select",
    "/Library/Preferences/com.apple.dt.Xcode.plist",
    "/dev",
];

/// The system directories that every sandboxed process may read on Linux.
#[cfg(not(target_os = "macos"))]
const SYSTEM_READ: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/dev", "/proc",
];

/// The link that `xcode-select --switch` sets to the active developer
/// directory, which `xcode-select`, `xcrun` and the `/usr/bin` shims of
/// the developer tools read. On macOS [`SYSTEM_READ`] grants the link, and
/// the spec grants the directory it names, which [`developer_dir`] finds.
/// Linux has no such link, so there the spec grants nothing for it.
const DEVELOPER_DIR_LINK: &str = "/private/var/select/developer_dir";

/// The directories inside [`SYSTEM_READ`] that the macOS profile denies
/// again: the data and configuration of a Homebrew under `/usr/local`.
/// Landlock cannot deny a path inside a granted one, so Linux has none.
#[cfg(target_os = "macos")]
const SYSTEM_DENY: &[&str] = &["/usr/local/var", "/usr/local/etc"];

/// The directories inside [`SYSTEM_READ`] that the profile denies again,
/// which Linux cannot.
#[cfg(not(target_os = "macos"))]
const SYSTEM_DENY: &[&str] = &[];

/// The paths in the home directory that every sandbox protects: the
/// credentials and configuration of ssh, AWS, GitHub's CLI, Docker, netrc,
/// git, gpg, Cargo, npm, PyPI, Kubernetes, Google Cloud and Azure, the
/// shells' startup files, the agents' own directories, mise's
/// configuration, and on macOS the Keychains and the user's launch agents.
const HOME_PROTECTED: &[&str] = &[
    ".ssh",
    ".aws",
    ".config/gh",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".gnupg",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".cargo/config.toml",
    ".cargo/config",
    ".gitconfig",
    ".config/git",
    ".npmrc",
    ".pypirc",
    ".kube",
    ".config/gcloud",
    ".azure",
    ".config/mise",
    ".claude",
    ".codex",
    ".profile",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".config/fish",
    #[cfg(target_os = "macos")]
    "Library/Keychains",
    #[cfg(target_os = "macos")]
    "Library/LaunchAgents",
];

/// Returns the protected paths that every sandbox has on this platform:
/// [`HOME_PROTECTED`], the system Keychains, and the directory of the
/// agent's socket.
fn always_protected(home: Option<&Path>) -> Vec<Protected> {
    let mut paths: Vec<PathBuf> = home
        .map(|home| HOME_PROTECTED.iter().map(|name| home.join(name)).collect())
        .unwrap_or_default();
    // Every per-user temporary directory that macOS hands out, including
    // the one that holds the agent's socket, lies under /private/var/folders.
    #[cfg(target_os = "macos")]
    paths.extend([
        PathBuf::from("/private/var/folders"),
        PathBuf::from("/Library/Keychains"),
    ]);
    #[cfg(target_os = "linux")]
    paths.push(PathBuf::from(format!(
        "/run/user/{}",
        rustix::process::getuid().as_raw()
    )));
    paths.iter().map(|path| Protected::path(path)).collect()
}

/// The directories of a Homebrew prefix that a sandboxed process may read.
/// Its `var` and `etc`, which hold services' data and configuration, are
/// left out. [`homebrew_read`] adds one exception inside `etc`: OpenSSL's
/// directories, `etc/openssl` and each `etc/openssl@<version>`, which hold
/// OpenSSL's configuration and the CA bundle that Homebrew copies from the
/// system keychain. Neither is secret, and every TLS client that links
/// Homebrew's OpenSSL, such as Homebrew's node, opens them when it starts.
const HOMEBREW_READ: &[&str] = &[
    "bin",
    "sbin",
    "Cellar",
    "opt",
    "lib",
    "libexec",
    "share",
    "include",
    "Frameworks",
];

/// Returns the toolchain directories that the `PATH` entry `entry` lets a
/// sandboxed process read: for Cargo its `bin`, `registry` and `git`
/// directories and rustup's home, never Cargo's credentials; mise's data
/// directory; the program directories and OpenSSL's directories of a
/// Homebrew prefix; and otherwise the entry itself.
fn toolchain(entry: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    if let Some(home) = home {
        let cargo = home.join(".cargo");
        if entry.starts_with(&cargo) {
            return vec![
                cargo.join("bin"),
                cargo.join("registry"),
                cargo.join("git"),
                home.join(".rustup"),
            ];
        }
        for root in [home.join(".rustup"), home.join(".local/share/mise")] {
            if entry.starts_with(&root) {
                return vec![root];
            }
        }
    }
    for root in ["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"] {
        if entry.starts_with(root) {
            return homebrew_read(Path::new(root));
        }
    }
    vec![entry.to_path_buf()]
}

/// Returns the directories of the Homebrew prefix `prefix` that a
/// sandboxed process may read: [`HOMEBREW_READ`], and the entries of its
/// `etc` named `openssl` or `openssl@<version>`. No other entry of `etc`
/// is returned.
fn homebrew_read(prefix: &Path) -> Vec<PathBuf> {
    let is_openssl = |name: &str| name == "openssl" || name.starts_with("openssl@");
    let openssl = std::fs::read_dir(prefix.join("etc"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_str().is_some_and(is_openssl))
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_else(|_| Vec::new());
    HOMEBREW_READ
        .iter()
        .map(|name| prefix.join(name))
        .chain(openssl)
        .collect()
}

/// Returns the sandbox of `process` of the host `app`, whose permissions
/// are `permissions`, after creating the app's own directories.
///
/// # Errors
///
/// Returns a [`SandboxError`] when the app runs from a release or from the
/// workspace's own directory, an allowed path reaches a protected one, a
/// writable path reaches one the app may only read, a path cannot be used
/// or held, or the launcher cannot be named.
pub(crate) fn spec(
    app: &AppPlan,
    process: &ProcessPlan,
    permissions: &Permissions,
    context: &Context<'_>,
) -> Result<SandboxSpec, SandboxError> {
    let Source::Checkout { dir, .. } = &app.source else {
        return Err(SandboxError::Release);
    };
    let checkout = canonical(dir);
    let workspace_dir = context
        .workspace_paths
        .dir
        .as_deref()
        .map(canonical)
        .or_else(|| checkout.parent().map(Path::to_path_buf));
    if workspace_dir.as_ref() == Some(&checkout) {
        return Err(SandboxError::WorkspaceRoot(checkout));
    }
    let dirs = AppDirs::of(context.state_dir, context.workspace, &app.name);
    let mut protected = always_protected(context.home);
    // The app's own state and tmp directories lie inside phase's state
    // directory, which is protected apart from them. Their parent stays
    // protected, so the app cannot replace either with a symbolic link.
    let own: [&Path; 2] = [&dirs.state, &dirs.tmp];
    protected.push(Protected::except(context.state_dir, &own));
    // The host program may protect a directory that holds phase's state,
    // as mxv protects its state directory, so its protection also leaves
    // out the app's own directories.
    protected.extend(
        context
            .protected
            .iter()
            .chain(&context.workspace_paths.protected)
            .map(|path| Protected::except(path, &own)),
    );
    if let Some(workspace) = &workspace_dir {
        protected.push(Protected::except(workspace, &[&checkout]));
    }
    let reaches = |path: &Path| protected.iter().any(|found| found.reached_by(path));
    let mut write: BTreeSet<PathBuf> = [canonical(&dirs.state), canonical(&dirs.tmp)].into();
    for entry in &permissions.write {
        write.insert(if entry == STATE {
            canonical(&dirs.state)
        } else {
            canonical(&checkout.join(entry))
        });
    }
    let mut read: BTreeSet<PathBuf> = system_read()
        .iter()
        .filter(|path| path.exists())
        .map(|path| canonical(path))
        .collect();
    if let Some(dir) = developer_dir(Path::new(DEVELOPER_DIR_LINK)).filter(|dir| !reaches(dir)) {
        read.insert(dir);
    }
    read.insert(checkout.clone());
    if let Some(program) = process.run.first().map(Path::new)
        && program.is_absolute()
    {
        read.insert(canonical(program));
    }
    let mut read_only: BTreeSet<PathBuf> = BTreeSet::new();
    for program in link_chain(&context.launcher.program) {
        if let Some(parent) = program.parent() {
            read_only.insert(canonical(parent));
        }
        read_only.insert(canonical(&program));
    }
    read_only.insert(checkout.join(".git"));
    let mut path_dirs = Vec::new();
    for entry in context.path.unwrap_or_default().split(':') {
        let entry = Path::new(entry);
        if !entry.is_absolute() {
            continue;
        }
        read_only.insert(canonical(entry));
        // A `PATH` entry that reaches a protected path, such as the home
        // directory, lets the app look nothing up in it.
        if !reaches(&canonical(entry)) {
            path_dirs.push(entry.to_path_buf());
        }
        for dir in toolchain(entry, context.home) {
            let dir = canonical(&dir);
            read_only.insert(dir.clone());
            // A `PATH` entry that reaches a protected path, such as one
            // that names the home directory, gives the app nothing.
            if dir.exists() && !reaches(&dir) {
                read.insert(dir);
            }
        }
    }
    let mut deny: BTreeSet<PathBuf> = protected.iter().map(|found| found.path.clone()).collect();
    deny.extend(SYSTEM_DENY.iter().map(PathBuf::from));
    let listen = permissions
        .listen
        .iter()
        .filter_map(|port| app.ports.get(port))
        .map(|port| port.get())
        .collect();
    let spec = SandboxSpec {
        read,
        write,
        path_dirs,
        protected,
        read_only,
        deny,
        listen,
        connect: context.connect.clone(),
        tmp: canonical(&dirs.tmp),
        launcher: context.launcher.clone(),
    };
    check(&spec)?;
    for path in &spec.write {
        if let Some(dir) = shared_ancestor(path) {
            return Err(SandboxError::SharedDirectory {
                path: path.clone(),
                dir,
            });
        }
    }
    let own = [canonical(&dirs.state), canonical(&dirs.tmp)];
    if let Some(path) = spec
        .write
        .iter()
        .find(|path| !path.starts_with(&checkout) && !own.contains(path))
    {
        return Err(SandboxError::OutsideCheckout {
            path: path.clone(),
            checkout: checkout.clone(),
        });
    }
    for path in spec
        .read
        .iter()
        .chain(&spec.write)
        .chain(&spec.deny)
        .chain(&spec.read_only)
        .chain(&spec.path_dirs)
    {
        if !is_profile_path(path) {
            return Err(SandboxError::Unwritable(path.clone()));
        }
    }
    dirs.create()?;
    Ok(spec)
}

/// Checks that no read or write path of `spec` reaches a protected path,
/// and that no write path reaches a path the app may only read. The
/// engine checks a spec when it builds it, and the launcher checks it
/// again before it applies it.
///
/// # Errors
///
/// Returns [`SandboxError::Protected`] or [`SandboxError::ReadOnly`] for
/// the first path that reaches one.
pub(crate) fn check(spec: &SandboxSpec) -> Result<(), SandboxError> {
    for path in spec.read.iter().chain(&spec.write) {
        if let Some(found) = spec.protected.iter().find(|found| found.reached_by(path)) {
            return Err(SandboxError::Protected {
                allowed: path.clone(),
                protected: found.path.clone(),
            });
        }
    }
    for path in &spec.write {
        if let Some(found) = spec
            .read_only
            .iter()
            .find(|read_only| Protected::path(read_only).reached_by(path))
        {
            return Err(SandboxError::ReadOnly {
                allowed: path.clone(),
                read_only: found.clone(),
            });
        }
    }
    Ok(())
}

/// Checks `spec` as the launcher sees it just before it applies it: each
/// read and write path still resolves to itself, so no symbolic link has
/// moved it since the engine built the spec, and [`check`] still passes.
/// Then it creates each write directory that does not exist yet.
///
/// # Errors
///
/// Returns [`SandboxError::Moved`] for a path that resolves elsewhere, the
/// errors of [`check`], and [`SandboxError::Unusable`] for a directory that
/// cannot be created.
pub(crate) fn verify(spec: &SandboxSpec) -> Result<(), SandboxError> {
    // The directories are created first, so no directory appears after the
    // check. Each name is created and opened without following a symbolic
    // link, so nothing is created through a link that the app planted.
    for path in &spec.write {
        create_dir_nofollow(path).map_err(|error| SandboxError::Unusable {
            path: path.clone(),
            reason: error.to_string(),
        })?;
    }
    for path in spec.read.iter().chain(&spec.write) {
        let now = canonical(path);
        if &now != path {
            return Err(SandboxError::Moved {
                path: path.clone(),
                now,
            });
        }
    }
    check(spec)?;
    for path in &spec.write {
        if let Some(dir) = shared_ancestor(path) {
            return Err(SandboxError::SharedDirectory {
                path: path.clone(),
                dir,
            });
        }
    }
    Ok(())
}

/// Creates the absolute directory `path` and each missing directory above
/// it with mode 0700, and returns it opened. Each name is opened with
/// `O_NOFOLLOW` and created with `mkdirat`, so a symbolic link anywhere in
/// the path is refused and nothing is created through one.
fn create_dir_nofollow(path: &Path) -> std::io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{CWD, Mode, OFlags};
    use rustix::io::Errno;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut names = path.components();
    if names.next() != Some(std::path::Component::RootDir) {
        return Err(std::io::Error::other("the path is not absolute"));
    }
    let mut dir = rustix::fs::openat(CWD, "/", flags, Mode::empty())?;
    for name in names {
        let std::path::Component::Normal(name) = name else {
            return Err(std::io::Error::other("the path is not canonical"));
        };
        let opened = match rustix::fs::openat(&dir, name, flags, Mode::empty()) {
            Err(Errno::NOENT) => {
                match rustix::fs::mkdirat(&dir, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(errno) => return Err(errno.into()),
                }
                rustix::fs::openat(&dir, name, flags, Mode::empty())
            }
            other => other,
        };
        dir = opened.map_err(|errno| match errno {
            Errno::LOOP | Errno::NOTDIR => std::io::Error::other(format!(
                "{} is a symbolic link or not a directory",
                Path::new(name).display()
            )),
            errno => errno.into(),
        })?;
    }
    Ok(dir)
}

/// Returns the first directory from the root down to `path` that is
/// sticky, group-writable or world-writable, such as `/tmp`, where another
/// user or process could replace a name with a symbolic link, or `None`.
fn shared_ancestor(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    path.ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .filter_map(|dir| Some((dir, std::fs::symlink_metadata(dir).ok()?)))
        .find(|(_, metadata)| metadata.is_dir() && metadata.permissions().mode() & 0o1022 != 0)
        .map(|(dir, _)| dir.to_path_buf())
}

/// Returns `program` and each path that its chain of symbolic links passes
/// through, ending at the file it resolves to. A chain longer than 40
/// links stops there, as the kernel's does.
fn link_chain(program: &Path) -> Vec<PathBuf> {
    let mut chain = vec![program.to_path_buf()];
    let mut current = program.to_path_buf();
    for _ in 0..40 {
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
        chain.push(current.clone());
    }
    chain.push(canonical(program));
    chain
}

/// Returns the system directories that every sandboxed process may read.
/// On Linux `/usr` is listed by its entries, and `/usr/local` by its
/// entries apart from `etc` and `var`, because Landlock cannot deny a path
/// inside a granted one.
fn system_read() -> Vec<PathBuf> {
    let paths = SYSTEM_READ.iter().map(PathBuf::from);
    if cfg!(target_os = "linux") {
        paths
            .flat_map(|path| {
                if path == Path::new("/usr") {
                    usr_without_local_data(&path)
                } else {
                    vec![path]
                }
            })
            .collect()
    } else {
        paths.collect()
    }
}

/// Returns the resolved directory that the symbolic link `link` names,
/// when the link holds an absolute path to a directory at least two names
/// below the root. A link to `/` or to a directory beside `/Applications`
/// therefore names nothing, and a sandbox never reads all of one. When the
/// directory is the `Contents/Developer` of an app bundle, such as
/// `/Applications/Xcode.app`, it returns the bundle, because Xcode's tools
/// also read the bundle's `Info.plist` and load its `SharedFrameworks`.
fn developer_dir(link: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(link).ok()?;
    if !target.is_absolute() {
        return None;
    }
    let dir = target.canonicalize().ok()?;
    let bundle = dir
        .ancestors()
        .nth(2)
        .filter(|bundle| {
            dir.ends_with("Contents/Developer")
                && bundle.extension() == Some(std::ffi::OsStr::new("app"))
        })
        .map(Path::to_path_buf);
    let dir = bundle.unwrap_or(dir);
    (dir.is_dir() && dir.components().count() > 2).then_some(dir)
}

/// Returns the entries of `usr`, with `local` replaced by its entries
/// apart from `etc` and `var`.
fn usr_without_local_data(usr: &Path) -> Vec<PathBuf> {
    let entries = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    entries(usr)
        .into_iter()
        .flat_map(|entry| {
            if entry.file_name() == Some(std::ffi::OsStr::new("local")) {
                entries(&entry)
                    .into_iter()
                    .filter(|inner| {
                        !matches!(
                            inner.file_name().and_then(std::ffi::OsStr::to_str),
                            Some("etc" | "var")
                        )
                    })
                    .collect()
            } else {
                vec![entry]
            }
        })
        .collect()
}

/// Reports whether `path` is UTF-8 and holds no quote, backslash or control
/// character, so a Seatbelt profile can hold it between quotes.
fn is_profile_path(path: &Path) -> bool {
    path.to_str().is_some_and(|text| {
        !text
            .chars()
            .any(|character| character == '"' || character == '\\' || character.is_control())
    })
}

/// Returns the launcher of the keeper program `program`, whose hidden
/// subcommand's words are `leading`.
///
/// # Errors
///
/// Returns [`SandboxError::Launcher`] when a word is not UTF-8.
pub(crate) fn launcher(program: &Path, leading: &[OsString]) -> Result<Launcher, SandboxError> {
    let words = leading
        .iter()
        .map(|word| {
            word.to_str()
                .map(str::to_owned)
                .ok_or_else(|| SandboxError::Launcher(format!("the word {word:?} is not UTF-8")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Launcher {
        program: program.to_path_buf(),
        words,
    })
}
