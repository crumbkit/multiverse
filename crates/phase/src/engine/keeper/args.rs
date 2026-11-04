//! The keeper's command-line arguments, which name its workspace, app,
//! process and state directory and hold no environment value.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::name::{AppName, ProcessName, WorkspaceId};

/// The flag that names the workspace's stable id.
pub(crate) const WORKSPACE: &str = "--workspace";
/// The flag that names the app.
pub(crate) const APP: &str = "--app";
/// The flag that names the process.
pub(crate) const PROCESS: &str = "--process";
/// The flag that names phase's state directory.
pub(crate) const STATE_DIR: &str = "--state-dir";

/// What a keeper is told on its command line: the workspace's stable id,
/// the app, the process and phase's state directory, which holds the run
/// record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeeperArgs {
    /// The workspace's stable id, which keys the run record.
    workspace: WorkspaceId,
    /// The app.
    app: AppName,
    /// The process that the keeper keeps.
    process: ProcessName,
    /// phase's state directory.
    state_dir: PathBuf,
}

impl KeeperArgs {
    /// Returns the arguments of the keeper of `process` of `app` in the
    /// workspace `workspace`, whose run record is under `state_dir`.
    #[must_use]
    pub const fn new(
        workspace: WorkspaceId,
        app: AppName,
        process: ProcessName,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            workspace,
            app,
            process,
            state_dir,
        }
    }

    /// Parses the arguments that follow the hidden `phase keep`
    /// subcommand: `--workspace <id> --app <app> --process <process>
    /// --state-dir <dir>`, each flag once, in any order.
    ///
    /// # Errors
    ///
    /// Returns a [`BadKeeperArgs`] that names the flag when a flag is
    /// unknown, repeated, missing or lacks its value, or when a value
    /// breaks its name's rule.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, BadKeeperArgs> {
        let mut workspace = None;
        let mut app = None;
        let mut process = None;
        let mut state_dir = None;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let flag = flag
                .into_string()
                .map_err(|flag| BadKeeperArgs::Unknown(flag.to_string_lossy().into_owned()))?;
            let value = args
                .next()
                .ok_or_else(|| BadKeeperArgs::Missing(flag.clone()))?;
            let slot = match flag.as_str() {
                WORKSPACE => &mut workspace,
                APP => &mut app,
                PROCESS => &mut process,
                STATE_DIR => &mut state_dir,
                _ => return Err(BadKeeperArgs::Unknown(flag)),
            };
            if slot.replace(value).is_some() {
                return Err(BadKeeperArgs::Repeated(flag));
            }
        }
        let name = |slot: Option<OsString>, flag: &str| {
            slot.ok_or_else(|| BadKeeperArgs::Missing(flag.to_owned()))?
                .into_string()
                .map_err(|_| BadKeeperArgs::Invalid(flag.to_owned()))
        };
        Ok(Self {
            workspace: name(workspace, WORKSPACE)?
                .parse()
                .map_err(|_| BadKeeperArgs::Invalid(WORKSPACE.to_owned()))?,
            app: name(app, APP)?
                .parse()
                .map_err(|_| BadKeeperArgs::Invalid(APP.to_owned()))?,
            process: name(process, PROCESS)?
                .parse()
                .map_err(|_| BadKeeperArgs::Invalid(PROCESS.to_owned()))?,
            state_dir: state_dir
                .map(PathBuf::from)
                .ok_or_else(|| BadKeeperArgs::Missing(STATE_DIR.to_owned()))?,
        })
    }

    /// Returns the workspace's stable id.
    #[must_use]
    pub const fn workspace(&self) -> &WorkspaceId {
        &self.workspace
    }

    /// Returns the app.
    #[must_use]
    pub const fn app(&self) -> &AppName {
        &self.app
    }

    /// Returns the process.
    #[must_use]
    pub const fn process(&self) -> &ProcessName {
        &self.process
    }

    /// Returns phase's state directory.
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

/// The keeper's arguments cannot be read. Each variant names the flag.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadKeeperArgs {
    /// The flag is not one of the keeper's.
    Unknown(String),
    /// The flag appears more than once.
    Repeated(String),
    /// The flag, or its value, is missing.
    Missing(String),
    /// The flag's value breaks its name's rule.
    Invalid(String),
}

impl fmt::Display for BadKeeperArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(flag) => write!(formatter, "the keeper has no flag {flag}"),
            Self::Repeated(flag) => write!(formatter, "the keeper's flag {flag} appears twice"),
            Self::Missing(flag) => write!(formatter, "the keeper's flag {flag} has no value"),
            Self::Invalid(flag) => {
                write!(
                    formatter,
                    "the value of the keeper's flag {flag} is not valid"
                )
            }
        }
    }
}

impl std::error::Error for BadKeeperArgs {}
