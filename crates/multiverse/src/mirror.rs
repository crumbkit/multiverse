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
    /// The key of a remote URL is its host in lower case, with its port
    /// when the port is not the scheme's default, followed by its path,
    /// ending in `.git`. A user name, a password and a scheme are left
    /// out, so `https://github.com/acme/app`,
    /// `git@github.com:acme/app.git` and
    /// `ssh://git@github.com:22/acme/app` share the key
    /// `github.com/acme/app.git`. The path keeps its case, because some
    /// hosts tell `Acme/App` and `acme/app` apart.
    ///
    /// A local path, or a `file://` URL, has the key `local/` followed by
    /// its absolute path with each link resolved, and `.git` added to its
    /// last component, so `/srv/app` and `/srv/app.git`, which are
    /// different repositories, have mirrors of their own.
    ///
    /// Each component is encoded as [`encoded`] describes, and a component
    /// before the last that ends in `.git` or `.lock` has its last `.`
    /// encoded too, so no key can leave the mirror directory, two
    /// different URLs never share a key, and no mirror or mirror lock lies
    /// inside another mirror's path.
    pub(crate) fn parse(url: &str, repository: &Path) -> Option<Self> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        let (host, path, url) = if let Some((scheme, rest)) = url.split_once("://") {
            if scheme.eq_ignore_ascii_case("file") {
                (None, resolved(Path::new(rest))?, url.to_owned())
            } else {
                let (authority, path) = rest.split_once('/')?;
                (
                    Some(host_of(authority, default_port(scheme))),
                    path.to_owned(),
                    url.to_owned(),
                )
            }
        } else if let Some((authority, path)) = scp_like(url) {
            (
                Some(host_of(authority, None)),
                path.to_owned(),
                url.to_owned(),
            )
        } else {
            let absolute = lexical(&repository.join(url));
            let text = absolute.to_str()?.to_owned();
            (None, resolved(&absolute)?, text)
        };
        let mut components: Vec<String> = path
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .map(str::to_owned)
            .collect();
        let last = components.pop()?;
        let last = if host.is_some() {
            last.strip_suffix(".git").unwrap_or(&last).to_owned()
        } else {
            last
        };
        if last.is_empty() {
            return None;
        }
        let mut key = PathBuf::from(host.map_or_else(|| LOCAL.to_owned(), |host| encoded(&host)));
        for component in &components {
            key.push(directory(component));
        }
        key.push(format!("{}.git", encoded(&last)));
        Some(Self { url, key })
    }
}

/// Returns `url` with the user name and password of each `scheme://`
/// URL in it left out, such as `https://example.com/app.git` for
/// `https://someone:token@example.com/app.git`, so a credential that a
/// URL carries never reaches a message, a record or a result. A URL in
/// git's scp-like form, such as `git@github.com:acme/app`, cannot carry a
/// password and is left as it is.
pub(crate) fn redacted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let (before, after) = rest.split_at(index + 3);
        out.push_str(before);
        let end = after
            .find(|character: char| {
                character == '/'
                    || character.is_whitespace()
                    || character == '\''
                    || character == '"'
            })
            .unwrap_or(after.len());
        let authority = after.get(..end).unwrap_or_default();
        match authority.rfind('@') {
            Some(at) => out.push_str(authority.get(at + 1..).unwrap_or_default()),
            None => out.push_str(authority),
        }
        rest = after.get(end..).unwrap_or_default();
    }
    out.push_str(rest);
    out
}

/// Returns the host of `authority` in lower case, without its user name
/// and password, and without its port when the port is `default`.
fn host_of(authority: &str, default: Option<&str>) -> String {
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_ascii_lowercase();
    match (host.rsplit_once(':'), default) {
        (Some((name, port)), Some(default)) if port == default => name.to_owned(),
        _ => host,
    }
}

/// Returns the port that a URL with `scheme` uses when it names none.
fn default_port(scheme: &str) -> Option<&'static str> {
    match scheme.to_ascii_lowercase().as_str() {
        "ssh" | "git+ssh" | "ssh+git" => Some("22"),
        "https" => Some("443"),
        "http" => Some("80"),
        "git" => Some("9418"),
        _ => None,
    }
}

/// Returns `path` with each link resolved when it exists, or as its text
/// says when it does not, as text.
fn resolved(path: &Path) -> Option<String> {
    let path = path.canonicalize().unwrap_or_else(|_| lexical(path));
    path.to_str().map(str::to_owned)
}

/// Returns `component`, a directory above a mirror, encoded as
/// [`encoded`] describes, with the last `.` of a `.git` or `.lock` ending
/// encoded as well, so the directory is never a mirror or a mirror's
/// lock.
pub(crate) fn directory(component: &str) -> String {
    let text = encoded(component);
    for ending in [".git", ".lock"] {
        if let Some(stem) = text.strip_suffix(ending) {
            return format!("{stem}%2E{}", ending.get(1..).unwrap_or_default());
        }
    }
    text
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
