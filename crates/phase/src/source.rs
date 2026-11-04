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
    #[non_exhaustive]
    Release {
        /// The release's version, which is its tag.
        version: Version,
        /// Where the release's asset is and how phase verifies it.
        spec: ReleaseSpec,
    },
    /// The workspace's checkout of the app's repository. A host app uses
    /// it, and its processes start in the checkout.
    #[non_exhaustive]
    Checkout {
        /// The checkout's absolute directory.
        dir: PathBuf,
        /// How phase builds the app's program in the checkout, or `None`
        /// when the processes run commands from the checkout, such as
        /// `npm run dev`, and no process names `{program}`.
        #[serde(default)]
        build: Option<BuildSpec>,
    },
    /// A container image that already exists. A container app uses it, and
    /// each of the app's processes runs this image. phase builds no image.
    #[non_exhaustive]
    Image {
        /// The image, pinned by a tag other than `latest` or by a digest.
        image: ImageReference,
    },
}

impl Source {
    /// Returns the source of a release of `version`, found and verified as
    /// `spec` says.
    #[must_use]
    pub const fn release(version: Version, spec: ReleaseSpec) -> Self {
        Self::Release { version, spec }
    }

    /// Returns the source of the checkout in `dir`, built as `build` says,
    /// or not built when `build` is `None`.
    #[must_use]
    pub const fn checkout(dir: PathBuf, build: Option<BuildSpec>) -> Self {
        Self::Checkout { dir, build }
    }

    /// Returns the source of the container image `image`.
    #[must_use]
    pub const fn image(image: ImageReference) -> Self {
        Self::Image { image }
    }
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
    /// It holds no `/`.
    pub asset: String,
    /// The path of the program inside the asset. It is relative and holds no
    /// `..`.
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
    /// The path of the built program, relative to the checkout. It holds no
    /// `..`.
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
    /// The text starts with a character other than an ASCII letter or digit.
    BadStart,
    /// The text holds this character, which it never holds.
    #[non_exhaustive]
    BadCharacter(char),
    /// A repository is not exactly `owner/name` with both parts non-empty
    /// and neither part `.` or `..`.
    NotOwnerAndName,
    /// An image has neither a digest of the form `@sha256:` and 64
    /// lowercase hexadecimal digits nor a tag, or its tag is `latest`, so
    /// the image it names can change without the plan changing.
    Unpinned,
}

impl fmt::Display for BadSourceText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the text is empty"),
            Self::TooLong => formatter.write_str("the text is longer than its limit"),
            Self::BadStart => formatter
                .write_str("the text starts with a character other than a letter or a digit"),
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
/// `-`, `+` and `_`, and starts with a letter or a digit.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version(String);

impl TryFrom<String> for Version {
    type Error = BadSourceText;

    fn try_from(text: String) -> Result<Self, BadSourceText> {
        check_text(&text, MOST_VERSION_BYTES, ".-+_")?;
        if !text.starts_with(|first: char| first.is_ascii_alphanumeric()) {
            return Err(BadSourceText::BadStart);
        }
        Ok(Self(text))
    }
}

text_type!(Version);

/// A GitHub repository, written `owner/name`, such as
/// `crumbkit/example-api`.
///
/// Each part holds ASCII letters, digits, `.`, `-` and `_`, and is neither
/// `.` nor `..`. The whole holds at most [`MOST_REPOSITORY_BYTES`] bytes.
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
            Some((owner, name)) if is_part(owner) && is_part(name) => Ok(Self(text)),
            _ => Err(BadSourceText::NotOwnerAndName),
        }
    }
}

text_type!(GithubRepository);

/// Reports whether `part` can be the owner or the name of a repository.
fn is_part(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains('/')
}

/// Reports whether `digest` is `sha256:` followed by 64 lowercase
/// hexadecimal digits.
fn is_sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .chars()
                .all(|digit| matches!(digit, '0'..='9' | 'a'..='f'))
    })
}

/// A container image that already exists, such as `postgres:17`.
///
/// A reference holds 1 to [`MOST_IMAGE_BYTES`] bytes of ASCII letters,
/// digits, `.`, `-`, `_`, `/`, `:` and `@`. It is pinned by a digest, which
/// is `@sha256:` followed by 64 lowercase hexadecimal digits, or by a tag
/// other than `latest`, so the image a plan names changes only when the
/// plan does.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ImageReference(String);

impl TryFrom<String> for ImageReference {
    type Error = BadSourceText;

    fn try_from(text: String) -> Result<Self, BadSourceText> {
        check_text(&text, MOST_IMAGE_BYTES, ".-_/:@")?;
        if let Some((name, digest)) = text.split_once('@') {
            return if !name.is_empty() && is_sha256_digest(digest) {
                Ok(Self(text))
            } else {
                Err(BadSourceText::Unpinned)
            };
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
