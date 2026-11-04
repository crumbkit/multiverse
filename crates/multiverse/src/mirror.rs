//! The bare mirrors that the clones of workspaces borrow their objects
//! from.
//!
//! mxv keeps one bare mirror for each origin, in a directory that the
//! caller of [`Workspaces::with_mirrors`](crate::Workspaces::with_mirrors)
//! names. A mirror's path below that directory is its origin's
//! [`Origin::key`], so an `https` URL and an `ssh` URL of the same
//! repository share one mirror. Each workspace's checkout is a
//! `git clone --shared` of the mirror, which reads the mirror's objects
//! through `.git/objects/info/alternates` and writes its own objects into
//! its own `.git`. The mirror must therefore never lose an object: mxv
//! sets `gc.auto=0` and `gc.pruneExpire=never` on it, never runs `git gc`
//! or `git prune` in it, and never deletes it.

use std::path::{Component, Path, PathBuf};

#[cfg(test)]
mod tests;

/// The name of the directory below the local data directory that holds
/// the mirrors of origins that are paths on this machine.
const LOCAL: &str = "local";

/// An origin URL, made absolute, and the path of its mirror below the
/// mirror directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    /// The URL that git fetches from. A relative path is made absolute
    /// against the repository that names it, so it stays the same origin
    /// from the mirror and from each clone.
    pub(crate) url: String,
    /// The mirror's path below the mirror directory, such as
    /// `github.com/acme/app.git`.
    pub(crate) key: PathBuf,
}

impl Origin {
    /// Returns the origin `url` of the repository at `repository`, or
    /// `None` when the URL names no repository path.
    ///
    /// The key is the URL's host in lower case, with its port when it has
    /// one, followed by its path, ending in `.git`. A user name and a
    /// scheme are left out, so `https://github.com/acme/app`,
    /// `git@github.com:acme/app.git` and `ssh://git@github.com/acme/app`
    /// share the key `github.com/acme/app.git`. A local path, or a
    /// `file://` URL, has the key `local/` followed by its absolute path.
    /// Each component is encoded as [`encoded`] describes, so no key can
    /// leave the mirror directory and two different URLs never share a
    /// key.
    pub(crate) fn parse(url: &str, repository: &Path) -> Option<Self> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        let (host, path, url) = if let Some((scheme, rest)) = url.split_once("://") {
            if scheme.eq_ignore_ascii_case("file") {
                (None, rest.to_owned(), url.to_owned())
            } else {
                let (authority, path) = rest.split_once('/')?;
                let host = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                (
                    Some(host.to_ascii_lowercase()),
                    path.to_owned(),
                    url.to_owned(),
                )
            }
        } else if let Some((authority, path)) = scp_like(url) {
            let host = authority
                .rsplit_once('@')
                .map_or(authority, |(_, host)| host);
            (
                Some(host.to_ascii_lowercase()),
                path.to_owned(),
                url.to_owned(),
            )
        } else {
            let absolute = lexical(&repository.join(url));
            let text = absolute.to_str()?.to_owned();
            (None, text.clone(), text)
        };
        let mut components: Vec<String> = path
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .map(str::to_owned)
            .collect();
        let last = components.pop()?;
        let last = last.strip_suffix(".git").unwrap_or(&last).to_owned();
        if last.is_empty() {
            return None;
        }
        components.push(format!("{last}.git"));
        let mut key = PathBuf::from(host.map_or_else(|| LOCAL.to_owned(), |host| encoded(&host)));
        for component in &components {
            key.push(encoded(component));
        }
        Some(Self { url, key })
    }
}

/// Returns the host and path of a URL in git's scp-like form, such as
/// `git@github.com:acme/app.git`, which has a `:` before its first `/`.
fn scp_like(url: &str) -> Option<(&str, &str)> {
    let colon = url.find(':')?;
    if url.find('/').is_some_and(|slash| slash < colon) {
        return None;
    }
    let (authority, path) = (url.get(..colon)?, url.get(colon + 1..)?);
    (!authority.is_empty()).then_some((authority, path))
}

/// Returns `path` with each `.` and `..` component resolved by its text,
/// as git resolves a relative origin path against the repository.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved
}

/// Returns `component` as one safe path component: each byte other than an
/// ASCII letter, a digit, `-`, `_` and `.` becomes `%` and two hex digits,
/// as does a `.` at the start, so the component is never `.`, `..` or a
/// hidden name, and the encoding has one result for each input.
fn encoded(component: &str) -> String {
    let mut text = String::new();
    for (index, byte) in component.bytes().enumerate() {
        let plain = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'_'
            || (byte == b'.' && index > 0);
        if plain {
            text.push(char::from(byte));
        } else {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}
