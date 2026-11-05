//! The keeper's tests. The ones that need real processes start this test
//! binary as the keeper: its arguments run only the test
//! [`the_test_binary_runs_a_keeper_when_it_is_started_as_one`], which
//! finds `keep` among them and runs the keeper as the program that links
//! phase would.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write};
use std::num::NonZeroU32;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use rustix::process::Signal;
use tempfile::TempDir;

use super::recorder::Note;
use super::spawn::{command_line, read_handshake};
use super::spec::{Handshake, NotInherited, read_spec, write_handshake};
use super::supervise::keep;
use super::{
    BadKeeperArgs, KeeperArgs, KeeperCommand, KeeperEnvironment, KeeperExit, KeeperSpec, Kept,
    Launch, run, spawn,
};
use crate::cancel::Cancel;
use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::restart::RunEnd;
use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName};
use crate::plan::restart::{Backoff, Resolve, Restart, RestartLimits, Stop};
use crate::record::process::{Processes, Start, System};
use crate::record::run::{Ended, EntryState, Exit, HostProcess, ObjectKey, Ready};
use crate::record::store::{Journal, RunStore};
use crate::record::testing::{app, empty_record, process, workspace};

/// The full name of the test that runs a keeper.
const ENTRY: &str =
    "engine::keeper::tests::the_test_binary_runs_a_keeper_when_it_is_started_as_one";

/// The full name of the test that starts a keeper and waits.
const SPAWNER: &str =
    "engine::keeper::tests::the_test_binary_starts_a_keeper_when_it_is_started_as_a_spawner";

/// How long a test waits for something a keeper does.
const PATIENCE: Duration = Duration::from_secs(30);

/// A loop of 1200 sleeps of 0.05 seconds, which stands in for an app that
/// runs until it is stopped. It ends by itself after a minute or more, so
/// that an app that a test fails to stop does not run on for long.
macro_rules! spin {
    () => {
        "i=0; while [ $i -lt 1200 ]; do i=$((i + 1)); sleep 0.05; done"
    };
}

#[test]
fn the_test_binary_runs_a_keeper_when_it_is_started_as_one() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(at) = args.iter().position(|arg| arg == "keep") else {
        return;
    };
    let keeper_args = KeeperArgs::parse(args.into_iter().skip(at + 1)).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary runs as a keeper, and reads its environment as the main of the program that links phase does"
    )]
    let environment = KeeperEnvironment::from_vars(std::env::vars_os());
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary runs as a keeper, which writes its handshake to standard output"
    )]
    let stdout = std::io::stdout();
    if keeper_args.process().as_str() == "default-sigpipe" {
        // The program that links phase may leave `SIGPIPE` at its default,
        // which ends a process that writes to a closed pipe.
        #[expect(
            unsafe_code,
            reason = "the test keeper restores the default SIGPIPE disposition, which only libc can set"
        )]
        // SAFETY: `signal` only changes the process's disposition of
        // `SIGPIPE` to `SIG_DFL`, which is a valid disposition for it, and
        // touches no memory of this program.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    let output: Box<dyn Write> = if keeper_args.process().as_str() == "slow-handshake" {
        Box::new(Slow(stdout))
    } else {
        Box::new(stdout)
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary runs as a keeper, which reads its specification from standard input"
    )]
    let exit = run(
        &keeper_args,
        environment,
        &Source,
        std::io::stdin().lock(),
        output,
    );
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary runs as a keeper, whose exit status the engine reads"
    )]
    std::process::exit(i32::from(exit.code()));
}

#[test]
fn the_test_binary_starts_a_keeper_when_it_is_started_as_a_spawner() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let Some(at) = args.iter().position(|arg| arg == "spawn-keeper") else {
        return;
    };
    let fixture = Fixture::at(PathBuf::from(&args[at + 1]));
    let spec = fixture.spec(spin!(), Restart::No, Stop::default());
    let running = fixture.start(&spec).unwrap();
    println!(
        "spawned {} {} {} {}",
        running.kept.keeper.pid,
        running.kept.keeper.started_at_us,
        running.kept.app.pid,
        running.kept.app.started_at_us
    );
    // The spawner leaves the keeper running, and waits for a signal or for
    // its standard input to close.
    std::mem::forget(running);
    #[expect(
        clippy::disallowed_methods,
        reason = "the spawner waits until its test closes its standard input"
    )]
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
}

/// How long the keepers of the processes `slow-handshake` and
/// `slow-resolve` take over their handshake and their resolving.
const SLOW: Duration = Duration::from_secs(2);

/// An output that pauses for [`SLOW`] before each write, as a keeper whose
/// handshake comes late.
struct Slow<W>(W);

impl<W: Write> Write for Slow<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        pause(SLOW);
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// The environment source of the keepers that tests start. It resolves
/// `GREETING=fresh`, fails for the process `unresolvable`, resolves no
/// variable for the process `missing-required`, and takes
/// [`SLOW`] for the process `slow-resolve`.
struct Source;

impl EnvironmentSource for Source {
    fn resolve(
        &self,
        _app: &AppName,
        process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        if process.as_str() == "slow-resolve" {
            pause(SLOW);
        }
        if process.as_str() == "missing-required" {
            return Ok(Box::new(Fixed(Vec::new())));
        }
        if process.as_str() == "unresolvable" {
            return Err(EnvironmentError::new(
                "GREETING".to_owned(),
                "vault".to_owned(),
                "the vault is locked".into(),
            ));
        }
        Ok(Box::new(Fixed(vec![("GREETING", "fresh")])))
    }
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

/// A state directory whose run record plans `api/<process>`.
struct Fixture {
    /// The temporary state directory, which a fixture made in another
    /// process owns instead.
    _owned: Option<TempDir>,
    /// The state directory.
    dir: PathBuf,
    /// The process, whose name tells [`Source`] how to answer.
    process: &'static str,
}

impl Fixture {
    /// Returns a fixture of the process `server`.
    fn new() -> Self {
        Self::of("server")
    }

    /// Returns a fixture of the process `name`.
    fn of(name: &'static str) -> Self {
        let state = tempfile::tempdir().unwrap();
        let store = RunStore::new(state.path());
        let mut journal = Journal::open(&store, &workspace(), PATIENCE).unwrap();
        journal
            .plan(empty_record().host_process(&app("api"), &process(name)))
            .unwrap();
        Self {
            dir: state.path().to_path_buf(),
            _owned: Some(state),
            process: name,
        }
    }

    /// Returns the fixture of the process `server` whose state directory
    /// another process made at `dir`.
    const fn at(dir: PathBuf) -> Self {
        Self {
            _owned: None,
            dir,
            process: "server",
        }
    }

    fn args(&self) -> KeeperArgs {
        KeeperArgs::new(
            workspace(),
            app("api"),
            process(self.process),
            self.dir.clone(),
        )
    }

    fn log(&self) -> PathBuf {
        self.dir.join("logs").join(format!("{}.log", self.process))
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    fn diagnostics_text(&self) -> String {
        let path = self
            .dir
            .join("logs")
            .join(format!("api.{}.keeper.log", self.process));
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Returns the specification of an app that runs `sh -c script` under
    /// `restart` and `stop`.
    fn spec(&self, script: &str, restart: Restart, stop: Stop) -> KeeperSpec {
        KeeperSpec::new(
            vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            self.dir.clone(),
            self.log(),
            restart,
            stop,
            Duration::from_secs(5),
        )
        .inheriting(BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), self.dir.display().to_string()),
        ]))
        .unwrap()
    }

    /// Starts a keeper of `spec` from this test binary, with
    /// `GREETING=hello` as the resolved environment.
    fn start(&self, spec: &KeeperSpec) -> Result<Running, PhaseError> {
        self.start_with(spec, &Fixed(vec![("GREETING", "hello")]))
    }

    /// Starts a keeper of `spec` from this test binary, with `environment`
    /// as the resolved environment.
    fn start_with(
        &self,
        spec: &KeeperSpec,
        environment: &dyn AppliedEnvironment,
    ) -> Result<Running, PhaseError> {
        self.start_by(spec, environment, now() + PATIENCE)
    }

    /// Starts a keeper of `spec` from this test binary, with
    /// `environment` as the resolved environment, which must start the app
    /// by `deadline`.
    fn start_by(
        &self,
        spec: &KeeperSpec,
        environment: &dyn AppliedEnvironment,
        deadline: Instant,
    ) -> Result<Running, PhaseError> {
        let command = KeeperCommand::new(
            std::env::current_exe().unwrap(),
            ["--exact", "--nocapture", "--", ENTRY, "keep"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        );
        spawn(&Launch {
            command: &command,
            args: &self.args(),
            spec,
            environment,
            deadline,
        })
        .map(|kept| Running { kept })
    }

    /// Returns the state of the app in the run record, its process and its
    /// count of restarts.
    fn recorded(&self) -> Option<(EntryState, Option<HostProcess>, u32)> {
        let record = RunStore::new(&self.dir).read(&workspace()).unwrap()?;
        let key = ObjectKey::Process(app("api"), process(self.process));
        let state = record.state(&key)?;
        let entry = record
            .processes
            .iter()
            .find(|entry| entry.process == process(self.process))?;
        Some((state, entry.host.clone(), entry.restarts))
    }

    /// Returns when the run record says the app was last observed ready.
    fn ready(&self) -> Option<Ready> {
        let record = RunStore::new(&self.dir).read(&workspace()).unwrap()?;
        record
            .processes
            .iter()
            .find(|entry| entry.process == process(self.process))?
            .ready
            .clone()
    }

    /// Returns how the run record says the app last ended.
    fn ended(&self) -> Option<Ended> {
        let record = RunStore::new(&self.dir).read(&workspace()).unwrap()?;
        record
            .processes
            .iter()
            .find(|entry| entry.process == process(self.process))?
            .ended
    }
}

/// A started keeper. Dropping it kills the keeper and every process of
/// the keeper's session, and then checks that none is left, so that no app
/// that the keeper started, or restarted, outlives its test.
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
        // The keeper goes first, so that it starts no further app. Each app
        // it starts or restarts leads a new group, which a kill of the
        // first app's group misses, but every one of them stays in the
        // session that the keeper leads.
        let _ = System.signal(&self.kept.keeper, Signal::KILL);
        let _ = System.signal(&self.kept.app, Signal::KILL);
        let deadline = now() + PATIENCE;
        while self.kept.exited().is_none() && now() < deadline {
            pause(POLL_INTERVAL);
        }
        let left = end_session(self.kept.keeper.pid, deadline);
        // A second panic while the test unwinds would abort the test binary
        // and hide the first, so only a test that passed checks.
        if !std::thread::panicking() {
            assert!(
                left.is_empty(),
                "the processes {left:?} of the keeper's session outlived the test"
            );
        }
    }
}

/// Kills every process of the session `session` until none is left or
/// `deadline` passes, and returns the processes that are left.
fn end_session(session: u32, deadline: Instant) -> Vec<u32> {
    loop {
        let members = session_members(session);
        if members.is_empty() || now() >= deadline {
            return members;
        }
        for pid in &members {
            if let Some(pid) = rustix::process::Pid::from_raw(i32::try_from(*pid).unwrap()) {
                let _ = rustix::process::kill_process(pid, Signal::KILL);
            }
        }
        pause(POLL_INTERVAL);
    }
}

/// Returns the processes of the session `session` other than its leader.
fn session_members(session: u32) -> Vec<u32> {
    all_processes()
        .into_iter()
        .filter(|pid| *pid != session)
        .filter(|pid| {
            rustix::process::Pid::from_raw(i32::try_from(*pid).unwrap())
                .and_then(|pid| rustix::process::getsid(Some(pid)).ok())
                .is_some_and(|sid| u32::try_from(sid.as_raw_nonzero().get()) == Ok(session))
        })
        .collect()
}

/// Returns the ids of every process on this machine.
fn all_processes() -> Vec<u32> {
    #[cfg(target_os = "macos")]
    let ids = crate::record::process::darwin::all_processes()
        .unwrap()
        .into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect();
    #[cfg(target_os = "linux")]
    let ids = std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    ids
}

/// Processes that a test kills, with their groups, when it ends.
struct Killed(Vec<HostProcess>);

impl Drop for Killed {
    fn drop(&mut self) {
        for host in &self.0 {
            let _ = System.signal(host, Signal::KILL);
        }
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
fn a_keeper_exits_completed_or_failed_as_its_app_ended() {
    assert_eq!(KeeperExit::from(RunEnd::Completed), KeeperExit::Completed);
    assert_eq!(KeeperExit::from(RunEnd::Failed), KeeperExit::Failed);
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
        KeeperExit::EnvironmentUnresolved,
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

    let exit = keep(
        &fixture.args(),
        KeeperEnvironment::from_vars([]),
        &Source,
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
        fixture.recorded() == Some((EntryState::Running, Some(kept.app.clone()), 0))
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
    eventually("the keeper's record of the stop after two restarts", || {
        matches!(fixture.recorded(), Some((EntryState::Stopped, _, 2)))
    });
}

#[test]
fn a_keeper_stops_its_app_with_the_stop_signal_and_records_it_stopped_while_the_lock_is_free() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        concat!("trap 'echo stopping; exit 0' TERM; echo ready; ", spin!()),
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
    assert!(matches!(
        fixture.recorded(),
        Some((EntryState::Stopped, _, _))
    ));
}

#[test]
fn a_keeper_kills_an_app_that_ignores_its_stop_signal_once_the_grace_passes() {
    let fixture = Fixture::new();
    let wait = Duration::from_millis(200);
    let spec = fixture.spec(
        concat!("trap '' TERM; echo ready; ", spin!()),
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
        "i=0; while [ $i -lt 1200 ]; do i=$((i + 1)); echo tick; sleep 0.05; done",
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

#[test]
fn a_keeper_resolves_a_fresh_environment_for_a_restart_that_asks_for_one() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(Some(1), Duration::from_millis(10), Resolve::Always));
    let spec = fixture.spec(
        "echo \"greeting=$GREETING home=$HOME manifest=${CARGO_MANIFEST_DIR-unset}\"; exit 1",
        restart,
        Stop::default(),
    );
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::Failed.code()));
    let home = fixture.dir.display();
    assert_eq!(
        fixture.log_text(),
        format!(
            "greeting=hello home={home} manifest=unset\ngreeting=fresh home={home} manifest=unset\n"
        )
    );
}

#[test]
fn a_keeper_resolves_a_fresh_environment_once_its_environment_has_expired() {
    let fixture = Fixture::new();
    let restart = Restart::always(limits(
        Some(1),
        Duration::from_millis(10),
        Resolve::WhenExpired,
    ));
    let spec = fixture
        .spec("echo \"greeting=$GREETING\"", restart, Stop::default())
        .expiring(Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)));
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::Completed.code()));
    assert_eq!(fixture.log_text(), "greeting=hello\ngreeting=fresh\n");
}

#[test]
fn a_keeper_that_cannot_resolve_an_environment_retries_and_then_exits_unresolved() {
    let fixture = Fixture::of("unresolvable");
    let restart = Restart::on_failure(limits(Some(2), Duration::from_millis(10), Resolve::Always));
    let spec = fixture.spec("echo run; exit 1", restart, Stop::default());
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::EnvironmentUnresolved.code()));
    assert_eq!(fixture.log_text(), "run\n");
    let diagnostics = fixture.diagnostics_text();
    assert_eq!(
        diagnostics
            .matches("vault could not supply the variable GREETING")
            .count(),
        2
    );
    assert!(!diagnostics.contains("hello"));
}

#[test]
fn a_keeper_notes_its_start_restarts_and_exit_in_its_diagnostic_log_without_a_value() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(Some(1), Duration::from_millis(10), Resolve::Never));
    let spec = fixture.spec(
        "echo \"greeting=$GREETING\"; exit 1",
        restart,
        Stop::default(),
    );
    let mut running = fixture.start(&spec).unwrap();

    running.exit_code();

    let diagnostics = fixture.diagnostics_text();
    for expected in [
        "keeper ",
        "in group ",
        "the app started as process",
        "the app ended with exit status: 1",
        "restart 1 follows in 10 ms",
        "keeper exits with Failed (1)",
    ] {
        assert!(
            diagnostics.contains(expected),
            "{expected} is missing from {diagnostics}"
        );
    }
    assert!(fixture.log_text().contains("hello"));
    assert!(!diagnostics.contains("hello"));
}

#[test]
fn a_keeper_survives_a_hangup_sent_to_the_group_that_started_it() {
    let fixture = Fixture::new();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts this test binary as a spawner in a group of its own"
    )]
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "--nocapture", "--", SPAWNER, "spawn-keeper"])
        .arg(&fixture.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts this test binary as a spawner in a group of its own"
    )]
    let mut spawner = command.spawn().unwrap();
    let mut lines = BufReader::new(spawner.stdout.take().unwrap()).lines();
    let spawned = lines
        .find_map(|line| line.unwrap().strip_prefix("spawned ").map(str::to_owned))
        .unwrap();
    let numbers: Vec<u64> = spawned
        .split(' ')
        .map(|word| word.parse().unwrap())
        .collect();
    let host = |pid: u64, started_at_us: u64| HostProcess {
        pid: u32::try_from(pid).unwrap(),
        started_at_us,
        group: u32::try_from(pid).unwrap(),
        boot_id: None,
    };
    let keeper = host(numbers[0], numbers[1]);
    let app_process = host(numbers[2], numbers[3]);
    let _cleanup = Killed(vec![keeper.clone(), app_process.clone()]);
    let spawner_group =
        rustix::process::Pid::from_raw(i32::try_from(spawner.id()).unwrap()).unwrap();

    rustix::process::kill_process_group(spawner_group, Signal::HUP).unwrap();
    spawner.wait().unwrap();

    assert!(alive(&keeper));
    assert!(alive(&app_process));
    assert_eq!(group_of(keeper.pid), keeper.pid);
    let session = rustix::process::getsid(rustix::process::Pid::from_raw(
        i32::try_from(keeper.pid).unwrap(),
    ))
    .unwrap();
    assert_eq!(
        u32::try_from(session.as_raw_nonzero().get()).unwrap(),
        keeper.pid
    );
}

#[test]
fn an_app_receives_only_path_home_and_its_resolved_environment() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        "echo \"path=$PATH tmp=${TMPDIR-unset} manifest=${CARGO_MANIFEST_DIR-unset} greeting=$GREETING\"",
        Restart::No,
        Stop::default(),
    );
    let mut running = fixture.start(&spec).unwrap();

    running.exit_code();

    assert_eq!(
        fixture.log_text(),
        "path=/usr/bin:/bin tmp=unset manifest=unset greeting=hello\n"
    );
}

/// Reports whether a process with the id `pid` exists.
fn exists(pid: u32) -> bool {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::test_kill_process(pid).is_ok()
}

#[test]
fn a_keeper_whose_handshake_misses_the_deadline_is_unstartable_and_leaves_no_app_behind() {
    let fixture = Fixture::of("slow-handshake");
    let spec = fixture.spec("echo $$; exec sleep 30", Restart::No, Stop::default());

    let error = fixture
        .start_by(
            &spec,
            &Fixed(vec![("GREETING", "hello")]),
            now() + Duration::from_millis(500),
        )
        .err()
        .unwrap();

    assert_eq!(error.code(), "phase::keeper::unstartable");
    eventually("the app's pid in its log", || {
        fixture.log_text().ends_with('\n')
    });
    let pid: u32 = fixture.log_text().trim().parse().unwrap();
    eventually("the end of the app", || !exists(pid));
}

#[test]
fn a_stop_request_while_the_keeper_resolves_an_environment_leaves_the_app_stopped() {
    let fixture = Fixture::of("slow-resolve");
    let restart = Restart::on_failure(limits(Some(3), Duration::from_millis(10), Resolve::Always));
    let spec = fixture.spec("echo run; exit 1", restart, Stop::default());
    let mut running = fixture.start(&spec).unwrap();
    eventually("the keeper's note that it resolves", || {
        fixture
            .diagnostics_text()
            .contains("the keeper resolves a fresh environment\n")
    });

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    assert_eq!(fixture.log_text(), "run\n");
    assert!(
        fixture
            .diagnostics_text()
            .contains("while the keeper resolved")
    );
}

#[test]
fn a_stop_request_during_a_backoff_leaves_the_app_stopped() {
    let fixture = Fixture::new();
    let long = Duration::from_secs(20);
    let restart = Restart::on_failure(RestartLimits {
        max: NonZeroU32::new(3),
        backoff: Backoff::new(long, long).unwrap(),
        resolve: Resolve::Never,
    });
    let spec = fixture.spec("echo run; exit 1", restart, Stop::default());
    let mut running = fixture.start(&spec).unwrap();
    eventually("the keeper's note of the restart", || {
        fixture.diagnostics_text().contains("restart 1 follows")
    });
    let asked = Instant::now();

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    assert!(asked.elapsed() < long);
    assert_eq!(fixture.log_text(), "run\n");
}

#[test]
fn a_keeper_leaves_a_record_entry_that_names_another_process_unchanged() {
    let fixture = Fixture::new();
    let spec = fixture.spec(spin!(), Restart::No, Stop::default());
    let mut running = fixture.start(&spec).unwrap();
    let app_process = running.kept.app.clone();
    eventually("the keeper's record of the app", || {
        fixture.recorded() == Some((EntryState::Running, Some(app_process.clone()), 0))
    });
    let other = HostProcess {
        pid: 2,
        started_at_us: 1,
        group: 2,
        boot_id: None,
    };
    let store = RunStore::new(&fixture.dir);
    let key = ObjectKey::Process(app("api"), process("server"));
    Journal::open(&store, &workspace(), PATIENCE)
        .unwrap()
        .restarted(&key, other.clone(), 7)
        .unwrap();

    // The app ends on its own, so the keeper waits for its record writes,
    // which a keeper that a stop request ends does not.
    System.signal(&app_process, Signal::KILL).unwrap();

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Failed.code()));
    assert_eq!(
        fixture.recorded(),
        Some((EntryState::Running, Some(other), 7))
    );
    assert!(fixture.diagnostics_text().contains(
        "the run record names another process or already records its stop, so the keeper skips"
    ));
}

#[test]
fn a_specification_that_inherits_a_variable_other_than_path_and_home_is_refused() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec("true", Restart::No, Stop::default());
    spec.inherited
        .insert("TMPDIR".to_owned(), "/tmp".to_owned());
    let bytes = serde_json::to_vec(&spec).unwrap();

    let refused = read_spec(bytes.as_slice()).unwrap_err();

    assert!(refused.contains("TMPDIR"), "{refused}");
}

#[test]
fn a_specification_cannot_be_made_to_inherit_a_variable_other_than_path_and_home() {
    let fixture = Fixture::new();
    let spec = fixture.spec("true", Restart::No, Stop::default());

    let refused = spec
        .inheriting(BTreeMap::from([("TMPDIR".to_owned(), "/tmp".to_owned())]))
        .unwrap_err();

    assert_eq!(refused, NotInherited("TMPDIR".to_owned()));
}

#[test]
fn a_stop_request_ends_the_keepers_wait_for_its_last_record_writes_at_once() {
    let fixture = Fixture::new();
    let spec = fixture.spec("echo run; exit 1", Restart::No, Stop::default());
    let store = RunStore::new(&fixture.dir);
    let held = Journal::open(&store, &workspace(), PATIENCE).unwrap();
    let mut running = fixture.start(&spec).unwrap();
    eventually("the keeper's wait for its record writes", || {
        fixture
            .diagnostics_text()
            .contains("the keeper waits for its last record writes")
    });
    let asked = Instant::now();

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Failed.code()));
    // Without the stop request the writer would wait 30 minutes for the
    // lock. The bound is loose so that a busy machine does not fail it.
    assert!(asked.elapsed() < Duration::from_secs(10));
    assert!(
        fixture
            .diagnostics_text()
            .contains("another operation holds the lifecycle lock")
    );
    drop(held);
}

#[test]
fn a_specification_whose_resolved_names_are_not_variable_names_is_refused() {
    let fixture = Fixture::new();
    for name in ["", "A=B", "A\0B", "PATH", "HOME"] {
        let mut spec = fixture.spec("true", Restart::No, Stop::default());
        spec.resolved.insert(name.to_owned());
        let bytes = serde_json::to_vec(&spec).unwrap();

        let refused = read_spec(bytes.as_slice());

        assert!(refused.is_err(), "{name:?} was accepted");
    }
}

#[test]
fn a_keeper_environment_shows_its_names_and_no_value() {
    let environment =
        KeeperEnvironment::from_vars([(OsString::from("GREETING"), OsString::from("hello"))]);

    let shown = format!("{environment:?}");

    assert!(shown.contains("GREETING"));
    assert!(!shown.contains("hello"));
}

#[test]
fn a_keeper_records_the_exit_code_of_an_app_that_ended_by_itself() {
    let fixture = Fixture::new();
    let spec = fixture.spec("exit 3", Restart::No, Stop::default());
    let mut running = fixture.start(&spec).unwrap();

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Failed.code()));

    eventually("the keeper's record of the end", || {
        fixture.ended() == Some(Ended::by_itself(Some(Exit::Code(3))))
    });
    assert!(matches!(
        fixture.recorded(),
        Some((EntryState::Stopped, _, 0))
    ));
}

#[test]
fn a_keeper_records_a_stop_request_as_the_cause_of_its_apps_end() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        concat!("trap 'exit 7' TERM; echo ready; ", spin!()),
        Restart::No,
        Stop::default(),
    );
    let mut running = fixture.start(&spec).unwrap();
    eventually("the app's start", || fixture.log_text() == "ready\n");

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    assert_eq!(fixture.ended(), Some(Ended::requested(Some(Exit::Code(7)))));
}

#[test]
fn a_keeper_keeps_the_last_crash_of_an_app_that_it_restarted() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(None, Duration::from_millis(20), Resolve::Never));
    let spec = fixture.spec(
        concat!(
            "if [ -e crashed ]; then ",
            spin!(),
            "; exit 0; fi; touch crashed; kill -9 $$"
        ),
        restart,
        Stop::default(),
    );
    let _running = fixture.start(&spec).unwrap();

    eventually("the restart", || {
        matches!(fixture.recorded(), Some((EntryState::Running, _, 1)))
    });

    assert_eq!(
        fixture.ended(),
        Some(Ended::by_itself(Some(Exit::Signal(9))))
    );
}

/// A script that crashes on its first run and runs until stopped after.
const CRASH_ONCE: &str = concat!(
    "if [ -e crashed ]; then ",
    spin!(),
    "; exit 0; fi; touch crashed; kill -9 $$"
);

#[test]
fn a_keeper_records_a_restarted_app_without_a_readiness_probe_ready_after_its_start_grace() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(None, Duration::from_millis(20), Resolve::Never));
    let spec = fixture
        .spec(CRASH_ONCE, restart, Stop::default())
        .ready_after(Some(Duration::from_secs(1)));
    let _running = fixture.start(&spec).unwrap();

    eventually("the restart", || {
        matches!(fixture.recorded(), Some((EntryState::Running, _, 1)))
    });
    let restarted = now();
    assert_eq!(fixture.ready(), None);

    eventually("the readiness", || fixture.ready().is_some());

    assert!(now() >= restarted + Duration::from_millis(500));
    assert_eq!(fixture.ready().unwrap().restarts, 1);
}

#[test]
fn a_keeper_records_no_readiness_of_an_app_whose_stop_comes_within_its_start_grace() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(None, Duration::from_millis(20), Resolve::Never));
    let spec = fixture
        .spec(CRASH_ONCE, restart, Stop::default())
        .ready_after(Some(Duration::from_secs(5)));
    let mut running = fixture.start(&spec).unwrap();
    eventually("the restart", || {
        matches!(fixture.recorded(), Some((EntryState::Running, _, 1)))
    });

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    assert_eq!(fixture.ready(), None);
}

#[test]
fn a_keeper_spec_whose_start_grace_is_too_long_for_a_deadline_does_not_panic() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(None, Duration::from_millis(20), Resolve::Never));
    let mut spec = fixture.spec(CRASH_ONCE, restart, Stop::default());
    spec.ready_after_ms = Some(u64::MAX);
    let mut running = fixture.start(&spec).unwrap();
    eventually("the restart", || {
        matches!(fixture.recorded(), Some((EntryState::Running, _, 1)))
    });

    running.signal_keeper(Signal::TERM);

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Stopped.code()));
    assert_eq!(fixture.ready(), None);
}

#[test]
fn a_keeper_records_no_readiness_of_an_app_that_ends_within_its_start_grace() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(Some(2), Duration::from_millis(20), Resolve::Never));
    let spec = fixture
        .spec("sleep 0.1; exit 3", restart, Stop::default())
        .ready_after(Some(Duration::from_millis(500)));
    let mut running = fixture.start(&spec).unwrap();

    assert_eq!(running.exit_code(), i32::from(KeeperExit::Failed.code()));
    assert_eq!(fixture.recorded().map(|(_, _, restarts)| restarts), Some(2));
    assert_eq!(fixture.ready(), None);
}

#[test]
fn a_keeper_leaves_an_app_with_a_readiness_probe_unready_when_it_restarts_it() {
    let fixture = Fixture::new();
    let restart = Restart::on_failure(limits(None, Duration::from_millis(20), Resolve::Never));
    let spec = fixture.spec(CRASH_ONCE, restart, Stop::default());
    let _running = fixture.start(&spec).unwrap();

    eventually("the restart", || {
        matches!(fixture.recorded(), Some((EntryState::Running, _, 1)))
    });

    assert_eq!(fixture.ready(), None);
}

#[test]
fn the_engine_reads_from_the_keeper_how_its_app_ended_after_the_handshake() {
    for (script, exit) in [
        ("sleep 0.2; exit 3", Exit::Code(3)),
        ("sleep 0.2; kill -9 $$", Exit::Signal(9)),
    ] {
        let fixture = Fixture::new();
        let spec = fixture.spec(script, Restart::No, Stop::default());
        let mut running = fixture.start(&spec).unwrap();
        let end = std::mem::take(&mut running.kept.end);

        assert_eq!(
            end.wait(now() + PATIENCE, &Cancel::new()),
            Some(exit),
            "{script}"
        );
    }
}

#[test]
fn the_engine_reads_no_end_of_an_app_that_still_runs_once_its_wait_passes() {
    let fixture = Fixture::new();
    let spec = fixture.spec(spin!(), Restart::No, Stop::default());
    let mut running = fixture.start(&spec).unwrap();
    let end = std::mem::take(&mut running.kept.end);

    assert_eq!(
        end.wait(now() + Duration::from_millis(100), &Cancel::new()),
        None
    );
}

#[test]
fn the_engine_stops_waiting_for_the_end_line_once_its_operation_is_cancelled() {
    let fixture = Fixture::new();
    let spec = fixture.spec(spin!(), Restart::No, Stop::default());
    let mut running = fixture.start(&spec).unwrap();
    let end = std::mem::take(&mut running.kept.end);
    let cancel = Cancel::new();
    cancel.cancel();
    let started = Instant::now();

    assert_eq!(end.wait(now() + PATIENCE, &cancel), None);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_keeper_records_and_restarts_an_app_that_crashes_after_the_engine_stopped_reading() {
    // `default-sigpipe` runs the keeper with `SIGPIPE` at its default, as a
    // program that links phase may leave it.
    for name in ["server", "default-sigpipe"] {
        let fixture = Fixture::of(name);
        let restart = Restart::on_failure(limits(None, Duration::from_millis(20), Resolve::Never));
        let spec = fixture.spec(
            concat!(
                "if [ -e crashed ]; then ",
                spin!(),
                "; exit 0; fi; sleep 0.3; touch crashed; exit 3"
            ),
            restart,
            Stop::default(),
        );
        let mut running = fixture.start(&spec).unwrap();
        drop(std::mem::take(&mut running.kept.end));

        eventually("the restart after the crash", || {
            matches!(fixture.recorded(), Some((EntryState::Running, _, 1)))
        });
        assert_eq!(
            fixture.ended(),
            Some(Ended::by_itself(Some(Exit::Code(3)))),
            "{name}"
        );
        assert!(running.kept.exited().is_none(), "{name}");
    }
}

#[test]
fn an_app_receives_sigpipe_at_its_default_disposition() {
    let fixture = Fixture::new();
    let spec = fixture.spec(
        concat!(
            "(yes; echo \"yes ended with $?\" >&2) | head -n 1 >/dev/null; ",
            spin!()
        ),
        Restart::No,
        Stop::default(),
    );
    let _running = fixture.start(&spec).unwrap();

    // A shell reports a child that a signal ended as 128 plus the signal.
    eventually("the end of yes", || {
        fixture.log_text().contains("yes ended")
    });
    assert_eq!(fixture.log_text().trim(), "yes ended with 141");
}

#[test]
fn a_keepers_end_and_stop_skip_an_entry_that_another_operation_already_stopped() {
    let host = HostProcess {
        pid: 4242,
        started_at_us: 1,
        group: 4242,
        boot_id: None,
    };
    let ended = Ended::by_itself(Some(Exit::Code(3)));
    for note in [
        Note::Ending(host.clone(), ended),
        Note::Stopped(host.clone(), ended),
    ] {
        assert!(note.applies_to(Some(&host), EntryState::Stopping));
        assert!(!note.applies_to(Some(&host), EntryState::Stopped));
    }
}

#[test]
fn a_keeper_whose_fresh_environment_lacks_a_required_variable_exits_unresolved() {
    let fixture = Fixture::of("missing-required");
    let restart = Restart::on_failure(limits(Some(2), Duration::from_millis(10), Resolve::Always));
    let spec = fixture
        .spec("echo \"run $GREETING\"; exit 1", restart, Stop::default())
        .requiring(BTreeSet::from(["GREETING".to_owned()]));
    let mut running = fixture.start(&spec).unwrap();

    let code = running.exit_code();

    assert_eq!(code, i32::from(KeeperExit::EnvironmentUnresolved.code()));
    assert_eq!(fixture.log_text(), "run hello\n");
    let diagnostics = fixture.diagnostics_text();
    assert_eq!(
        diagnostics
            .matches("the environment source could not supply the variable GREETING")
            .count(),
        2,
        "{diagnostics}"
    );
    assert!(!diagnostics.contains("hello"));
}
