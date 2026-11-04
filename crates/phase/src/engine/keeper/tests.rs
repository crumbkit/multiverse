//! The keeper's tests. The ones that need real processes start this test
//! binary as the keeper: its arguments run only the test
//! [`the_test_binary_runs_a_keeper_when_it_is_started_as_one`], which
//! finds `keep` among them and runs the keeper as the program that links
//! phase would.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::Signal;
use tempfile::TempDir;

use super::spawn::{command_line, read_handshake};
use super::spec::{Handshake, write_handshake};
use super::{
    BadKeeperArgs, KeeperArgs, KeeperCommand, KeeperExit, KeeperSpec, Kept, Launch, run, spawn,
};
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::env::AppliedEnvironment;
use crate::error::PhaseError;
use crate::plan::restart::{Backoff, Resolve, Restart, RestartLimits, Stop};
use crate::record::process::{Processes, Start, System};
use crate::record::run::{EntryState, HostProcess, ObjectKey};
use crate::record::store::{Journal, RunStore};
use crate::record::testing::{app, empty_record, process, workspace};

/// The full name of the test that runs a keeper.
const ENTRY: &str =
    "engine::keeper::tests::the_test_binary_runs_a_keeper_when_it_is_started_as_one";

/// How long a test waits for something a keeper does.
const PATIENCE: Duration = Duration::from_secs(30);

#[test]
fn the_test_binary_runs_a_keeper_when_it_is_started_as_one() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(at) = args.iter().position(|arg| arg == "keep") else {
        return;
    };
    let keeper_args = KeeperArgs::parse(args.into_iter().skip(at + 1)).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary runs as a keeper, which reads its specification from standard input and writes its handshake to standard output"
    )]
    let exit = run(&keeper_args, std::io::stdin().lock(), std::io::stdout());
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary runs as a keeper, whose exit status the engine reads"
    )]
    std::process::exit(i32::from(exit.code()));
}

/// An environment that holds its values in a list, as a test double of
/// starmap's.
struct Fixed(Vec<(&'static str, &'static str)>);

impl AppliedEnvironment for Fixed {
    fn names(&self) -> BTreeSet<String> {
        self.0.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        for (name, value) in &self.0 {
            if let Some(reference) = reference(name) {
                command.env(reference, value);
            }
        }
    }
}

/// A state directory whose run record plans `api/server`.
struct Fixture {
    /// The state directory.
    state: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let store = RunStore::new(state.path());
        let mut journal = Journal::open(&store, &workspace(), PATIENCE).unwrap();
        journal
            .plan(empty_record().host_process(&app("api"), &process("server")))
            .unwrap();
        Self { state }
    }

    fn args(&self) -> KeeperArgs {
        KeeperArgs::new(
            workspace(),
            app("api"),
            process("server"),
            self.state.path().to_path_buf(),
        )
    }

    fn log(&self) -> PathBuf {
        self.state.path().join("logs").join("server.log")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    /// Returns the specification of an app that runs `sh -c script` under
    /// `restart` and `stop`.
    fn spec(&self, script: &str, restart: Restart, stop: Stop) -> KeeperSpec {
        KeeperSpec::new(
            vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            self.state.path().to_path_buf(),
            self.log(),
            restart,
            stop,
            Duration::from_secs(5),
        )
    }

    /// Starts a keeper of `spec` from this test binary.
    fn start(&self, spec: &KeeperSpec) -> Result<Running, PhaseError> {
        let command = KeeperCommand::new(
            std::env::current_exe().unwrap(),
            ["--exact", "--nocapture", "--", ENTRY, "keep"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        );
        let environment = Fixed(vec![("GREETING", "hello")]);
        let inherited = [
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
            (
                OsString::from("HOME"),
                self.state.path().as_os_str().to_owned(),
            ),
        ];
        spawn(&Launch {
            command: &command,
            args: &self.args(),
            spec,
            environment: &environment,
            inherited: &inherited,
            deadline: now() + PATIENCE,
        })
        .map(|kept| Running { kept })
    }

    /// Returns the state of the app in the run record.
    fn recorded(&self) -> Option<(EntryState, Option<HostProcess>)> {
        let record = RunStore::new(self.state.path())
            .read(&workspace())
            .unwrap()?;
        let key = ObjectKey::Process(app("api"), process("server"));
        let state = record.state(&key)?;
        let host = record
            .processes
            .iter()
            .find(|entry| entry.process == process("server"))
            .and_then(|entry| entry.host.clone());
        Some((state, host))
    }
}

/// A started keeper. Dropping it kills the keeper's and the app's groups.
struct Running {
    kept: Kept,
}

impl Running {
    /// Sends `signal` to the keeper alone.
    fn signal_keeper(&self, signal: Signal) {
        let pid = rustix::process::Pid::from_raw(i32::try_from(self.kept.keeper.pid).unwrap());
        rustix::process::kill_process(pid.unwrap(), signal).unwrap();
    }

    /// Waits for the keeper to exit, and returns its exit status.
    fn exit_code(&mut self) -> i32 {
        let deadline = now() + PATIENCE;
        loop {
            if let Some(status) = self.kept.exited() {
                return status.code().unwrap_or(-1);
            }
            assert!(now() < deadline, "the keeper did not exit");
            pause(POLL_INTERVAL);
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = System.signal(&self.kept.keeper, Signal::KILL);
        let _ = System.signal(&self.kept.app, Signal::KILL);
        let _ = self.kept.exited();
    }
}

/// Waits until `done` holds, and fails the test after [`PATIENCE`].
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = now() + PATIENCE;
    while !done() {
        assert!(now() < deadline, "{what} did not happen");
        pause(POLL_INTERVAL);
    }
}

fn limits(max: Option<u32>, first: Duration, resolve: Resolve) -> RestartLimits {
    RestartLimits {
        max: max.and_then(NonZeroU32::new),
        backoff: Backoff::new(first, Duration::from_secs(1)).unwrap(),
        resolve,
    }
}

fn grace(grace: Duration) -> Stop {
    Stop {
        grace,
        ..Stop::default()
    }
}

fn group_of(pid: u32) -> u32 {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap());
    let group = rustix::process::getpgid(pid).unwrap();
    u32::try_from(group.as_raw_nonzero().get()).unwrap()
}

fn alive(host: &HostProcess) -> bool {
    System.start_time(host.pid).unwrap() == Start::At(host.started_at_us)
}

#[test]
fn keeper_arguments_name_the_workspace_and_read_back_from_the_command_line() {
    let fixture = Fixture::new();
    let args = fixture.args();

    let line = command_line(&args);

    assert!(line.contains(&OsString::from(workspace().as_str())));
    assert_eq!(KeeperArgs::parse(line).unwrap(), args);
}

#[test]
fn keeper_arguments_refuse_an_unknown_flag_a_repeated_one_and_a_missing_one() {
    let words = |text: &str| text.split(' ').map(OsString::from).collect::<Vec<_>>();

    assert_eq!(
        KeeperArgs::parse(words("--color red")),
        Err(BadKeeperArgs::Unknown("--color".to_owned()))
    );
    assert_eq!(
        KeeperArgs::parse(words("--app api --app web")),
        Err(BadKeeperArgs::Repeated("--app".to_owned()))
    );
    assert_eq!(
        KeeperArgs::parse(words("--workspace 0b9f3c8e --app api --process server")),
        Err(BadKeeperArgs::Missing("--state-dir".to_owned()))
    );
    assert_eq!(
        KeeperArgs::parse(words(
            "--workspace UP --app api --process server --state-dir /s"
        )),
        Err(BadKeeperArgs::Invalid("--workspace".to_owned()))
    );
}

#[test]
fn each_way_a_keeper_ends_that_the_engine_acts_on_has_its_own_status() {
    let codes = [
        KeeperExit::Completed,
        KeeperExit::Failed,
        KeeperExit::Unstoppable,
        KeeperExit::BadInput,
        KeeperExit::Unstartable,
        KeeperExit::NeedsEnvironment,
    ]
    .map(KeeperExit::code);

    assert_eq!(BTreeSet::from(codes).len(), codes.len());
    assert_eq!(KeeperExit::Stopped.code(), 0);
}

#[test]
fn the_engine_finds_the_handshake_after_lines_that_come_before_it() {
    let mut output = b"\nrunning 1 test\n".to_vec();
    write_handshake(
        &mut output,
        &Handshake::Unstartable("no such file".to_owned()),
    );

    let handshake = read_handshake(output.as_slice()).unwrap();

    assert_eq!(
        handshake,
        Some(Handshake::Unstartable("no such file".to_owned()))
    );
    assert_eq!(
        read_handshake(b"running 1 test\n".as_slice()).unwrap(),
        None
    );
}

#[test]
fn a_keeper_refuses_a_specification_it_cannot_read() {
    let fixture = Fixture::new();
    let mut output = Vec::new();

    let exit = run(
        &fixture.args(),
        b"{\"schema\": \"phase/keeper/v9\"}".as_slice(),
        &mut output,
    );

    assert_eq!(exit, KeeperExit::BadInput);
    assert!(matches!(
        read_handshake(output.as_slice()).unwrap(),
        Some(Handshake::Refused(_))
    ));
}

#[test]
fn a_keeper_starts_its_app_in_a_group_of_its_own_with_only_the_environment_it_holds() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        "echo \"greeting=$GREETING manifest=${CARGO_MANIFEST_DIR-unset}\"; exec sleep 30",
        Restart::No,
        Stop::default(),
    );

    let running = fixture.start(&spec).unwrap();

    let kept = &running.kept;
    assert_eq!(group_of(kept.keeper.pid), kept.keeper.pid);
    assert_eq!(group_of(kept.app.pid), kept.app.pid);
    assert_eq!(kept.app.group, kept.app.pid);
    assert_ne!(kept.app.group, kept.keeper.group);
    eventually("the app's line in its log", || {
        fixture.log_text() == "greeting=hello manifest=unset\n"
    });
    eventually("the keeper's record of the app", || {
        fixture.recorded() == Some((EntryState::Running, Some(kept.app.clone())))
    });
}

#[test]
fn a_keeper_restarts_a_failing_app_after_each_backoff_until_its_most_restarts() {
    let fixture = Fixture::new();
    let first = Duration::from_millis(50);
    let restart = Restart::on_failure(limits(Some(2), first, Resolve::Never));
    let spec = fixture.spec("echo run; exit 3", restart, Stop::default());
    let mut running = fixture.start(&spec).unwrap();
    let started = Instant::now();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::Failed.code()));
    assert_eq!(fixture.log_text(), "run\nrun\nrun\n");
    assert!(started.elapsed() >= first * 3);
    eventually("the keeper's record of the stop", || {
        matches!(fixture.recorded(), Some((EntryState::Stopped, _)))
    });
}

#[test]
fn a_keeper_whose_restart_needs_a_fresh_environment_exits_with_its_own_status() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(None, Duration::from_millis(10), Resolve::Always));
    let spec = fixture.spec("echo run; exit 1", restart, Stop::default());
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::NeedsEnvironment.code()));
    assert_eq!(fixture.log_text(), "run\n");
}

#[test]
fn a_keeper_stops_its_app_with_the_stop_signal_and_records_it_stopped() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        "trap 'echo stopping; exit 0' TERM; echo ready; while :; do sleep 0.05; done",
        Restart::always(RestartLimits::default()),
        Stop::default(),
    );
    let mut running = fixture.start(&spec).unwrap();
    eventually("the app's start", || fixture.log_text() == "ready\n");

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    let log = fixture.log_text();
    assert!(log.starts_with("ready\n"));
    assert!(log.ends_with("stopping\n"));
    assert!(!System.group_exists(running.kept.app.group));
    eventually("the keeper's record of the stop", || {
        matches!(fixture.recorded(), Some((EntryState::Stopped, _)))
    });
}

#[test]
fn a_keeper_kills_an_app_that_ignores_its_stop_signal_once_the_grace_passes() {
    let fixture = Fixture::new();
    let wait = Duration::from_millis(200);
    let spec = fixture.spec(
        "trap '' TERM; echo ready; while :; do sleep 0.05; done",
        Restart::No,
        grace(wait),
    );
    let mut running = fixture.start(&spec).unwrap();
    eventually("the app's start", || fixture.log_text() == "ready\n");
    let asked = Instant::now();

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    assert!(asked.elapsed() >= wait);
    assert!(!System.group_exists(running.kept.app.group));
}

#[test]
fn killing_the_keeper_leaves_its_app_running_and_appending_to_its_log() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        "while :; do echo tick; sleep 0.05; done",
        Restart::always(RestartLimits::default()),
        Stop::default(),
    );
    let mut running = fixture.start(&spec).unwrap();
    eventually("the app's first line", || !fixture.log_text().is_empty());

    running.signal_keeper(Signal::KILL);
    running.exit_code();
    let logged = fixture.log_text().len();

    assert!(alive(&running.kept.app));
    eventually("a line after the keeper died", || {
        fixture.log_text().len() > logged
    });
    assert!(alive(&running.kept.app));
}

#[test]
fn a_keeper_that_cannot_start_its_program_reports_the_process_unstartable() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec("", Restart::No, Stop::default());
    spec.run = vec!["/nonexistent/program".to_owned()];

    let error = fixture.start(&spec).err().unwrap();

    assert_eq!(error.code(), "phase::process::unstartable");
}

#[test]
fn a_keeper_whose_environment_has_expired_exits_for_a_fresh_one_before_a_restart() {
    let fixture = Fixture::new();
    let restart = Restart::always(limits(
        None,
        Duration::from_millis(10),
        Resolve::WhenExpired,
    ));
    let spec = fixture
        .spec("echo run; exit 0", restart, Stop::default())
        .expiring(Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)));
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::NeedsEnvironment.code()));
    assert_eq!(fixture.log_text(), "run\n");
}

#[test]
fn a_keeper_counts_the_restarts_of_the_keepers_before_it_toward_the_most() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(Some(2), Duration::from_millis(10), Resolve::Never));
    let spec = fixture
        .spec("echo run; exit 1", restart, Stop::default())
        .after_restarts(2);
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::Failed.code()));
    assert_eq!(fixture.log_text(), "run\n");
}
