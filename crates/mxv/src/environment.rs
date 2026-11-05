//! The snapshot of the environment variables that mxv reads, and the
//! catalog that says what each one does (BASE-6, BASE-28).

use std::ffi::OsString;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use etcetera::BaseStrategy as _;
use multiverse::{CallerEnvironment, DEFAULT_PORT_RANGE, Runs};
use schemars::JsonSchema;
use serde::Serialize;

use crate::config::{self, Config};
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

/// Every environment variable that mxv depends on. The binary's `main`
/// reads `MXV_ROOT` once into an [`Environment`], and finds the base
/// directories once with etcetera's base strategy, which reads `HOME` and
/// the `XDG_*_HOME` variables, as crumbkit does (ADR-0027, ADR-0029).
/// crumbkit-usage reads `XDG_STATE_HOME` and `HOME` again for the usage
/// log.
pub const VARIABLES: &[Variable] = &[
    Variable {
        name: "HOME",
        purpose: "The person's home directory. Each base directory below defaults to a directory in it, and a config file's `root` may start with `~/` for it. When it is unset or empty, mxv reads the home directory from the user database. When mxv finds no home directory, a workspace command fails with mxv::environment::no_directory.",
    },
    Variable {
        name: "XDG_CONFIG_HOME",
        purpose: "The config directory, an absolute path. mxv reads its config file from `$XDG_CONFIG_HOME/mxv/config.toml`. When it is unset, empty or relative, mxv uses `~/.config`.",
    },
    Variable {
        name: "XDG_DATA_HOME",
        purpose: "The data directory, an absolute path. The workspace root defaults to `$XDG_DATA_HOME/mxv/workspaces`, and mxv keeps one bare mirror for each origin in `$XDG_DATA_HOME/mxv/mirrors`. When it is unset, empty or relative, mxv uses `~/.local/share`.",
    },
    Variable {
        name: "XDG_STATE_HOME",
        purpose: "The state directory, an absolute path. mxv keeps one record for each workspace in `$XDG_STATE_HOME/mxv/workspaces`, and the run files and logs of each workspace's processes in `$XDG_STATE_HOME/mxv/runs/<name>`. When it is unset, empty or relative, mxv uses `~/.local/state`.",
    },
    Variable {
        name: "MXV_ROOT",
        purpose: "The directory that holds one directory for each workspace, an absolute path. It wins over the config file's `root`, which wins over `$XDG_DATA_HOME/mxv/workspaces`. When it is unset or empty, mxv uses the next of those. When it is relative, a command that needs the root fails with mxv::environment::relative_root.",
    },
];

/// The base directories that etcetera's base strategy finds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bases {
    /// The home directory.
    pub home: PathBuf,
    /// The config directory, such as `~/.config`.
    pub config: PathBuf,
    /// The state directory, such as `~/.local/state`.
    pub state: PathBuf,
    /// The data directory, such as `~/.local/share`.
    pub data: PathBuf,
}

impl Bases {
    /// Returns the base directories that etcetera's base strategy finds, or
    /// `None` when it finds no home directory. An `XDG_*_HOME` variable
    /// that is unset, empty or relative falls back to its default below the
    /// home directory. Only `main` calls it, because it reads the
    /// environment.
    #[must_use]
    pub fn find() -> Option<Self> {
        let strategy = etcetera::choose_base_strategy().ok()?;
        Some(Self {
            home: strategy.home_dir().to_path_buf(),
            config: strategy.config_dir(),
            state: strategy.state_dir()?,
            data: strategy.data_dir(),
        })
    }
}

/// Where the workspace root came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RootSource {
    /// The `MXV_ROOT` environment variable named it.
    Env,
    /// The config file's `root` named it.
    Config,
    /// Neither named one, so it is `$XDG_DATA_HOME/mxv/workspaces`.
    Default,
}

/// The workspace root and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    /// The directory that holds one directory for each workspace.
    pub(crate) path: PathBuf,
    /// Where the directory came from.
    pub(crate) source: RootSource,
}

/// The value of `MXV_ROOT`, read once in `main`, and the base directories.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    /// The value of `MXV_ROOT`.
    root: Option<OsString>,
    /// The base directories, or `None` when no home directory was found.
    bases: Option<Bases>,
    /// The directory that mxv started in, which names the current
    /// workspace.
    current_dir: Option<PathBuf>,
    /// The variables of mxv's environment that phase's keepers and host
    /// processes receive.
    caller: CallerEnvironment,
    /// The mxv program, which phase starts each keeper with, or `None`
    /// when `main` could not find it.
    program: Option<PathBuf>,
}

impl Environment {
    /// Returns the snapshot of `MXV_ROOT`, read with `read`, and `bases`.
    pub fn read(mut read: impl FnMut(&str) -> Option<OsString>, bases: Option<Bases>) -> Self {
        Self {
            root: read("MXV_ROOT"),
            bases,
            current_dir: None,
            caller: CallerEnvironment::default(),
            program: None,
        }
    }

    /// Returns the base directories.
    fn bases(&self) -> Result<&Bases, MxvFailure> {
        self.bases.as_ref().ok_or(MxvFailure::NoHome)
    }

    /// Returns the snapshot with `dir` as the directory that mxv started
    /// in.
    #[must_use]
    pub fn with_current_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.current_dir = dir;
        self
    }

    /// Returns the snapshot with `caller` as the variables that phase's
    /// keepers and host processes receive, and `program` as the mxv
    /// program that phase starts each keeper with.
    #[must_use]
    pub fn with_caller(mut self, caller: CallerEnvironment, program: Option<PathBuf>) -> Self {
        self.caller = caller;
        self.program = program;
        self
    }

    /// Returns the run files under mxv's state directory, whose phase
    /// starts each keeper as `mxv phase keep` and passes the caller's
    /// variables to host processes.
    pub(crate) fn runs(&self) -> Result<Runs, MxvFailure> {
        // A sandboxed app may never touch mxv's workspace records and run
        // files, which the state directory holds, or the mirrors that
        // clones borrow objects from. The workspaces themselves also lie
        // under mxv's data directory, so only the mirrors are protected
        // there.
        let mut runs = Runs::new(&self.state()?)
            .with_caller(self.caller.clone())
            .with_protected(vec![self.state()?, self.mirrors()?]);
        if let Some(program) = &self.program {
            runs = runs.with_keeper(
                program.clone(),
                crate::KEEPER_WORDS.iter().map(OsString::from).collect(),
            );
        }
        Ok(runs)
    }

    /// Returns the run files that [`Environment::runs`] builds, or `None`
    /// when mxv's state or data directory is unknown. It is public only so
    /// that a test can start a sandboxed app under mxv's own layout.
    #[doc(hidden)]
    #[must_use]
    pub fn runs_for_tests(&self) -> Option<Runs> {
        self.runs().ok()
    }

    /// Returns the directory that mxv started in, when `main` could read
    /// it.
    pub(crate) fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Returns mxv's state directory, `$XDG_STATE_HOME/mxv`, which holds
    /// the workspace records, the run files of the workspaces' processes
    /// and phase's state.
    pub(crate) fn state(&self) -> Result<PathBuf, MxvFailure> {
        Ok(self.bases()?.state.join("mxv"))
    }

    /// Returns the directory of the workspace records,
    /// `$XDG_STATE_HOME/mxv/workspaces`.
    pub(crate) fn records(&self) -> Result<PathBuf, MxvFailure> {
        Ok(self.state()?.join("workspaces"))
    }

    /// Returns the directory of the bare mirrors that the workspaces' clones
    /// borrow objects from, `$XDG_DATA_HOME/mxv/mirrors`.
    pub(crate) fn mirrors(&self) -> Result<PathBuf, MxvFailure> {
        Ok(self.bases()?.data.join("mxv").join("mirrors"))
    }

    /// Returns the directory of the templates of workspace manifests,
    /// `$XDG_CONFIG_HOME/mxv/templates`.
    pub(crate) fn templates(&self) -> Result<PathBuf, MxvFailure> {
        Ok(self.bases()?.config.join("mxv").join("templates"))
    }

    /// Returns the path of the config file, `$XDG_CONFIG_HOME/mxv/config.toml`.
    pub(crate) fn config_file(&self) -> Result<PathBuf, MxvFailure> {
        Ok(self.bases()?.config.join("mxv").join("config.toml"))
    }

    /// Returns the settings of the config file, or the defaults when there
    /// is no config file.
    pub(crate) fn config(&self) -> Result<Config, MxvFailure> {
        config::read(&self.config_file()?, &self.bases()?.home)
    }

    /// Returns the host ports that `mxv up` allocates from: the config
    /// file's `ports_range`, or else [`DEFAULT_PORT_RANGE`].
    pub(crate) fn port_range(&self) -> Result<RangeInclusive<u16>, MxvFailure> {
        let bases = self.bases()?;
        Ok(config::read(&self.config_file()?, &bases.home)?
            .ports_range
            .unwrap_or(DEFAULT_PORT_RANGE))
    }

    /// Returns the directory that holds one directory for each workspace:
    /// `$MXV_ROOT`, or else the config file's `root`, or else
    /// `$XDG_DATA_HOME/mxv/workspaces`. mxv reads the config file only when
    /// `MXV_ROOT` is unset or empty.
    pub(crate) fn root(&self) -> Result<Root, MxvFailure> {
        if let Some(value) = self.root.as_ref().filter(|root| !root.is_empty()) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(MxvFailure::RelativeRoot { value: path });
            }
            return Ok(Root {
                path,
                source: RootSource::Env,
            });
        }
        let bases = self.bases()?;
        if let Some(path) = config::read(&self.config_file()?, &bases.home)?.root {
            return Ok(Root {
                path,
                source: RootSource::Config,
            });
        }
        Ok(Root {
            path: bases.data.join("mxv").join("workspaces"),
            source: RootSource::Default,
        })
    }
}
