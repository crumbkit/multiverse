//! The globs that name disposable paths and the directories that a new
//! workspace clones, and the set of disposable paths of a checkout.
//!
//! A glob names paths relative to a repository's root, in the form that
//! `git status` prints them: a directory ends with `/`. The syntax is a
//! small part of gitignore's:
//!
//! - A glob that ends with `/` matches only a directory.
//! - A glob with no `/` other than a trailing one, such as `node_modules/`,
//!   matches that name at any depth. Any other glob, such as
//!   `web/test-results/`, is anchored at the repository's root.
//! - `*` matches any characters inside one path component, `?` matches one
//!   character, and a component that is `**` matches any number of
//!   components, including none.
//!
//! A glob matches a path as `git status` reports it, and never a path
//! inside a directory that it names. A glob that is empty, starts with `/`,
//! holds a `..`, `.` or empty component, or holds a NUL character is
//! refused.
//!
//! A repository's own `mxv.toml` is not trusted to widen what mxv deletes,
//! because a repository is shared and its manifest can change with a pull.
//! Its globs, and every glob of directories to clone, must end with `/` and
//! have a literal last name, as [`PathGlob::parse_directory`] checks. Only
//! the person's config file may name files or use a wildcard in the last
//! name.

use std::fmt;

use crate::workspaces::DISPOSABLE;

#[cfg(test)]
mod tests;

/// The name of the directory that a new workspace never clones, because a
/// build of the checkout's own sources writes it.
pub const CLONE_NEVER: &str = "target";

/// The globs of the directories that a new workspace clones when neither
/// the config file nor the manifest names any.
pub const DEFAULT_CLONE_ON_NEW: &[&str] = &["node_modules/"];

/// A glob relative to a repository's root, as the module docs describe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathGlob {
    /// The glob as it was written.
    text: String,
    /// The glob's components, without a trailing empty one.
    components: Vec<String>,
    /// Whether the glob matches only a directory, because it ends with `/`.
    directory: bool,
    /// Whether the glob is anchored at the repository's root, because it
    /// holds a `/` other than a trailing one.
    anchored: bool,
}

/// Why a glob is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadGlob {
    /// The glob is empty.
    Empty,
    /// The glob starts with `/`, so it names a path outside the
    /// repository.
    Absolute,
    /// The glob holds a `..` component, so it can name a path outside the
    /// repository.
    Parent,
    /// The glob holds a `.` component.
    Dot,
    /// The glob holds an empty component, such as `web//x/`.
    EmptyComponent,
    /// The glob holds a NUL character, which no path can hold.
    Nul,
    /// A glob of directories to clone names `target`, which a new
    /// workspace never clones.
    Target,
    /// A glob from a repository's `mxv.toml`, or of directories to clone,
    /// does not end with `/`, so it could name a file.
    NotDirectory,
    /// A glob from a repository's `mxv.toml`, or of directories to clone,
    /// has a last component with `*` or `?`, so it could name any ignored
    /// directory, such as one that holds a nested repository.
    Wildcard,
}

impl fmt::Display for BadGlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "the glob is empty",
            Self::Absolute => {
                "the glob is an absolute path, and it must be relative to the repository's root"
            }
            Self::Parent => {
                "the glob holds a `..` component, which could name a path outside the repository"
            }
            Self::Dot => "the glob holds a `.` component, which it must leave out",
            Self::EmptyComponent => "the glob holds an empty component, such as `a//b`",
            Self::Nul => "the glob holds a NUL character, which no path can hold",
            Self::Target => {
                "a new workspace never clones `target`, because a build of the checkout's own sources writes it"
            }
            Self::NotDirectory => {
                "the glob must name a directory and end with `/`, such as `web/test-results/`"
            }
            Self::Wildcard => {
                "the glob must name a directory by a literal last name without `*` or `?`, such as `**/coverage/`"
            }
        })
    }
}

impl std::error::Error for BadGlob {}

impl PathGlob {
    /// Returns the glob that `text` writes.
    ///
    /// # Errors
    ///
    /// Returns the [`BadGlob`] that names why the glob is refused.
    pub fn parse(text: &str) -> Result<Self, BadGlob> {
        if text.is_empty() {
            return Err(BadGlob::Empty);
        }
        if text.contains('\0') {
            return Err(BadGlob::Nul);
        }
        if text.starts_with('/') {
            return Err(BadGlob::Absolute);
        }
        let (body, directory) = match text.strip_suffix('/') {
            Some(body) => (body, true),
            None => (text, false),
        };
        let components: Vec<String> = body.split('/').map(str::to_owned).collect();
        for component in &components {
            match component.as_str() {
                ".." => return Err(BadGlob::Parent),
                "." => return Err(BadGlob::Dot),
                "" => return Err(BadGlob::EmptyComponent),
                _ => {}
            }
        }
        Ok(Self {
            text: text.to_owned(),
            anchored: components.len() > 1,
            components,
            directory,
        })
    }

    /// Returns the glob that `text` writes, for a repository's own
    /// `mxv.toml`. A repository is not trusted to widen what mxv deletes, so
    /// the glob must name directories by a literal last name. It refuses
    /// what [`PathGlob::parse`] refuses, a glob that does not end with `/`,
    /// and a glob whose last component holds `*` or `?`. Components before
    /// the last may hold wildcards, as in `**/coverage/`.
    ///
    /// # Errors
    ///
    /// Returns the [`BadGlob`] that names why the glob is refused.
    pub fn parse_directory(text: &str) -> Result<Self, BadGlob> {
        let glob = Self::parse(text)?;
        if !glob.directory {
            return Err(BadGlob::NotDirectory);
        }
        if glob.last_name().is_none() {
            return Err(BadGlob::Wildcard);
        }
        Ok(glob)
    }

    /// Returns the glob that `text` writes, for the directories that a new
    /// workspace clones. It refuses what [`PathGlob::parse_directory`]
    /// refuses, and a glob whose last component is `target`.
    ///
    /// # Errors
    ///
    /// Returns the [`BadGlob`] that names why the glob is refused.
    pub fn parse_clone(text: &str) -> Result<Self, BadGlob> {
        let glob = Self::parse_directory(text)?;
        if glob.last_name() == Some(CLONE_NEVER) {
            return Err(BadGlob::Target);
        }
        Ok(glob)
    }

    /// Returns the glob as it was written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns the name that the glob's last component matches when it is
    /// a plain name without `*` or `?`, such as `target` for `target/`.
    #[must_use]
    pub fn last_name(&self) -> Option<&str> {
        self.components
            .last()
            .map(String::as_str)
            .filter(|name| !name.contains(['*', '?']))
    }

    /// Reports whether the glob matches `path`, a path relative to the
    /// repository's root as `git status` prints it, with a trailing `/` for
    /// a directory.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let (body, is_directory) = match path.strip_suffix('/') {
            Some(body) => (body, true),
            None => (path, false),
        };
        if self.directory && !is_directory {
            return false;
        }
        let parts: Vec<&str> = body.split('/').collect();
        if self.anchored {
            let pattern: Vec<&str> = self.components.iter().map(String::as_str).collect();
            components_match(&pattern, &parts)
        } else {
            match (self.components.first(), parts.last()) {
                (Some(pattern), Some(name)) => component_matches(pattern, name),
                _ => false,
            }
        }
    }
}

impl fmt::Display for PathGlob {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// Reports whether the glob components `pattern` match the path components
/// `parts`, where a `**` component matches any number of components.
fn components_match(pattern: &[&str], parts: &[&str]) -> bool {
    match pattern.split_first() {
        None => parts.is_empty(),
        Some((&"**", rest)) => (0..=parts.len()).any(|skip| {
            parts
                .get(skip..)
                .is_some_and(|tail| components_match(rest, tail))
        }),
        Some((first, rest)) => match parts.split_first() {
            Some((part, tail)) => component_matches(first, part) && components_match(rest, tail),
            None => false,
        },
    }
}

/// Reports whether the glob component `pattern` matches the path
/// component `name`, where `*` matches any characters and `?` matches one.
fn component_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    chars_match(&pattern, &name)
}

/// Reports whether the glob characters `pattern` match the characters
/// `name`.
fn chars_match(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len())
            .any(|skip| name.get(skip..).is_some_and(|tail| chars_match(rest, tail))),
        Some(('?', rest)) => name
            .split_first()
            .is_some_and(|(_, tail)| chars_match(rest, tail)),
        Some((first, rest)) => name
            .split_first()
            .is_some_and(|(char, tail)| char == first && chars_match(rest, tail)),
    }
}

/// What made a path disposable, from the most trusted source to the least.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum DisposableSource {
    /// One of mxv's built-in [`DISPOSABLE`] directory names.
    BuiltIn,
    /// A glob of the person's config file.
    Config,
    /// A glob of the repository's own `mxv.toml` and nothing else. A
    /// repository is shared, so `mxv remove` deletes such a path only after
    /// the person confirms it at the prompt or passes `--force`.
    Manifest,
}

/// The globs that name a checkout's disposable paths: the built-in
/// [`DISPOSABLE`] directories at any depth, and the globs that the
/// repository's manifest and the person's config file declare.
///
/// A path is disposable only when `git status` reports it as an ignored
/// path and one of the globs matches it, so a tracked file, and an ignored
/// file inside a directory that holds tracked files, is never disposable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisposableSet {
    /// The globs, each with where it came from.
    globs: Vec<(PathGlob, DisposableSource)>,
}

impl DisposableSet {
    /// Returns the set of the built-in [`DISPOSABLE`] directories, each as a
    /// glob such as `target/` that matches the directory at any depth.
    #[must_use]
    pub fn built_in() -> Self {
        Self {
            globs: DISPOSABLE
                .iter()
                .filter_map(|name| PathGlob::parse(&format!("{name}/")).ok())
                .map(|glob| (glob, DisposableSource::BuiltIn))
                .collect(),
        }
    }

    /// Returns the set with `globs`, which came from `source`, added.
    #[must_use]
    pub fn with(mut self, globs: &[PathGlob], source: DisposableSource) -> Self {
        self.globs
            .extend(globs.iter().cloned().map(|glob| (glob, source)));
        self
    }

    /// Reports whether `path`, an ignored path as `git status` prints it,
    /// is disposable.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        self.source(path).is_some()
    }

    /// Returns the most trusted source of a glob that matches `path`, or
    /// `None` when no glob matches it. A path that a built-in name or the
    /// config file matches never counts as coming from the manifest.
    #[must_use]
    pub fn source(&self, path: &str) -> Option<DisposableSource> {
        self.globs
            .iter()
            .filter(|(glob, _)| glob.matches(path))
            .map(|(_, source)| *source)
            .min()
    }
}
