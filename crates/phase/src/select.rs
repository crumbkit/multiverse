//! Which apps and processes an operation acts on.

use std::fmt;
use std::str::FromStr;

use crate::name::{AppName, BadName, ProcessName};

#[cfg(test)]
mod tests;

/// The apps and processes that an operation acts on. `up` also starts what
/// the selection depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Every app of the workspace.
    All,
    /// Only these targets. An empty list selects nothing.
    #[non_exhaustive]
    Only(Vec<Target>),
}

impl Selection {
    /// Returns the selection of only `targets`.
    #[must_use]
    pub const fn only(targets: Vec<Target>) -> Self {
        Self::Only(targets)
    }
}

/// One app, or one process of an app, written `APP` or `APP/PROCESS`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Target {
    /// The app.
    pub app: AppName,
    /// The process, or `None` for every process of the app.
    pub process: Option<ProcessName>,
}

impl fmt::Display for Target {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.process {
            Some(process) => write!(formatter, "{}/{process}", self.app),
            None => write!(formatter, "{}", self.app),
        }
    }
}

/// The reason that a text is not a valid [`Target`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadTarget {
    /// The part before the `/` is not a valid app name.
    #[non_exhaustive]
    App(BadName),
    /// The part after the `/` is not a valid process name.
    #[non_exhaustive]
    Process(BadName),
}

impl fmt::Display for BadTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::App(_) => formatter.write_str("the app name is not valid"),
            Self::Process(_) => formatter.write_str("the process name is not valid"),
        }
    }
}

impl std::error::Error for BadTarget {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::App(source) | Self::Process(source) => Some(source),
        }
    }
}

impl FromStr for Target {
    type Err = BadTarget;

    fn from_str(text: &str) -> Result<Self, BadTarget> {
        let (app, process) = match text.split_once('/') {
            Some((app, process)) => (app, Some(process)),
            None => (text, None),
        };
        Ok(Self {
            app: app.parse().map_err(BadTarget::App)?,
            process: process
                .map(str::parse)
                .transpose()
                .map_err(BadTarget::Process)?,
        })
    }
}
