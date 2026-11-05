//! The keeper's supervision of its app: it starts the app, waits for it to
//! end or for a stop request, stops its group, and restarts it as the
//! policy says, resolving a fresh environment when the policy asks for one.
//!
//! Every event that the keeper waits for arrives on one channel: the app's
//! end, from a thread that waits for the child, and a stop request, from a
//! thread that receives the keeper's `TERM` and `INT`. Only the app's group
//! after its leader has ended gives no event, so the keeper polls it.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::Signal;
use signal_hook::consts::{SIGINT, SIGPIPE, SIGTERM};
use signal_hook::iterator::Signals;

use crate::clock::{POLL_INTERVAL, now, pause, wall_now};
use crate::engine::keeper::logfile::{Diagnostics, diagnostic_log, open_append};
use crate::engine::keeper::recorder::{Hurry, RECORD_LOCK_WAIT, Recorder};
use crate::engine::keeper::sandbox::apply_sandbox;
use crate::engine::keeper::spec::{
    Handshake, KeeperSpec, StartedApp, read_spec, write_end, write_handshake,
};
use crate::engine::keeper::{KeeperArgs, KeeperEnvironment, KeeperExit, RESOLVE_WAIT};
use crate::engine::restart::{
    AfterExit, after_exit, backoff_delay, needs_environment, streak_after_run,
};
use crate::env::{AppliedEnvironment, EnvironmentSource};
use crate::plan::restart::StopSignal;
use crate::record::process::{Processes, Start, System};
use crate::record::run::{Ended, Exit, HostProcess, Ready};

/// What the keeper waits for.
#[derive(Debug)]
enum Event {
    /// The app's process ended, with this status when it could be read.
    Ended(Option<ExitStatus>),
    /// The keeper received `TERM` or `INT`.
    Stop,
    /// The record writer wrote every change and ended.
    Recorded,
}

/// Runs the keeper in a session of its own, as
/// [`crate::engine::keeper::run`] says.
pub(crate) fn run(
    args: &KeeperArgs,
    environment: KeeperEnvironment,
    source: &dyn EnvironmentSource,
    input: impl Read,
    output: impl Write,
) -> KeeperExit {
    // A new session leaves the group and the terminal of the engine that
    // started the keeper, so a hangup or Ctrl-C there never reaches it.
    // The engine starts the keeper as no group's leader, so it succeeds.
    // A keeper that already leads a group, such as one started by hand
    // from a shell, cannot lead a new session, and refuses to run.
    if let Err(error) = rustix::process::setsid() {
        write_handshake(
            output,
            &Handshake::Refused(format!(
                "the keeper cannot start a session of its own: {error}"
            )),
        );
        return KeeperExit::BadInput;
    }
    // The engine closes its end of the keeper's standard output once it
    // stops reading, and the end line may come after that. With `SIGPIPE`
    // at its default, that write would end the keeper. A handler of its
    // own makes the write fail with `EPIPE` instead, which the keeper
    // ignores, and unlike an ignored signal it reverts to the default in
    // the app when the app's program starts. signal-hook chains a SIGPIPE
    // handler that the program linking phase installed earlier, so that
    // handler still runs, but the program should not rely on it to change
    // what a write to a closed pipe does in the keeper.
    if let Err(error) = signal_hook::flag::register(SIGPIPE, Arc::new(AtomicBool::new(false))) {
        write_handshake(
            output,
            &Handshake::Unstartable(format!("the keeper cannot handle SIGPIPE: {error}")),
        );
        return KeeperExit::Unstartable;
    }
    keep(args, environment, source, input, output)
}

/// Runs the keeper in the session it has, which a test can call within its
/// own process.
pub(crate) fn keep(
    args: &KeeperArgs,
    mut environment: KeeperEnvironment,
    source: &dyn EnvironmentSource,
    input: impl Read,
    output: impl Write,
) -> KeeperExit {
    let spec = match read_spec(input) {
        Ok(spec) => spec,
        Err(reason) => {
            write_handshake(output, &Handshake::Refused(reason));
            return KeeperExit::BadInput;
        }
    };
    environment.retain(&spec.resolved);
    let diagnostics = Diagnostics::open(&diagnostic_log(&spec.log, args.app(), args.process()));
    diagnostics.note(&format!(
        "keeper {} started for {}/{} in group {}",
        std::process::id(),
        args.app(),
        args.process(),
        keeper_group()
    ));
    let exit = start_and_supervise(args, &environment, source, &spec, &diagnostics, output);
    diagnostics.note(&format!("keeper exits with {exit:?} ({})", exit.code()));
    exit
}

/// Starts the signal thread, the app's log and the record writer, then
/// supervises the app.
fn start_and_supervise(
    args: &KeeperArgs,
    environment: &KeeperEnvironment,
    source: &dyn EnvironmentSource,
    spec: &KeeperSpec,
    diagnostics: &Diagnostics,
    output: impl Write,
) -> KeeperExit {
    let (sender, events) = mpsc::channel();
    if let Err(error) = watch_signals(sender.clone()) {
        let reason = format!("the keeper cannot receive signals: {error}");
        diagnostics.note(&reason);
        write_handshake(output, &Handshake::Unstartable(reason));
        return KeeperExit::Unstartable;
    }
    let log = match open_append(&spec.log) {
        Ok(log) => log,
        Err(error) => {
            let reason = format!("the log {} cannot be opened: {error}", spec.log.display());
            diagnostics.note(&reason);
            write_handshake(output, &Handshake::Unstartable(reason));
            return KeeperExit::Unstartable;
        }
    };
    let recorded = sender.clone();
    let recorder = Recorder::start(args, diagnostics.clone(), move || {
        let _ = recorded.send(Event::Recorded);
    });
    let mut supervisor = Supervisor {
        args,
        environment,
        source,
        spec,
        log,
        sender,
        events,
        recorder: &recorder,
        diagnostics,
    };
    let exit = supervisor.supervise(output);
    let Supervisor { events, .. } = supervisor;
    let hurry = recorder.close();
    await_records(&events, exit, &hurry, diagnostics);
    exit
}

/// Waits for the record writer to write its last changes. A stop request
/// comes from an operation such as `down`, which usually holds the
/// lifecycle lock and records the stop itself. So after one the keeper
/// tells the writer to hurry: the writer tries the lock once more without
/// waiting, writes its changes when the lock is free, and drops them when
/// another operation holds it. After any other end nothing else records
/// how the app ended, so the keeper waits as long as the writer waits for
/// the lock, unless a stop request comes meanwhile.
fn await_records(
    events: &Receiver<Event>,
    exit: KeeperExit,
    hurry: &Hurry,
    diagnostics: &Diagnostics,
) {
    diagnostics.note("the keeper waits for its last record writes");
    if exit == KeeperExit::Stopped {
        hurry.now();
    }
    let deadline = now() + RECORD_LOCK_WAIT;
    loop {
        match events.recv_timeout(deadline.saturating_duration_since(now())) {
            Ok(Event::Recorded) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Event::Stop) => hurry.now(),
            Ok(Event::Ended(_)) => {}
            Err(RecvTimeoutError::Timeout) => {
                diagnostics.note("the keeper exits before its last record writes");
                return;
            }
        }
    }
}

/// Returns the keeper's process group.
fn keeper_group() -> u32 {
    rustix::process::getpgrp()
        .as_raw_nonzero()
        .get()
        .unsigned_abs()
}

/// Starts the thread that turns the keeper's `TERM` and `INT` into stop
/// requests.
fn watch_signals(sender: Sender<Event>) -> std::io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    thread::spawn(move || {
        for _ in signals.forever() {
            if sender.send(Event::Stop).is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// A started app process.
#[derive(Debug)]
struct App {
    /// The process as the run record names it. It leads its own group.
    host: HostProcess,
}

/// One keeper's supervision of its app.
struct Supervisor<'a> {
    /// The keeper's arguments, which name the app and the process.
    args: &'a KeeperArgs,
    /// The resolved values that the engine gave the keeper.
    environment: &'a KeeperEnvironment,
    /// The source of a fresh environment.
    source: &'a dyn EnvironmentSource,
    /// The specification.
    spec: &'a KeeperSpec,
    /// The app's log, which each start of the app receives a copy of.
    log: File,
    /// The sender of events, which each app's waiter thread receives.
    sender: Sender<Event>,
    /// The events.
    events: Receiver<Event>,
    /// The writer of the run record.
    recorder: &'a Recorder,
    /// The keeper's diagnostic log.
    diagnostics: &'a Diagnostics,
}

/// What the supervisor tracks across restarts.
struct Runs {
    /// The restarts so far, counting earlier keepers', which count toward
    /// the policy's most restarts and which the run record shows.
    restarts: u32,
    /// The restarts in a row since the app last ran healthily, which set
    /// the backoff.
    streak: u32,
    /// When a value of the environment that the app receives expires.
    expires_at: Option<SystemTime>,
    /// The freshly resolved environment, once the keeper has resolved one.
    fresh: Option<Box<dyn AppliedEnvironment>>,
}

impl Supervisor<'_> {
    /// Starts the app, writes the handshake to `output` after the first
    /// start and the end line once that start ends, and supervises the app
    /// until the keeper exits.
    fn supervise<W: Write>(&mut self, output: W) -> KeeperExit {
        let mut output = Some(output);
        // The output after the handshake, until the end line is written.
        let mut end_output: Option<W> = None;
        let mut runs = Runs {
            restarts: self.spec.restarts,
            streak: 0,
            expires_at: self.spec.expires_at(),
            fresh: None,
        };
        let mut previous: Option<HostProcess> = None;
        loop {
            if output.is_none() && self.stop_waiting() {
                self.diagnostics
                    .note("a stop request came before the restart");
                return KeeperExit::Stopped;
            }
            let app = match self.start(runs.fresh.as_deref()) {
                Ok(app) => app,
                Err(reason) => {
                    self.diagnostics.note(&reason);
                    if let Some(output) = output.take() {
                        write_handshake(output, &Handshake::Unstartable(reason));
                        return KeeperExit::Unstartable;
                    }
                    // A restart that cannot start is a failed run.
                    if let Some(exit) = self.resume(&mut runs, true, Duration::ZERO) {
                        return exit;
                    }
                    continue;
                }
            };
            self.diagnostics
                .note(&format!("the app started as process {}", app.host.pid));
            if let Some(mut output) = output.take() {
                self.recorder.started(app.host.clone());
                let started = StartedApp {
                    keeper_group: keeper_group(),
                    app: app.host.clone(),
                };
                write_handshake(&mut output, &Handshake::Started(started));
                end_output = Some(output);
            } else if let Some(previous) = previous.take() {
                // An app without a readiness probe is ready once it runs.
                let ready = self.spec.ready_on_start.then(|| Ready::now(runs.restarts));
                self.recorder
                    .restarted(app.host.clone(), previous, runs.restarts, ready);
            }
            previous = Some(app.host.clone());
            let began = now();
            let Some(status) = self.app_ended() else {
                return self.stop(&app, end_output.take());
            };
            self.diagnostics.note(&match status {
                Some(status) => format!("the app ended with {status}"),
                None => "the app ended with an unreadable status".to_owned(),
            });
            let ended = Ended::by_itself(status.and_then(Exit::of));
            if let Some(output) = end_output.take() {
                write_end(output, ended);
            }
            self.recorder.ending(app.host.clone(), ended);
            if !self.end_group(&app, false).gone {
                self.diagnostics
                    .note("the app's group was still there after KILL");
                return KeeperExit::Unstoppable;
            }
            self.recorder.stopped(app.host.clone(), ended);
            // The restart policy's failure is any end but status 0, which
            // includes a signal and a status that could not be read.
            let failed = ended.exit != Some(Exit::Code(0));
            if let Some(exit) =
                self.resume(&mut runs, failed, now().saturating_duration_since(began))
            {
                return exit;
            }
        }
    }

    /// Decides what follows a run of the app that lasted `ran` and
    /// `failed` or not: it waits each backoff, resolves a fresh
    /// environment when the policy asks for one, and tries again after a
    /// backoff when resolving fails, until the policy's most restarts. It
    /// returns how the keeper exits, or `None` when it restarts the app,
    /// and then holds any fresh environment in `runs`.
    fn resume(&self, runs: &mut Runs, failed: bool, ran: Duration) -> Option<KeeperExit> {
        let mut failed = failed;
        let mut unresolved = false;
        let mut ran = Some(ran);
        loop {
            let (backoff, resolve) = match after_exit(&self.spec.restart, failed, runs.restarts) {
                AfterExit::Leave(_) if unresolved => {
                    return Some(KeeperExit::EnvironmentUnresolved);
                }
                AfterExit::Leave(ended) => return Some(ended.into()),
                AfterExit::Restart { backoff, resolve } => (backoff, resolve),
            };
            if let Some(ran) = ran.take() {
                runs.streak = streak_after_run(runs.streak, ran, backoff);
            }
            let delay = backoff_delay(backoff, runs.streak);
            runs.restarts = runs.restarts.saturating_add(1);
            runs.streak = runs.streak.saturating_add(1);
            self.diagnostics.note(&format!(
                "restart {} follows in {} ms",
                runs.restarts,
                delay.as_millis()
            ));
            if self.stop_requested_within(now() + delay) {
                self.diagnostics
                    .note("a stop request came during the backoff");
                return Some(KeeperExit::Stopped);
            }
            if !needs_environment(resolve, wall_now(), runs.expires_at) {
                return None;
            }
            self.diagnostics
                .note("the keeper resolves a fresh environment");
            let resolved = self
                .source
                .resolve(self.args.app(), self.args.process(), now() + RESOLVE_WAIT)
                .and_then(|fresh| {
                    crate::env::require(&self.spec.required, fresh.as_ref()).map(|()| fresh)
                });
            // Resolving can take long, and a stop request meanwhile waits
            // on the channel until the keeper looks.
            if self.stop_waiting() {
                self.diagnostics
                    .note("a stop request came while the keeper resolved the environment");
                return Some(KeeperExit::Stopped);
            }
            match resolved {
                Ok(fresh) => {
                    self.diagnostics
                        .note("the keeper resolved a fresh environment for the restart");
                    runs.expires_at = fresh.expires_at();
                    runs.fresh = Some(fresh);
                    return None;
                }
                Err(error) => {
                    self.diagnostics.note(&format!(
                        "the keeper could not resolve the environment: {error}"
                    ));
                    failed = true;
                    unresolved = true;
                }
            }
        }
    }

    /// Starts the app in a group of its own, with the log as its standard
    /// output and standard error, and a thread that waits for it to end.
    /// The app receives only the inherited variables and either `fresh` or
    /// the environment resolved first, which the keeper holds as its own.
    fn start(&self, fresh: Option<&dyn AppliedEnvironment>) -> Result<App, String> {
        let (program, arguments) = self
            .spec
            .run
            .split_first()
            .ok_or_else(|| "the specification's command is empty".to_owned())?;
        let unopened = |error: std::io::Error| format!("the log cannot be shared: {error}");
        let stdout = self.log.try_clone().map_err(unopened)?;
        let stderr = self.log.try_clone().map_err(unopened)?;
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/engine/keeper/supervise.rs starts the keeper's app process (PROC-1)"
        )]
        let mut command = Command::new(program);
        command
            .args(arguments)
            .current_dir(&self.spec.cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0)
            .env_clear()
            .envs(&self.spec.inherited);
        match fresh {
            Some(fresh) => fresh.apply(&mut command),
            None => self
                .environment
                .apply_named(&self.spec.resolved, &mut command),
        }
        apply_sandbox(&mut command);
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/phase/src/engine/keeper/supervise.rs starts the keeper's app process (PROC-1)"
        )]
        let spawned = command.spawn();
        let mut child = spawned.map_err(|error| format!("{program} cannot start: {error}"))?;
        let pid = child.id();
        // The child is not reaped yet, so its start time is readable even
        // when it has already exited.
        let started = match System.start_time(pid) {
            Ok(Start::At(started)) => Ok(started),
            Ok(_) => Err(format!("the start time of the process {pid} is unreadable")),
            Err(error) => Err(error.to_string()),
        };
        let boot_id = System.boot_id().map_err(|error| error.to_string());
        let (started_at_us, boot_id) = match (started, boot_id) {
            (Ok(started), Ok(boot_id)) => (started, boot_id),
            (Err(reason), _) | (_, Err(reason)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(reason);
            }
        };
        let sender = self.sender.clone();
        thread::spawn(move || {
            let status = child.wait().ok();
            let _ = sender.send(Event::Ended(status));
        });
        Ok(App {
            host: HostProcess {
                pid,
                started_at_us,
                group: pid,
                boot_id,
            },
        })
    }

    /// Waits for the running app to end and returns its status, which is
    /// `None` when it could not be read, or returns `None` when a stop
    /// request comes first. The supervisor holds a sender, so the channel
    /// never closes, and a closed channel reads as a stop request.
    fn app_ended(&self) -> Option<Option<ExitStatus>> {
        loop {
            match self.events.recv() {
                Ok(Event::Ended(status)) => return Some(status),
                Ok(Event::Recorded) => {}
                Ok(Event::Stop) | Err(_) => return None,
            }
        }
    }

    /// Stops the running `app` after a stop request, and writes how it
    /// ended to `end_output` when the end line is still owed.
    fn stop(&self, app: &App, end_output: Option<impl Write>) -> KeeperExit {
        self.diagnostics
            .note("a stop request came, and the keeper stops the app");
        self.recorder.stopping(app.host.clone());
        let group = self.end_group(app, true);
        let ended = Ended::requested(group.status.and_then(Exit::of));
        if let Some(output) = end_output {
            write_end(output, ended);
        }
        if !group.gone {
            self.diagnostics
                .note("the app's group was still there after KILL");
            return KeeperExit::Unstoppable;
        }
        self.recorder.stopped(app.host.clone(), ended);
        KeeperExit::Stopped
    }

    /// Ends `app`'s group: it sends the stop signal and waits the grace,
    /// then sends `KILL` and waits the kill wait, and reports whether the
    /// group is gone and the status of a leader that ended meanwhile.
    /// `running` says whether the app's leader is still running. A group
    /// with no member left receives no signal.
    fn end_group(&self, app: &App, running: bool) -> GroupEnd {
        let mut running = running;
        let mut status = None;
        let steps = [
            (
                StopSignal::signal(self.spec.stop.signal),
                self.spec.stop.grace,
            ),
            (Signal::KILL, self.spec.kill_wait),
        ];
        for (signal, wait) in steps {
            if !running && !System.group_exists(app.host.group) {
                return GroupEnd { gone: true, status };
            }
            let _ = System.signal(&app.host, signal);
            let deadline = now() + wait;
            if running && let Some(ended) = self.ended_by(deadline) {
                running = false;
                status = ended;
            }
            if !running && group_gone_by(app.host.group, deadline) {
                return GroupEnd { gone: true, status };
            }
        }
        GroupEnd {
            gone: false,
            status,
        }
    }

    /// Waits until the app's end arrives or `deadline` passes, and returns
    /// the app's status, which is `None` when it could not be read, or
    /// returns `None` when the end did not arrive. A stop request
    /// meanwhile changes nothing.
    fn ended_by(&self, deadline: Instant) -> Option<Option<ExitStatus>> {
        loop {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(now()))
            {
                Ok(Event::Ended(status)) => return Some(status),
                Ok(Event::Stop | Event::Recorded) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Reports whether a stop request is waiting, while no app runs,
    /// without waiting for one.
    fn stop_waiting(&self) -> bool {
        loop {
            match self.events.try_recv() {
                Ok(Event::Stop) | Err(mpsc::TryRecvError::Disconnected) => return true,
                Ok(Event::Ended(_) | Event::Recorded) => {}
                Err(mpsc::TryRecvError::Empty) => return false,
            }
        }
    }

    /// Waits until `deadline` for a stop request, while no app runs, and
    /// reports whether one arrived.
    fn stop_requested_within(&self, deadline: Instant) -> bool {
        loop {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(now()))
            {
                Ok(Event::Stop) => return true,
                Ok(Event::Ended(_) | Event::Recorded) => {}
                Err(RecvTimeoutError::Timeout) => return false,
                Err(RecvTimeoutError::Disconnected) => return true,
            }
        }
    }
}

/// What ending an app's group found.
struct GroupEnd {
    /// Whether the group is gone.
    gone: bool,
    /// The status of the app's leader when it ended while the keeper ended
    /// the group, which is `None` when it could not be read or the leader
    /// had already ended.
    status: Option<ExitStatus>,
}

/// Polls until the group `group` has no member or `deadline` passes, and
/// reports whether it went. A group whose members phase did not start
/// gives no event when they end, so the keeper polls it (BASE-11).
fn group_gone_by(group: u32, deadline: Instant) -> bool {
    loop {
        if !System.group_exists(group) {
            return true;
        }
        if now() >= deadline {
            return false;
        }
        pause(POLL_INTERVAL);
    }
}
