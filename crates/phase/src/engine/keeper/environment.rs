//! The environment that the keeper received from the engine, which the
//! program that links phase reads in `main` and hands to the keeper as an
//! opaque value. The library itself never reads the environment (BASE-6,
//! LIB-1).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::process::Command;

/// The keeper's own environment, as the program that links phase read it
/// in `main`. It holds the values of the process's resolved environment
/// that the engine gave the keeper, which the keeper passes on to its
/// app's first start, and those of any later start that does not resolve
/// a fresh environment.
///
/// The type shows no value: its `Debug` lists only the names, and it has
/// no `Display`, no serialization and no getter of a value. Its only use
/// of a value is to set it on the app's command.
///
/// mxv's hidden `phase keep` subcommand builds it in `main`, before
/// anything else reads the environment, with
/// `KeeperEnvironment::from_vars(std::env::vars_os())`, and passes it to
/// [`crate::keeper::run`] with the parsed arguments.
pub struct KeeperEnvironment {
    /// The variables, by name.
    vars: BTreeMap<OsString, OsString>,
}

impl KeeperEnvironment {
    /// Returns the environment that holds `vars`, such as the program's
    /// own from `std::env::vars_os()`.
    pub fn from_vars(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self {
            vars: vars.into_iter().collect(),
        }
    }

    /// Keeps only the variables that `names` lists, so the keeper holds no
    /// value that its app does not receive.
    pub(crate) fn retain(&mut self, names: &BTreeSet<String>) {
        self.vars
            .retain(|name, _| name.to_str().is_some_and(|name| names.contains(name)));
    }

    /// Sets each variable that `names` lists and the environment holds on
    /// `command`.
    pub(crate) fn apply_named(&self, names: &BTreeSet<String>, command: &mut Command) {
        for name in names {
            if let Some(value) = self.vars.get(std::ffi::OsStr::new(name)) {
                command.env(name, value);
            }
        }
    }
}

impl fmt::Debug for KeeperEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeeperEnvironment")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}
