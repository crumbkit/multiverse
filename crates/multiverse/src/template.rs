//! The templates that `mxv new` copies into a new workspace as its
//! workspace manifest.
//!
//! A template is a manifest with one more top-level table, `[template]`,
//! which lists the origins it is for. A key after a table header belongs
//! to that table, so the manifest's own top-level keys, such as `ports`,
//! come before every table:
//!
//! ```toml
//! ports = ["web"]
//!
//! [processes.web]
//! argv = ["npm", "run", "dev"]
//! ports = { PORT = "web" }
//!
//! [template]
//! origins = ["github.com/acme/app", "git@gitlab.example.com:team/app.git"]
//! ```
//!
//! An origin is written as a URL that git accepts, such as
//! `https://github.com/acme/app` or `git@github.com:acme/app.git`, or as a
//! host and path without a scheme, such as `github.com/acme/app` or
//! `github.com:443/acme/app`, where digits after the colon are a port.
//! Origins are compared by their normalised form, the key of their
//! mirror, so the `https` and `ssh` forms of a repository match each
//! other. The host is compared in lower case, and the path as written, so
//! `github.com/Acme/app` does not match `github.com/acme/app`.
//!
//! `[template]` holds only `origins`. When mxv copies the template, it
//! removes the table's header and keys, from the header to the next table
//! header, and keeps every comment and blank line.
//!
//! A template is the person's own, like the config file, so its
//! `disposable` globs are deleted without asking once it is copied. mxv
//! therefore reads only a template that is a regular file, never a
//! symbolic link, and that lies outside the root of the workspaces, where
//! a checkout could write it.
//!
//! A template that mxv cannot use, such as one that is not TOML, is never
//! copied. When the origins it lists can be read and include the
//! repository's, `mxv new` fails, because the person meant it for the
//! repository. Otherwise mxv skips it and returns a warning that names it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::manifest::{MOST_MANIFEST_BYTES, Manifest};
use crate::mirror::{Origin, redacted};

#[cfg(test)]
mod tests;

/// The top-level table of a template that lists the origins it is for.
pub const TEMPLATE_TABLE: &str = "template";

/// A template that matches a repository's origin, ready to be copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    /// The template's file.
    pub path: PathBuf,
    /// The template's text without its `[template]` table, which is a
    /// manifest that parses.
    pub text: String,
}

/// What [`find`] found in the directory of templates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The one template whose `[template]` table lists the origin, or
    /// `None`.
    pub template: Option<Template>,
    /// One sentence for each template that mxv cannot use and skipped,
    /// because the origins it lists cannot be read or do not include the
    /// repository's.
    pub skipped: Vec<String>,
}

/// One template file as mxv read it.
struct Read {
    /// The file.
    path: PathBuf,
    /// The normalised origins that it lists.
    keys: Vec<PathBuf>,
    /// Its text without the `[template]` table.
    text: String,
}

/// A template that mxv cannot use.
struct Fault {
    /// Why, as the error that names the file.
    error: Error,
    /// The normalised origins that the template lists, when they can be
    /// read in spite of the fault.
    keys: Option<Vec<PathBuf>>,
}

/// Returns the template in `dir` whose `[template]` table lists `url`, the
/// origin of the repository at `repository`, and the templates it skipped.
/// It finds none when `dir` holds no such template or does not exist.
/// mxv reads each `*.toml` file in `dir`, in name order, and ignores every
/// other entry. `root` is the root of the workspaces, under which no
/// template may lie.
///
/// # Errors
///
/// Returns [`Error::TemplateInvalid`] for a template that lists the origin
/// and cannot be used: one that is not a manifest with a valid
/// `[template]` table, has a `[template]` table that mxv cannot remove
/// cleanly, is a symbolic link, or lies under `root`. It returns
/// [`Error::TemplateAmbiguous`] when more than one template matches.
pub fn find(dir: &Path, url: &str, repository: &Path, root: &Path) -> Result<Found, Error> {
    let Some(origin) = Origin::parse(url, repository) else {
        return Ok(Found::default());
    };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut matched: Vec<Read> = Vec::new();
    let mut skipped = Vec::new();
    for path in files(dir)? {
        match read(&path, dir, &root) {
            Ok(read) if read.keys.contains(&origin.key) => matched.push(read),
            Ok(_) => {}
            Err(fault)
                if fault
                    .keys
                    .as_ref()
                    .is_some_and(|keys| keys.contains(&origin.key)) =>
            {
                return Err(fault.error);
            }
            Err(fault) => skipped.push(match fault.error {
                Error::TemplateInvalid { path, message } => format!(
                    "mxv skipped the template {}, which it cannot use: {message}",
                    path.display()
                ),
                other => format!("mxv skipped a template: {other}"),
            }),
        }
    }
    if matched.len() > 1 {
        return Err(Error::TemplateAmbiguous {
            origin: redacted(url),
            templates: matched.into_iter().map(|read| read.path).collect(),
        });
    }
    Ok(Found {
        template: matched.pop().map(|read| Template {
            path: read.path,
            text: read.text,
        }),
        skipped,
    })
}

/// Returns the `*.toml` files in `dir`, in name order, or none when `dir`
/// does not exist.
fn files(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Inaccessible {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Inaccessible {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Returns `message` as the failure of the template at `path`.
fn invalid(path: &Path, message: impl Into<String>) -> Error {
    Error::TemplateInvalid {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

/// Reads the template at `path`, whose relative local origins are relative
/// to `dir`, and which must lie outside `root`.
fn read(path: &Path, dir: &Path, root: &Path) -> Result<Read, Fault> {
    let unknown = |error| Fault { error, keys: None };
    let text = text(path).map_err(unknown)?;
    // The origins are read before anything else is checked, so a template
    // that is meant for the repository fails rather than being skipped.
    let keys = listed(&text, dir);
    let known = |error| Fault {
        error,
        keys: keys.clone(),
    };
    Manifest::parse(&text, path).map_err(|error| known(invalid(path, error.to_string())))?;
    let origins = origins(path, &text).map_err(known)?;
    let mut keys = Vec::new();
    for written in &origins {
        let url = as_url(written);
        let origin = Origin::parse(&url, dir).ok_or_else(|| {
            unknown(invalid(
                path,
                format!("the origin `{written}` of the [template] table names no repository"),
            ))
        })?;
        keys.push(origin.key);
    }
    let known = |error| Fault {
        error,
        keys: Some(keys.clone()),
    };
    let linked = !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file());
    if linked {
        return Err(known(invalid(
            path,
            "the template is a symbolic link, and mxv copies only a template that is a regular file, so no repository can have written it",
        )));
    }
    if path
        .canonicalize()
        .is_ok_and(|resolved| resolved.starts_with(root))
    {
        return Err(known(invalid(
            path,
            format!(
                "the template lies under the root of the workspaces {}, where a checkout could write it",
                root.display()
            ),
        )));
    }
    let stripped = strip(path, &text).map_err(known)?;
    Ok(Read {
        path: path.to_path_buf(),
        keys,
        text: stripped,
    })
}

/// Returns the text of the template at `path`, which must be at most
/// [`MOST_MANIFEST_BYTES`] long.
fn text(path: &Path) -> Result<String, Error> {
    let size = fs::metadata(path)
        .map_err(|error| invalid(path, error.to_string()))?
        .len();
    if size > MOST_MANIFEST_BYTES {
        return Err(invalid(
            path,
            format!("the template is larger than {MOST_MANIFEST_BYTES} bytes"),
        ));
    }
    fs::read_to_string(path).map_err(|error| invalid(path, error.to_string()))
}

/// Returns the normalised origins that the `[template]` table of `text`
/// lists, when the table can be read as a list of strings that each name
/// a repository, whatever else is wrong with the template.
fn listed(text: &str, dir: &Path) -> Option<Vec<PathBuf>> {
    let table: toml::Table = text.parse().ok()?;
    let toml::Value::Table(template) = table.get(TEMPLATE_TABLE)? else {
        return None;
    };
    let toml::Value::Array(listed) = template.get("origins")? else {
        return None;
    };
    listed
        .iter()
        .map(|item| {
            let written = item.as_str()?.trim();
            Origin::parse(&as_url(written), dir).map(|origin| origin.key)
        })
        .collect()
}

/// Returns `written`, an origin of a template, as a URL: as it is when it
/// has a scheme, is in git's scp-like form or is an absolute path, and
/// with `https://` in front when it is a host and a path, such as
/// `github.com/acme/app` or `github.com:443/acme/app`.
fn as_url(written: &str) -> String {
    // `host:443/path` names a port, and `host:team/app` is git's
    // scp-like form.
    let scp_like = written.find(':').is_some_and(|colon| {
        let slash = written.find('/');
        let port = written
            .get(colon.saturating_add(1)..slash.unwrap_or(written.len()))
            .is_some_and(|between| {
                !between.is_empty() && between.bytes().all(|byte| byte.is_ascii_digit())
            });
        slash.is_none_or(|slash| colon < slash) && !port
    });
    if written.contains("://") || scp_like || written.starts_with('/') || written.starts_with('.') {
        written.to_owned()
    } else {
        format!("https://{written}")
    }
}

/// Returns the origins of the `[template]` table of `text`, the template
/// at `path`.
fn origins(path: &Path, text: &str) -> Result<Vec<String>, Error> {
    let table: toml::Table = text
        .parse()
        .map_err(|error: toml::de::Error| invalid(path, error.message().trim().to_owned()))?;
    let shape = "the template needs a top-level [template] table that holds only `origins`, a list of the origin URLs it is for, such as `origins = [\"github.com/acme/app\"]`, and the manifest's own top-level keys, such as `ports`, come before every table";
    let Some(toml::Value::Table(template)) = table.get(TEMPLATE_TABLE) else {
        return Err(invalid(path, shape));
    };
    if template.keys().any(|key| key != "origins") {
        return Err(invalid(path, shape));
    }
    let Some(toml::Value::Array(listed)) = template.get("origins") else {
        return Err(invalid(path, shape));
    };
    let mut origins = Vec::new();
    for item in listed {
        match item {
            toml::Value::String(origin) if !origin.trim().is_empty() => {
                origins.push(origin.trim().to_owned());
            }
            _ => return Err(invalid(path, shape)),
        }
    }
    if origins.is_empty() {
        return Err(invalid(path, shape));
    }
    Ok(origins)
}

/// Reports whether `line` is the header of the `[template]` table or of a
/// table inside it.
fn is_template_header(line: &str) -> bool {
    let line = line.trim_start();
    line.strip_prefix('[')
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix(TEMPLATE_TABLE))
        .map(str::trim_start)
        .is_some_and(|rest| rest.starts_with(']') || rest.starts_with('.'))
}

/// Returns `text`, the template at `path`, without its `[template]` table:
/// each line from the table's header to the next header of another
/// table, except comments and blank lines. The result must hold every other key of `text` and nothing of
/// the table, or the template fails as invalid.
fn strip(path: &Path, text: &str) -> Result<String, Error> {
    let mut kept = String::with_capacity(text.len());
    let mut inside = false;
    let mut found = false;
    for line in text.split_inclusive('\n') {
        if is_template_header(line) {
            inside = true;
            found = true;
            continue;
        }
        if inside && line.trim_start().starts_with('[') {
            inside = false;
        }
        // A comment or a blank line changes no key, so it is kept, as is
        // a comment above the next table's header.
        let trimmed = line.trim_start();
        if !inside || trimmed.is_empty() || trimmed.starts_with('#') {
            kept.push_str(line);
        }
    }
    let unclean = || {
        invalid(
            path,
            "mxv cannot remove the [template] table cleanly; write it as a `[template]` header followed by its `origins` key, with no inline `template = { ... }` form",
        )
    };
    if !found {
        return Err(unclean());
    }
    let mut original: toml::Table = text.parse().map_err(|_| unclean())?;
    original.remove(TEMPLATE_TABLE);
    let stripped: toml::Table = kept.parse().map_err(|_| unclean())?;
    if stripped != original {
        return Err(unclean());
    }
    Ok(kept)
}
