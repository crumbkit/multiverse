//! The snapshot of the environment variables that mxv reads, and the
//! catalog that says what each one does (BASE-6, BASE-28).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::failures::MxvFailure;

#[cfg(test)]
mod tests;

/// One environment variable that mxv reads.
#[derive(Debug)]
pub struct Variable {
    /// The variable's name.
    pub name: &'static str,
    /// What the variable sets, the form it takes, and what mxv does when it
    /// is absent, empty or not an absolute path.
    pub purpose: &'static str,
}

/// Every environment variable that mxv reads itself. The binary's `main`
/// reads exactly these, once, into an [`Environment`]. crumbkit-usage reads
/// `XDG_STATE_HOME` and `HOME` again for the usage log.
pub const VARIABLES: &[Variable] = &[
    Variable {
        name: "HOME",
        purpose: "The person's home directory, an absolute path. The workspace root defaults to `$HOME/mxv`, and the state directory to `$HOME/.local/state`. When it is absent, empty or relative, and MXV_ROOT or XDG_STATE_HOME does not stand in, a workspace command fails with mxv::environment::no_directory.",
    },
    Variable {
        name: "XDG_STATE_HOME",
        purpose: "The state directory, an absolute path. mxv keeps one record for each workspace in `$XDG_STATE_HOME/mxv/workspaces`. When it is absent, empty or relative, mxv uses `$HOME/.local/state`.",
    },
    Variable {
        name: "MXV_ROOT",
        purpose: "The directory that holds one directory for each workspace, an absolute path. When it is absent or empty, mxv uses `$HOME/mxv`. When it is relative, a workspace command that needs the root fails with mxv::environment::no_directory.",
    },
];

/// The values of the variables in [`VARIABLES`], read once in `main`.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    /// The value of `HOME`.
    home: Option<OsString>,
    /// The value of `XDG_STATE_HOME`.
    state_home: Option<OsString>,
    /// The value of `MXV_ROOT`.
    root: Option<OsString>,
}

impl Environment {
    /// Returns the snapshot of the variables in [`VARIABLES`], each read
    /// with `read`.
    pub fn read(mut read: impl FnMut(&str) -> Option<OsString>) -> Self {
        Self {
            home: read("HOME"),
            state_home: read("XDG_STATE_HOME"),
            root: read("MXV_ROOT"),
        }
    }

    /// Returns the directory of the workspace records:
    /// `$XDG_STATE_HOME/mxv/workspaces`, or
    /// `$HOME/.local/state/mxv/workspaces`.
    pub(crate) fn records(&self) -> Result<PathBuf, MxvFailure> {
        let state = match absolute(self.state_home.as_ref()) {
            Some(state) => state,
            None => absolute(self.home.as_ref())
                .ok_or(MxvFailure::NoDirectory {
                    variable: "XDG_STATE_HOME",
                })?
                .join(".local")
                .join("state"),
        };
        Ok(state.join("mxv").join("workspaces"))
    }

    /// Returns the directory that holds one directory for each workspace:
    /// `$MXV_ROOT`, or `$HOME/mxv`.
    pub(crate) fn root(&self) -> Result<PathBuf, MxvFailure> {
        match self.root.as_ref().filter(|root| !root.is_empty()) {
            Some(root) => absolute(Some(root)).ok_or(MxvFailure::NoDirectory {
                variable: "MXV_ROOT",
            }),
            None => absolute(self.home.as_ref())
                .map(|home| home.join("mxv"))
                .ok_or(MxvFailure::NoDirectory {
                    variable: "MXV_ROOT",
                }),
        }
    }
}

/// Returns `value` as a path when it is a non-empty absolute path.
fn absolute(value: Option<&OsString>) -> Option<PathBuf> {
    value
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
}
