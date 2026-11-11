//! Tests of [`Runs::build`] with an admission that records what it is asked
//! and real `sh` builds.

use std::cell::RefCell;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use phase::{Cancel, VariableName};

use super::{Admission, Asked, BuildEnd, BuildStop, Place};
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace};
use crate::runs::{Granting, Runs};
use crate::store::{Created, Store};
use crate::testing::write;

/// What a [`Recording`] admission saw.
#[derive(Default)]
struct Log {
    /// The command text of each admission.
    admitted: Vec<String>,
    /// The directory and the label of each admission.
    asked: Vec<(PathBuf, String)>,
    /// The workspace name of each admission.
    workspaces: Vec<String>,
    /// The exit code of each end of a place.
    ended: Vec<Option<i32>>,
}

/// An admission that grants at once, and gives each build `JOB=7`.
#[derive(Default)]
struct Recording {
    /// What it saw.
    log: Rc<RefCell<Log>>,
    /// Whether it cancels the builds once it grants the place.
    interrupt: bool,
    /// Whether it cancels the builds as the place ends.
    interrupt_at_end: bool,
    /// What the builds wrote.
    written: Arc<Mutex<Vec<u8>>>,
    /// Whether writing a build's output blocks until the test ends.
    stuck: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
}

/// A stream that keeps what it is given.
struct Kept(Arc<Mutex<Vec<u8>>>);

impl Write for Kept {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A stream whose first write blocks until the sender of its receiver
/// closes.
struct Stuck(Arc<Mutex<mpsc::Receiver<()>>>);

impl Write for Stuck {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.lock().unwrap().recv();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A place that records its end.
struct Recorded {
    /// What the admission saw.
    log: Rc<RefCell<Log>>,
    /// The flag that the place sets as it ends.
    cancel_at_end: Option<Cancel>,
}

impl Place for Recorded {
    fn variables(&self) -> Vec<(VariableName, String)> {
        vec![("JOB".parse().unwrap(), "7".to_owned())]
    }

    fn end(self: Box<Self>, exit_code: Option<i32>) {
        self.log.borrow_mut().ended.push(exit_code);
        if let Some(cancel) = &self.cancel_at_end {
            cancel.cancel();
        }
    }
}

impl Admission for Recording {
    fn admit(&mut self, asked: &Asked<'_>) -> Box<dyn Place> {
        let mut log = self.log.borrow_mut();
        log.admitted.push(asked.command.to_owned());
        log.asked
            .push((asked.cwd.to_path_buf(), asked.label.to_owned()));
        log.workspaces.push(asked.workspace.to_string());
        if self.interrupt {
            asked.cancel.cancel();
        }
        Box::new(Recorded {
            log: Rc::clone(&self.log),
            cancel_at_end: self.interrupt_at_end.then(|| asked.cancel.clone()),
        })
    }

    fn output(&self) -> Box<dyn Write + Send> {
        match &self.stuck {
            Some(receiver) => Box::new(Stuck(Arc::clone(receiver))),
            None => Box::new(Kept(Arc::clone(&self.written))),
        }
    }
}

/// A store, run files and a workspace of one checkout.
struct World {
    /// The temporary directory.
    dir: tempfile::TempDir,
    /// The workspace.
    workspace: Workspace,
    /// Whether [`World::build`] leaves the units' permissions ungranted,
    /// as when no person answered.
    ungranted: bool,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let name: WorkspaceName = "triage".parse().unwrap();
        let path = dir.path().join("triage");
        fs::create_dir_all(path.join("app")).unwrap();
        let checkout = Checkout::new(
            "app".to_owned(),
            Path::new("/nowhere/app"),
            path.join("app"),
            name.to_string(),
            "origin/main".to_owned(),
        );
        let mut workspace = Workspace::new(name, path, vec![checkout]);
        let store = Store::new(dir.path().join("workspaces"));
        assert_eq!(store.create(&mut workspace).unwrap(), Created::Written);
        Self {
            dir,
            workspace,
            ungranted: false,
        }
    }

    fn runs(&self) -> Runs {
        Runs::new(self.dir.path())
    }

    fn checkout(&self) -> PathBuf {
        self.workspace.path.join("app")
    }

    /// Runs the builds of `manifest` with `runs` and `admission`, and
    /// returns a word for how they ended, as the tests compare it.
    fn build(&mut self, runs: &Runs, manifest: &str, admission: &mut Recording) -> BuildEnded {
        let manifest = Manifest::parse(manifest, Path::new("mxv.toml")).unwrap();
        let store = Store::new(self.dir.path().join("workspaces"));
        // A person at a terminal grants the units' permissions, as `mxv up`
        // asks before it takes the lock.
        if !self.ungranted
            && let Granting::Asks(ask) = runs
                .granting(&store, &self.workspace, &manifest, &[])
                .unwrap()
        {
            runs.grant(&ask).unwrap();
        }
        let lock = runs.lock(&self.workspace.name).unwrap();
        let end = runs
            .build(
                &lock,
                &store,
                &mut self.workspace,
                &manifest,
                &[],
                admission,
            )
            .unwrap();
        match end {
            BuildEnd::Passed(_) => BuildEnded::Passed,
            BuildEnd::Stopped { process, why } => match why {
                BuildStop::Failed { code, .. } => BuildEnded::Failed(process, code),
                BuildStop::Unstartable { .. } => BuildEnded::Unstartable(process),
                BuildStop::TimedOut { .. } => BuildEnded::TimedOut(process),
                BuildStop::Interrupted => BuildEnded::Interrupted(process),
            },
            BuildEnd::UnknownProcess(_) | BuildEnd::Unrunnable(_) => BuildEnded::Refused,
            BuildEnd::NotGranted(_) => BuildEnded::NotGranted,
        }
    }
}

/// How [`World::build`] ended, without its proof of the lock.
#[derive(Debug, PartialEq, Eq)]
enum BuildEnded {
    Passed,
    Failed(String, Option<i32>),
    Unstartable(String),
    TimedOut(String),
    Interrupted(String),
    Refused,
    NotGranted,
}

fn process(name: &str, script: &str, more: &str) -> String {
    format!("[processes.{name}]\nargv = [\"true\"]\nbuild = [\"sh\", \"-c\", {script:?}]\n{more}")
}

#[test]
fn a_manifest_without_a_build_passes_without_asking_for_a_place() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();

    let ended = world.build(&runs, "[processes.a]\nargv = [\"true\"]\n", &mut admission);

    assert_eq!(ended, BuildEnded::Passed);
    assert!(admission.log.borrow().admitted.is_empty());
    assert!(admission.log.borrow().ended.is_empty());
}

#[test]
fn every_build_runs_under_one_place_in_start_order_with_the_places_variables() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();
    let manifest = format!(
        "{}\n{}",
        process("web", "echo web=$JOB >> built", "depends_on = [\"api\"]\n"),
        process("api", "echo api=$JOB >> built", ""),
    );

    let ended = world.build(&runs, &manifest, &mut admission);

    assert_eq!(ended, BuildEnded::Passed);
    assert_eq!(
        fs::read_to_string(world.checkout().join("built")).unwrap(),
        "api=7\nweb=7\n"
    );
    let log = admission.log.borrow();
    assert_eq!(log.admitted.len(), 1);
    assert!(log.admitted[0].starts_with("api: sh -c"));
    assert_eq!(log.asked, [(world.checkout(), "triage".to_owned())]);
    assert_eq!(log.ended, [Some(0)]);
}

#[test]
fn a_unit_whose_permissions_no_person_granted_runs_no_build_and_asks_for_no_place() {
    let mut world = World::new();
    world.ungranted = true;
    let mut admission = Recording::default();
    let runs = world.runs();
    let manifest = process("api", "touch built", "");

    let ended = world.build(&runs, &manifest, &mut admission);

    assert_eq!(ended, BuildEnded::NotGranted);
    assert!(!world.checkout().join("built").exists());
    assert!(admission.log.borrow().admitted.is_empty());
}

#[test]
fn a_selection_without_build_steps_is_still_checked_against_the_grant() {
    let mut world = World::new();
    world.ungranted = true;
    let mut admission = Recording::default();
    let runs = world.runs();

    let ended = world.build(
        &runs,
        "[processes.api]\nargv = [\"true\"]\n",
        &mut admission,
    );

    assert_eq!(ended, BuildEnded::NotGranted);
}

#[test]
fn a_selection_of_containers_alone_asks_for_nothing() {
    let mut world = World::new();
    world.ungranted = true;
    let mut admission = Recording::default();
    let runs = world.runs();

    let ended = world.build(
        &runs,
        "[containers.db]\nimage = \"postgres:17.2\"\n",
        &mut admission,
    );

    assert_eq!(ended, BuildEnded::Passed);
    assert!(!world.dir.path().join("phase").join("grants").exists());
}

#[test]
fn a_build_asks_the_admission_with_the_workspaces_name() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();

    let ended = world.build(&runs, &process("api", "true", ""), &mut admission);

    assert_eq!(ended, BuildEnded::Passed);
    assert_eq!(admission.log.borrow().workspaces, ["triage"]);
}

#[test]
fn a_failing_build_ends_the_place_with_its_code_and_the_later_builds_do_not_run() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();
    let manifest = format!(
        "{}\n{}",
        process("web", "touch web-built", "depends_on = [\"api\"]\n"),
        process("api", "exit 3", ""),
    );

    let ended = world.build(&runs, &manifest, &mut admission);

    assert_eq!(ended, BuildEnded::Failed("api".to_owned(), Some(3)));
    assert!(!world.checkout().join("web-built").exists());
    assert_eq!(admission.log.borrow().ended, [Some(3)]);
}

#[test]
fn a_build_that_cannot_start_names_its_process_and_ends_the_place_without_a_code() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();
    let manifest = "[processes.api]\nargv = [\"true\"]\nbuild = [\"/no/such/program\"]\n";

    let ended = world.build(&runs, manifest, &mut admission);

    assert_eq!(ended, BuildEnded::Unstartable("api".to_owned()));
    assert_eq!(admission.log.borrow().ended, [None]);
}

#[test]
fn a_build_that_outlives_the_deadline_ends_with_its_group() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs().with_build_wait(Duration::from_millis(300));
    let script = "sleep 60 & echo $$ > group; wait";

    let ended = world.build(&runs, &process("api", script, ""), &mut admission);

    assert_eq!(ended, BuildEnded::TimedOut("api".to_owned()));
    assert_eq!(admission.log.borrow().ended, [None]);
    let group: i32 = fs::read_to_string(world.checkout().join("group"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let group = rustix::process::Pid::from_raw(group).unwrap();
    assert!(rustix::process::test_kill_process_group(group).is_err());
}

#[test]
fn a_cancel_after_the_grant_stops_the_builds_as_an_interruption() {
    let mut world = World::new();
    let mut admission = Recording {
        interrupt: true,
        ..Recording::default()
    };
    let runs = world.runs();

    let ended = world.build(&runs, &process("api", "touch built", ""), &mut admission);

    assert_eq!(ended, BuildEnded::Interrupted("api".to_owned()));
    assert!(!world.checkout().join("built").exists());
    assert_eq!(admission.log.borrow().ended, [None]);
}

#[test]
fn a_build_runs_in_its_processs_directory() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();
    fs::create_dir_all(world.checkout().join("server")).unwrap();
    write(world.checkout().join("server/seen"), "");

    let ended = world.build(
        &runs,
        &process("api", "test -e seen", "cwd = \"server\"\n"),
        &mut admission,
    );

    assert_eq!(ended, BuildEnded::Passed);
    assert_eq!(
        admission.log.borrow().asked[0].0,
        world.checkout().join("server")
    );
}

#[test]
fn a_cancel_as_the_place_ends_stops_the_command_after_the_builds_passed() {
    let mut world = World::new();
    let mut admission = Recording {
        interrupt_at_end: true,
        ..Recording::default()
    };
    let runs = world.runs();

    let ended = world.build(&runs, &process("api", "touch built", ""), &mut admission);

    assert_eq!(ended, BuildEnded::Interrupted("api".to_owned()));
    assert!(world.checkout().join("built").exists());
    assert_eq!(admission.log.borrow().ended, [Some(0)]);
}

#[test]
fn a_builds_output_and_errors_reach_the_admissions_stream_in_order() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();
    let script = "echo first; echo second >&2; echo third";

    let ended = world.build(&runs, &process("api", script, ""), &mut admission);

    assert_eq!(ended, BuildEnded::Passed);
    assert_eq!(
        String::from_utf8_lossy(&admission.written.lock().unwrap()),
        "first\nsecond\nthird\n"
    );
}

/// Sends `SIGKILL` to the process whose id the file at `path` holds.
fn kill(path: &Path) {
    let pid: i32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
}

#[test]
fn a_process_left_holding_the_builds_output_does_not_hold_up_the_command() {
    let mut world = World::new();
    let mut admission = Recording::default();
    let runs = world.runs();
    let script = "sleep 40 & echo $! > left; echo the-last-line";
    let started = Instant::now();

    let ended = world.build(&runs, &process("api", script, ""), &mut admission);

    let took = started.elapsed();
    kill(&world.checkout().join("left"));
    assert_eq!(ended, BuildEnded::Passed);
    assert!(took < Duration::from_secs(20), "{took:?}");
    assert_eq!(
        String::from_utf8_lossy(&admission.written.lock().unwrap()),
        "the-last-line\n"
    );
}

#[test]
fn the_deadline_ends_a_build_whose_output_nobody_reads() {
    let mut world = World::new();
    let (keep, receiver) = mpsc::channel::<()>();
    let mut admission = Recording {
        stuck: Some(Arc::new(Mutex::new(receiver))),
        ..Recording::default()
    };
    let runs = world.runs().with_build_wait(Duration::from_millis(300));
    let script = "while :; do echo the-pipe-fills; done";
    let started = Instant::now();

    let ended = world.build(&runs, &process("api", script, ""), &mut admission);

    let took = started.elapsed();
    drop(keep);
    assert_eq!(ended, BuildEnded::TimedOut("api".to_owned()));
    assert!(took < Duration::from_secs(20), "{took:?}");
    assert_eq!(admission.log.borrow().ended, [None]);
}
