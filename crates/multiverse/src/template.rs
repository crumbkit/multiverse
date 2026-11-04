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
//! host and path without a scheme, such as `github.com/acme/app`. Origins
//! are compared by their normalised form, the key of their mirror, so the
//! `https` and `ssh` forms of a repository match each other. `[template]`
//! holds only `origins`, and mxv removes the table, from its header to the
//! next table header, when it copies the template.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::manifest::Manifest;
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

/// One template file as mxv read it.
struct Read {
    /// The file.
    path: PathBuf,
    /// The normalised origins that it lists.
    keys: Vec<PathBuf>,
    /// Its text without the `[template]` table.
    text: String,
}

/// Returns the template in `dir` whose `[template]` table lists `url`, the
/// origin of the repository at `repository`, or `None` when `dir` holds no
/// such template or does not exist. mxv reads each `*.toml` file in `dir`,
/// in name order, and ignores every other entry.
///
/// # Errors
///
/// Returns [`Error::TemplateInvalid`] for a template that cannot be read,
/// is not a manifest with a valid `[template]` table, or has a `[template]`
/// table that mxv cannot remove cleanly, whether or not it matches, and
/// [`Error::TemplateAmbiguous`] when more than one template matches.
pub fn find(dir: &Path, url: &str, repository: &Path) -> Result<Option<Template>, Error> {
    let Some(origin) = Origin::parse(url, repository) else {
        return Ok(None);
    };
    let mut matched: Vec<Read> = Vec::new();
    for path in files(dir)? {
        let read = read(&path, dir)?;
        if read.keys.contains(&origin.key) {
            matched.push(read);
        }
    }
    if matched.len() > 1 {
        return Err(Error::TemplateAmbiguous {
            origin: redacted(url),
            templates: matched.into_iter().map(|read| read.path).collect(),
        });
    }
    Ok(matched.pop().map(|read| Template {
        path: read.path,
        text: read.text,
    }))
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
/// to `dir`.
fn read(path: &Path, dir: &Path) -> Result<Read, Error> {
    let text = Manifest::read_text(path)
        .map_err(|error| invalid(path, error.to_string()))?
        .ok_or_else(|| invalid(path, "the template disappeared while mxv read it"))?;
    let origins = origins(path, &text)?;
    let mut keys = Vec::new();
    for written in &origins {
        let url = as_url(written);
        let origin = Origin::parse(&url, dir).ok_or_else(|| {
            invalid(
                path,
                format!("the origin `{written}` of the [template] table names no repository"),
            )
        })?;
        keys.push(origin.key);
    }
    let stripped = strip(path, &text)?;
    Ok(Read {
        path: path.to_path_buf(),
        keys,
        text: stripped,
    })
}

/// Returns `written`, an origin of a template, as a URL: as it is when it
/// has a scheme, is in git's scp-like form or is an absolute path, and
/// with `https://` in front when it is a host and a path, such as
/// `github.com/acme/app`.
fn as_url(written: &str) -> String {
    let scp_like = written
        .find(':')
        .is_some_and(|colon| written.find('/').is_none_or(|slash| colon < slash));
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
/// table. The result must hold every other key of `text` and nothing of
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
        if !inside {
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
