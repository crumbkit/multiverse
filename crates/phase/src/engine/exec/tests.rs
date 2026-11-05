//! Tests of `Phase::exec` for host processes, which run real commands, and
//! on macOS a real sandbox through this test binary as the launcher.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::{ExecIo, ExecRequest, run};
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

    let status = run(command, io, &cancel, Duration::from_secs(2)).unwrap();

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

/// Builds the sandboxed or unsandboxed app of a workspace whose files lie
/// in the build's target directory, since the per-user temporary directory
/// is protected, and runs `script` in its context. Returns what ran, the
/// directory outside the app's paths, and the root that holds both.
#[cfg(target_os = "macos")]
fn exec_in_workspace(sandboxed: bool, script: &str) -> (Ran, PathBuf, tempfile::TempDir) {
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
