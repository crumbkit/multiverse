//! The docker hygiene of the tests that start real containers, shared by
//! phase's runner and exec tests and by mxv's `containers.rs` and
//! `services.rs`, which include this file with a `#[path]` attribute. It
//! uses only the standard library and `serde_json`, so it compiles in
//! either crate.
//!
//! Each harness removes its own objects when it drops, but a test process
//! that is killed, such as by nextest after a timeout or by a person's
//! Ctrl-C, never runs its drop. So each harness also holds an
//! [`OwnerLock`] on its temp root for as long as it lives, and sweeps once
//! when it starts with [`sweep_left_behind`]. The operating system
//! releases the lock when the process ends, even on `SIGKILL`.
//!
//! A temp root is the directory that `tempfile` made for one harness, a
//! directory whose name starts with `.tmp` directly below the temp
//! directory, below a base that the harness names, or below a path's
//! `target/tmp`. The sweep removes an object whose run-record label lies
//! in a temp root only when that test run has ended: the root no longer
//! exists, or its owner file exists and no process holds its lock, or it
//! has no owner file and was last modified more than
//! [`MOST_UNOWNED_AGE`] ago. A root without an owner file was made by a
//! harness from before the owner lock, which may still run in another
//! checkout. The sweep never touches an object whose run record lies
//! outside a temp root.

#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the real clock and the files and directories that they and the code under test write"
)]
#![expect(
    clippy::disallowed_macros,
    reason = "the sweep prints on standard output what it removed, which the test runner shows"
)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// The name of the owner file in a temp root.
pub(crate) const OWNER_FILE: &str = ".mxv-test-owner.lock";

/// How long the sweep keeps the objects of a temp root without an owner
/// file, counted from the root's last modification. After this, the sweep
/// removes them.
pub(crate) const MOST_UNOWNED_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The exclusive lock on a temp root's owner file, which says that the
/// root's test is still running.
#[derive(Debug)]
pub(crate) struct OwnerLock {
    /// The locked owner file.
    file: File,
}

impl OwnerLock {
    /// Creates the owner file in `root` and locks it until the value drops
    /// or the process ends.
    pub(crate) fn take(root: &Path) -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "the test fixture creates the owner file of its own temp root"
        )]
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(OWNER_FILE))
            .unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "the test fixture locks its owner file for the test's lifetime"
        )]
        file.lock().unwrap();
        Self { file }
    }

    /// Unlocks the owner file and keeps it open, as a killed process
    /// leaves it.
    pub(crate) fn release(&self) {
        self.file.unlock().unwrap();
    }
}

/// Runs `docker <args>` and returns its standard output, also when docker
/// failed for some of the objects it was given.
fn docker(args: &[&str]) -> Option<(bool, String)> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test fixture lists and removes objects in a real docker daemon"
    )]
    let output = Command::new("docker").args(args).output().ok()?;
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// A kind of docker object that phase labels.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Container,
    Volume,
    Network,
}

impl Kind {
    /// All kinds, containers first, so a swept container no longer holds a
    /// volume or a network when the sweep reaches them.
    const ALL: [Self; 3] = [Self::Container, Self::Volume, Self::Network];

    /// The `docker` command of the kind.
    const fn command(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Volume => "volume",
            Self::Network => "network",
        }
    }

    /// The arguments that list every object of the kind by id or name.
    fn list(self) -> Vec<&'static str> {
        match self {
            Self::Container => vec!["container", "ls", "--all", "--quiet"],
            Self::Volume | Self::Network => vec![self.command(), "ls", "--quiet"],
        }
    }

    /// The `docker inspect` template that prints an object's name and its
    /// labels as JSON.
    const fn template(self) -> &'static str {
        match self {
            Self::Container => "{{json .Name}}\t{{json .Config.Labels}}",
            Self::Volume | Self::Network => "{{json .Name}}\t{{json .Labels}}",
        }
    }

    /// Removes the object `name`, and returns whether docker removed it.
    fn remove(self, name: &str) -> bool {
        let args: Vec<&str> = match self {
            Self::Container => vec!["rm", "--force", "--volumes", "--", name],
            Self::Volume | Self::Network => vec![self.command(), "rm", "--", name],
        };
        docker(&args).is_some_and(|(removed, _)| removed)
    }
}

/// Returns the name and labels of each object of `kind`.
fn labelled(kind: Kind) -> Vec<(String, BTreeMap<String, String>)> {
    let Some((_, listed)) = docker(&kind.list()) else {
        return Vec::new();
    };
    let ids: Vec<&str> = listed.split_whitespace().collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let mut args = vec![kind.command(), "inspect", "--format", kind.template(), "--"];
    args.extend(ids);
    let Some((_, printed)) = docker(&args) else {
        return Vec::new();
    };
    printed
        .lines()
        .filter_map(|line| {
            let (name, labels) = line.split_once('\t')?;
            let name: String = serde_json::from_str(name).ok()?;
            let labels: Option<BTreeMap<String, String>> = serde_json::from_str(labels).ok()?;
            Some((name.trim_start_matches('/').to_owned(), labels?))
        })
        .collect()
}

/// Returns the path of the run-record label among `labels`, under any
/// prefix that phase accepts.
pub(crate) fn run_record(labels: &BTreeMap<String, String>) -> Option<&Path> {
    labels.iter().find_map(|(key, value)| {
        let prefix = key.strip_suffix(".run-record")?;
        let valid = prefix
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
            && prefix.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            });
        let path = Path::new(value);
        (valid && path.is_absolute()).then_some(path)
    })
}

/// Returns the directories below which a temp root lies: the temp
/// directory as the process sees it and with its links resolved, and each
/// of `extra` the same two ways.
pub(crate) fn temp_bases(extra: &[&Path]) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    for base in
        std::iter::once(std::env::temp_dir()).chain(extra.iter().map(|path| path.to_path_buf()))
    {
        if let Ok(resolved) = base.canonicalize() {
            bases.push(resolved);
        }
        bases.push(base);
    }
    bases
}

/// Returns the temp root that holds `run_record`, or `None` when the run
/// record lies in no temp root.
pub(crate) fn temp_root_of(run_record: &Path, bases: &[PathBuf]) -> Option<PathBuf> {
    let root = bases
        .iter()
        .find_map(|base| {
            let mut below = run_record.strip_prefix(base).ok()?.components();
            let first = below.next()?;
            below.next()?;
            Some(base.join(first))
        })
        .or_else(|| below_target_tmp(run_record))?;
    root.file_name()?
        .to_str()?
        .starts_with(".tmp")
        .then_some(root)
}

/// Returns the directory directly below the first `target/tmp` of `path`,
/// when `path` lies below it.
fn below_target_tmp(path: &Path) -> Option<PathBuf> {
    let parts: Vec<Component<'_>> = path.components().collect();
    let at = parts.windows(2).position(|pair| {
        matches!(pair, [Component::Normal(target), Component::Normal(tmp)]
            if *target == "target" && *tmp == "tmp")
    })?;
    let end = at + 3;
    (parts.len() > end).then(|| parts.iter().take(end).collect())
}

/// Reports whether the test run of the temp root `root` has ended, as of
/// `now`.
pub(crate) fn has_ended(root: &Path, now: SystemTime) -> bool {
    let metadata = match std::fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) => return error.kind() == ErrorKind::NotFound,
    };
    match File::open(root.join(OWNER_FILE)) {
        #[expect(
            clippy::disallowed_methods,
            reason = "the sweep asks whether a test still holds its owner file"
        )]
        Ok(file) => file.try_lock().is_ok(),
        Err(error) if error.kind() == ErrorKind::NotFound => metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MOST_UNOWNED_AGE),
        Err(_) => false,
    }
}

/// Removes each container, volume and network whose run-record label lies
/// in a temp root whose test run has ended, and returns their names. A
/// temp root lies below the temp directory, below one of `extra`, or below
/// a `target/tmp`. The sweep prints one line for each object it removes,
/// which the test's captured output holds.
pub(crate) fn sweep_left_behind(extra: &[&Path]) -> Vec<String> {
    let bases = temp_bases(extra);
    let now = SystemTime::now();
    let mut ended: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut swept = Vec::new();
    for kind in Kind::ALL {
        for (name, labels) in labelled(kind) {
            let Some(root) = run_record(&labels).and_then(|path| temp_root_of(path, &bases)) else {
                continue;
            };
            let over = *ended
                .entry(root.clone())
                .or_insert_with(|| has_ended(&root, now));
            if over && kind.remove(&name) {
                println!("swept {name}: its test run at {} ended", root.display());
                swept.push(name);
            }
        }
    }
    swept
}

/// Removes each container, volume and network that carries the label
/// `label`, such as `mxv.workspace-id=<id>`, and with each container its
/// anonymous volumes.
pub(crate) fn remove_labelled(label: &str) {
    let filter = format!("label={label}");
    for kind in Kind::ALL {
        let mut args = kind.list();
        args.extend(["--filter", &filter]);
        let Some((_, listed)) = docker(&args) else {
            continue;
        };
        for object in listed.split_whitespace() {
            kind.remove(object);
        }
    }
}
