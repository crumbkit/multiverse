//! The name of a workspace, which is also the name of its directory and of
//! the branch that each of its checkouts is on.

use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// The most bytes that a workspace name holds.
pub const MOST_NAME_BYTES: usize = 64;

/// The name of a workspace, such as `reading-list`.
///
/// A name holds 1 to [`MOST_NAME_BYTES`] ASCII letters, digits, `-`, `_`
/// and `.`. It starts with a letter or a digit, holds no `..`, and does not
/// end with `.` or `.lock`. These rules keep the name usable both as a
/// directory name and as a git branch name, so the same text names the
/// workspace, its directory and its branches.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkspaceName(String);

impl WorkspaceName {
    /// Returns the name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The reason that a text is not a workspace name.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadName {
    /// The text is empty.
    Empty,
    /// The text holds more than [`MOST_NAME_BYTES`] bytes.
    TooLong,
    /// The text starts with a character other than a letter or a digit.
    BadStart,
    /// The text holds this character, which a name never holds.
    BadCharacter(char),
    /// The text holds this uppercase letter. Names are lowercase, so two
    /// names never refer to the same record on a file system that ignores
    /// case, such as the default one on macOS, and a name is never `HEAD`.
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

impl TryFrom<String> for WorkspaceName {
    type Error = BadName;

    fn try_from(text: String) -> Result<Self, BadName> {
        let Some(first) = text.chars().next() else {
            return Err(BadName::Empty);
        };
        if text.len() > MOST_NAME_BYTES {
            return Err(BadName::TooLong);
        }
        if !first.is_ascii_alphanumeric() {
            return Err(BadName::BadStart);
        }
        if let Some(upper) = text.chars().find(char::is_ascii_uppercase) {
            return Err(BadName::Uppercase(upper));
        }
        if let Some(bad) = text
            .chars()
            .find(|character| !(character.is_ascii_alphanumeric() || "-_.".contains(*character)))
        {
            return Err(BadName::BadCharacter(bad));
        }
        if text.contains("..") || text.ends_with('.') || text.ends_with(".lock") {
            return Err(BadName::BadDots);
        }
        Ok(Self(text))
    }
}

impl std::str::FromStr for WorkspaceName {
    type Err = BadName;

    fn from_str(text: &str) -> Result<Self, BadName> {
        Self::try_from(text.to_owned())
    }
}

impl From<WorkspaceName> for String {
    fn from(name: WorkspaceName) -> Self {
        name.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
