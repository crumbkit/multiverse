//! The permissions that a person granted the host apps of one repository,
//! and the comparison of an app's permissions with them.
//!
//! A grant file is `<state_dir>/grants/<label>-<hash>.json`, where
//! `<label>` is the scope's label reduced to letters, digits and hyphens,
//! and `<hash>` is 16 hex digits of the SHA-256 of the scope's paths. It
//! holds, for each app, either that the app may run outside the sandbox or
//! the sets of paths it may write, ports it may listen on, apps and hosts
//! it may connect to, and names of its secrets. It never holds a secret's
//! value.
//!
//! A grant only grows. [`Phase::grant`](crate::Phase::grant) takes an
//! exclusive `flock` on the file's `.lock` beside it, reads the file again,
//! adds what the growth names and writes the union to a temporary file that
//! it renames over the grant, so a crash leaves the old grant or the new
//! one. [`Grants::growth`] compares and writes nothing.
//!
//! phase keeps the grants under its own state directory, which the sandbox
//! never lets a sandboxed app read or write. A process outside the sandbox
//! runs with the person's own access and can edit the file, so a grant
//! makes a widening visible to a person; it does not stop such a process.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::PhaseError;
use crate::name::AppName;
use crate::plan::{Permissions, STATE};
use crate::record::store::{replace_file, secure_dir, sync_directory, take_lock};

#[cfg(test)]
mod tests;

/// The directory under phase's state directory that holds the grants.
const GRANTS_DIR: &str = "grants";

/// The schema prefix of every version of the grant file.
const SCHEMA_PREFIX: &str = "phase/grants/v";

/// The version of the grant file that this phase writes.
const GRANTS_VERSION: u32 = 1;

/// The most bytes of a grant file that phase reads. A larger file is
/// unreadable.
const MOST_GRANTS_BYTES: u64 = 1024 * 1024;

/// The most bytes of a scope's label in a grant file's name.
const MOST_LABEL_BYTES: usize = 48;

/// The repositories whose apps share one grant, such as the checkouts of a
/// workspace, named by their source directories and a label that a person
/// recognises in the grant file's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantScope {
    /// The label, reduced to letters, digits and hyphens.
    label: String,
    /// The source directories, sorted and without repeats.
    paths: Vec<PathBuf>,
}

impl GrantScope {
    /// Returns the scope of the repositories at `paths`, in any order, with
    /// `label`, such as the repository's folder name. Every character of
    /// the label other than an ASCII letter, a digit or a hyphen becomes a
    /// hyphen, and a label longer than 48 bytes is cut.
    #[must_use]
    pub fn new(label: &str, paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut label: String = label
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        label.truncate(MOST_LABEL_BYTES);
        if label.is_empty() {
            label.push_str("scope");
        }
        let paths: BTreeSet<PathBuf> = paths.into_iter().collect();
        Self {
            label,
            paths: paths.into_iter().collect(),
        }
    }

    /// Returns the label as it appears in the grant file's name.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the source directories, sorted.
    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Returns the grant file's name without its extension:
    /// `<label>-<16 hex digits of the SHA-256 of the paths>`.
    fn stem(&self) -> String {
        let mut hasher = Sha256::new();
        for path in &self.paths {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update([0]);
        }
        let digest = hasher.finalize();
        let mut stem = format!("{}-", self.label);
        for byte in digest.iter().take(8) {
            let _ = write!(stem, "{byte:02x}");
        }
        stem
    }
}

/// What one app may do: run outside the sandbox, or what its permissions
/// allow inside it. A request has the same form, so phase compares the two.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GrantFile", into = "GrantFile")]
#[non_exhaustive]
pub enum Grant {
    /// The app runs outside the sandbox, with everything the person's
    /// account can reach.
    Unsandboxed,
    /// The app runs inside the sandbox with these permissions. A `$STATE`
    /// write path is left out, because the sandbox always allows it.
    Sandboxed(Permissions),
}

impl Grant {
    /// Returns the grant that an app's `permissions` ask for: `None`, an
    /// app that declares no permissions, runs outside the sandbox.
    #[must_use]
    pub fn of(permissions: Option<&Permissions>) -> Self {
        match permissions {
            None => Self::Unsandboxed,
            Some(permissions) => {
                let mut permissions = permissions.clone();
                permissions.write.remove(STATE);
                Self::Sandboxed(permissions)
            }
        }
    }

    /// Returns what `requested` asks for beyond this grant, or `None` when
    /// this grant covers it. An unsandboxed grant covers every request.
    fn beyond(&self, requested: &Self) -> Option<Asked> {
        match (self, requested) {
            (Self::Unsandboxed, _) => None,
            (Self::Sandboxed(_), Self::Unsandboxed) => Some(Asked::Unsandboxed),
            (Self::Sandboxed(granted), Self::Sandboxed(requested)) => {
                let more = Permissions {
                    write: difference(&requested.write, &granted.write),
                    listen: difference(&requested.listen, &granted.listen),
                    connect: difference(&requested.connect, &granted.connect),
                    secrets: difference(&requested.secrets, &granted.secrets),
                };
                (more != Permissions::default()).then_some(Asked::More(more))
            }
        }
    }

    /// Returns this grant with `asked` added. A grant only grows.
    fn with(self, asked: &Asked) -> Self {
        let added = match asked {
            Asked::New(grant) => grant.clone(),
            Asked::Unsandboxed => Self::Unsandboxed,
            Asked::More(more) => Self::Sandboxed(more.clone()),
        };
        match (self, added) {
            (Self::Unsandboxed, _) | (_, Self::Unsandboxed) => Self::Unsandboxed,
            (Self::Sandboxed(mut granted), Self::Sandboxed(added)) => {
                granted.write.extend(added.write);
                granted.listen.extend(added.listen);
                granted.connect.extend(added.connect);
                granted.secrets.extend(added.secrets);
                Self::Sandboxed(granted)
            }
        }
    }
}

/// Returns the entries of `requested` that `granted` lacks.
fn difference<T: Ord + Clone>(requested: &BTreeSet<T>, granted: &BTreeSet<T>) -> BTreeSet<T> {
    requested.difference(granted).cloned().collect()
}

/// A grant as JSON: `{"unsandboxed": true}`, or the permission sets.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum GrantFile {
    /// The app may run outside the sandbox.
    Unsandboxed(UnsandboxedFile),
    /// The app's permission sets.
    Sandboxed(Permissions),
}

/// The JSON of an unsandboxed grant.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnsandboxedFile {
    /// Always `true`.
    unsandboxed: bool,
}

impl TryFrom<GrantFile> for Grant {
    type Error = String;

    fn try_from(file: GrantFile) -> Result<Self, Self::Error> {
        match file {
            GrantFile::Unsandboxed(UnsandboxedFile { unsandboxed: true }) => Ok(Self::Unsandboxed),
            GrantFile::Unsandboxed(UnsandboxedFile { unsandboxed: false }) => {
                Err("a grant's `unsandboxed` is true or absent".to_owned())
            }
            GrantFile::Sandboxed(permissions) => Ok(Self::Sandboxed(permissions)),
        }
    }
}

impl From<Grant> for GrantFile {
    fn from(grant: Grant) -> Self {
        match grant {
            Grant::Unsandboxed => Self::Unsandboxed(UnsandboxedFile { unsandboxed: true }),
            Grant::Sandboxed(permissions) => Self::Sandboxed(permissions),
        }
    }
}

/// What a person granted the apps of one scope, by app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Grants {
    /// Each app's grant.
    apps: BTreeMap<AppName, Grant>,
}

impl Grants {
    /// Returns the grant of `app`, or `None` when the scope has none.
    #[must_use]
    pub fn get(&self, app: &AppName) -> Option<&Grant> {
        self.apps.get(app)
    }

    /// Returns what `requested` asks for beyond these grants, by app. An app
    /// asks for more when its request holds an entry that its grant lacks,
    /// when it would run outside the sandbox and its grant is sandboxed, or
    /// when it has no grant. A request that stays the same or shrinks asks
    /// for nothing. It writes nothing.
    #[must_use]
    pub fn growth(&self, requested: &BTreeMap<AppName, Grant>) -> Growth {
        let apps = requested
            .iter()
            .filter_map(|(app, request)| {
                let asked = match self.apps.get(app) {
                    None => Some(Asked::New(request.clone())),
                    Some(granted) => granted.beyond(request),
                };
                asked.map(|asked| (app.clone(), asked))
            })
            .collect();
        Growth { apps }
    }
}

/// What one app asks for beyond its grant.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Asked {
    /// The app has no grant, and asks for all of this.
    New(Grant),
    /// The app's grant is sandboxed, and the app would run outside the
    /// sandbox.
    Unsandboxed,
    /// The app asks for these entries beyond its sandboxed grant.
    More(Permissions),
}

impl Asked {
    /// Returns the plain words of each thing the app asks for, such as
    /// `write to target in its checkout`.
    fn asks(&self) -> Vec<String> {
        match self {
            Self::New(Grant::Unsandboxed) | Self::Unsandboxed => {
                vec!["run outside the sandbox, with everything your account can reach".to_owned()]
            }
            Self::New(Grant::Sandboxed(permissions)) if *permissions == Permissions::default() => {
                vec![
                    "run in the sandbox, writing only its own state and temporary directories"
                        .to_owned(),
                ]
            }
            Self::New(Grant::Sandboxed(permissions)) | Self::More(permissions) => {
                let mut asks = Vec::new();
                for path in permissions.write.iter().filter(|path| *path != STATE) {
                    if Path::new(path).is_absolute() {
                        asks.push(format!("write to {path}"));
                    } else {
                        asks.push(format!("write to {path} in its checkout"));
                    }
                }
                asks.extend(
                    permissions
                        .listen
                        .iter()
                        .map(|port| format!("listen on the port {port}")),
                );
                asks.extend(
                    permissions
                        .connect
                        .iter()
                        .map(|target| format!("connect to {target}")),
                );
                asks.extend(
                    permissions
                        .secrets
                        .iter()
                        .map(|name| format!("receive the secret {name}")),
                );
                asks
            }
        }
    }

    /// Returns how the app's heading describes it: `a new unit`, or `more
    /// than it was granted`.
    const fn heading(&self) -> &'static str {
        match self {
            Self::New(_) => "a new unit",
            Self::Unsandboxed | Self::More(_) => "more than it was granted",
        }
    }
}

/// What the apps of one request ask for beyond their grants, by app.
/// [`Grants::growth`] returns it, and [`Phase::grant`](crate::Phase::grant)
/// adds it to the grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Growth {
    /// What each app asks for.
    apps: BTreeMap<AppName, Asked>,
}

impl Growth {
    /// Reports whether no app asks for more.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.apps.is_empty()
    }

    /// Returns what each app asks for, in name order.
    pub fn apps(&self) -> impl Iterator<Item = (&AppName, &Asked)> {
        self.apps.iter()
    }

    /// Returns the plain words that a question and a refusal print: for
    /// each app, a line that names it and says whether it is new, then a
    /// line for each thing it asks for. The lines show no hash.
    ///
    /// ```text
    ///   api, a new unit:
    ///     write to target in its checkout
    ///     listen on the port api
    ///   web, more than it was granted:
    ///     write to /tmp/cache
    /// ```
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (app, asked) in &self.apps {
            lines.push(format!("  {app}, {}:", asked.heading()));
            lines.extend(asked.asks().into_iter().map(|ask| format!("    {ask}")));
        }
        lines
    }

    /// Returns one sentence for each app, such as `api, a new unit, asks
    /// to write to target in its checkout and listen on the port api`, for
    /// a refusal that prints on one line.
    #[must_use]
    pub fn sentences(&self) -> Vec<String> {
        self.apps
            .iter()
            .map(|(app, asked)| {
                format!(
                    "{app}, {}, asks to {}",
                    asked.heading(),
                    asked.asks().join(" and ")
                )
            })
            .collect()
    }
}

/// The grant file as JSON.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantsRecord {
    /// The schema, `phase/grants/v1`.
    schema: String,
    /// The scope's source directories.
    paths: Vec<PathBuf>,
    /// Each app's grant.
    apps: BTreeMap<AppName, Grant>,
}

/// The part of a grant file that phase reads first, so a newer file is
/// refused before its other fields are read.
#[derive(Deserialize)]
struct Versioned {
    /// The file's schema.
    schema: String,
}

/// Returns the path of the grant file of `scope` under `state_dir`.
fn path(state_dir: &Path, scope: &GrantScope) -> PathBuf {
    state_dir
        .join(GRANTS_DIR)
        .join(format!("{}.json", scope.stem()))
}

/// Returns the grants of `scope` under `state_dir`, or `None` when it has
/// no file.
///
/// # Errors
///
/// Returns [`PhaseError::GrantsUnreadable`] when the file cannot be read or
/// is no grant file of `scope`, and [`PhaseError::NewerGrants`] when a
/// newer phase wrote it.
pub(crate) fn read(state_dir: &Path, scope: &GrantScope) -> Result<Option<Grants>, PhaseError> {
    let path = path(state_dir, scope);
    let unreadable = |reason: String| PhaseError::GrantsUnreadable {
        path: path.clone(),
        reason,
    };
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(MOST_GRANTS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MOST_GRANTS_BYTES {
        return Err(unreadable(format!(
            "it holds more than {MOST_GRANTS_BYTES} bytes"
        )));
    }
    let versioned: Versioned =
        serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
    let version = versioned
        .schema
        .strip_prefix(SCHEMA_PREFIX)
        .and_then(|version| version.parse::<u32>().ok())
        .ok_or_else(|| {
            unreadable(format!(
                "its schema {:?} is no grant file",
                versioned.schema
            ))
        })?;
    if version > GRANTS_VERSION {
        return Err(PhaseError::NewerGrants {
            path,
            schema: versioned.schema,
        });
    }
    let record: GrantsRecord =
        serde_json::from_slice(&bytes).map_err(|error| unreadable(error.to_string()))?;
    if record.paths != scope.paths {
        return Err(unreadable(
            "it names other repositories than the ones its name was made from".to_owned(),
        ));
    }
    Ok(Some(Grants { apps: record.apps }))
}

/// Adds `growth` to the grants of `scope` under `state_dir`, while it
/// holds the file's lock, waiting at most `wait`, and returns the grants
/// as written. It reads the file again under the lock, so two grants at
/// once keep both additions.
///
/// # Errors
///
/// Returns the errors of [`read`], [`PhaseError::GrantsBusy`] when another
/// grant holds the lock for all of `wait`, and
/// [`PhaseError::StateUnwritable`] when the file cannot be written.
pub(crate) fn grant(
    state_dir: &Path,
    scope: &GrantScope,
    growth: &Growth,
    wait: Duration,
) -> Result<Grants, PhaseError> {
    let dir = state_dir.join(GRANTS_DIR);
    secure_dir(&dir)?;
    let target = path(state_dir, scope);
    let lock_path = dir.join(format!("{}.lock", scope.stem()));
    let Some(_lock) = take_lock(&lock_path, wait, &|| false)? else {
        return Err(PhaseError::GrantsBusy {
            path: lock_path,
            waited: wait,
        });
    };
    let mut grants = read(state_dir, scope)?.unwrap_or_default();
    for (app, asked) in &growth.apps {
        let current = grants
            .apps
            .remove(app)
            .unwrap_or_else(|| Grant::Sandboxed(Permissions::default()));
        grants.apps.insert(app.clone(), current.with(asked));
    }
    let record = GrantsRecord {
        schema: format!("{SCHEMA_PREFIX}{GRANTS_VERSION}"),
        paths: scope.paths.clone(),
        apps: grants.apps.clone(),
    };
    let mut bytes =
        serde_json::to_vec_pretty(&record).map_err(|error| PhaseError::StateUnwritable {
            path: target.clone(),
            source: io::Error::other(error),
        })?;
    bytes.push(b'\n');
    let temp = dir.join(format!(".{}.json.{}.tmp", scope.stem(), std::process::id()));
    replace_file(&temp, &target, &bytes)?;
    sync_directory(&dir)?;
    Ok(grants)
}
