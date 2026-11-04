//! The keeper's specification, which the engine writes to the keeper's
//! standard input, and the handshake line, which the keeper writes to its
//! standard output. Neither holds an environment value.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::engine::keeper::MOST_SPEC_BYTES;
use crate::plan::restart::{Restart, Stop};
use crate::record::run::HostProcess;

/// The version of the specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SpecSchema {
    /// `phase/keeper/v1`.
    #[serde(rename = "phase/keeper/v1")]
    V1,
}

/// What a keeper runs and how it supervises it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeeperSpec {
    /// The specification's version.
    schema: SpecSchema,
    /// The app's command, with `{program}` already replaced. It is not
    /// empty.
    pub(crate) run: Vec<String>,
    /// The directory the app starts in.
    pub(crate) cwd: PathBuf,
    /// The app's log file, which the keeper creates when it does not exist.
    pub(crate) log: PathBuf,
    /// When the keeper restarts the app.
    pub(crate) restart: Restart,
    /// How the keeper stops the app.
    pub(crate) stop: Stop,
    /// How long the keeper waits after `KILL`.
    #[serde(rename = "kill_wait_ms", with = "crate::millis")]
    pub(crate) kill_wait: Duration,
    /// When a value of the environment expires, in milliseconds since the
    /// Unix epoch, or `None` when none expires.
    pub(crate) expires_at_ms: Option<u64>,
    /// How many times earlier keepers of this process restarted it, which
    /// counts toward the policy's most restarts.
    pub(crate) restarts: u32,
    /// The variables that the keeper and its app inherit from the caller,
    /// which are only ever `PATH` and `HOME` and are never secret. The
    /// keeper sets them on the app on every start.
    pub(crate) inherited: BTreeMap<String, String>,
    /// The names of the variables of the environment that the engine
    /// resolved and gave the keeper, which the keeper passes on to the
    /// app's first start. The spawn side fills them in.
    #[serde(default)]
    pub(crate) resolved: BTreeSet<String>,
}

/// The only variables that a keeper and its app inherit from the caller.
pub(crate) const INHERITED: [&str; 2] = ["PATH", "HOME"];

/// The specification names an inherited variable other than `PATH` and
/// `HOME`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NotInherited(pub(crate) String);

impl std::fmt::Display for NotInherited {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the inherited variable {} is neither PATH nor HOME",
            self.0
        )
    }
}

/// Returns the first name of `inherited` other than `PATH` and `HOME`.
fn check_inherited(inherited: &BTreeMap<String, String>) -> Result<(), NotInherited> {
    match inherited
        .keys()
        .find(|name| !INHERITED.contains(&name.as_str()))
    {
        Some(name) => Err(NotInherited(name.clone())),
        None => Ok(()),
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the host runner builds a keeper's specification when `up` lands"
    )
)]
impl KeeperSpec {
    /// Returns the specification of an app that runs `run` in `cwd`, logs
    /// to `log`, restarts under `restart`, stops with `stop`, and has
    /// `kill_wait` after `KILL`, with no expiry and no earlier restarts.
    pub(crate) const fn new(
        run: Vec<String>,
        cwd: PathBuf,
        log: PathBuf,
        restart: Restart,
        stop: Stop,
        kill_wait: Duration,
    ) -> Self {
        Self {
            schema: SpecSchema::V1,
            run,
            cwd,
            log,
            restart,
            stop,
            kill_wait,
            expires_at_ms: None,
            restarts: 0,
            inherited: BTreeMap::new(),
            resolved: BTreeSet::new(),
        }
    }

    /// Returns this specification with the environment expiring at
    /// `expires_at`, as [`crate::AppliedEnvironment::expires_at`] reports.
    pub(crate) fn expiring(mut self, expires_at: Option<SystemTime>) -> Self {
        self.expires_at_ms = expires_at.map(|time| {
            time.duration_since(UNIX_EPOCH).map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            })
        });
        self
    }

    /// Returns this specification with `inherited` as the variables that
    /// the keeper and its app inherit from the caller.
    ///
    /// # Errors
    ///
    /// Returns [`NotInherited`] when `inherited` names a variable other
    /// than `PATH` and `HOME`.
    pub(crate) fn inheriting(
        mut self,
        inherited: BTreeMap<String, String>,
    ) -> Result<Self, NotInherited> {
        check_inherited(&inherited)?;
        self.inherited = inherited;
        Ok(self)
    }

    /// Returns this specification counting `restarts` earlier restarts.
    pub(crate) const fn after_restarts(mut self, restarts: u32) -> Self {
        self.restarts = restarts;
        self
    }

    /// Returns when a value of the environment expires, or `None`.
    pub(crate) fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at_ms
            .map(|millis| UNIX_EPOCH + Duration::from_millis(millis))
    }
}

/// Reads a specification of at most [`MOST_SPEC_BYTES`] bytes from
/// `input`.
///
/// # Errors
///
/// Returns why the specification cannot be read, which holds no value of
/// the environment.
pub(crate) fn read_spec(input: impl Read) -> Result<KeeperSpec, String> {
    let mut bytes = Vec::new();
    input
        .take(MOST_SPEC_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("the specification cannot be read: {error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MOST_SPEC_BYTES {
        return Err(format!(
            "the specification holds more than {MOST_SPEC_BYTES} bytes"
        ));
    }
    let spec: KeeperSpec = serde_json::from_slice(&bytes)
        .map_err(|error| format!("the specification is not valid: {error}"))?;
    if spec.run.is_empty() {
        return Err("the specification's command is empty".to_owned());
    }
    check_inherited(&spec.inherited).map_err(|error| error.to_string())?;
    Ok(spec)
}

/// The text that starts the handshake line. The engine skips any line
/// before it, because the test harness that runs keepers in phase's own
/// tests prints a header first.
pub(crate) const HANDSHAKE_PREFIX: &str = "phase-keeper/v1 ";

/// What a keeper tells the engine once its app has started or failed to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum Handshake {
    /// The app started.
    Started(StartedApp),
    /// The keeper could not start the app, for this reason.
    Unstartable(String),
    /// The keeper's specification could not be read, for this reason.
    Refused(String),
}

/// A keeper whose app started: the keeper's own process group, and the
/// app's process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartedApp {
    /// The keeper's process group, which the engine records.
    pub(crate) keeper_group: u32,
    /// The app's process.
    pub(crate) app: HostProcess,
}

/// Writes `handshake` to `output` as one line. A failed write is ignored,
/// because the engine that reads it reports a missing handshake.
pub(crate) fn write_handshake(mut output: impl Write, handshake: &Handshake) {
    let Ok(json) = serde_json::to_string(handshake) else {
        return;
    };
    let line = format!("{HANDSHAKE_PREFIX}{json}\n");
    let _ = output
        .write_all(line.as_bytes())
        .and_then(|()| output.flush());
}
