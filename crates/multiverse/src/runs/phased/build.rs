//! `build`: runs the `build` command of each selected process before `up`
//! or `restart` starts anything, under one place from the machine's job
//! limiter.
//!
//! A heavy build must not hold one of the machine's places for a server's
//! whole life, so a `build` step is separate from a process's `argv`. Only
//! the `mxv` crate links the limiter. It implements [`Admission`], so this
//! crate names none of its types.
//!
//! [`Runs::build`] returns a [`BuildsPassed`] only when every build exited
//! 0, or when none is declared, and [`Runs::up`] takes one by value. So
//! `up` and a restart's stop are reached only after a build has passed, and
//! a build that fails stops, starts and replaces nothing.
//!
//! These are the limits that this design accepts. Each says what it misses
//! and what would replace it.
//!
//! - A build gets only its process's sandbox grants, so a sandboxed process
//!   needs `write = ["target"]`, and its build cannot download crates. A
//!   grant table of the build's own would replace this.
//! - While a build waits and runs, the caller holds the workspace's lock, so
//!   other commands that need it fail as busy, and `mxv reclaim` skips the
//!   workspace. Taking the lock only around the start, with a check that
//!   nothing changed during the build, would replace this.
//! - `up` asks for a place even when every build has nothing to do, so an
//!   `up` of a running workspace can wait on a busy machine. A step that asks
//!   each build tool whether it has work would replace this, and no such
//!   question exists across tools.
//! - A build that survives a `SIGKILL` of mxv runs on, uncounted, because
//!   macOS has no parent-death signal. A small supervising process that holds
//!   the place and outlives mxv would replace this.
//! - A process that outlives its build and keeps the build's output open,
//!   such as a compiler's daemon, delays the command by at most
//!   [`OUTPUT_GRACE`]. Its output is still copied until mxv exits. After
//!   that the pipe has no reader, so its next write fails, and by default
//!   the process dies of `SIGPIPE`. Without the limit, such a process would
//!   hold `up` until it exited. Giving each build a log file of its own,
//!   which needs no copying, would replace this.
//! - A sandboxed build that itself exits with the launcher's status 78 is
//!   reported as unable to start. A status that only the launcher can give
//!   would replace this.

use std::io::{self, PipeReader, Read as _, Write};
use std::marker::PhantomData;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use phase::keeper::SANDBOX_REFUSED;
use phase::{Cancel, ExecIo, ExecRequest, PhaseError, VariableName, WorkspaceId};

use super::exec::EXEC_WAIT;
use super::{app, main_process};
use crate::error::Error;
use crate::manifest::{Command, Manifest};
use crate::phase_plan::{self, Built, Unrunnable};
use crate::record::Workspace;
use crate::runs::Runs;
use crate::store::{Lock, Store};

#[cfg(test)]
mod tests;

/// How long a build's output may take to reach its end once the build has
/// exited. It passes only when a process that the build left behind holds
/// the output open, or when the admission's stream has stopped accepting
/// writes.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// What [`Admission::admit`] asks for: one place for all of a command's
/// builds.
#[derive(Debug)]
#[non_exhaustive]
pub struct Asked<'a> {
    /// The directory of the first build, which the limiter matches against
    /// the person's priorities.
    pub cwd: &'a Path,
    /// The name that the limiter shows for the job.
    pub label: &'a str,
    /// The command text that the limiter shows for the job.
    pub command: &'a str,
    /// The flag that the admission sets when the person interrupts the
    /// builds. The admission may watch for that only once it has a place,
    /// because the wait for a place cannot be cancelled.
    pub cancel: &'a Cancel,
}

/// A way to wait for a place on the machine before heavy work starts.
pub trait Admission {
    /// Waits until the work `asked` describes may start, and returns the
    /// place that counts it as running. It has no deadline: a machine that
    /// is busy for an hour holds the work for an hour.
    fn admit(&mut self, asked: &Asked<'_>) -> Box<dyn Place>;

    /// Returns where the output of one build is copied. A build always
    /// writes to a pipe that this library copies from, so a stream that is
    /// a terminal never reaches a sandboxed build.
    fn output(&self) -> Box<dyn Write + Send>;
}

/// A place on the machine, which counts the work as running until it ends.
pub trait Place {
    /// Returns the variables that the work starts with.
    fn variables(&self) -> Vec<(VariableName, String)>;

    /// Ends the work with `exit_code`, which is `None` when a signal ended
    /// it or it did not start. It takes `self`, so a place ends only once.
    fn end(self: Box<Self>, exit_code: Option<i32>);
}

/// The proof that every build of one command passed, while it holds the
/// workspace's lock. Only [`Runs::build`] makes one, and [`Runs::up`] takes
/// it by value.
#[derive(Debug)]
#[must_use = "only a BuildsPassed lets `up` start the workspace's units"]
pub struct BuildsPassed<'lock> {
    /// The workspace's lock, which the proof borrows so that the proof
    /// cannot outlive it.
    lock: PhantomData<&'lock Lock>,
}

/// How [`Runs::build`] ended.
#[derive(Debug)]
#[must_use]
pub enum BuildEnd<'lock> {
    /// Every build exited 0, or the selection declares none.
    Passed(BuildsPassed<'lock>),
    /// A build did not pass, and the builds after it did not run.
    Stopped {
        /// The process whose build did not pass.
        process: String,
        /// Why it did not pass.
        why: BuildStop,
    },
    /// The manifest declares no process with the name.
    UnknownProcess(String),
    /// phase cannot run the manifest.
    Unrunnable(Unrunnable),
    /// A selected host unit is new or asks for more than the grant of the
    /// workspace's repository, so nothing was built, stopped or started.
    NotGranted(super::PermissionAsk),
}

/// Why a build did not pass.
#[derive(Debug)]
pub enum BuildStop {
    /// The build ended with a status other than 0.
    Failed {
        /// The build's exit code, or `None` when a signal ended it.
        code: Option<i32>,
        /// The signal that ended the build, or `None`.
        signal: Option<i32>,
    },
    /// The build could not start.
    Unstartable {
        /// Why, in phase's words, which hold no variable's value.
        reason: String,
    },
    /// The builds outlived [`Runs::build`]'s deadline, so phase ended this
    /// one's process group.
    TimedOut {
        /// How long the builds ran.
        waited: Duration,
    },
    /// The person interrupted the builds, so phase ended this one's process
    /// group.
    Interrupted,
}

impl BuildStop {
    /// Returns the exit code that the place ends with.
    const fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Failed { code, .. } => *code,
            Self::Unstartable { .. } | Self::TimedOut { .. } | Self::Interrupted => None,
        }
    }
}

/// The timer that sets a command's [`Cancel`] when its builds' deadline
/// passes. Dropping it ends its thread.
struct Timer {
    /// Whether the deadline passed.
    fired: Arc<AtomicBool>,
    /// Dropping it wakes the thread, which then ends without cancelling.
    stop: Option<mpsc::Sender<()>>,
    /// The thread that waits for the deadline.
    thread: Option<JoinHandle<()>>,
}

impl Timer {
    /// Starts the timer that sets `cancel` after `wait`.
    ///
    /// When the thread cannot be spawned, the builds run without a
    /// deadline. A spawn fails only when the machine is out of threads, so
    /// no retry replaces it.
    fn start(wait: Duration, cancel: Cancel) -> Self {
        let fired = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel::<()>();
        let flag = Arc::clone(&fired);
        let thread = std::thread::Builder::new()
            .name("mxv-build-deadline".to_owned())
            .spawn(move || {
                if stopped.recv_timeout(wait) == Err(RecvTimeoutError::Timeout) {
                    flag.store(true, Ordering::SeqCst);
                    cancel.cancel();
                }
            })
            .ok();
        Self {
            fired,
            stop: Some(stop),
            thread,
        }
    }

    /// Reports whether the deadline passed.
    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The thread that copies a build's output to the admission's stream.
struct Copier {
    /// Closes when the thread ends.
    ended: mpsc::Receiver<()>,
}

impl Copier {
    /// Starts the thread that copies `reader` to `writer` until the pipe's
    /// write end has closed.
    ///
    /// # Errors
    ///
    /// Returns the error of spawning the thread.
    fn start(reader: PipeReader, writer: Box<dyn Write + Send>) -> io::Result<Self> {
        let (done, ended) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("mxv-build-output".to_owned())
            .spawn(move || {
                copy(reader, writer);
                drop(done);
            })?;
        Ok(Self { ended })
    }

    /// Waits for the output of a build that has exited to reach its end,
    /// for at most [`OUTPUT_GRACE`].
    ///
    /// A process that the build left behind can hold the write end open for
    /// as long as it lives, so the end may never come. This then returns
    /// without the copier, which goes on copying until mxv exits. The
    /// build's cancel and deadline do not depend on this, because phase
    /// watches them on the caller's thread while the copier runs on its own.
    fn finish(self) {
        let _ = self.ended.recv_timeout(OUTPUT_GRACE);
    }
}

/// Copies `reader` to `writer` until the write end has closed. A write that
/// fails is ignored, and the copy goes on reading, so a closed standard
/// error does not end the build.
fn copy(mut reader: PipeReader, mut writer: Box<dyn Write + Send>) {
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        if let Some(bytes) = buffer.get(..read) {
            drop(writer.write_all(bytes));
        }
    }
}

/// One build of a command.
struct Step<'a> {
    /// The process that the build belongs to.
    process: &'a str,
    /// The build's program and arguments.
    command: &'a Command,
}

/// What the builds of one command run with.
struct Builds<'a> {
    /// The run files and the phase that run the builds.
    runs: &'a Runs,
    /// The workspace's stable id.
    id: &'a WorkspaceId,
    /// The workspace's plan and its variables.
    built: &'a Built,
    /// The variables of the place.
    variables: &'a [(VariableName, String)],
    /// Where each build's output is copied.
    admission: &'a dyn Admission,
    /// The flag that stops the build that runs.
    cancel: &'a Cancel,
    /// The deadline of the builds.
    timer: &'a Timer,
    /// When the deadline started.
    started: Instant,
}

/// Returns the words of the job that the limiter shows for `steps`: each
/// process's name and its command.
fn job_text(steps: &[Step<'_>]) -> String {
    steps
        .iter()
        .map(|step| format!("{}: {}", step.process, step.command.words().join(" ")))
        .collect::<Vec<_>>()
        .join("; ")
}

impl Builds<'_> {
    /// Runs the build `step`, and returns `None` when it passed.
    fn run(&self, step: &Step<'_>) -> Result<Option<BuildStop>, Error> {
        if let Some(stopped) = self.stopped() {
            return Ok(Some(stopped));
        }
        let (Some(app), Some(process)) = (app(step.process), main_process()) else {
            return Ok(Some(BuildStop::Unstartable {
                reason: "the process's name is not one that phase can run".to_owned(),
            }));
        };
        let (streams, copier) = match self.streams() {
            Ok(piped) => piped,
            Err(error) => {
                return Ok(Some(BuildStop::Unstartable {
                    reason: format!("mxv cannot give the build its output streams: {error}"),
                }));
            }
        };
        let sandboxed = self
            .runs
            .phase
            .exec_sandboxed(&self.built.plan, &app, &process)
            .unwrap_or(false);
        let request = ExecRequest::new(app, process, step.command.words().to_vec(), streams)
            .with_variables(self.variables.to_vec());
        let ended = self.runs.phase.exec(
            &self.built.plan,
            request,
            &self.built.environment,
            Instant::now() + EXEC_WAIT,
            self.cancel,
        );
        copier.finish();
        match ended {
            Ok(status) if status.success() => Ok(self.stopped()),
            Ok(status) => Ok(Some(self.stopped().unwrap_or_else(|| {
                if sandboxed && status.code() == Some(i32::from(SANDBOX_REFUSED)) {
                    BuildStop::Unstartable {
                        reason: format!(
                            "the sandbox's launcher exited with {SANDBOX_REFUSED}, so it could not apply the sandbox or start the program, and the build's output says why"
                        ),
                    }
                } else {
                    BuildStop::Failed {
                        code: status.code(),
                        signal: status.signal(),
                    }
                }
            }))),
            Err(PhaseError::ExecUnstartable { reason, .. }) => {
                Ok(Some(BuildStop::Unstartable { reason }))
            }
            Err(other) => Err(self.runs.phase_error(self.id, other)),
        }
    }

    /// Returns why the builds must stop now, or `None`.
    fn stopped(&self) -> Option<BuildStop> {
        if self.timer.fired() {
            Some(BuildStop::TimedOut {
                waited: self.started.elapsed(),
            })
        } else if self.cancel.is_cancelled() {
            Some(BuildStop::Interrupted)
        } else {
            None
        }
    }

    /// Returns the streams of a build, with the copier of its output: empty
    /// standard input, and both output streams on one pipe, so their lines
    /// stay in order. They are [`ExecIo::Streams`], so the build runs in a
    /// process group of its own and a cancel ends the whole group.
    fn streams(&self) -> io::Result<(ExecIo, Copier)> {
        let (reader, stdout) = io::pipe()?;
        let stderr = stdout.try_clone()?;
        let copier = Copier::start(reader, self.admission.output())?;
        let streams = ExecIo::Streams {
            stdin: None,
            stdout: Some(stdout.into()),
            stderr: Some(stderr.into()),
        };
        Ok((streams, copier))
    }
}

impl Runs {
    /// Runs the `build` command of each process that `names` selects, or
    /// of every process when `names` is empty, with the selected units'
    /// dependencies first, one after another. `names` selects as
    /// [`Runs::up`] selects. It builds a selected process whether or not
    /// it runs, because phase decides which running units it replaces
    /// inside `up`.
    ///
    /// All the builds share one place from `admission`, which ends once,
    /// after the last build. With no `build` declared, `admission` is not
    /// asked. Each build runs in its process's context, with its sandbox
    /// and variables, and with the place's variables beside them. It reads
    /// no input, and a pipe carries its output to `admission`'s stream.
    ///
    /// The builds' deadline starts after the place is granted. A build that
    /// outlives it, or that the person interrupts, ends with its process
    /// group. Whichever way the builds end, nothing has stopped or started
    /// when this returns.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Store::update`] and [`Runs::status`],
    /// [`Error::NoFreePort`] when no port can be allocated, and
    /// [`Error::Phase`] for a failure of phase other than a build that
    /// cannot start.
    pub fn build<'lock>(
        &self,
        _lock: &'lock Lock,
        store: &Store,
        workspace: &mut Workspace,
        manifest: &Manifest,
        names: &[String],
        admission: &mut dyn Admission,
    ) -> Result<BuildEnd<'lock>, Error> {
        let order = match manifest.start_order(names) {
            Ok(order) => order,
            Err(unknown) => return Ok(BuildEnd::UnknownProcess(unknown)),
        };
        // The check comes before every build, stop and start, so a unit
        // whose permissions grew beyond its grant runs nothing.
        let id = super::saved_workspace_id(store, &workspace.name)?;
        workspace.id = Some(id.clone());
        match self.ask_of(workspace, &id, manifest, &order)? {
            Ok(None) => {}
            Ok(Some(ask)) => return Ok(BuildEnd::NotGranted(ask)),
            Err(unrunnable) => return Ok(BuildEnd::Unrunnable(unrunnable)),
        }
        let steps: Vec<Step<'_>> = order
            .iter()
            .filter_map(|name| {
                let command = manifest.processes.get(name)?.build.as_ref()?;
                Some(Step {
                    process: name,
                    command,
                })
            })
            .collect();
        let Some(first) = steps.first() else {
            return Ok(BuildEnd::Passed(BuildsPassed { lock: PhantomData }));
        };
        let (id, built) = self.prepared(store, workspace, manifest)?;
        let built = match built {
            Ok(built) => built,
            Err(unrunnable) => return Ok(BuildEnd::Unrunnable(unrunnable)),
        };
        let inputs = self.inputs(workspace, &id, manifest);
        let directory = manifest
            .processes
            .get(first.process)
            .and_then(|process| phase_plan::placed(&inputs, first.process, process).ok())
            .map(|(root, cwd)| cwd.map_or_else(|| root.clone(), |cwd| root.join(cwd)))
            .unwrap_or_else(|| inputs.checkout.to_path_buf());
        let cancel = Cancel::new();
        let place = admission.admit(&Asked {
            cwd: &directory,
            label: workspace.name.as_str(),
            command: &job_text(&steps),
            cancel: &cancel,
        });
        let variables = place.variables();
        let timer = Timer::start(self.build_wait, cancel.clone());
        let builds = Builds {
            runs: self,
            id: &id,
            built: &built,
            variables: &variables,
            admission,
            cancel: &cancel,
            timer: &timer,
            started: Instant::now(),
        };
        for step in &steps {
            match builds.run(step) {
                Ok(None) => {}
                Ok(Some(why)) => {
                    place.end(why.exit_code());
                    return Ok(BuildEnd::Stopped {
                        process: step.process.to_owned(),
                        why,
                    });
                }
                Err(error) => {
                    place.end(None);
                    return Err(error);
                }
            }
        }
        place.end(Some(0));
        // A signal handled after the last build's check only sets the flag,
        // so this check keeps a late Ctrl-C from going on to replace units.
        if let (Some(last), Some(why)) = (steps.last(), builds.stopped()) {
            return Ok(BuildEnd::Stopped {
                process: last.process.to_owned(),
                why,
            });
        }
        Ok(BuildEnd::Passed(BuildsPassed { lock: PhantomData }))
    }
}
