//! Starts a sandboxed app through the run files that mxv's environment
//! builds, so phase's sandbox runs under mxv's real layout: phase keeps its
//! state in `<mxv state>/phase`, mxv protects its state directory and its
//! mirrors, and the built `mxv` binary is the keeper and the launcher.
//! mxv's manifest cannot declare permissions yet, so the test gives the
//! app its permissions in phase's plan.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use multiverse::CallerEnvironment;
use multiverse::phase::{
    AppName, AppOutcome, AppPlan, AppliedEnvironment, Cancel, EnvironmentError, EnvironmentSource,
    Permissions, Plan, ProcessName, ProcessPlan, RunnerKind, Sandboxing, Selection, Source,
    UpDeadlines, WorkspaceId,
};
use mxv::{Bases, Environment};

/// An environment source that gives each process no variables.
struct Nothing;

impl EnvironmentSource for Nothing {
    fn resolve(
        &self,
        _app: &AppName,
        _process: &ProcessName,
        _deadline: Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        Ok(Box::new(Nothing))
    }
}

impl AppliedEnvironment for Nothing {
    fn names(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        *key
    }

    fn apply_as(&self, _command: &mut Command, _reference: &dyn Fn(&str) -> Option<String>) {}
}

#[test]
fn a_sandboxed_app_starts_under_mxvs_own_layout_and_writes_its_own_state() {
    // The per-user temporary directory is protected, so the test's files
    // lie in the build's target directory.
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = root.path().canonicalize().unwrap();
    let bases = Bases {
        config: home.join(".config"),
        state: home.join(".local/state"),
        data: home.join(".local/share"),
        home: home.clone(),
    };
    let workspace = bases.data.join("mxv/workspaces/reading-list");
    let checkout = workspace.join("api");
    std::fs::create_dir_all(&checkout).unwrap();
    let caller = CallerEnvironment::from_vars([
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("HOME"), home.clone().into_os_string()),
    ]);
    let environment = Environment::read(|_| None, Some(bases.clone()))
        .with_caller(caller, Some(PathBuf::from(env!("CARGO_BIN_EXE_mxv"))));
    let runs = environment.runs_for_tests().unwrap();
    let phase = runs.phase();
    let mxv_state = bases.state.join("mxv");
    let mut api = AppPlan::new(
        "api".parse().unwrap(),
        RunnerKind::Host,
        Source::checkout(checkout.clone(), None),
    );
    api.permissions = Some(Permissions::default());
    let script = "echo state > \"$TMPDIR/../state/file\" && echo ready; exec sleep 30".to_owned();
    let mut server = ProcessPlan::new(
        "server".parse().unwrap(),
        vec!["/bin/sh".into(), "-c".into(), script],
    );
    server.ready = Some(multiverse::phase::Probe::new(
        multiverse::phase::ProbeCheck::log("^ready$".to_owned()),
    ));
    api.processes.push(server);
    let id: WorkspaceId = "0b9f3c8e".parse().unwrap();
    let plan = Plan::new("reading-list".parse().unwrap(), id.clone(), vec![api])
        .unwrap()
        .with_workspace_dir(workspace.clone(), vec![workspace.join("mxv.toml")]);
    let soon = Instant::now() + Duration::from_secs(30);

    let up = phase
        .up(
            &plan,
            &Selection::All,
            &Nothing,
            UpDeadlines::new(soon, soon),
            &Cancel::new(),
        )
        .unwrap();
    let state_file = mxv_state.join(format!("phase/apps/{id}/api/state/file"));
    let written = std::fs::read_to_string(&state_file);
    let status = phase.status(&id, Some(&plan));
    phase.down(&id, &Selection::All, soon).unwrap();

    assert_eq!(up.apps[0].outcome, AppOutcome::Started, "{up:?}");
    assert_eq!(written.unwrap(), "state\n");
    let rows = status.unwrap().rows;
    let sandbox = rows[0].sandbox.clone().unwrap();
    assert!(
        matches!(sandbox, Sandboxing::Seatbelt { .. })
            || cfg!(target_os = "linux") && matches!(sandbox, Sandboxing::Unsandboxed { .. }),
        "{sandbox:?}"
    );
}
