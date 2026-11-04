//! Where an app's program comes from: a published release, a checkout that
//! phase builds, or a container image that already exists.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// The most bytes of a release version. A longer one is refused with
/// [`BadSourceText::TooLong`].
pub const MOST_VERSION_BYTES: usize = 128;

/// The most bytes of a GitHub repository's `owner/name`. A longer one is
/// refused with [`BadSourceText::TooLong`].
pub const MOST_REPOSITORY_BYTES: usize = 140;

/// The most bytes of a container image reference. A longer one is refused
/// with [`BadSourceText::TooLong`].
pub const MOST_IMAGE_BYTES: usize = 255;

/// Where an app's program comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// A published release, which phase downloads, verifies and keeps in a
    /// content-addressed cache. A host app uses it.
    Release {
        /// The release's version, which is its tag.
        version: Version,
        /// Where the release's asset is and how phase verifies it.
        spec: ReleaseSpec,
    },
    /// The workspace's checkout of the app's repository, which phase builds.
    /// A host app uses it.
    Checkout {
        /// The checkout's directory.
        dir: PathBuf,
        /// How phase builds the program in the checkout.
        build: BuildSpec,
    },
    /// A container image that already exists. A container app uses it, and
    /// each of the app's processes runs this image. phase builds no image.
    Image {
        /// The image, pinned by a tag other than `latest` or by a digest.
        image: ImageReference,
    },
}

/// Where a release's asset is and how phase verifies it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ReleaseSpec {
    /// The repository that publishes the release.
    pub repository: GithubRepository,
    /// The asset's file name, in which `{target}` stands for the Rust
    /// target triple of this machine, such as `example-api-{target}.tar.gz`.
    pub asset: String,
    /// The path of the program inside the asset.
    pub binary: PathBuf,
    /// How phase verifies the asset before it runs the program.
    pub verify: Verification,
}

impl ReleaseSpec {
    /// Returns the spec of a release of `repository` whose `asset` holds the
    /// program at `binary`, verified by `verify`.
    #[must_use]
    pub const fn new(
        repository: GithubRepository,
        asset: String,
        binary: PathBuf,
        verify: Verification,
    ) -> Self {
        Self {
            repository,
            asset,
            binary,
            verify,
        }
    }
}

/// How phase verifies a release's asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Verification {
    /// A build attestation that ties the asset to the repository's CI.
    Attestation,
    /// A SHA-256 digest that the release publishes.
    Sha256,
    /// A minisign signature that the release publishes.
    Minisign,
}

/// How phase builds an app's program in its checkout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct BuildSpec {
    /// The build command and its arguments, as separate values. No shell
    /// reads them. The command runs in the checkout.
    pub run: Vec<String>,
    /// The path of the built program, relative to the checkout.
    pub output: PathBuf,
}

impl BuildSpec {
    /// Returns the spec of a build that runs `run` and leaves the program at
    /// `output`.
    #[must_use]
    pub const fn new(run: Vec<String>, output: PathBuf) -> Self {
        Self { run, output }
    }
}

/// The reason that a text is not a valid version, repository or image.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BadSourceText {
    /// The text is empty.
    Empty,
    /// The text holds more bytes than its limit.
    TooLong,
    /// The text holds this character, which it never holds.
    BadCharacter(char),
    /// A repository is not exactly `owner/name` with both parts non-empty.
    NotOwnerAndName,
    /// An image has neither a digest nor a tag, or its tag is `latest`, so
    /// the image it names can change without the plan changing.
    Unpinned,
}

impl fmt::Display for BadSourceText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the text is empty"),
            Self::TooLong => formatter.write_str("the text is longer than its limit"),
            Self::BadCharacter(character) => {
                write!(formatter, "the text holds the character {character:?}")
            }
            Self::NotOwnerAndName => formatter.write_str("the repository is not `owner/name`"),
            Self::Unpinned => {
                formatter.write_str("the image has no digest and no tag, or its tag is `latest`")
            }
        }
    }
}

impl std::error::Error for BadSourceText {}

/// Checks that `text` is non-empty, at most `most` bytes, and holds only
/// ASCII letters, digits and the characters of `punctuation`.
fn check_text(text: &str, most: usize, punctuation: &str) -> Result<(), BadSourceText> {
    if text.is_empty() {
        return Err(BadSourceText::Empty);
    }
    if text.len() > most {
        return Err(BadSourceText::TooLong);
    }
    match text
        .chars()
        .find(|character| !(character.is_ascii_alphanumeric() || punctuation.contains(*character)))
    {
        Some(bad) => Err(BadSourceText::BadCharacter(bad)),
        None => Ok(()),
    }
}

/// Implements the text accessor, `FromStr`, the conversion to `String` and
/// `Display` of a type whose `TryFrom<String>` checks it.
macro_rules! text_type {
    ($name:ident) => {
        impl $name {
            /// Returns the value as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = BadSourceText;

            fn from_str(text: &str) -> Result<Self, BadSourceText> {
                Self::try_from(text.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

/// The version of a release, which is its git tag, such as `v1.2.0`.
///
/// A version holds 1 to [`MOST_VERSION_BYTES`] ASCII letters, digits, `.`,
/// `-`, `+` and `_`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl TryFrom<String> for Version {
    type Error = BadSourceText;

    fn try_from(text: String) -> Result<Self, BadSourceText> {
        check_text(&text, MOST_VERSION_BYTES, ".-+_")?;
        Ok(Self(text))
    }
}

text_type!(Version);

/// A GitHub repository, written `owner/name`, such as
/// `crumbkit/example-api`.
///
/// Each part holds ASCII letters, digits, `.`, `-` and `_`, and the whole
/// holds at most [`MOST_REPOSITORY_BYTES`] bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GithubRepository(String);

impl GithubRepository {
    /// Returns the owner, the part before the `/`.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.0.split_once('/').map_or("", |(owner, _)| owner)
    }

    /// Returns the repository's name, the part after the `/`.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, name)| name)
    }
}

impl TryFrom<String> for GithubRepository {
    type Error = BadSourceText;

    fn try_from(text: String) -> Result<Self, BadSourceText> {
        check_text(&text, MOST_REPOSITORY_BYTES, "./-_")?;
        match text.split_once('/') {
            Some((owner, name)) if !owner.is_empty() && !name.is_empty() && !name.contains('/') => {
                Ok(Self(text))
            }
            _ => Err(BadSourceText::NotOwnerAndName),
        }
    }
}

text_type!(GithubRepository);

/// A container image that already exists, such as `postgres:17`.
///
/// A reference holds 1 to [`MOST_IMAGE_BYTES`] bytes of ASCII letters,
/// digits, `.`, `-`, `_`, `/`, `:` and `@`. It is pinned by a digest
/// (`@sha256:…`) or by a tag other than `latest`, so the image a plan names
/// changes only when the plan does.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageReference(String);

impl TryFrom<String> for ImageReference {
    type Error = BadSourceText;

    fn try_from(text: String) -> Result<Self, BadSourceText> {
        check_text(&text, MOST_IMAGE_BYTES, ".-_/:@")?;
        if text.contains('@') {
            return Ok(Self(text));
        }
        // The tag follows the last `:` of the last path segment. A `:` in an
        // earlier segment separates a registry's host from its port.
        let last_segment = text.rsplit('/').next().unwrap_or_default();
        match last_segment.split_once(':') {
            Some((_, tag)) if !tag.is_empty() && tag != "latest" => Ok(Self(text)),
            _ => Err(BadSourceText::Unpinned),
        }
    }
}

text_type!(ImageReference);
