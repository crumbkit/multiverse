//! The person's config file, `$XDG_CONFIG_HOME/mxv/config.toml`, which can
//! name the workspace root, the range of host ports that `mxv up`
//! allocates from, and how mxv treats the disk that build output takes.
//!
//! The file is TOML with these optional keys.
//!
//! - `root` is a string that is an absolute path, or a path that starts
//!   with `~/`, which starts at the home directory.
//! - `ports_range` is an array of two ports, the first and the last, such
//!   as `[20000, 29999]`, from 1024 to 32767 with the first no greater than
//!   the last.
//! - `disposable` is an array of globs relative to a repository's root,
//!   such as `[".venv/"]`, which extend the built-in disposable directories
//!   in every workspace.
//! - `clone_on_new` is an array of globs of the ignored directories that
//!   `mxv new` clones from the repository into the new checkout. It
//!   replaces the default `["node_modules/"]`, so `[]` clones nothing.
//! - `clean_idle` is how long a checkout must go unmodified before
//!   `mxv clean` treats it as idle, such as `"7d"`.
//! - `max_disposable` is the size that `mxv clean --over-limit` keeps the
//!   disposable directories of every workspace under, such as `"50GiB"`.
//! - `stop_idle` is how long a running workspace must go unused before
//!   `mxv reclaim` may stop it, such as `"2h"`.
//! - `archive_max_file` is the largest file of a checkout's uncommitted
//!   work that `mxv archive` keeps, such as `"1GiB"`. A larger file
//!   refuses the archive. It defaults to `"100MiB"`.
//! - `proxy_http` and `proxy_https` are the loopback addresses, such as
//!   `"127.0.0.1:80"`, where the proxy serves the routes of workspaces. They
//!   default to ports 80 and 443 on `127.0.0.1`.
//! - `caddy` is the absolute path, or a path that starts with `~/`, of the
//!   `caddy` program. Without it mxv uses the first `caddy` on `PATH`.
//!
//! mxv parses the file strictly: any other key, a value of another type, a
//! relative path, a glob that is absolute or holds `..`, a `clone_on_new`
//! glob that names `target`, or text that is not TOML fails with
//! `mxv::config::invalid`, which names the file and the line.

use std::io;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

use multiverse::{BadGlob, PathGlob};
use toml::Spanned;
use toml::de::{DeTable, DeValue};

use crate::failures::MxvFailure;
use crate::units::{parse_age, parse_size};

#[cfg(test)]
mod tests;

/// The settings that the config file holds.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Config {
    /// The workspace root that the file names, with a leading `~`
    /// expanded, or `None` when the file names none.
    pub(crate) root: Option<PathBuf>,
    /// The host ports that `mxv up` allocates from, or `None` when the file
    /// sets no range.
    pub(crate) ports_range: Option<RangeInclusive<u16>>,
    /// The person's globs of disposable paths, which extend the built-in
    /// set in every workspace.
    pub(crate) disposable: Vec<PathGlob>,
    /// The globs of the ignored directories that `mxv new` clones, or
    /// `None` when the file names none and the default applies.
    pub(crate) clone_on_new: Option<Vec<PathGlob>>,
    /// How long a checkout must go unmodified before `mxv clean` treats it
    /// as idle, or `None` for the default.
    pub(crate) clean_idle: Option<Duration>,
    /// The bytes that `mxv clean --over-limit` keeps the disposable
    /// directories of every workspace under, or `None` when the file sets
    /// no limit.
    pub(crate) max_disposable: Option<u64>,
    /// How long a running workspace must go unused before `mxv reclaim`
    /// may stop it, or `None` for the default.
    pub(crate) stop_idle: Option<Duration>,
    /// The largest file of a checkout's uncommitted work that `mxv archive`
    /// keeps, in bytes, or `None` for the default.
    pub(crate) archive_max_file: Option<u64>,
    /// The loopback address where the proxy serves HTTP, or `None` for the
    /// default.
    pub(crate) proxy_http: Option<SocketAddr>,
    /// The loopback address where the proxy serves HTTPS, or `None` for
    /// the default.
    pub(crate) proxy_https: Option<SocketAddr>,
    /// The path of the `caddy` program, or `None` to search `PATH`.
    pub(crate) caddy: Option<PathBuf>,
}

/// The lowest port that `ports_range` may hold. Ports below it are
/// privileged on Unix.
const LOWEST_PORT: u16 = 1024;

/// The highest port that `ports_range` may hold. Ports above it fall in the
/// ephemeral range of Linux, which starts at 32768, or of macOS, which
/// starts at 49152, where outgoing connections take their ports.
const HIGHEST_PORT: u16 = 32767;

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
/// than `root` and `ports_range`, a `root` that is not a string, a `root`
/// that is not an absolute path once `~` is expanded, and a `ports_range`
/// that is not two ports in order from 1024 to 32767.
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
        match name {
            "root" => {
                config.root = Some(root(value.get_ref(), home).map_err(|reason| bad(at, reason))?);
            }
            "ports_range" => {
                config.ports_range =
                    Some(ports_range(value.get_ref()).map_err(|reason| bad(at, reason))?);
            }
            "disposable" => {
                config.disposable = globs(name, value, PathGlob::parse, &bad)?;
            }
            "clone_on_new" => {
                config.clone_on_new = Some(globs(name, value, PathGlob::parse_clone, &bad)?);
            }
            "clean_idle" => {
                config.clean_idle =
                    Some(unit(name, value.get_ref(), parse_age).map_err(|reason| bad(at, reason))?);
            }
            "max_disposable" => {
                config.max_disposable = Some(
                    unit(name, value.get_ref(), parse_size).map_err(|reason| bad(at, reason))?,
                );
            }
            "stop_idle" => {
                config.stop_idle =
                    Some(unit(name, value.get_ref(), parse_age).map_err(|reason| bad(at, reason))?);
            }
            "archive_max_file" => {
                config.archive_max_file = Some(
                    unit(name, value.get_ref(), parse_size).map_err(|reason| bad(at, reason))?,
                );
            }
            "proxy_http" | "proxy_https" => {
                let address = loopback(name, value.get_ref()).map_err(|reason| bad(at, reason))?;
                if name == "proxy_http" {
                    config.proxy_http = Some(address);
                } else {
                    config.proxy_https = Some(address);
                }
            }
            "caddy" => {
                config.caddy =
                    Some(caddy(value.get_ref(), home).map_err(|reason| bad(at, reason))?);
            }
            _ => {
                return Err(bad(
                    at,
                    format!(
                        "the key {name:?} is not a setting; the settings are {}",
                        SETTINGS.join(", ")
                    ),
                ));
            }
        }
    }
    Ok(config)
}

/// The keys that the config file may set, as a failure names them.
pub(crate) const SETTINGS: &[&str] = &[
    "`root`",
    "`ports_range`",
    "`disposable`",
    "`clone_on_new`",
    "`clean_idle`",
    "`max_disposable`",
    "`stop_idle`",
    "`archive_max_file`",
    "`proxy_http`",
    "`proxy_https`",
    "`caddy`",
];

/// Returns the globs of the array `value` of the key `key`, each read with
/// `parse`, or the failure that `bad` builds at the line of the value or
/// of the glob at fault.
fn globs(
    key: &str,
    value: &Spanned<DeValue<'_>>,
    parse: fn(&str) -> Result<PathGlob, BadGlob>,
    bad: &impl Fn(Option<usize>, String) -> MxvFailure,
) -> Result<Vec<PathGlob>, MxvFailure> {
    let shape = || {
        bad(
            Some(value.span().start),
            format!("`{key}` must be an array of globs, such as [\"web/test-results/\"]"),
        )
    };
    let items = value.get_ref().as_array().ok_or_else(shape)?;
    items
        .iter()
        .map(|item| {
            let text = item.get_ref().as_str().ok_or_else(shape)?;
            parse(text).map_err(|reason| {
                bad(
                    Some(item.span().start),
                    format!("the `{key}` glob {text:?} is refused: {reason}"),
                )
            })
        })
        .collect()
}

/// Returns the value of the key `key`, a string that `parse` reads, or the
/// reason it is not one.
fn unit<T>(
    key: &str,
    value: &DeValue<'_>,
    parse: fn(&str) -> Result<T, String>,
) -> Result<T, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{key}` must be a string, such as \"7d\" or \"50GiB\""))?;
    parse(text).map_err(|reason| format!("`{key}` is refused: {reason}"))
}

/// Returns the loopback socket address that `value` holds for the key
/// `key`, or the reason it is not one.
fn loopback(key: &str, value: &DeValue<'_>) -> Result<SocketAddr, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{key}` must be a string, such as \"127.0.0.1:80\""))?;
    let address: SocketAddr = text.parse().map_err(|_| {
        format!(
            "`{key}` is {text:?}, and it must be an address with a port, such as \"127.0.0.1:80\""
        )
    })?;
    if !address.ip().is_loopback() {
        return Err(format!(
            "`{key}` is {text:?}, and it must be a loopback address, because the proxy serves only this machine"
        ));
    }
    Ok(address)
}

/// Returns the `caddy` path that `value` names, with `~` meaning `home`, or
/// the reason it is not an absolute path.
fn caddy(value: &DeValue<'_>, home: &Path) -> Result<PathBuf, String> {
    let text = value
        .as_str()
        .ok_or_else(|| "`caddy` must be a string that holds an absolute path".to_owned())?;
    let expanded = expand(text, home)
        .ok_or_else(|| format!("`caddy` is {text:?}, and only `~` or `~/` may start a path"))?;
    if !expanded.is_absolute() {
        return Err(format!(
            "`caddy` is the relative path {text:?}, and it must be an absolute path"
        ));
    }
    Ok(expanded)
}

/// Returns the root that `value` names, with `~` meaning `home`, or the
/// reason it is not an absolute path.
fn root(value: &DeValue<'_>, home: &Path) -> Result<PathBuf, String> {
    let Some(written) = value.as_str() else {
        return Err("`root` must be a string that holds an absolute path".to_owned());
    };
    if written.contains('\0') {
        return Err("`root` holds a NUL character, which no path can hold".to_owned());
    }
    let expanded = expand(written, home)
        .ok_or_else(|| format!("`root` is {written:?}, and only `~` or `~/` may start a path"))?;
    if !expanded.is_absolute() {
        return Err(format!(
            "`root` is the relative path {written:?}, and it must be an absolute path"
        ));
    }
    Ok(expanded)
}

/// Returns the range that `value` holds, or the reason it is not two
/// ports in order from [`LOWEST_PORT`] to [`HIGHEST_PORT`].
fn ports_range(value: &DeValue<'_>) -> Result<RangeInclusive<u16>, String> {
    let shape = || {
        format!(
            "`ports_range` must be an array of two ports, the first and the last, such as [20000, 29999], from {LOWEST_PORT} to {HIGHEST_PORT}"
        )
    };
    let ports: Vec<u16> = value
        .as_array()
        .ok_or_else(shape)?
        .iter()
        .map(|item| {
            item.get_ref()
                .as_integer()
                .and_then(|integer| u16::from_str_radix(integer.as_str(), integer.radix()).ok())
                .ok_or_else(shape)
        })
        .collect::<Result<_, _>>()?;
    let [first, last] = ports.as_slice() else {
        return Err(shape());
    };
    if first > last {
        return Err(format!(
            "`ports_range` starts at {first}, after its last port {last}"
        ));
    }
    if *first < LOWEST_PORT || *last > HIGHEST_PORT {
        return Err(format!(
            "`ports_range` is {first} to {last}, and it must lie from {LOWEST_PORT} to {HIGHEST_PORT}, below the ephemeral ports that outgoing connections take"
        ));
    }
    Ok(*first..=*last)
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
