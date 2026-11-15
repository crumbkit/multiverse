use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;

use super::*;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, fixture, write};
use crate::workspaces::{Allow, Mode, NewOutcome};

type Hook = Box<dyn FnMut(&str) -> Result<(), Error>>;

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

/// Runs the hook of this test's thread at `step`.
fn at_checkpoint(step: &str) -> Result<(), Error> {
    HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(run) => run(step),
        None => Ok(()),
    })
}

fn set_hook(run: impl FnMut(&str) -> Result<(), Error> + 'static) {
    HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(run)));
}

/// Makes the run fail at `wanted`, as a run that stopped there would.
fn stop_at(wanted: &'static str) {
    set_hook(move |step| {
        if step == wanted {
            Err(Error::Unwritable {
                path: PathBuf::from(step),
                source: io::Error::other("stopped by the test"),
            })
        } else {
            Ok(())
        }
    });
}

struct Setup {
    fixture: Fixture,
    base: PathBuf,
    _dir: TempDir,
    workspaces: Workspaces<Git>,
    runs: Runs,
}

fn fresh() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut workspaces = Workspaces::new(Store::new(base.join("state")), Git);
    workspaces.checkpoint = at_checkpoint;
    HOOK.with(|hook| *hook.borrow_mut() = None);
    Setup {
        fixture: fixture(),
        runs: Runs::new(&base.join("runs")).with_lock_wait(Duration::from_millis(100)),
        base,
        _dir: dir,
        workspaces,
    }
}

fn name() -> WorkspaceName {
    "triage".parse().unwrap()
}

impl Setup {
    fn create(&self) -> Workspace {
        match self
            .workspaces
            .create(
                &self.base.join("mxv"),
                &name(),
                &self.fixture.clone,
                Mode::Change,
            )
            .unwrap()
        {
            NewOutcome::Created(workspace) => workspace,
            other => panic!("{other:?}"),
        }
    }

    fn removal(&self) -> Removal {
        match self
            .workspaces
            .plan_removal(&name(), Allow::default())
            .unwrap()
        {
            RemovalPlan::Ready(removal) => removal,
            other => panic!("{other:?}"),
        }
    }

    fn lifecycle(&self) -> Lifecycle<'_, Git> {
        Lifecycle::new(&self.workspaces, &self.runs)
    }

    /// Writes a file in the run directory of the workspace, as a run that
    /// started a process would.
    fn leave_run_file(&self) -> PathBuf {
        let file = self.runs.workspace_dir(&name()).join("web.log");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write(&file, "ready\n");
        file
    }

    fn record_exists(&self) -> bool {
        self.workspaces.store.read(&name()).unwrap().is_some()
    }
}

#[test]
fn remove_fails_busy_while_another_command_holds_the_lock_and_deletes_nothing() {
    let setup = fresh();
    let workspace = setup.create();
    let removal = setup.removal();
    let held = setup.runs.lock(&name()).unwrap();

    let outcome = setup.lifecycle().remove(removal, &RemoveAsked::default());

    assert!(matches!(outcome, Err(Error::Busy { .. })), "{outcome:?}");
    assert!(workspace.checkouts[0].path.is_dir());
    assert!(setup.record_exists());
    drop(held);
}

#[test]
fn remove_holds_the_lock_at_every_step() {
    let setup = fresh();
    setup.create();
    setup.leave_run_file();
    let removal = setup.removal();
    let probe = Runs::new(&setup.base.join("runs")).with_lock_wait(Duration::ZERO);
    let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
    let log = std::rc::Rc::clone(&seen);
    set_hook(move |step| {
        log.borrow_mut()
            .push((step.to_owned(), probe.lock(&name()).is_err()));
        Ok(())
    });

    let outcome = setup
        .lifecycle()
        .remove(removal, &RemoveAsked::default())
        .unwrap();

    assert!(matches!(outcome, Removed::Done(_)), "{outcome:?}");
    let steps = seen.borrow().clone();
    assert_eq!(
        steps,
        ["stopped", "removed", "forgotten", "volumes"].map(|step| (step.to_owned(), true))
    );
    assert!(
        setup.runs.lock(&name()).is_ok(),
        "the lock is free afterwards"
    );
}

#[test]
fn a_removal_stopped_after_each_step_is_finished_by_remove_or_by_repair_orphans() {
    // A stop after `stopped` leaves the whole workspace, and a second
    // removal finishes it.
    let setup = fresh();
    setup.create();
    let run_file = setup.leave_run_file();
    stop_at("stopped");
    let first = setup
        .lifecycle()
        .remove(setup.removal(), &RemoveAsked::default());
    assert!(matches!(first, Err(Error::Unwritable { .. })), "{first:?}");
    assert!(setup.record_exists());
    assert!(run_file.exists());
    HOOK.with(|hook| *hook.borrow_mut() = None);
    let second = setup
        .lifecycle()
        .remove(setup.removal(), &RemoveAsked::default())
        .unwrap();
    assert!(matches!(second, Removed::Done(_)), "{second:?}");
    assert!(!setup.record_exists());
    assert!(!run_file.exists());

    // A stop after `removed` or `forgotten` leaves no record. A second
    // removal reports that nothing is left to remove, and the run files
    // stay for `mxv repair --orphans`, which deletes the files of a name
    // that no record holds.
    for step in ["removed", "forgotten"] {
        let each = fresh();
        each.create();
        let run_file = each.leave_run_file();
        let removal = each.removal();
        stop_at(step);
        let first = each
            .lifecycle()
            .remove(removal.clone(), &RemoveAsked::default());
        assert!(matches!(first, Err(Error::Unwritable { .. })), "{step}");
        assert!(!each.record_exists(), "{step}");
        HOOK.with(|hook| *hook.borrow_mut() = None);
        let second = each
            .lifecycle()
            .remove(removal, &RemoveAsked::default())
            .unwrap();
        assert!(matches!(second, Removed::Unchanged), "{step}: {second:?}");
        assert_eq!(run_file.exists(), step == "removed", "{step}");
    }
}

#[test]
fn remove_of_a_workspace_recreated_under_its_name_stops_nothing_and_refuses() {
    let setup = fresh();
    setup.create();
    let removal = setup.removal();
    // Another command removed the workspace and made a new one under the
    // name, which stores a different record.
    let mut recreated = removal.workspace.clone();
    recreated.ports.insert("api".to_owned(), 31_000);
    setup.workspaces.store.update(&mut recreated).unwrap();
    let steps = std::rc::Rc::new(RefCell::new(0_u32));
    let count = std::rc::Rc::clone(&steps);
    set_hook(move |_| {
        *count.borrow_mut() += 1;
        Ok(())
    });

    let outcome = setup
        .lifecycle()
        .remove(removal, &RemoveAsked::default())
        .unwrap();

    assert!(matches!(outcome, Removed::Changed), "{outcome:?}");
    assert_eq!(*steps.borrow(), 0, "nothing stopped or deleted");
    assert!(setup.record_exists());
}
