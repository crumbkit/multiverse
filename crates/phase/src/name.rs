//! The names that a plan uses: a workspace's name and stable id, an app, a
//! process, a port and a volume.
//! Each name has one type with one checking constructor, and every way to
//! build it, including reading it from JSON, goes through that check.

use std::fmt;
use std::str::FromStr;

#[cfg(test)]
mod tests;

/// The most bytes that a workspace, app, process or port name holds. A
/// longer name is refused with [`BadName::TooLong`].
pub const MOST_NAME_BYTES: usize = 64;

/// The reason that a text is not a valid name.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadName {
    /// The text is empty.
    Empty,
    /// The text holds more than [`MOST_NAME_BYTES`] bytes.
    TooLong,
    /// The text starts with a character other than an ASCII letter or digit.
    BadStart,
    /// The text holds this character, which the name never holds.
    #[non_exhaustive]
    BadCharacter(char),
    /// The text holds this uppercase letter, and the name is lowercase.
    #[non_exhaustive]
    Uppercase(char),
    /// The text holds `..`, or ends with `.` or `.lock`, which git refuses
    /// in a branch name.
    BadDots,
}

impl fmt::Display for BadName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the name is empty"),
            Self::TooLong => write!(formatter, "the name is longer than {MOST_NAME_BYTES} bytes"),
            Self::BadStart => formatter
                .write_str("the name starts with a character other than a letter or a digit"),
            Self::BadCharacter(character) => {
                write!(formatter, "the name holds the character {character:?}")
            }
            Self::Uppercase(character) => {
                write!(
                    formatter,
                    "the name holds the uppercase letter {character:?}"
                )
            }
            Self::BadDots => {
                formatter.write_str("the name holds `..`, or ends with `.` or `.lock`")
            }
        }
    }
}

impl std::error::Error for BadName {}

/// The rule that a kind of name follows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// A workspace name, which is also a directory name and a git branch
    /// name.
    Workspace,
    /// An app name, which is also a Docker Compose project name.
    App,
    /// A process or port name, which is also a Compose service name.
    Unit,
    /// A workspace's stable id, which labels Docker objects.
    Id,
}

/// Reports whether `text` is a valid name of a manifest's process,
/// container, volume or port: 1 to [`MOST_NAME_BYTES`] ASCII letters,
/// digits, `-` and `_`, starting with a letter or a digit. Such a name
/// never holds a `.`. It is the rule of a [`ProcessName`], a [`PortName`]
/// and a [`VolumeName`].
#[must_use]
pub fn is_unit_name(text: &str) -> bool {
    check(text, Rule::Unit).is_ok()
}

/// Checks `text` against `rule`.
fn check(text: &str, rule: Rule) -> Result<(), BadName> {
    let Some(first) = text.chars().next() else {
        return Err(BadName::Empty);
    };
    if text.len() > MOST_NAME_BYTES {
        return Err(BadName::TooLong);
    }
    if !first.is_ascii_alphanumeric() {
        return Err(BadName::BadStart);
    }
    if rule != Rule::Unit
        && let Some(upper) = text.chars().find(char::is_ascii_uppercase)
    {
        return Err(BadName::Uppercase(upper));
    }
    let punctuation = match rule {
        Rule::Workspace => "-_.",
        Rule::App | Rule::Unit => "-_",
        Rule::Id => "-",
    };
    if let Some(bad) = text
        .chars()
        .find(|character| !(character.is_ascii_alphanumeric() || punctuation.contains(*character)))
    {
        return Err(BadName::BadCharacter(bad));
    }
    if rule == Rule::Workspace
        && (text.contains("..") || text.ends_with('.') || text.ends_with(".lock"))
    {
        return Err(BadName::BadDots);
    }
    Ok(())
}

/// Implements the checking constructor, the text accessor, JSON support and
/// `Display` of a name type that wraps a `String`.
macro_rules! name_type {
    ($name:ident, $rule:expr) => {
        impl $name {
            /// Returns the name as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = BadName;

            fn try_from(text: String) -> Result<Self, BadName> {
                check(&text, $rule)?;
                Ok(Self(text))
            }
        }

        impl FromStr for $name {
            type Err = BadName;

            fn from_str(text: &str) -> Result<Self, BadName> {
                Self::try_from(text.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> Self {
                name.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

/// The name of a workspace, such as `reading-list`.
///
/// phase keeps its own type, because it does not depend on the multiverse
/// crate. A workspace name follows multiverse's rule, so the same text names the
/// workspace, its directory and its branches. It holds 1 to
/// [`MOST_NAME_BYTES`] lowercase ASCII letters, digits, `-`, `_` and `.`,
/// starts with a letter or a digit, holds no `..`, and does not end with `.`
/// or `.lock`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspaceName(String);

name_type!(WorkspaceName, Rule::Workspace);

/// The name of an app, such as `api`, which is unique within its workspace.
///
/// phase names each app's Docker Compose project after it, so an app name
/// follows Compose's rule for project names. It holds 1 to
/// [`MOST_NAME_BYTES`] lowercase ASCII letters, digits, `-` and `_`, and
/// starts with a letter or a digit.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct AppName(String);

name_type!(AppName, Rule::App);

/// The name of one process of an app, such as `server`, which is unique
/// within its app.
///
/// A process of a container app is a Compose service of that name. A
/// process name holds 1 to [`MOST_NAME_BYTES`] ASCII letters, digits, `-`
/// and `_`, and starts with a letter or a digit.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct ProcessName(String);

name_type!(ProcessName, Rule::Unit);

/// The name of a port in multiverse's port ledger, such as `http`.
///
/// A port name follows the same rule as a [`ProcessName`].
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct PortName(String);

name_type!(PortName, Rule::Unit);

/// The name of a Docker volume of a container app, such as `data` or
/// `cargo-cache`. phase creates the volume for the workspace and labels it
/// with the workspace's stable id.
///
/// A volume name follows the same rule as a [`ProcessName`].
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct VolumeName(String);

name_type!(VolumeName, Rule::Unit);

/// The stable id of a workspace, such as a UUID, which multiverse's
/// workspace record holds.
///
/// The id stays the same when the workspace is moved or renamed. phase keys
/// the run record by it and labels every container, network and volume
/// with it. An id holds 1 to [`MOST_NAME_BYTES`] lowercase ASCII letters,
/// digits and `-`, and starts with a letter or a digit.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspaceId(String);

name_type!(WorkspaceId, Rule::Id);

impl WorkspaceId {
    /// Returns a new id: 32 lowercase hex digits from the operating
    /// system's cryptographically secure random number generator. The host
    /// program mints a workspace's id once, keeps it in the workspace's
    /// record, and never changes it.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the random number generator, whose
    /// device is `/dev/urandom`.
    pub fn mint() -> std::io::Result<Self> {
        let text = crate::random::hex_id()?;
        Self::try_from(text).map_err(std::io::Error::other)
    }
}
