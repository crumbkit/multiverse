//! Tests of `Phase::exec` for host processes, which run real commands, and
//! on macOS a real sandbox through this test binary as the launcher.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::{ExecIo, ExecRequest, Session, run};
use crate::cancel::Cancel;
use crate::env::{AppliedEnvironment, EnvironmentError, EnvironmentSource};
use crate::error::PhaseError;
use crate::name::{AppName, ProcessName, WorkspaceId};
use crate::phase::Phase;
use crate::plan::{AppPlan, Permissions, Plan, ProcessPlan, RunnerKind};
use crate::record::store::RunStore;
use crate::source::Source;

/// The full name of the keeper tests' test that runs a keeper, or the
/// launcher, when this test binary is started as one.
const KEEPER_ENTRY: &str =
    "engine::keeper::tests::the_test_binary_runs_a_keeper_when_it_is_started_as_one";

fn id() -> WorkspaceId {
    "5d2e7a10".parse().unwrap()
}

/// An environment source and environment that give the variables they
/// list.
#[derive(Clone)]
struct Listed(Vec<(&'static str, &'static str)>);

impl EnvironmentSource for Listed {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(self.clone()))
    }
}

impl AppliedEnvironment for Listed {
    fn names(&self) -> BTreeSet<String> {
        self.0.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    fn expires_at(&self) -> Option<std::time::SystemTime> {
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

/// A phase in `state` whose keeper is this test binary, and the plan of
/// one host app, `api`, from `checkout`, whose process `server` requires
/// `GREETING`, may receive `LANG`, and starts in `cwd` within the checkout.
fn world(state: &Path, checkout: &Path, cwd: Option<&str>) -> (Phase, Plan) {
    let phase = Phase::new(state.to_path_buf())
        .with_keeper(
            std::env::current_exe().unwrap(),
            ["--exact", "--nocapture", "--", KEEPER_ENTRY, "keep"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect(),
        )
        .with_inherited(BTreeMap::from([
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ("HOME".to_owned(), state.display().to_string()),
        ]))
        .with_kill_wait(Duration::from_secs(2));
    let mut api = AppPlan::new(
        "api".parse().unwrap(),
        RunnerKind::Host,
        Source::checkout(checkout.to_path_buf(), None),
    );
    let mut server = ProcessPlan::new(
        "server".parse().unwrap(),
        vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()],
    );
    server.env.insert("GREETING".to_owned());
    server.env.insert("LANG".to_owned());
    server.optional_env.insert("LANG".to_owned());
    server.cwd = cwd.map(PathBuf::from);
    api.processes.push(server);
    let plan = Plan::new("reading-list".parse().unwrap(), id(), vec![api]).unwrap();
    (phase, plan)
}

/// The output of a command that `exec` ran with its standard output and
/// standard error in a file.
struct Ran {
    result: Result<ExitStatus, PhaseError>,
    output: String,
}

/// Runs `script` in `sh` in the context of `api/server` of `plan`, with
/// `source` as the environment, its output in a file in `scratch`.
fn exec_script(phase: &Phase, plan: &Plan, source: &Listed, script: &str, scratch: &Path) -> Ran {
    let out = scratch.join("exec.out");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test gives the command a file of its own for its output"
    )]
    let file = File::create(&out).unwrap();
    let request = ExecRequest::new(
        "api".parse().unwrap(),
        "server".parse().unwrap(),
        vec!["/bin/sh".into(), "-c".into(), script.into()],
        ExecIo::Streams {
            stdin: Stdio::null(),
            stdout: Stdio::from(file.try_clone().unwrap()),
            stderr: Stdio::from(file),
        },
    );
    let result = phase.exec(
        plan,
        request,
        source,
        Instant::now() + Duration::from_secs(10),
        &Cancel::new(),
    );
    Ran {
        result,
        output: std::fs::read_to_string(&out).unwrap_or_default(),
    }
}

fn greeting() -> Listed {
    Listed(vec![("GREETING", "hello")])
}

#[test]
fn exec_returns_the_exit_code_of_a_host_command() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);

    let ran = exec_script(&phase, &plan, &greeting(), "exit 7", state.path());

    assert_eq!(ran.result.unwrap().code(), Some(7));
}

#[test]
fn exec_returns_the_signal_that_ended_a_host_command() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);

    let ran = exec_script(&phase, &plan, &greeting(), "kill -TERM $$", state.path());

    let status = ran.result.unwrap();
    assert_eq!(status.code(), None);
    assert_eq!(status.signal(), Some(15));
}

#[test]
fn a_host_command_receives_the_resolved_environment_and_nothing_else_of_the_callers() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let script = "echo \"$GREETING lang=${LANG-unset} path=$PATH manifest=${CARGO_MANIFEST_DIR-unset} tmp=${TMPDIR-unset}\"";

    let without = exec_script(&phase, &plan, &greeting(), script, state.path());
    let with = exec_script(
        &phase,
        &plan,
        &Listed(vec![("GREETING", "hello"), ("LANG", "C")]),
        script,
        state.path(),
    );

    assert!(without.result.unwrap().success());
    assert_eq!(
        without.output,
        "hello lang=unset path=/usr/bin:/bin manifest=unset tmp=unset\n"
    );
    assert!(with.result.unwrap().success());
    assert_eq!(
        with.output,
        "hello lang=C path=/usr/bin:/bin manifest=unset tmp=unset\n"
    );
}

#[test]
fn a_host_command_runs_in_its_processs_directory() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir(checkout.path().join("web")).unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), Some("web"));

    let ran = exec_script(&phase, &plan, &greeting(), "pwd -P", state.path());

    assert!(ran.result.unwrap().success());
    assert_eq!(
        ran.output.trim_end(),
        checkout
            .path()
            .join("web")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[test]
fn exec_refuses_a_host_command_whose_required_variable_has_no_value_and_runs_nothing() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let marker = state.path().join("ran");

    let ran = exec_script(
        &phase,
        &plan,
        &Listed(vec![("LANG", "C")]),
        &format!("touch '{}'", marker.display()),
        state.path(),
    );

    let error = ran.result.unwrap_err();
    assert_eq!(error.code(), "phase::environment");
    assert_eq!(
        std::error::Error::source(&error).unwrap().to_string(),
        "the environment source could not supply the variable GREETING"
    );
    assert!(!marker.exists());
}

#[test]
fn exec_writes_no_run_record() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);

    let ran = exec_script(&phase, &plan, &greeting(), "true", state.path());

    assert!(ran.result.unwrap().success());
    assert_eq!(RunStore::new(state.path()).read(&id()).unwrap(), None);
}

#[test]
fn exec_refuses_a_process_that_the_plan_lacks() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let request = ExecRequest::new(
        "api".parse().unwrap(),
        "worker".parse().unwrap(),
        vec!["true".into()],
        ExecIo::Inherit { terminal: false },
    );

    let error = phase
        .exec(
            &plan,
            request,
            &greeting(),
            Instant::now() + Duration::from_secs(5),
            &Cancel::new(),
        )
        .unwrap_err();

    assert_eq!(error.code(), "phase::select::unknown");
    assert_eq!(error.to_string(), "the plan has no api/worker");
}

#[test]
fn exec_waits_for_the_lifecycle_lock_until_its_deadline() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let store = RunStore::new(state.path());
    let held = store.lock(&id(), Duration::ZERO).unwrap();
    let marker = state.path().join("ran");

    let request = ExecRequest::new(
        "api".parse().unwrap(),
        "server".parse().unwrap(),
        vec!["touch".into(), marker.display().to_string()],
        ExecIo::Inherit { terminal: false },
    );
    let busy = phase.exec(
        &plan,
        request,
        &greeting(),
        Instant::now() + Duration::from_millis(100),
        &Cancel::new(),
    );

    assert_eq!(busy.unwrap_err().code(), "phase::lock::busy");
    assert!(!marker.exists());
    drop(held);
}

#[test]
fn a_cancelled_command_is_stopped_with_term() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test builds a command for the exec runner"
    )]
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec sleep 30"]);
    let cancel = Cancel::new();
    let setter = cancel.clone();
    let canceller = std::thread::spawn(move || {
        crate::clock::pause(Duration::from_millis(200));
        setter.cancel();
    });
    let io = ExecIo::Streams {
        stdin: Stdio::null(),
        stdout: Stdio::null(),
        stderr: Stdio::null(),
    };

    let status = run(
        command,
        Session::Caller,
        io,
        &cancel,
        Duration::from_secs(2),
    )
    .unwrap();

    canceller.join().unwrap();
    assert_eq!(status.signal(), Some(15));
}

#[test]
fn a_command_that_cannot_start_is_reported_by_name() {
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let request = ExecRequest::new(
        "api".parse().unwrap(),
        "server".parse().unwrap(),
        vec!["/nonexistent/program".into()],
        ExecIo::Inherit { terminal: false },
    );

    let error = phase
        .exec(
            &plan,
            request,
            &greeting(),
            Instant::now() + Duration::from_secs(5),
            &Cancel::new(),
        )
        .unwrap_err();

    assert_eq!(error.code(), "phase::exec::unstartable");
    assert!(!error.to_string().contains("hello"), "{error}");
}

/// Returns a phase and the plan of its sandboxed or unsandboxed app, in a
/// workspace whose files lie in the build's target directory, since the
/// per-user temporary directory is protected, and the root that holds them.
#[cfg(target_os = "macos")]
fn workspace(sandboxed: bool) -> (Phase, Plan, tempfile::TempDir) {
    let target = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("sandbox-fixtures");
    std::fs::create_dir_all(&target).unwrap();
    let root = tempfile::Builder::new()
        .prefix("phase-exec-")
        .tempdir_in(target)
        .unwrap();
    let base = root.path().canonicalize().unwrap();
    let state = base.join("state/mxv");
    let workspace = base.join("data/workspaces/reading-list");
    let checkout = workspace.join("api");
    let outside = base.join("outside");
    for dir in [&state, &checkout, &outside] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let (phase, plan) = world(&state.join("phase"), &checkout, None);
    let phase = phase.with_protected(vec![state.clone()]);
    let mut apps = plan.apps().to_vec();
    if sandboxed {
        apps[0].permissions = Some(Permissions::default());
    }
    let plan = Plan::new(plan.workspace().clone(), id(), apps)
        .unwrap()
        .with_workspace_dir(workspace.clone(), vec![workspace.join("mxv.toml")]);
    (phase, plan, root)
}

/// Runs `script` in the context of the app of [`workspace`], and returns
/// what ran, the directory outside the app's paths, and the root that holds
/// both.
#[cfg(target_os = "macos")]
fn exec_in_workspace(sandboxed: bool, script: &str) -> (Ran, PathBuf, tempfile::TempDir) {
    let (phase, plan, root) = workspace(sandboxed);
    let base = root.path().canonicalize().unwrap();
    let outside = base.join("outside");
    let script = script.replace("OUTSIDE", outside.to_str().unwrap());
    let ran = exec_script(&phase, &plan, &greeting(), &script, &base);
    (ran, outside, root)
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_apps_exec_is_denied_a_write_outside_its_paths_and_may_write_its_tmpdir() {
    use crate::sandbox::{Platform, platform};

    if !matches!(platform(), Platform::Available(_)) {
        return;
    }
    let (ran, outside, _root) = exec_in_workspace(
        true,
        "if echo x > OUTSIDE/file 2>/dev/null; then echo wrote; else echo denied; fi; \
         if echo x > \"$HOME/file\" 2>/dev/null; then echo home-wrote; else echo home-denied; fi; \
         echo x > \"$TMPDIR/file\" && echo tmp-wrote; exit 4",
    );

    let status = ran.result.unwrap();
    assert_eq!(status.code(), Some(4), "{}", ran.output);
    let lines: Vec<&str> = ran.output.lines().collect();
    assert!(lines.contains(&"denied"), "{}", ran.output);
    assert!(lines.contains(&"home-denied"), "{}", ran.output);
    assert!(lines.contains(&"tmp-wrote"), "{}", ran.output);
    assert!(!lines.contains(&"wrote"), "{}", ran.output);
    assert!(!outside.join("file").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn an_unsandboxed_apps_exec_may_write_outside_its_checkout() {
    let (ran, outside, _root) = exec_in_workspace(false, "echo x > OUTSIDE/file && echo wrote");

    assert!(ran.result.unwrap().success(), "{}", ran.output);
    assert!(outside.join("file").exists());
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_apps_exec_without_a_keeper_program_is_refused_rather_than_run_unsandboxed() {
    use crate::sandbox::{Platform, platform};

    if !matches!(platform(), Platform::Available(_)) {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (_, plan) = world(state.path(), checkout.path(), None);
    let phase = Phase::new(state.path().to_path_buf()).with_inherited(BTreeMap::from([(
        "PATH".to_owned(),
        "/usr/bin:/bin".to_owned(),
    )]));
    let mut apps = plan.apps().to_vec();
    apps[0].permissions = Some(Permissions::default());
    let plan = Plan::new(plan.workspace().clone(), id(), apps).unwrap();

    let ran = exec_script(&phase, &plan, &greeting(), "true", state.path());

    assert_eq!(ran.result.unwrap_err().code(), "phase::exec::unstartable");
}

#[test]
fn a_cancel_ends_the_whole_process_group_of_a_command_with_streams() {
    let scratch = tempfile::tempdir().unwrap();
    let pid_file = scratch.path().join("child.pid");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test builds a command for the exec runner"
    )]
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        &format!("sleep 30 & echo $! > '{}'; wait", pid_file.display()),
    ]);
    let cancel = Cancel::new();
    let setter = cancel.clone();
    let watched = pid_file.clone();
    let canceller = std::thread::spawn(move || {
        while std::fs::read_to_string(&watched).map_or(true, |text| !text.ends_with('\n')) {
            crate::clock::pause(Duration::from_millis(20));
        }
        setter.cancel();
    });
    let io = ExecIo::Streams {
        stdin: Stdio::null(),
        stdout: Stdio::null(),
        stderr: Stdio::null(),
    };

    run(
        command,
        Session::Caller,
        io,
        &cancel,
        Duration::from_secs(2),
    )
    .unwrap();

    canceller.join().unwrap();
    let child: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let child = rustix::process::Pid::from_raw(child).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rustix::process::test_kill_process(child).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the command's child survived the cancel"
        );
        crate::clock::pause(Duration::from_millis(20));
    }
}

#[test]
fn exec_sandboxed_says_whether_an_apps_commands_run_under_the_sandbox() {
    use crate::sandbox::{Platform, platform};

    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let mut apps = plan.apps().to_vec();
    apps[0].permissions = Some(Permissions::default());
    let sandboxed = Plan::new(plan.workspace().clone(), id(), apps).unwrap();
    let api: AppName = "api".parse().unwrap();
    let server: ProcessName = "server".parse().unwrap();

    assert!(!phase.exec_sandboxed(&plan, &api, &server).unwrap());
    assert_eq!(
        phase.exec_sandboxed(&sandboxed, &api, &server).unwrap(),
        matches!(platform(), Platform::Available(_))
    );
    assert_eq!(
        phase
            .exec_sandboxed(&plan, &api, &"worker".parse().unwrap())
            .unwrap_err()
            .code(),
        "phase::select::unknown"
    );
}

#[test]
fn exec_refuses_a_sandboxed_command_whose_callers_input_is_a_terminal_and_runs_nothing() {
    use crate::sandbox::{Platform, platform};

    if !matches!(platform(), Platform::Available(_)) {
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let (phase, plan) = world(state.path(), checkout.path(), None);
    let mut apps = plan.apps().to_vec();
    apps[0].permissions = Some(Permissions::default());
    let plan = Plan::new(plan.workspace().clone(), id(), apps).unwrap();
    let marker = state.path().join("ran");
    let request = ExecRequest::new(
        "api".parse().unwrap(),
        "server".parse().unwrap(),
        vec!["touch".into(), marker.display().to_string()],
        ExecIo::Inherit { terminal: true },
    );

    let error = phase
        .exec(
            &plan,
            request,
            &greeting(),
            Instant::now() + Duration::from_secs(5),
            &Cancel::new(),
        )
        .unwrap_err();

    assert_eq!(error.code(), "phase::exec::sandboxed_terminal");
    assert_eq!(
        error.to_string(),
        "api/server runs under the sandbox, and a sandboxed command cannot have the terminal yet; run it without a terminal on standard input, since a relay of the terminal is not built yet"
    );
    assert!(!marker.exists());
}

/// The full name of the test that probes its terminal when this test
/// binary is started as a sandboxed command.
#[cfg(target_os = "macos")]
const PROBE_ENTRY: &str =
    "engine::exec::tests::the_test_binary_probes_its_terminal_when_it_is_started_as_a_command";

/// The full name of the test that runs execs from a terminal of its own
/// when this test binary is started as one.
#[cfg(target_os = "macos")]
const TERMINAL_ENTRY: &str =
    "engine::exec::tests::the_test_binary_runs_execs_from_a_terminal_when_it_is_started_as_one";

/// The variable that starts the probe, which names the byte it pushes.
#[cfg(target_os = "macos")]
const PROBE_VAR: &str = "PHASE_EXEC_TEST_PROBE";

/// The variable that starts the terminal's caller.
#[cfg(target_os = "macos")]
const TERMINAL_VAR: &str = "PHASE_EXEC_TEST_TERMINAL";

/// macOS's `TIOCSTI`, `_IOW('t', 114, char)`, which pushes one byte into
/// a terminal's input. libc does not declare it for macOS.
#[cfg(target_os = "macos")]
const TIOCSTI: libc::c_ulong = 0x8001_7472;

/// Reads the variable `name` of this test binary's own environment.
#[cfg(target_os = "macos")]
fn own_var(name: &str) -> Option<String> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary started as a helper reads the variable that started it"
    )]
    std::env::var(name).ok()
}

/// Pushes `byte` into the input of the terminal on standard input, and
/// reports whether the kernel allowed it.
#[cfg(target_os = "macos")]
fn push_byte(byte: u8) -> bool {
    #[expect(
        unsafe_code,
        reason = "the test pushes a byte into a terminal with TIOCSTI, which only libc can call"
    )]
    // SAFETY: `ioctl` reads one byte from `&byte`, which lives for the call,
    // and touches no other memory of this program.
    let result = unsafe { libc::ioctl(0, TIOCSTI, &raw const byte) };
    result == 0
}

#[cfg(target_os = "macos")]
#[test]
fn the_test_binary_probes_its_terminal_when_it_is_started_as_a_command() {
    let Some(byte) = own_var(PROBE_VAR) else {
        return;
    };
    // The launcher is this test binary too, and its arguments name this
    // test after the keeper's, so the probe runs only in the command that
    // the launcher becomes.
    if std::env::args().any(|arg| arg == "--sandboxed") {
        return;
    }
    let tty = rustix::fs::open(
        "/dev/tty",
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
        rustix::fs::Mode::empty(),
    );
    println!("probe tty={}", if tty.is_ok() { "open" } else { "denied" });
    let pushed = push_byte(byte.as_bytes()[0]);
    println!("probe sti={}", if pushed { "ok" } else { "denied" });
}

#[cfg(target_os = "macos")]
#[test]
fn the_test_binary_runs_execs_from_a_terminal_when_it_is_started_as_one() {
    use std::os::fd::AsFd as _;

    if own_var(TERMINAL_VAR).is_none() {
        return;
    }
    // Standard input is the slave of the test's pty, which becomes this
    // process's controlling terminal, as a user's terminal is a shell's.
    rustix::process::setsid().unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test binary started as a helper makes its standard input its controlling terminal"
    )]
    let stdin = std::io::stdin();
    rustix::process::ioctl_tiocsctty(stdin.as_fd()).unwrap();
    println!(
        "control sti={}",
        if push_byte(b'A') { "ok" } else { "denied" }
    );
    for (sandboxed, byte) in [(false, "U"), (true, "Z")] {
        let (phase, plan, _root) = workspace(sandboxed);
        let mut source = greeting();
        source.0.push((PROBE_VAR, byte));
        let request = ExecRequest::new(
            "api".parse().unwrap(),
            "server".parse().unwrap(),
            vec![
                std::env::current_exe().unwrap().display().to_string(),
                "--exact".into(),
                "--nocapture".into(),
                "--".into(),
                PROBE_ENTRY.into(),
            ],
            ExecIo::Inherit { terminal: false },
        );
        let status = phase
            .exec(
                &plan,
                request,
                &source,
                Instant::now() + Duration::from_secs(10),
                &Cancel::new(),
            )
            .unwrap();
        println!("exec sandboxed={sandboxed} code={:?}", status.code());
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_exec_cannot_reach_the_callers_controlling_terminal() {
    use std::io::Read as _;
    use std::os::fd::{FromRawFd as _, OwnedFd};

    use crate::sandbox::{Platform, platform};

    if !matches!(platform(), Platform::Available(_)) {
        return;
    }
    let (mut master, mut slave) = (-1, -1);
    #[expect(
        unsafe_code,
        reason = "the test opens a pty, which only libc's openpty does without a feature of rustix that phase does not use"
    )]
    // SAFETY: `openpty` writes two descriptors into `master` and `slave`,
    // which live for the call, and reads no name, termios or window size,
    // since each of those pointers is null.
    let opened = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0);
    #[expect(
        unsafe_code,
        reason = "the test takes ownership of the descriptors that openpty returned"
    )]
    // SAFETY: openpty returned both descriptors open, and nothing else owns
    // them.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // A process whose controlling terminal has unread output waits for it
    // to drain when it exits, so a thread reads the master until every
    // slave descriptor has closed.
    let reader = std::thread::spawn(move || {
        let mut master = File::from(master);
        let mut echoed = Vec::new();
        let mut buffer = [0_u8; 256];
        while let Ok(read) = master.read(&mut buffer) {
            if read == 0 {
                break;
            }
            echoed.extend_from_slice(&buffer[..read]);
        }
        echoed
    });
    #[expect(
        clippy::disallowed_methods,
        reason = "the test starts this test binary as a caller whose terminal is the pty"
    )]
    let spawned = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--nocapture", "--", TERMINAL_ENTRY])
        .env(TERMINAL_VAR, "1")
        .stdin(Stdio::from(slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let output = spawned.unwrap().wait_with_output().unwrap();
    let printed = String::from_utf8_lossy(&output.stdout).into_owned();
    let echoed = reader.join().unwrap();
    let echoed = String::from_utf8_lossy(&echoed).into_owned();

    assert!(output.status.success(), "{printed}");
    let lines: Vec<&str> = printed.lines().collect();
    // The caller itself, and an unsandboxed command in its session, may
    // push input into the terminal, so the test can see a push.
    assert!(lines.contains(&"control sti=ok"), "{printed}");
    assert!(echoed.contains('A'), "{echoed:?}");
    assert!(echoed.contains('U'), "{echoed:?}");
    let (unsandboxed, sandboxed) = printed
        .split_once("exec sandboxed=false code=Some(0)")
        .unwrap_or_else(|| panic!("{printed}"));
    assert!(unsandboxed.contains("probe tty=open"), "{printed}");
    assert!(unsandboxed.contains("probe sti=ok"), "{printed}");
    // The sandboxed command has no controlling terminal.
    assert!(sandboxed.contains("probe tty=denied"), "{printed}");
    assert!(sandboxed.contains("probe sti=denied"), "{printed}");
    assert!(!sandboxed.contains("probe tty=open"), "{printed}");
    assert!(!sandboxed.contains("probe sti=ok"), "{printed}");
    assert!(!echoed.contains('Z'), "{echoed:?}");
    assert!(
        lines.contains(&"exec sandboxed=true code=Some(0)"),
        "{printed}"
    );
}
