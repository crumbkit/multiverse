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
//! controlling terminal, and the launcher goes on only when opening
//! `/dev/tty` fails with `ENXIO`, which says that the session has none.
//!
//! Just before it replaces itself with the app, the launcher sets
//! close-on-exec on every descriptor above 2, so the app inherits only its
//! standard streams. A terminal that the caller left open on a higher
//! descriptor would otherwise let the app read what the user types, because
//! Seatbelt and Landlock check paths and not descriptors that are already
//! open. No keeper or other caller passes a descriptor above 2 on purpose.
//! The launcher lists the descriptors in a directory that the sandbox lets
//! every app read, so it can do this once the sandbox applies.
//!
//! On macOS the launcher also lets the app read the metadata of each
//! regular file that its standard input, output or error already has open,
//! such as the log in phase's state directory that a keeper gives it.
//! Seatbelt checks `fstat` on a descriptor against the file's path, so
//! without the grant `fstat` of a log the app may not read failed with
//! `EPERM`, and node, which checks its standard streams with `fstat` when
//! it starts, aborted with signal 6.

use std::convert::Infallible;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, BorrowedFd, RawFd};
use std::process::{Command, ExitCode};

use super::SandboxSpec;

/// The flag that starts the launcher.
const SANDBOXED: &str = "--sandboxed";

/// The exit status of a launcher that could not apply the sandbox or start
/// the app. It is `EX_CONFIG` of sysexits(3). The launcher writes why to
/// the app's log first.
pub const SANDBOX_REFUSED: u8 = 78;

/// The directory that names each descriptor that this process has open.
#[cfg(target_os = "linux")]
const OPEN_DESCRIPTORS: &str = "/proc/self/fd";

/// The directory that names each descriptor that this process has open.
#[cfg(not(target_os = "linux"))]
const OPEN_DESCRIPTORS: &str = "/dev/fd";

/// The highest descriptor that the app inherits: standard error.
const LAST_INHERITED: RawFd = 2;

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
    let mut spec: SandboxSpec = serde_json::from_str(spec)
        .map_err(|error| format!("the launcher's sandbox cannot be read: {error}"))?;
    leave_terminal()?;
    // The checkout or an app's directory may have changed since the engine
    // built the sandbox, for example by a restart after the app replaced a
    // directory with a symbolic link, so the launcher checks it again.
    super::verify(&spec).map_err(|error| error.to_string())?;
    spec.metadata.extend(standard_files());
    apply(&spec)?;
    close_higher_descriptors_on_exec()?;
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

/// Sets close-on-exec on every descriptor above standard error, as the
/// module docs say, or returns why it cannot, and the launcher then does
/// not start the app.
fn close_higher_descriptors_on_exec() -> Result<(), String> {
    let failed = |error: rustix::io::Errno| {
        format!(
            "the launcher cannot close its other descriptors on exec ({error}), so it does not start the app under the sandbox"
        )
    };
    let listing = rustix::fs::open(
        OPEN_DESCRIPTORS,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(failed)?;
    let own = listing.as_raw_fd();
    let mut open = Vec::new();
    for entry in rustix::fs::Dir::new(listing).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name().to_string_lossy();
        // The directory also holds `.` and `..`, which name no descriptor.
        if let Ok(fd) = name.parse::<RawFd>()
            && fd > LAST_INHERITED
            && fd != own
        {
            open.push(fd);
        }
    }
    for fd in open {
        #[expect(
            unsafe_code,
            reason = "only a raw descriptor number reaches a descriptor that nothing in this process owns (BASE-3)"
        )]
        // SAFETY: the listing named `fd` as open, the launcher closes no
        // descriptor before it replaces itself, and the borrow ends with
        // this one call.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC).map_err(failed)?;
    }
    Ok(())
}

/// Leaves the controlling terminal of the process that started the
/// launcher, as the module docs say, or returns why it cannot.
fn leave_terminal() -> Result<(), String> {
    if rustix::process::setsid().is_ok() {
        return Ok(());
    }
    // The launcher leads its group, so it stays in its session. It may go
    // on only when that session has no controlling terminal, which
    // opening `/dev/tty` reports with `ENXIO`. Any other answer, such as
    // a descriptor or a refusal to open the file, leaves the question open,
    // so the launcher refuses.
    let opened = rustix::fs::open(
        "/dev/tty",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    );
    match opened {
        Err(rustix::io::Errno::NXIO) => Ok(()),
        Ok(_) => Err(
            "the launcher cannot leave the controlling terminal, so it does not start the app under the sandbox"
                .to_owned(),
        ),
        Err(error) => Err(format!(
            "the launcher cannot tell whether it has a controlling terminal ({error}), so it does not start the app under the sandbox"
        )),
    }
}

/// Returns the path of each regular file that the standard input, output
/// or error of this process has open, as the module docs say. A stream
/// that is a pipe, a socket or a device, or whose path the profile cannot
/// hold, adds nothing.
#[cfg(target_os = "macos")]
fn standard_files() -> Vec<std::path::PathBuf> {
    use std::os::fd::AsFd as _;
    use std::os::unix::ffi::OsStringExt as _;

    #[expect(
        clippy::disallowed_methods,
        reason = "crates/phase/src/sandbox/launch.rs reads the paths of the standard streams that the app inherits, and writes nothing to them"
    )]
    let (stdin, stdout, stderr) = (std::io::stdin(), std::io::stdout(), std::io::stderr());
    [stdin.as_fd(), stdout.as_fd(), stderr.as_fd()]
        .into_iter()
        .filter(|fd| {
            rustix::fs::fstat(fd).is_ok_and(|stat| {
                rustix::fs::FileType::from_raw_mode(stat.st_mode)
                    == rustix::fs::FileType::RegularFile
            })
        })
        .filter_map(|fd| rustix::fs::getpath(fd).ok())
        .map(|path| std::path::PathBuf::from(OsString::from_vec(path.into_bytes())))
        .filter(|path| super::is_profile_path(path))
        .collect()
}

/// Returns nothing, because Landlock does not restrict metadata, so a
/// process can `fstat` its standard streams there.
#[cfg(not(target_os = "macos"))]
fn standard_files() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// Applies the sandbox of `spec` to this process.
#[cfg(all(feature = "sandbox", target_os = "macos"))]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let caps = super::capabilities(spec)?;
    nono::Sandbox::apply_auto(&caps)
        .map_err(|error| format!("the sandbox cannot be applied: {error}"))
}

/// Applies the sandbox of `spec` to this process with Landlock alone, which
/// fails rather than fall back to seccomp when the kernel cannot restrict
/// TCP ports.
#[cfg(all(feature = "sandbox", target_os = "linux"))]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let caps = super::capabilities(spec)?;
    nono::Sandbox::apply_landlock(&caps)
        .map_err(|error| format!("the sandbox cannot be applied: {error}"))
}

/// Refuses to start the app, because this platform has no sandbox. The
/// engine never asks for one here.
#[cfg(all(
    feature = "sandbox",
    not(any(target_os = "macos", target_os = "linux"))
))]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let _ = spec;
    Err(format!("phase has no sandbox for {}", std::env::consts::OS))
}

/// Refuses to start the app, because phase was built without its `sandbox`
/// feature. The engine never asks for one in such a build.
#[cfg(not(feature = "sandbox"))]
fn apply(spec: &SandboxSpec) -> Result<(), String> {
    let _ = spec;
    Err(super::UNBUILT.to_owned())
}
