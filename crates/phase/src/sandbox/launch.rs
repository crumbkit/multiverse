//! The launcher: the program that links phase, run by a keeper as
//! `<keeper words> --sandboxed <spec> -- <app command>`. It applies the
//! sandbox to itself and then replaces itself with the app, so the app's
//! pid is the launcher's and every start runs under the sandbox. Before it
//! applies the sandbox it checks the spec again on the paths as they
//! resolve then, and refuses to start the app when a path moved or reaches
//! a protected one.
//!
//! The launcher first leaves the caller's controlling terminal. A process
//! whose controlling terminal is the user's could push input into the
//! user's shell with `TIOCSTI`, which the shell would then run outside the
//! sandbox once the app ends, and Seatbelt allows `ioctl` on terminal
//! devices. The launcher therefore starts a session of its own with
//! `setsid`, which leaves it with no controlling terminal. A launcher that
//! already leads its process group cannot start a session, as when a keeper
//! starts its app in a group of its own. The keeper's own session has no
//! controlling terminal, and the launcher checks that `/dev/tty` cannot be
//! opened before it goes on.

use std::convert::Infallible;
use std::ffi::OsString;
use std::io::Write as _;
use std::process::{Command, ExitCode};

use super::SandboxSpec;

/// The flag that starts the launcher.
const SANDBOXED: &str = "--sandboxed";

/// The exit status of a launcher that could not apply the sandbox or start
/// the app. It is `EX_CONFIG` of sysexits(3). The launcher writes why to
/// the app's log first.
pub const SANDBOX_REFUSED: u8 = 78;

/// Returns the command that runs `run`, the app's program and arguments,
/// through the launcher of `spec`.
pub(crate) fn launcher_command(spec: &SandboxSpec, run: &[String]) -> Result<Command, String> {
    let json = serde_json::to_string(spec).map_err(|error| error.to_string())?;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/sandbox/launch.rs builds the keeper's app command through the launcher (PROC-1)"
    )]
    let mut command = Command::new(&spec.launcher.program);
    command
        .args(&spec.launcher.words)
        .arg(SANDBOXED)
        .arg(json)
        .arg("--")
        .args(run);
    Ok(command)
}

/// Runs the launcher when `args`, the arguments that follow the keeper's
/// words, start with `--sandboxed`, and returns `None` otherwise. The
/// launcher applies the sandbox that the next argument describes, then
/// replaces itself with the program and arguments after `--`, which keep
/// the environment the keeper gave the launcher. It returns only when it
/// fails: it writes why to standard error, which is the app's log, and
/// returns [`SANDBOX_REFUSED`].
///
/// The program that links phase calls it from its hidden keeper subcommand
/// before it reads anything else, as `mxv phase keep` does.
#[must_use]
pub fn launch(args: &[OsString]) -> Option<ExitCode> {
    if args.first().is_none_or(|first| first != SANDBOXED) {
        return None;
    }
    let Err(reason) = run(args.get(1..).unwrap_or_default());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/sandbox/launch.rs writes why the sandbox failed to the app's log, which is the launcher's standard error"
    )]
    let mut stderr = std::io::stderr();
    // The launcher exits either way, and a line that cannot be written
    // changes nothing.
    drop(writeln!(stderr, "phase: {reason}"));
    Some(ExitCode::from(SANDBOX_REFUSED))
}

/// Applies the sandbox of `args` and replaces this process with the app,
/// so it returns only why it could not.
fn run(args: &[OsString]) -> Result<Infallible, String> {
    let [spec, separator, program, rest @ ..] = args else {
        return Err("the launcher needs a sandbox, `--` and a program".to_owned());
    };
    if separator != "--" {
        return Err("the launcher's sandbox is not followed by `--`".to_owned());
    }
    let spec = spec
        .to_str()
        .ok_or_else(|| "the launcher's sandbox is not UTF-8".to_owned())?;
    let spec: SandboxSpec = serde_json::from_str(spec)
        .map_err(|error| format!("the launcher's sandbox cannot be read: {error}"))?;
    leave_terminal()?;
    // The checkout or an app's directory may have changed since the engine
    // built the sandbox, for example by a restart after the app replaced a
    // directory with a symbolic link, so the launcher checks it again.
    super::verify(&spec).map_err(|error| error.to_string())?;
    apply(&spec)?;
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/sandbox/launch.rs replaces the launcher with the app once the sandbox applies (PROC-1)"
    )]
    let mut command = Command::new(program);
    command.args(rest);
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/sandbox/launch.rs replaces the launcher with the app, so the app's pid is the one its keeper records (PROC-1)"
    )]
    let error = std::os::unix::process::CommandExt::exec(&mut command);
    Err(format!(
        "{} cannot start under the sandbox: {error}",
        program.to_string_lossy()
    ))
}

/// Leaves the controlling terminal of the process that started the
/// launcher, as the module docs say, or returns why it cannot.
fn leave_terminal() -> Result<(), String> {
    if rustix::process::setsid().is_ok() {
        return Ok(());
    }
    // The launcher leads its group, so it stays in its session. It may go
    // on only when that session has no controlling terminal.
    let opened = rustix::fs::open(
        "/dev/tty",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    );
    match opened {
        Ok(_) => Err(
            "the launcher cannot leave the controlling terminal, so it does not start the app under the sandbox"
                .to_owned(),
        ),
        Err(_) => Ok(()),
    }
}

/// Applies the sandbox of `spec` to this process.
#[cfg(target_os = "macos")]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let caps = super::capabilities(spec)?;
    nono::Sandbox::apply_auto(&caps)
        .map_err(|error| format!("the sandbox cannot be applied: {error}"))
}

/// Applies the sandbox of `spec` to this process with Landlock alone, which
/// fails rather than fall back to seccomp when the kernel cannot restrict
/// TCP ports.
#[cfg(target_os = "linux")]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let caps = super::capabilities(spec)?;
    nono::Sandbox::apply_landlock(&caps)
        .map_err(|error| format!("the sandbox cannot be applied: {error}"))
}

/// Refuses to start the app, because this platform has no sandbox. The
/// engine never asks for one here.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let _ = spec;
    Err(format!("phase has no sandbox for {}", std::env::consts::OS))
}
