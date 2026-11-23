//! The data that a host keeps in a workspace's record: the name of the host
//! profile that owns the workspace and a small map of strings that only
//! that host reads.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::host::HostName;
use crate::name::WorkspaceName;

/// The most entries of [`HostData::values`]. A record whose host data holds
/// more is refused before it is written, with
/// [`Error::HostDataTooLarge`].
pub const MOST_HOST_VALUES: usize = 64;

/// The most bytes of a [`HostKey`].
pub const MOST_HOST_KEY_BYTES: usize = 64;

/// The most bytes of a record's [`HostData`] written as JSON. Larger host
/// data is refused before it is written, with [`Error::HostDataTooLarge`].
/// The store's [`MOST_RECORD_BYTES`](crate::MOST_RECORD_BYTES) stays the
/// limit of the whole record.
pub const MOST_HOST_DATA_BYTES: usize = 16_384;

/// The key of one value of [`HostData`]: 1 to [`MOST_HOST_KEY_BYTES`] bytes
/// of lowercase ASCII letters, digits, `_`, `.` and `-`. Every way to build
/// one, including reading it from JSON, checks the rule.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostKey(String);

impl HostKey {
    /// Returns the key `text`.
    ///
    /// # Errors
    ///
    /// Returns why `text` breaks the rule of the type.
    pub fn new(text: &str) -> Result<Self, String> {
        let valid = (1..=MOST_HOST_KEY_BYTES).contains(&text.len())
            && text.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(&byte)
            });
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(format!(
                "the host key {text:?} is not 1 to {MOST_HOST_KEY_BYTES} bytes of lowercase letters, digits, `_`, `.` and `-`"
            ))
        }
    }

    /// Returns the key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostKey {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        Self::new(&text)
    }
}

impl From<HostKey> for String {
    fn from(key: HostKey) -> Self {
        key.0
    }
}

impl fmt::Display for HostKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The data that a host keeps in a workspace's record. multiverse stores
/// it, carries it through an archive and a restore, and reads only
/// `profile`. A verb of [`Workspaces`](crate::Workspaces) that runs under
/// another profile than `profile` refuses to change the workspace, with
/// [`Error::OtherHost`].
///
/// The record is a plain file with the permission bits 0600 in the
/// caller's state directory, so a host must not keep a secret here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostData {
    /// The name of the host profile that owns the workspace.
    pub profile: HostName,
    /// The host's values, which multiverse never reads. A host that needs
    /// structure encodes it in its own values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<HostKey, String>,
}

impl HostData {
    /// Returns the data of the host `profile` with `values`.
    #[must_use]
    pub const fn new(profile: HostName, values: BTreeMap<HostKey, String>) -> Self {
        Self { profile, values }
    }

    /// Checks the data against [`MOST_HOST_VALUES`] and
    /// [`MOST_HOST_DATA_BYTES`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::HostDataTooLarge`] for the workspace `name` when
    /// the data passes either limit.
    pub(crate) fn check(&self, name: &WorkspaceName) -> Result<(), Error> {
        let bytes = serde_json::to_vec(self).map_or(usize::MAX, |json| json.len());
        if self.values.len() > MOST_HOST_VALUES || bytes > MOST_HOST_DATA_BYTES {
            return Err(Error::HostDataTooLarge {
                name: name.clone(),
                values: self.values.len(),
                bytes,
            });
        }
        Ok(())
    }
}
