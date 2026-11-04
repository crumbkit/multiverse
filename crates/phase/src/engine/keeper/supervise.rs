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
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::process::Signal;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::keeper::logfile::{Diagnostics, diagnostic_log, open_append};
use crate::engine::keeper::policy::{
    AfterExit, after_exit, backoff_delay, needs_environment, streak_after_run,
};
use crate::engine::keeper::recorder::Recorder;
use crate::engine::keeper::sandbox::apply_sandbox;
use crate::engine::keeper::spec::{Handshake, KeeperSpec, read_spec, write_handshake};
use crate::engine::keeper::{EXIT_RECORD_WAIT, KeeperArgs, KeeperExit, RESOLVE_WAIT};
use crate::env::{AppliedEnvironment, EnvironmentSource};
use crate::plan::restart::StopSignal;
use crate::record::process::{Processes, Start, System};
use crate::record::run::HostProcess;

/// What the keeper waits for.
#[derive(Debug)]
enum Event {
    /// The app's process ended, with this status when it could be read.
    Ended(Option<ExitStatus>),
    /// The keeper received `TERM` or `INT`.
    Stop,
}

/// Runs the keeper in a session of its own, as
/// [`crate::engine::keeper::run`] says.
pub(crate) fn run(
    args: &KeeperArgs,
    source: &dyn EnvironmentSource,
    input: impl Read,
    output: impl Write,
) -> KeeperExit {
    // A new session leaves the group and the terminal of the engine that
    // started the keeper, so a hangup or Ctrl-C there never reaches it.
    // The engine starts the keeper as no group's leader, so it succeeds;
    // a keeper started by hand from a shell already leads its own group
    // and keeps the shell's session.
    let session = rustix::process::setsid();
    keep(args, source, input, output, session.is_ok())
}

/// Runs the keeper in the session it has, which a test can call within its
/// own process. `own_session` says whether the keeper leads a new session.
pub(crate) fn keep(
    args: &KeeperArgs,
    source: &dyn EnvironmentSource,
    input: impl Read,
    output: impl Write,
    own_session: bool,
) -> KeeperExit {
    let spec = match read_spec(input) {
        Ok(spec) => spec,
        Err(reason) => {
            write_handshake(output, &Handshake::Refused(reason));
            return KeeperExit::BadInput;
        }
    };
    let diagnostics = Diagnostics::open(&diagnostic_log(&spec.log, args.app(), args.process()));
    diagnostics.note(&format!(
        "keeper {} started for {}/{} in {}",
        std::process::id(),
        args.app(),
        args.process(),
        if own_session {
            "a session of its own"
        } else {
            "its caller's session"
        }
    ));
    let exit = start_and_supervise(args, source, &spec, &diagnostics, output);
    diagnostics.note(&format!("keeper exits with {exit:?} ({})", exit.code()));
    exit
}

/// Starts the signal thread, the app's log and the record writer, then
/// supervises the app.
fn start_and_supervise(
    args: &KeeperArgs,
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
    let recorder = Recorder::start(args, diagnostics.clone());
    let mut supervisor = Supervisor {
        args,
        source,
        spec,
        log,
        sender,
        events,
        recorder: &recorder,
        diagnostics,
    };
    let exit = supervisor.supervise(output);
    recorder.finish(EXIT_RECORD_WAIT);
    exit
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

/// How the wait before a restart ended.
enum Resumed {
    /// The keeper restarts the app with this environment, or with the one
    /// it holds when it is `None`.
    Restart(Option<Box<dyn AppliedEnvironment>>),
    /// The keeper leaves the app ended and exits so.
    Exit(KeeperExit),
}

/// One keeper's supervision of its app.
struct Supervisor<'a> {
    /// The keeper's arguments, which name the app and the process.
    args: &'a KeeperArgs,
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
    /// start, and supervises the app until the keeper exits.
    fn supervise(&mut self, output: impl Write) -> KeeperExit {
        let mut output = Some(output);
        let mut runs = Runs {
            restarts: self.spec.restarts,
            streak: 0,
            expires_at: self.spec.expires_at(),
            fresh: None,
        };
        loop {
            let app = match self.start(runs.fresh.as_deref()) {
                Ok(app) => app,
                Err(reason) => {
                    self.diagnostics.note(&reason);
                    if let Some(output) = output.take() {
                        write_handshake(output, &Handshake::Unstartable(reason));
                    }
                    self.recorder.stopped();
                    return KeeperExit::Unstartable;
                }
            };
            self.diagnostics
                .note(&format!("the app started as process {}", app.host.pid));
            if let Some(output) = output.take() {
                self.recorder.started(app.host.clone());
                write_handshake(output, &Handshake::Started(app.host.clone()));
            } else {
                self.recorder.restarted(app.host.clone(), runs.restarts);
            }
            let began = now();
            let status = match self.next_event() {
                Event::Stop => return self.stop(&app),
                Event::Ended(status) => status,
            };
            self.diagnostics.note(&match status {
                Some(status) => format!("the app ended with {status}"),
                None => "the app ended with an unreadable status".to_owned(),
            });
            self.recorder.stopping();
            if !self.end_group(&app, false) {
                return KeeperExit::Unstoppable;
            }
            self.recorder.stopped();
            let failed = !status.is_some_and(|status| status.success());
            match self.resume(&mut runs, failed, now().saturating_duration_since(began)) {
                Resumed::Restart(Some(fresh)) => runs.fresh = Some(fresh),
                Resumed::Restart(None) => {}
                Resumed::Exit(exit) => return exit,
            }
        }
    }

    /// Decides what follows a run of the app that lasted `ran` and
    /// `failed` or not: it waits each backoff, resolves a fresh
    /// environment when the policy asks for one, and tries again after a
    /// backoff when resolving fails, until the policy's most restarts.
    fn resume(&self, runs: &mut Runs, failed: bool, ran: Duration) -> Resumed {
        let mut failed = failed;
        let mut unresolved = false;
        let mut ran = Some(ran);
        loop {
            let (backoff, resolve) = match after_exit(&self.spec.restart, failed, runs.restarts) {
                AfterExit::Leave(_) if unresolved => {
                    return Resumed::Exit(KeeperExit::EnvironmentUnresolved);
                }
                AfterExit::Leave(exit) => return Resumed::Exit(exit),
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
                return Resumed::Exit(KeeperExit::Stopped);
            }
            if !needs_environment(resolve, SystemTime::now(), runs.expires_at) {
                return Resumed::Restart(None);
            }
            match self
                .source
                .resolve(self.args.app(), self.args.process(), now() + RESOLVE_WAIT)
            {
                Ok(fresh) => {
                    self.diagnostics
                        .note("the keeper resolved a fresh environment for the restart");
                    runs.expires_at = fresh.expires_at();
                    return Resumed::Restart(Some(fresh));
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
    /// With `fresh`, the app receives only the inherited variables and
    /// `fresh`. Otherwise it inherits the keeper's environment, which holds
    /// the same inherited variables and the environment resolved first.
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
            .process_group(0);
        if let Some(fresh) = fresh {
            command.env_clear().envs(&self.spec.inherited);
            fresh.apply(&mut command);
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

    /// Returns the next event. The supervisor holds a sender, so the
    /// channel never closes, and a closed channel reads as a stop request.
    fn next_event(&self) -> Event {
        self.events.recv().unwrap_or(Event::Stop)
    }

    /// Stops the running `app` after a stop request.
    fn stop(&self, app: &App) -> KeeperExit {
        self.diagnostics
            .note("a stop request came, and the keeper stops the app");
        self.recorder.stopping();
        if !self.end_group(app, true) {
            self.diagnostics
                .note("the app's group was still there after KILL");
            return KeeperExit::Unstoppable;
        }
        self.recorder.stopped();
        KeeperExit::Stopped
    }

    /// Ends `app`'s group: it sends the stop signal and waits the grace,
    /// then sends `KILL` and waits the kill wait, and reports whether the
    /// group is gone. `running` says whether the app's leader is still
    /// running. A group with no member left receives no signal.
    fn end_group(&self, app: &App, running: bool) -> bool {
        let mut running = running;
        let steps = [
            (stop_signal(self.spec.stop.signal), self.spec.stop.grace),
            (Signal::KILL, self.spec.kill_wait),
        ];
        for (signal, wait) in steps {
            if !running && !System.group_exists(app.host.group) {
                return true;
            }
            let _ = System.signal(&app.host, signal);
            let deadline = now() + wait;
            if running {
                running = !self.ended_by(deadline);
            }
            if !running && group_gone_by(app.host.group, deadline) {
                return true;
            }
        }
        false
    }

    /// Waits until the app's end arrives or `deadline` passes, and reports
    /// whether it arrived. A stop request meanwhile changes nothing.
    fn ended_by(&self, deadline: Instant) -> bool {
        loop {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(now()))
            {
                Ok(Event::Ended(_)) => return true,
                Ok(Event::Stop) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
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
                Ok(Event::Ended(_)) => {}
                Err(RecvTimeoutError::Timeout) => return false,
                Err(RecvTimeoutError::Disconnected) => return true,
            }
        }
    }
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

/// Returns the signal that `stop` names.
const fn stop_signal(stop: StopSignal) -> Signal {
    match stop {
        StopSignal::Term => Signal::TERM,
        StopSignal::Int => Signal::INT,
        StopSignal::Quit => Signal::QUIT,
        StopSignal::Hup => Signal::HUP,
        StopSignal::Usr1 => Signal::USR1,
        StopSignal::Usr2 => Signal::USR2,
    }
}
