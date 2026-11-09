//! The place on the machine that `mxv up` and `mxv restart` wait for before
//! they run a process's `build`: the `deadband` library's admission call,
//! behind the library's [`Admission`] trait.
//!
//! [`Deadband::admit`] installs mxv's signal handling only after the call
//! returns. The call cannot be cancelled, so until it returns Ctrl-C ends
//! mxv as it does for any command, and the closed connection takes the job
//! out of the daemon's queue. While the builds run, Ctrl-C, `SIGHUP` and
//! `SIGQUIT` set the cancel flag, phase ends the running build's process
//! group, and mxv exits with 128 plus the signal. Once the place ends those
//! signals end mxv as they did before.
//!
//! `up` and `restart` never lower mxv's priority with
//! `Admitted::lower_priority`, because the lowering lasts for the calling
//! process's life and passes to the servers that they go on to start. So a
//! build in the normal lane runs at normal priority. A helper process that
//! holds the place and runs the build would let the build alone be lowered.
//!
//! `mxv run` does lower its priority, through [`Deadband::for_task`],
//! because that process ends with its task. A task with `gate = false` asks
//! the daemon nothing, and keeps the same handling of signals.
//!
//! When mxv itself runs inside a job that deadband granted, deadband's call
//! asks nothing and returns no variables. phase starts a build from an empty
//! environment, so the build then has neither the outer job's thread counts
//! nor `DEADBAND_HELD`, and it can use every core under the outer job's one
//! place. Passing those variables on from mxv's own environment would replace
//! this.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use deadband::admission::{Admitted, Asked as DeadbandAsked, admit};
use deadband::environment::Environment as DeadbandEnvironment;
use multiverse::phase::{Cancel, VariableName};
use multiverse::{Admission, Asked, Place};

use crate::environment::Environment;

#[cfg(test)]
mod tests;

/// The most bytes of command text that mxv sends the daemon. deadband's
/// `admit` has no deadline on sending its request, so a daemon that has
/// stopped reading blocks a request longer than the socket's send buffer,
/// which is about 8 KiB on macOS. The cut shortens only what `deadband
/// status` shows.
const MOST_COMMAND_BYTES: usize = 4096;

/// The variable that makes mise run one task at a time, so a `mise run` in
/// a build cannot run several tasks in parallel under one place.
const MISE_JOBS: &str = "MISE_JOBS";

/// The signal number that means no signal has been caught.
const NONE_CAUGHT: i32 = 0;

/// Waits for a place through the `deadband` library.
pub(crate) struct Deadband {
    /// The variables of mxv's environment that deadband reads.
    variables: BTreeMap<String, OsString>,
    /// The number of the signal that interrupted the builds, or
    /// [`NONE_CAUGHT`].
    caught: Arc<AtomicI32>,
    /// Whether mxv asks the daemon for a place.
    asks: bool,
    /// Whether mxv lowers its own priority once it has a place.
    lowers: bool,
}

impl Deadband {
    /// Returns the admission that reads deadband's variables from
    /// `environment`.
    pub(crate) fn new(environment: &Environment) -> Self {
        Self {
            variables: environment.deadband_variables().clone(),
            caught: Arc::new(AtomicI32::new(NONE_CAUGHT)),
            asks: true,
            lowers: false,
        }
    }

    /// Returns the admission of one `mxv run` task, which lowers mxv's
    /// priority once it has a place. When `gate` is false it asks the daemon
    /// nothing, so the task gets no job variables.
    pub(crate) fn for_task(environment: &Environment, gate: bool) -> Self {
        Self {
            asks: gate,
            lowers: true,
            ..Self::new(environment)
        }
    }

    /// Returns the signal that interrupted the builds, or `None`.
    pub(crate) fn caught(&self) -> Option<i32> {
        match self.caught.load(Ordering::SeqCst) {
            NONE_CAUGHT => None,
            signal => Some(signal),
        }
    }
}

impl Admission for Deadband {
    fn admit(&mut self, asked: &Asked<'_>) -> Box<dyn Place> {
        let admitted = self.asks.then(|| self.ask(asked));
        if self.lowers
            && let Some(admitted) = &admitted
        {
            admitted.lower_priority(&mut crate::notice);
        }
        let building = catch_interrupts(asked.cancel.clone(), Arc::clone(&self.caught));
        Box::new(Held { admitted, building })
    }

    fn output(&self) -> Box<dyn Write + Send> {
        crate::stderr_writer()
    }
}

impl Deadband {
    /// Asks the daemon for the place that `asked` describes.
    fn ask(&self, asked: &Asked<'_>) -> Admitted {
        let read = |name: &str| self.variables.get(name).cloned();
        let environment = DeadbandEnvironment::read(read, Some(PathBuf::from(asked.cwd)));
        let socket = deadband::socket::path(read);
        admit(
            &DeadbandAsked {
                socket: &socket,
                environment: &environment,
                cores: std::thread::available_parallelism().ok(),
                label: asked.label,
                command: cut(asked.command),
                lane: None,
            },
            &mut crate::notice,
        )
    }
}

/// A place that the daemon granted, or that it could not be asked for.
struct Held {
    /// The lease, or `None` for a task that asks the daemon nothing.
    admitted: Option<Admitted>,
    /// Whether the builds still run, which tells the signal thread that
    /// a signal cancels them. It is `None` when mxv could not catch the
    /// signals.
    building: Option<Arc<AtomicBool>>,
}

impl Place for Held {
    fn variables(&self) -> Vec<(VariableName, String)> {
        self.admitted
            .as_ref()
            .map_or_else(Vec::new, |admitted| job_variables(admitted.variables()))
    }

    fn end(self: Box<Self>, exit_code: Option<i32>) {
        let Self { admitted, building } = *self;
        if let Some(building) = building {
            // In this order a `SIGINT` always has a handler: the usage log's
            // thread ends mxv once it has the signal back, and until then
            // this module's thread cancels.
            crumbkit::events::taken_back();
            building.store(false, Ordering::SeqCst);
        }
        if let Some(admitted) = admitted {
            admitted.end(exit_code);
        }
    }
}

/// Returns `variables` as phase names them, with [`MISE_JOBS`] beside them.
fn job_variables(variables: &[(&'static str, String)]) -> Vec<(VariableName, String)> {
    variables
        .iter()
        .map(|(name, value)| (*name, value.clone()))
        .chain([(MISE_JOBS, "1".to_owned())])
        .filter_map(|(name, value)| Some((name.parse().ok()?, value)))
        .collect()
}

/// Returns the longest start of `command` that fits in
/// [`MOST_COMMAND_BYTES`] and ends on a character boundary.
fn cut(command: &str) -> &str {
    let mut end = command.len().min(MOST_COMMAND_BYTES);
    while !command.is_char_boundary(end) {
        end -= 1;
    }
    command.get(..end).unwrap_or_default()
}

/// Catches `SIGINT`, `SIGHUP` and `SIGQUIT` for the rest of mxv's run, and
/// tells crumbkit that a child holds the terminal, so the usage log's
/// signal thread stops ending mxv on `SIGINT`. Returns the flag that says
/// the builds run, or `None` when the handlers cannot be installed, which
/// leaves a signal ending mxv and the build running on.
///
/// While the flag is set, each signal stores its number in `caught` and
/// sets `cancel`. Once it is clear, a signal ends mxv as it would have
/// without this handler. The usage log's thread still does that for
/// `SIGINT`, so this thread leaves `SIGINT` to it.
///
/// The signals are those of [`interrupts`], so a `SIGHUP` or `SIGQUIT` that
/// mxv started out ignoring stays ignored and `nohup mxv up` keeps working.
///
/// `SIGTERM` is not caught here, because crumbkit's usage log ends mxv on it
/// whether or not a child was handed over, as it does for `mxv exec`. A
/// `SIGTERM` during a build therefore ends mxv and frees the place, while
/// the build's process group runs on, uncounted by deadband, and the next
/// `up` starts a second build. A `SIGTERM` rule in crumbkit's usage log like
/// its `SIGINT` rule would replace this.
fn catch_interrupts(cancel: Cancel, caught: Arc<AtomicI32>) -> Option<Arc<AtomicBool>> {
    use signal_hook::consts::SIGINT;
    use signal_hook::iterator::Signals;
    let mut signals = Signals::new(interrupts()).ok()?;
    let building = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&building);
    std::thread::Builder::new()
        .name("mxv-build-interrupts".to_owned())
        .spawn(move || {
            for signal in signals.forever() {
                if flag.load(Ordering::SeqCst) {
                    caught.store(signal, Ordering::SeqCst);
                    cancel.cancel();
                } else if signal != SIGINT || !crumbkit::events::handles_interrupt() {
                    drop(signal_hook::low_level::emulate_default_handler(signal));
                }
            }
        })
        .ok()?;
    crumbkit::events::handed_over();
    Some(building)
}

/// Returns the signals that mxv catches while a child runs: `SIGINT`,
/// `SIGHUP` and `SIGQUIT`, without any that is set to be ignored. Installing
/// a handler would replace the ignored setting, so mxv leaves such a signal
/// alone, and it neither cancels the child nor ends mxv.
///
/// This misses `SIGINT`. A run started with `SIGINT` ignored still has its
/// build cancelled on `SIGINT`, because the usage log's handler is installed
/// first, so `SIGINT` no longer reads as ignored here. Reading the setting
/// before `crumbkit_usage::start()` would replace this, as would the
/// `start_under` follow-up task that covers `SIGTERM`.
pub(crate) fn interrupts() -> Vec<i32> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT};
    [SIGINT, SIGHUP, SIGQUIT]
        .into_iter()
        .filter(|signal| !ignored(*signal))
        .collect()
}

/// Reports whether `signal` is set to be ignored. A signal whose setting
/// cannot be read counts as not ignored, which keeps mxv's handling of it.
fn ignored(signal: i32) -> bool {
    let mut current = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    #[expect(
        unsafe_code,
        reason = "only sigaction reads a signal's setting, and neither rustix nor signal-hook offers it (BASE-3)"
    )]
    // SAFETY: the new action is null, so the call changes nothing, and
    // `current` lives for the call and is zeroed, so reading it afterwards
    // reads initialised bytes whether or not the call succeeded.
    let (read, current) = unsafe {
        let read = libc::sigaction(signal, std::ptr::null(), current.as_mut_ptr());
        (read, current.assume_init())
    };
    read == 0 && current.sa_sigaction == libc::SIG_IGN
}
