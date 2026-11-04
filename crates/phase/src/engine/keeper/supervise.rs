//! The keeper's supervision of its app: it starts the app, waits for it to
//! end or for a stop request, stops its group, and restarts it as the
//! policy says.
//!
//! Every event that the keeper waits for arrives on one channel: the app's
//! end, from a thread that waits for the child, and a stop request, from a
//! thread that receives the keeper's `TERM` and `INT`. Only the app's group
//! after its leader has ended gives no event, so the keeper polls it.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Instant, SystemTime};

use rustix::process::Signal;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::clock::{POLL_INTERVAL, now, pause};
use crate::engine::keeper::policy::{AfterExit, after_exit, needs_environment};
use crate::engine::keeper::recorder::Recorder;
use crate::engine::keeper::sandbox::apply_sandbox;
use crate::engine::keeper::spec::{Handshake, KeeperSpec, read_spec, write_handshake};
use crate::engine::keeper::{EXIT_RECORD_WAIT, KeeperArgs, KeeperExit};
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

/// Runs the keeper, as [`crate::engine::keeper::run`] says.
pub(crate) fn run(args: &KeeperArgs, input: impl Read, output: impl Write) -> KeeperExit {
    let spec = match read_spec(input) {
        Ok(spec) => spec,
        Err(reason) => {
            write_handshake(output, &Handshake::Refused(reason));
            return KeeperExit::BadInput;
        }
    };
    let (sender, events) = mpsc::channel();
    if let Err(error) = watch_signals(sender.clone()) {
        let reason = format!("the keeper cannot receive signals: {error}");
        write_handshake(output, &Handshake::Unstartable(reason));
        return KeeperExit::Unstartable;
    }
    let log = match open_log(&spec) {
        Ok(log) => log,
        Err(reason) => {
            write_handshake(output, &Handshake::Unstartable(reason));
            return KeeperExit::Unstartable;
        }
    };
    let recorder = Recorder::start(args);
    let mut supervisor = Supervisor {
        spec: &spec,
        log,
        sender,
        events,
        recorder: &recorder,
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

/// Opens the app's log for appending, creating it with mode 0600 and its
/// directory with mode 0700 when they do not exist. It refuses a log that
/// is a symbolic link.
fn open_log(spec: &KeeperSpec) -> Result<File, String> {
    let failed =
        |error: std::io::Error| format!("the log {} cannot be opened: {error}", spec.log.display());
    if let Some(dir) = spec.log.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(failed)?;
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/engine/keeper/supervise.rs opens the app's log, which the app appends to and phase never rewrites"
    )]
    let opened = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).unwrap_or(0))
        .open(&spec.log);
    opened.map_err(failed)
}

/// A started app process.
#[derive(Debug)]
struct App {
    /// The process as the run record names it. It leads its own group.
    host: HostProcess,
}

/// One keeper's supervision of its app.
struct Supervisor<'a> {
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
}

impl Supervisor<'_> {
    /// Starts the app, writes the handshake to `output` after the first
    /// start, and supervises the app until the keeper exits.
    fn supervise(&mut self, output: impl Write) -> KeeperExit {
        let mut output = Some(output);
        let mut restarts = self.spec.restarts;
        loop {
            let app = match self.start() {
                Ok(app) => app,
                Err(reason) => {
                    if let Some(output) = output.take() {
                        write_handshake(output, &Handshake::Unstartable(reason));
                    }
                    self.recorder.stopped();
                    return KeeperExit::Unstartable;
                }
            };
            self.recorder.started(app.host.clone());
            if let Some(output) = output.take() {
                write_handshake(output, &Handshake::Started(app.host.clone()));
            }
            let status = match self.next_event() {
                Event::Stop => return self.stop(&app),
                Event::Ended(status) => status,
            };
            self.recorder.stopping();
            if !self.end_group(&app, false) {
                return KeeperExit::Unstoppable;
            }
            self.recorder.stopped();
            let failed = !status.is_some_and(|status| status.success());
            match after_exit(&self.spec.restart, failed, restarts) {
                AfterExit::Leave(exit) => return exit,
                AfterExit::Restart { delay, resolve } => {
                    if self.stop_requested_within(now() + delay) {
                        return KeeperExit::Stopped;
                    }
                    if needs_environment(resolve, SystemTime::now(), self.spec.expires_at()) {
                        return KeeperExit::NeedsEnvironment;
                    }
                    restarts = restarts.saturating_add(1);
                }
            }
        }
    }

    /// Starts the app in a group of its own, with the log as its standard
    /// output and standard error, and a thread that waits for it to end.
    fn start(&self) -> Result<App, String> {
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
        self.recorder.stopping();
        if !self.end_group(app, true) {
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
