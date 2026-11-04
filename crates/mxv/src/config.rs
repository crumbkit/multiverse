//! The person's config file, `$XDG_CONFIG_HOME/mxv/config.toml`, which can
//! name the workspace root.
//!
//! The file is TOML with one key, `root`, whose value is a string that is an
//! absolute path, or a path that starts with `~/`, which starts at the home
//! directory. mxv parses it strictly: any other key, a value of another
//! type, a relative path or text that is not TOML fails with
//! `mxv::config::invalid`, which names the file and the line.

use std::io;
use std::path::{Path, PathBuf};

use toml::de::DeTable;

use crate::failures::MxvFailure;

#[cfg(test)]
mod tests;

/// The settings that the config file holds.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Config {
    /// The workspace root that the file names, with a leading `~`
    /// expanded, or `None` when the file names none.
    pub(crate) root: Option<PathBuf>,
}

/// Reads the config file at `path`, with `~` meaning `home`. A missing
/// file holds no settings.
///
/// # Errors
///
/// Returns [`MxvFailure::ConfigUnreadable`] when the file is there but
/// cannot be read, and the errors of [`parse`].
pub(crate) fn read(path: &Path, home: &Path) -> Result<Config, MxvFailure> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(path, &text, home),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(MxvFailure::ConfigUnreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Parses `text`, the config file at `path`, with `~` meaning `home`.
///
/// # Errors
///
/// Returns [`MxvFailure::BadConfig`] for text that is not TOML, a key other
/// than `root`, a `root` that is not a string, and a `root` that is not an
/// absolute path once `~` is expanded.
pub(crate) fn parse(path: &Path, text: &str, home: &Path) -> Result<Config, MxvFailure> {
    let bad = |at: Option<usize>, reason: String| MxvFailure::BadConfig {
        path: path.to_path_buf(),
        line: at.map(|offset| line(text, offset)),
        reason,
    };
    let table = DeTable::parse(text).map_err(|error| {
        bad(
            error.span().map(|span| span.start),
            error.message().to_owned(),
        )
    })?;
    let mut config = Config::default();
    for (key, value) in table.get_ref() {
        let at = Some(key.span().start);
        let name: &str = key.get_ref().as_ref();
        if name != "root" {
            return Err(bad(
                at,
                format!("the key {name:?} is not a setting; the only setting is `root`"),
            ));
        }
        let Some(written) = value.get_ref().as_str() else {
            return Err(bad(
                at,
                "`root` must be a string that holds an absolute path".to_owned(),
            ));
        };
        let expanded = expand(written, home).ok_or_else(|| {
            bad(
                at,
                format!("`root` is {written:?}, and only `~` or `~/` may start a path"),
            )
        })?;
        if !expanded.is_absolute() {
            return Err(bad(
                at,
                format!("`root` is the relative path {written:?}, and it must be an absolute path"),
            ));
        }
        config.root = Some(expanded);
    }
    Ok(config)
}

/// Returns `written` with a leading `~` or `~/` replaced by `home`, or
/// `None` for `~user`, which mxv does not look up.
fn expand(written: &str, home: &Path) -> Option<PathBuf> {
    if written == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(rest) = written.strip_prefix("~/") {
        return Some(home.join(rest));
    }
    if written.starts_with('~') {
        return None;
    }
    Some(PathBuf::from(written))
}

/// Returns the line number, counted from 1, of the byte `offset` of
/// `text`.
fn line(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(1, |before| before.matches('\n').count() + 1)
}
