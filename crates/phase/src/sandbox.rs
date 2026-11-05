//! The operating system's sandbox around a host process (decision P9).
//!
//! A host app that declares `[permissions]` runs each of its processes
//! under the sandbox, through the `nono` library: Seatbelt on macOS and
//! Landlock on Linux. An app without permissions runs unsandboxed, and the
//! plan records that, because it writes the app's permissions as `null`.
//! On a platform with no sandbox that phase supports, or a Linux kernel
//! whose Landlock cannot restrict TCP ports, an app with permissions runs
//! unsandboxed too, and `status` shows why.
//!
//! The engine turns the plan into a [`SandboxSpec`]: the paths the process
//! may read and write, the ports it may listen on and connect to, and the
//! paths it is always denied. The spec holds paths and ports, never an
//! environment value. The keeper starts the app through the launcher, the
//! program that links phase run as `<keeper words> --sandboxed <spec> --
//! <app command>`, which applies the sandbox to itself and then replaces
//! itself with the app. So every start of the app, including each restart
//! by its keeper, runs under the sandbox, and the app's pid is the
//! launcher's.
//!
//! The spec allows reading the system's own directories, the toolchain
//! directories that `PATH` names (Cargo, rustup, mise and Homebrew), the
//! app's checkout and the paths the app may write. It allows writing the
//! app's own state directory, `<phase state>/apps/<workspace id>/<app>/state`,
//! its temporary directory `.../tmp`, which becomes the app's `TMPDIR`, and
//! the paths its permissions add. It refuses to build a spec whose allowed
//! paths cover or lie inside a protected path: `~/.ssh`, the Keychains, the
//! per-user temporary directory that holds the agent's socket, phase's own
//! state directory, the protected paths the host program names, and the
//! workspace directory outside the app's checkout. On macOS the profile
//! also denies those paths explicitly.
//!
//! The network is loopback only until phase runs its allowlist proxy: the
//! app may listen on the ports its permissions name and connect to the
//! ports of the apps it depends on. The plan refuses a `connect` that
//! names an external host. On macOS phase writes its own Seatbelt rules for
//! those ports, because nono's port rules allow binding any port. Seatbelt
//! applies the last rule that matches, so the rules work only because nono
//! writes them after its `(deny network*)`, which it does for every rule
//! that starts with `(allow network`. The exact pin of nono in Cargo.toml
//! and the macOS test of a sandboxed process's ports guard that order.

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
    /// The process runs under macOS's Seatbelt.
    Seatbelt,
    /// The process runs under Linux's Landlock.
    Landlock,
    /// The process runs without a sandbox, for this reason.
    #[non_exhaustive]
    Unsandboxed {
        /// Why, which names no environment value.
        reason: String,
    },
}

/// The reason of a process whose app declares no permissions.
pub(crate) const UNDECLARED: &str = "its app declares no permissions";

/// The sandbox that this machine offers, or why it offers none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Platform {
    /// The sandbox, which is [`Sandboxing::Seatbelt`] or
    /// [`Sandboxing::Landlock`].
    Available(Sandboxing),
    /// This machine has no sandbox that phase supports, for this reason.
    Unavailable(String),
}

/// Returns the sandbox that this machine offers.
#[cfg(target_os = "macos")]
pub(crate) fn platform() -> Platform {
    if nono::Sandbox::is_supported() {
        Platform::Available(Sandboxing::Seatbelt)
    } else {
        Platform::Unavailable(nono::Sandbox::support_info().details)
    }
}

/// Returns the sandbox that this machine offers. Landlock counts only when
/// it can restrict TCP ports, which needs its ABI 4.
#[cfg(target_os = "linux")]
pub(crate) fn platform() -> Platform {
    match nono::Sandbox::detect_abi() {
        Ok(abi) if abi.has_network() => Platform::Available(Sandboxing::Landlock),
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
/// launcher. It holds paths and ports, never an environment value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SandboxSpec {
    /// The directories and files that the process may read.
    pub(crate) read: BTreeSet<PathBuf>,
    /// The directories that the process may read and write.
    pub(crate) write: BTreeSet<PathBuf>,
    /// The directories of `PATH`, which the process may look a program up
    /// in.
    pub(crate) path_dirs: Vec<PathBuf>,
    /// The paths that the process is always denied, which the macOS
    /// profile names explicitly.
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
    /// An allowed path covers or lies inside a path that the sandbox always
    /// denies.
    Protected {
        /// The allowed path.
        allowed: PathBuf,
        /// The protected path.
        protected: PathBuf,
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
            Self::Protected { allowed, protected } => write!(
                formatter,
                "the sandbox would allow {}, which overlaps {}, a path that the sandbox always denies",
                allowed.display(),
                protected.display()
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

/// A path that the sandbox always denies, apart from one directory inside
/// it that the app may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Protected {
    /// The path, canonical when it exists.
    path: PathBuf,
    /// The directory inside it that the app may use, or `None`.
    except: Option<PathBuf>,
}

impl Protected {
    /// Returns the protection of `path`.
    pub(crate) fn path(path: &Path) -> Self {
        Self {
            path: canonical_or_given(path),
            except: None,
        }
    }

    /// Returns the protection of `path` apart from `except`.
    fn except(path: &Path, except: &Path) -> Self {
        Self {
            path: canonical_or_given(path),
            except: Some(canonical_or_given(except)),
        }
    }

    /// Reports whether allowing `allowed`, a canonical path, would reach
    /// the protected path: it covers the protected path, or lies inside it
    /// and outside its exception.
    fn reached_by(&self, allowed: &Path) -> bool {
        let covers = self.path.starts_with(allowed);
        let inside = allowed.starts_with(&self.path)
            && self
                .except
                .as_ref()
                .is_none_or(|except| !allowed.starts_with(except));
        covers || inside
    }
}

/// Returns `path` with its symbolic links resolved, or `path` itself when
/// it does not exist.
fn canonical_or_given(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// What the engine knows when it builds a process's sandbox, besides the
/// plan.
#[derive(Clone, Debug)]
pub(crate) struct Context<'a> {
    /// phase's state directory.
    pub(crate) state_dir: &'a Path,
    /// The workspace's stable id.
    pub(crate) workspace: &'a WorkspaceId,
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
    /// The app's state directory, which `$STATE` names.
    pub(crate) state: PathBuf,
    /// The app's temporary directory.
    pub(crate) tmp: PathBuf,
}

impl AppDirs {
    /// Returns the directories of `app` in `workspace` under `state_dir`:
    /// `apps/<workspace>/<app>/state` and `.../tmp`.
    pub(crate) fn of(state_dir: &Path, workspace: &WorkspaceId, app: &AppName) -> Self {
        let base = state_dir
            .join("apps")
            .join(workspace.to_string())
            .join(app.as_str());
        Self {
            state: base.join("state"),
            tmp: base.join("tmp"),
        }
    }

    /// Creates both directories, readable only by the user.
    fn create(&self) -> Result<(), SandboxError> {
        use std::os::unix::fs::DirBuilderExt as _;
        for dir in [&self.state, &self.tmp] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|error| SandboxError::Unusable {
                    path: dir.clone(),
                    reason: error.to_string(),
                })?;
        }
        Ok(())
    }
}

/// The system directories that every sandboxed process may read on macOS.
/// `/etc`, `/var` and `/tmp` are symbolic links into `/private`, and the
/// profile allows the resolved paths.
#[cfg(target_os = "macos")]
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/private/var/db/timezone",
    "/private/var/select",
    "/dev",
];

/// The system directories that every sandboxed process may read on Linux.
#[cfg(not(target_os = "macos"))]
const SYSTEM_READ: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/dev", "/proc",
];

/// Returns the protected paths that every sandbox has on this platform:
/// `~/.ssh`, the Keychains and the directory of the agent's socket.
fn always_protected(home: Option<&Path>) -> Vec<Protected> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        paths.push(home.join(".ssh"));
        #[cfg(target_os = "macos")]
        paths.push(home.join("Library/Keychains"));
    }
    // Every per-user temporary directory that macOS hands out, including
    // the one that holds the agent's socket, lies under /private/var/folders.
    #[cfg(target_os = "macos")]
    paths.push(PathBuf::from("/private/var/folders"));
    #[cfg(target_os = "linux")]
    paths.push(PathBuf::from(format!(
        "/run/user/{}",
        rustix::process::getuid().as_raw()
    )));
    paths.iter().map(|path| Protected::path(path)).collect()
}

/// Returns the toolchain directories that the `PATH` entry `entry` lets a
/// sandboxed process read: for Cargo its `bin`, `registry` and `git`
/// directories and rustup's home, never Cargo's credentials; mise's data
/// directory; Homebrew's prefix; and otherwise the entry itself.
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
            return vec![PathBuf::from(root)];
        }
    }
    vec![entry.to_path_buf()]
}

/// Returns the sandbox of `process` of the host `app`, whose permissions
/// are `permissions`, after creating the app's own directories.
///
/// # Errors
///
/// Returns a [`SandboxError`] when the app runs from a release, an allowed
/// path reaches a protected one, a path cannot be used or held, or the
/// launcher cannot be named.
pub(crate) fn spec(
    app: &AppPlan,
    process: &ProcessPlan,
    permissions: &Permissions,
    context: &Context<'_>,
) -> Result<SandboxSpec, SandboxError> {
    let Source::Checkout { dir: checkout, .. } = &app.source else {
        return Err(SandboxError::Release);
    };
    let dirs = AppDirs::of(context.state_dir, context.workspace, &app.name);
    // The app's own directories lie inside phase's state directory, so they
    // are checked only against the paths that every sandbox protects.
    let fixed = always_protected(context.home);
    for dir in [&dirs.state, &dirs.tmp] {
        let dir = canonical_or_given(dir);
        if let Some(found) = fixed.iter().find(|found| found.reached_by(&dir)) {
            return Err(SandboxError::Protected {
                allowed: dir,
                protected: found.path.clone(),
            });
        }
    }
    let mut protected = fixed;
    protected.push(Protected::path(context.state_dir));
    protected.extend(context.protected.iter().map(|path| Protected::path(path)));
    if let Some(workspace) = checkout.parent() {
        protected.push(Protected::except(workspace, checkout));
    }
    dirs.create()?;
    let mut write: BTreeSet<PathBuf> = [dirs.state.clone(), dirs.tmp.clone()].into();
    for entry in &permissions.write {
        write.insert(if entry == STATE {
            dirs.state.clone()
        } else {
            checkout.join(entry)
        });
    }
    let mut read: BTreeSet<PathBuf> = SYSTEM_READ
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect();
    read.insert(checkout.clone());
    if let Some(program) = process.run.first().map(Path::new)
        && program.is_absolute()
    {
        read.insert(program.to_path_buf());
    }
    let mut path_dirs = Vec::new();
    for entry in context.path.unwrap_or_default().split(':') {
        let entry = Path::new(entry);
        if !entry.is_absolute() {
            continue;
        }
        path_dirs.push(entry.to_path_buf());
        for dir in toolchain(entry, context.home) {
            let canonical = canonical_or_given(&dir);
            // A `PATH` entry that reaches a protected path, such as one
            // that names the home directory, gives the app nothing.
            if dir.exists() && !protected.iter().any(|found| found.reached_by(&canonical)) {
                read.insert(dir);
            }
        }
    }
    for path in read.iter().chain(write.iter()) {
        if [&dirs.state, &dirs.tmp].contains(&path) {
            continue;
        }
        let canonical = canonical_or_given(path);
        if let Some(found) = protected.iter().find(|found| found.reached_by(&canonical)) {
            return Err(SandboxError::Protected {
                allowed: path.clone(),
                protected: found.path.clone(),
            });
        }
    }
    let deny: BTreeSet<PathBuf> = protected.iter().map(|found| found.path.clone()).collect();
    for path in read.iter().chain(&write).chain(&deny).chain(&path_dirs) {
        if !is_profile_path(path) {
            return Err(SandboxError::Unwritable(path.clone()));
        }
    }
    let listen = permissions
        .listen
        .iter()
        .filter_map(|port| app.ports.get(port))
        .map(|port| port.get())
        .collect();
    Ok(SandboxSpec {
        read,
        write,
        path_dirs,
        deny,
        listen,
        connect: context.connect.clone(),
        tmp: dirs.tmp,
        launcher: context.launcher.clone(),
    })
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
