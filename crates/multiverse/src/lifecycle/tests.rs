use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use tempfile::TempDir;

use super::*;
use crate::archive::RestoreOutcome;
use crate::archive::RestorePlan;
use crate::archive::{
    Archive, ArchiveOptions, ArchiveOutcome, ArchivePlan, ArchivePlanned, ArchiveState,
};
use crate::archive::{Archives, RestoreOptions, RestorePlanned};
use crate::git::Git;
use crate::health::Repaired;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::store::Store;
use crate::testing::{Fixture, fixture, git, write};
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
    archives: Archives,
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
        archives: Archives::new(base.join("archives"), base.join("data/archives")),
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
        Lifecycle::new(&self.workspaces, &self.archives, &self.runs)
    }

    /// Writes a file in the run directory of the workspace, as a run that
    /// started a process would.
    fn leave_run_file(&self) -> PathBuf {
        let file = self.runs.workspace_dir(&name()).join("web.log");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write(&file, "ready\n");
        file
    }

    /// Archives the workspace and finishes the archive.
    fn archive(&self) -> Archive {
        let plan = match self
            .workspaces
            .plan_archive(
                &self.archives,
                &name(),
                ArchiveOptions::default(),
                Mode::Change,
            )
            .unwrap()
        {
            ArchivePlanned::Ready(plan) => *plan,
            other => panic!("{other:?}"),
        };
        match self.workspaces.archive(&self.archives, plan).unwrap() {
            ArchiveOutcome::Retired { mut archive, .. } => {
                self.workspaces
                    .finish_archive(&self.archives, &mut archive)
                    .unwrap();
                *archive
            }
            ArchiveOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    /// Restores the archive, which rebuilds the workspace and deletes the
    /// archive.
    fn restore(&self) {
        let plan = match self
            .workspaces
            .plan_restore(&self.archives, &name(), RestoreOptions::default())
            .unwrap()
        {
            RestorePlanned::Ready(plan) => *plan,
            other => panic!("{other:?}"),
        };
        match self
            .workspaces
            .restore(&self.archives, plan, &self.runs)
            .unwrap()
        {
            RestoreOutcome::Restored(_) => {}
            RestoreOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn archive_record(&self) -> Option<Archive> {
        self.archives.read(&name()).unwrap()
    }

    /// Returns the pin references that the repository holds for `archive`.
    fn pins_of(&self, archive: &Archive) -> String {
        let pin = &archive.checkouts[0].pin;
        let dir = pin.strip_suffix("head").unwrap();
        git(
            &self.fixture.clone,
            &["for-each-ref", "--format=%(refname)", dir],
        )
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

/// Archives the workspace, which holds an ignored file, restores it, and
/// archives it again, so the name holds a second archive with its own id,
/// pins and tar of ignored files.
fn archived_twice(setup: &Setup) -> (Archive, Archive) {
    let workspace = setup.create();
    let checkout = &workspace.checkouts[0].path;
    write(checkout.join(".gitignore"), ".env\n");
    git(checkout, &["add", ".gitignore"]);
    git(checkout, &["commit", "--quiet", "-m", "Ignore"]);
    write(checkout.join(".env"), "SECRET=1\n");
    let first = setup.archive();
    setup.restore();
    let second = setup.archive();
    assert_ne!(first.id, second.id);
    (first, second)
}

#[test]
fn deleting_an_archive_that_was_restored_and_archived_again_keeps_the_new_archive_its_pins_and_its_tar()
 {
    let setup = fresh();
    let (first, second) = archived_twice(&setup);
    let tar = setup.archives.data_of(&second.id).join("ignored.tar");
    assert!(tar.is_file());
    assert!(!setup.pins_of(&second).is_empty());

    let outcome = setup
        .lifecycle()
        .delete_archive(&first, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, ArchiveDeleted::Changed), "{outcome:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&second));
    assert!(tar.is_file(), "the new archive's tar is gone");
    assert!(
        !setup.pins_of(&second).is_empty(),
        "the new archive's pins are gone"
    );
}

#[test]
fn deleting_an_archive_that_another_command_deleted_has_nothing_to_do() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let first = setup
        .lifecycle()
        .delete_archive(&archive, &VolumesAsked::default())
        .unwrap();
    assert!(matches!(first, ArchiveDeleted::Done(_)), "{first:?}");
    assert_eq!(setup.archive_record(), None);

    let second = setup
        .lifecycle()
        .delete_archive(&archive, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(second, ArchiveDeleted::Unchanged), "{second:?}");
}

#[test]
fn deleting_an_archive_fails_busy_while_another_command_holds_the_lock_and_deletes_nothing() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let held = setup.runs.lock(&name()).unwrap();

    let outcome = setup
        .lifecycle()
        .delete_archive(&archive, &VolumesAsked::default());

    assert!(matches!(outcome, Err(Error::Busy { .. })), "{outcome:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));
    assert!(!setup.pins_of(&archive).is_empty());
    drop(held);
}

#[test]
fn an_archive_deletion_stopped_after_each_step_finishes_when_it_runs_again() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    stop_at("volumes");
    let first = setup
        .lifecycle()
        .delete_archive(&archive, &VolumesAsked::default());
    assert!(matches!(first, Err(Error::Unwritable { .. })), "{first:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));
    HOOK.with(|hook| *hook.borrow_mut() = None);

    let second = setup
        .lifecycle()
        .delete_archive(&archive, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(second, ArchiveDeleted::Done(_)), "{second:?}");
    assert_eq!(setup.archive_record(), None);
    assert_eq!(setup.pins_of(&archive), "");
}

#[test]
fn deleting_an_archive_holds_the_lock_at_every_step() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
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
        .delete_archive(&archive, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, ArchiveDeleted::Done(_)), "{outcome:?}");
    assert_eq!(*seen.borrow(), [("volumes".to_owned(), true)]);
    assert!(setup.runs.lock(&name()).is_ok());
}

#[test]
fn deleting_an_archive_whose_state_is_not_archived_changes_nothing() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let record = setup.archives.dir_of(&name()).join("archive.json");
    let text = std::fs::read_to_string(&record).unwrap();
    assert!(text.contains("\"state\": \"archived\""), "{text}");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test sets the state of a record, as an interrupted run would leave it"
    )]
    std::fs::write(
        &record,
        text.replace("\"state\": \"archived\"", "\"state\": \"retired\""),
    )
    .unwrap();
    let retired = setup.archive_record().unwrap();
    assert_eq!(retired.state, ArchiveState::Retired);

    let outcome = setup
        .lifecycle()
        .delete_archive(&archive, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, ArchiveDeleted::Changed), "{outcome:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&retired));
    assert!(!setup.pins_of(&archive).is_empty());
}

#[test]
fn a_held_deletion_runs_under_the_lock_of_its_caller_without_taking_it_again() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let lock = setup.runs.lock(&name()).unwrap();

    let outcome = setup
        .lifecycle()
        .delete_archive_held(&lock, &archive, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, ArchiveDeleted::Done(_)), "{outcome:?}");
    assert_eq!(setup.archive_record(), None);
    drop(lock);
}

impl Setup {
    /// Returns the plan of an archive of the workspace, as the person
    /// confirms it.
    fn archive_plan(&self) -> ArchivePlan {
        match self
            .lifecycle()
            .plan_archive(&name(), ArchiveOptions::default())
            .unwrap()
        {
            ArchivePlanned::Ready(plan) => *plan,
            other => panic!("{other:?}"),
        }
    }

    /// Runs the archive of `plan` until the run stops at `step`, as a
    /// process that died there leaves it, and returns the archive record.
    fn stopped_archive(&self, plan: ArchivePlan, step: &'static str) -> Archive {
        stop_at(step);
        let first = self.lifecycle().archive(plan, &VolumesAsked::default());
        assert!(matches!(first, Err(Error::Unwritable { .. })), "{first:?}");
        HOOK.with(|hook| *hook.borrow_mut() = None);
        self.archive_record().unwrap()
    }

    /// Returns a probe that asks for the lock with no wait.
    fn probe(&self) -> Runs {
        Runs::new(&self.base.join("runs")).with_lock_wait(Duration::ZERO)
    }
}

#[test]
fn an_archive_plan_releases_the_lock_before_the_question() {
    let setup = fresh();
    setup.create();

    let planned = setup
        .lifecycle()
        .plan_archive(&name(), ArchiveOptions::default())
        .unwrap();

    assert!(matches!(planned, ArchivePlanned::Ready(_)), "{planned:?}");
    assert!(setup.probe().lock(&name()).is_ok());
}

#[test]
fn an_archive_plan_fails_busy_while_another_command_holds_the_lock() {
    let setup = fresh();
    let workspace = setup.create();
    let held = setup.runs.lock(&name()).unwrap();

    let planned = setup
        .lifecycle()
        .plan_archive(&name(), ArchiveOptions::default());

    assert!(matches!(planned, Err(Error::Busy { .. })), "{planned:?}");
    assert!(workspace.checkouts[0].path.is_dir());
    drop(held);
}

#[test]
fn a_second_archive_that_plans_during_the_first_archives_retire_fails_busy_and_moves_no_checkout() {
    let setup = fresh();
    let workspace = setup.create();
    let plan = setup.archive_plan();
    let checkout = workspace.checkouts[0].path.clone();
    let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
    let log = std::rc::Rc::clone(&seen);
    let base = setup.base.clone();
    set_hook(move |step| {
        if step.starts_with("moved:") {
            // A second archive plans while the first holds a checkout in
            // quarantine, with its own handles on the same records.
            let workspaces = Workspaces::new(Store::new(base.join("state")), Git);
            let archives = Archives::new(base.join("archives"), base.join("data/archives"));
            let runs = Runs::new(&base.join("runs")).with_lock_wait(Duration::ZERO);
            let second = Lifecycle::new(&workspaces, &archives, &runs)
                .plan_archive(&name(), ArchiveOptions::default());
            log.borrow_mut()
                .push((matches!(second, Err(Error::Busy { .. })), checkout.exists()));
        }
        Ok(())
    });

    let outcome = setup
        .lifecycle()
        .archive(plan, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, Archived::Done(_)), "{outcome:?}");
    assert_eq!(*seen.borrow(), [(true, false)]);
}

#[test]
fn a_second_archive_finishes_the_archive_that_the_first_retired() {
    let setup = fresh();
    setup.create();
    let plan = setup.archive_plan();
    let retired = setup.stopped_archive(plan.clone(), "removed");
    assert_eq!(retired.state, ArchiveState::Retired);
    assert!(!setup.record_exists());

    let outcome = setup
        .lifecycle()
        .archive(plan, &VolumesAsked::default())
        .unwrap();

    let Archived::Finished(done) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(done.archive.id, retired.id);
    let kept = setup.archive_record().unwrap();
    assert_eq!(kept.state, ArchiveState::Archived);
    assert_eq!(kept.id, retired.id);
}

#[test]
fn a_second_archive_after_the_first_finished_has_nothing_to_do() {
    let setup = fresh();
    setup.create();
    let plan = setup.archive_plan();
    let archived = setup.archive();

    let outcome = setup
        .lifecycle()
        .archive(plan, &VolumesAsked::default())
        .unwrap();

    let Archived::Unchanged(found) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(found.id, archived.id);
    assert_eq!(setup.archive_record().as_ref(), Some(&archived));
}

#[test]
fn an_archive_holds_the_lock_from_its_second_plan_to_its_last_write() {
    let setup = fresh();
    setup.create();
    let plan = setup.archive_plan();
    let probe = setup.probe();
    let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
    let log = std::rc::Rc::clone(&seen);
    set_hook(move |step| {
        log.borrow_mut()
            .push((step.to_owned(), probe.lock(&name()).is_err()));
        Ok(())
    });

    let outcome = setup
        .lifecycle()
        .archive(plan, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, Archived::Done(_)), "{outcome:?}");
    let steps = seen.borrow();
    assert!(
        steps.iter().any(|(step, _)| step == "forgotten"),
        "{steps:?}"
    );
    assert!(steps.iter().all(|(_, held)| *held), "{steps:?}");
    assert!(setup.probe().lock(&name()).is_ok());
}

#[test]
fn a_finish_stopped_after_each_step_finishes_when_it_runs_again() {
    for step in ["forgotten", "volumes"] {
        let setup = fresh();
        setup.create();
        let plan = setup.archive_plan();
        let retired = setup.stopped_archive(plan, "removed");
        setup.leave_run_file();
        stop_at(step);

        let first = setup.lifecycle().finish(&retired, &VolumesAsked::default());

        assert!(
            matches!(first, Err(Error::Unwritable { .. })),
            "{step}: {first:?}"
        );
        assert_eq!(setup.archive_record().unwrap().state, ArchiveState::Retired);
        HOOK.with(|hook| *hook.borrow_mut() = None);

        let second = setup
            .lifecycle()
            .finish(&retired, &VolumesAsked::default())
            .unwrap();

        assert!(matches!(second, Finished::Done(_)), "{step}: {second:?}");
        assert_eq!(
            setup.archive_record().unwrap().state,
            ArchiveState::Archived
        );
    }
}

#[test]
fn a_finish_of_an_archive_that_is_already_finished_has_nothing_to_do() {
    let setup = fresh();
    setup.create();
    let plan = setup.archive_plan();
    let retired = setup.stopped_archive(plan, "removed");
    let first = setup
        .lifecycle()
        .finish(&retired, &VolumesAsked::default())
        .unwrap();
    assert!(matches!(first, Finished::Done(_)), "{first:?}");

    let second = setup
        .lifecycle()
        .finish(&retired, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(second, Finished::Unchanged(_)), "{second:?}");
}

#[test]
fn a_finish_fails_busy_while_another_command_holds_the_lock_and_changes_nothing() {
    let setup = fresh();
    setup.create();
    let plan = setup.archive_plan();
    let retired = setup.stopped_archive(plan, "removed");
    let held = setup.runs.lock(&name()).unwrap();

    let outcome = setup.lifecycle().finish(&retired, &VolumesAsked::default());

    assert!(matches!(outcome, Err(Error::Busy { .. })), "{outcome:?}");
    assert_eq!(setup.archive_record().unwrap().state, ArchiveState::Retired);
    drop(held);
}

#[test]
fn a_finish_of_another_archive_than_the_one_confirmed_changes_nothing() {
    let setup = fresh();
    setup.create();
    let plan = setup.archive_plan();
    let retired = setup.stopped_archive(plan, "removed");
    let mut other = retired.clone();
    other.id = crate::archive::ArchiveId::mint().unwrap();

    let outcome = setup
        .lifecycle()
        .finish(&other, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, Finished::Changed), "{outcome:?}");
    assert_eq!(setup.archive_record().unwrap().state, ArchiveState::Retired);
}

#[test]
fn an_archive_whose_in_use_check_names_a_process_stops_before_it_retires() {
    let setup = fresh();
    let workspace = setup.create();
    let plan = setup.archive_plan();
    let lifecycle = setup
        .lifecycle()
        .with_in_use(|_| Ok(Some("a shell works in it".to_owned())));

    let outcome = lifecycle.archive(plan, &VolumesAsked::default()).unwrap();

    let Archived::InUse(reason) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(reason, "a shell works in it");
    assert!(workspace.checkouts[0].path.is_dir());
    assert!(setup.record_exists());
    assert_eq!(setup.archive_record(), None);
}

#[test]
fn an_archive_of_a_workspace_that_was_removed_and_made_again_during_the_question_changes_nothing() {
    let setup = fresh();
    let first = setup.create();
    let plan = setup.archive_plan();
    let removed = setup.workspaces.remove(setup.removal()).unwrap();
    assert!(matches!(removed, RemovalOutcome::Done(_)), "{removed:?}");
    let second = setup.create();
    assert_ne!(first.id, second.id);

    let outcome = setup
        .lifecycle()
        .archive(plan, &VolumesAsked::default())
        .unwrap();

    assert!(matches!(outcome, Archived::Changed), "{outcome:?}");
    assert!(setup.workspaces.store.read(&name()).unwrap().is_some());
    assert!(second.path.exists());
}

impl Setup {
    /// Returns the plan of a restore of the archive, as the person confirms
    /// it.
    fn restore_plan(&self) -> RestorePlan {
        match self
            .workspaces
            .plan_restore(&self.archives, &name(), RestoreOptions::default())
            .unwrap()
        {
            RestorePlanned::Ready(plan) => *plan,
            other => panic!("{other:?}"),
        }
    }

    /// Returns what the repair of the name plans, as the person confirms
    /// it.
    fn repair_plan(&self) -> Repaired {
        self.workspaces
            .repair(&self.archives, &name(), Mode::DryRun)
            .unwrap()
    }
}

#[test]
fn a_restore_fails_busy_while_another_command_holds_the_lock_and_restores_nothing() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let plan = setup.restore_plan();
    let held = setup.runs.lock(&name()).unwrap();

    let outcome = setup.lifecycle().restore(&plan);

    assert!(matches!(outcome, Err(Error::Busy { .. })), "{outcome:?}");
    drop(held);
    assert!(!setup.record_exists());
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));
}

#[test]
fn a_restore_holds_the_lock_at_every_step() {
    let setup = fresh();
    setup.create();
    setup.archive();
    let plan = setup.restore_plan();
    let probe = setup.probe();
    let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
    let log = std::rc::Rc::clone(&seen);
    set_hook(move |step| {
        log.borrow_mut()
            .push((step.to_owned(), probe.lock(&name()).is_err()));
        Ok(())
    });

    let outcome = setup.lifecycle().restore(&plan).unwrap();

    assert!(matches!(outcome, Restoration::Done(_)), "{outcome:?}");
    let seen = seen.borrow();
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|(_, busy)| *busy), "{seen:?}");
    assert!(setup.runs.lock(&name()).is_ok());
}

#[test]
fn a_second_restore_after_the_first_restored_reports_unchanged_and_changes_nothing() {
    let setup = fresh();
    let workspace = setup.create();
    setup.archive();
    let plan = setup.restore_plan();
    let first = setup.lifecycle().restore(&plan).unwrap();
    assert!(matches!(first, Restoration::Done(_)), "{first:?}");
    let record = setup.workspaces.store.read(&name()).unwrap();

    let second = setup.lifecycle().restore(&plan).unwrap();

    assert!(matches!(second, Restoration::Unchanged), "{second:?}");
    assert_eq!(setup.workspaces.store.read(&name()).unwrap(), record);
    assert!(workspace.checkouts[0].path.is_dir());
}

#[test]
fn a_restore_of_another_archive_than_the_one_confirmed_changes_nothing() {
    let setup = fresh();
    let (first, second) = archived_twice(&setup);
    // The plan names the second archive. A restore and a new archive of
    // the name then replace it with a third while the question is open.
    let plan = setup.restore_plan();
    assert_eq!(plan.archive, second);
    setup.restore();
    let third = setup.archive();
    assert_ne!(third.id, second.id);

    let outcome = setup.lifecycle().restore(&plan).unwrap();

    assert!(matches!(outcome, Restoration::Changed), "{outcome:?}");
    assert!(!setup.record_exists());
    assert_eq!(setup.archive_record().as_ref(), Some(&third));
    assert_ne!(first.id, third.id);
}

#[test]
fn a_restore_stopped_after_its_record_leaves_an_archive_that_a_second_restore_reports_unchanged() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let plan = setup.restore_plan();
    stop_at("record");
    let first = setup.lifecycle().restore(&plan);
    assert!(matches!(first, Err(Error::Unwritable { .. })), "{first:?}");
    HOOK.with(|hook| *hook.borrow_mut() = None);
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));

    let second = setup.lifecycle().restore(&plan).unwrap();

    assert!(matches!(second, Restoration::Unchanged), "{second:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));
    assert!(setup.record_exists());
}

impl Setup {
    /// Leaves a restored workspace beside the archive that the restore
    /// stopped before it deleted, which `mxv repair` deletes.
    fn stopped_restore(&self) -> Archive {
        self.create();
        let archive = self.archive();
        let plan = self.restore_plan();
        stop_at("record");
        let stopped = self.lifecycle().restore(&plan);
        assert!(
            matches!(stopped, Err(Error::Unwritable { .. })),
            "{stopped:?}"
        );
        HOOK.with(|hook| *hook.borrow_mut() = None);
        archive
    }

    /// Leaves a retired archive of the name beside a newer workspace of
    /// it, which `mxv repair` finishes under a dated name.
    fn retired_beside_a_newer_workspace(&self) -> Archive {
        self.create();
        let plan = self.archive_plan();
        let archive = self.stopped_archive(plan, "removed");
        // A worktree archive keeps its branch in the repository, which the
        // newer workspace of the name needs.
        git(&self.fixture.clone, &["branch", "-D", "triage"]);
        self.create();
        archive
    }
}

#[test]
fn a_repair_fails_busy_while_another_command_holds_the_lock_and_deletes_nothing() {
    let setup = fresh();
    let archive = setup.stopped_restore();
    let planned = setup.repair_plan();
    assert!(matches!(planned, Repaired::Deleted(_)), "{planned:?}");
    let held = setup.runs.lock(&name()).unwrap();

    let outcome = setup.lifecycle().repair(&name(), &planned);

    assert!(matches!(outcome, Err(Error::Busy { .. })), "{outcome:?}");
    drop(held);
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));
    assert!(!setup.pins_of(&archive).is_empty());
}

#[test]
fn a_repair_deletes_the_archive_that_a_restore_already_rebuilt() {
    let setup = fresh();
    let archive = setup.stopped_restore();
    let planned = setup.repair_plan();

    let outcome = setup.lifecycle().repair(&name(), &planned).unwrap();

    let Repairing::Done(done) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(matches!(done.repaired, Repaired::Deleted(_)));
    assert!(done.finish.is_none());
    assert_eq!(setup.archive_record(), None);
    assert_eq!(setup.pins_of(&archive), "");
    assert!(setup.runs.lock(&name()).is_ok());
}

#[test]
fn a_repair_whose_plan_changed_after_the_question_changes_nothing() {
    let setup = fresh();
    let archive = setup.stopped_restore();
    let planned = setup.repair_plan();
    // Another repair deletes the archive while the question is open.
    let first = setup.lifecycle().repair(&name(), &planned).unwrap();
    assert!(matches!(first, Repairing::Done(_)), "{first:?}");

    let second = setup.lifecycle().repair(&name(), &planned).unwrap();

    assert!(matches!(second, Repairing::Changed), "{second:?}");
    assert_eq!(setup.pins_of(&archive), "");
    assert!(setup.record_exists());
}

#[test]
fn a_repair_that_would_delete_another_archive_than_the_one_confirmed_changes_nothing() {
    let setup = fresh();
    let first = setup.stopped_restore();
    let planned = setup.repair_plan();
    // While the question is open, another repair deletes the first
    // archive, the workspace is archived again, and a restore of the
    // second archive stops after its record, so the name needs the same
    // kind of repair of another archive.
    let other = setup.lifecycle().repair(&name(), &planned).unwrap();
    assert!(matches!(other, Repairing::Done(_)), "{other:?}");
    let second = setup.archive();
    assert_ne!(first.id, second.id);
    let plan = setup.restore_plan();
    stop_at("record");
    let stopped = setup.lifecycle().restore(&plan);
    assert!(
        matches!(stopped, Err(Error::Unwritable { .. })),
        "{stopped:?}"
    );
    HOOK.with(|hook| *hook.borrow_mut() = None);
    assert!(matches!(setup.repair_plan(), Repaired::Deleted(_)));

    let outcome = setup.lifecycle().repair(&name(), &planned).unwrap();

    assert!(matches!(outcome, Repairing::Changed), "{outcome:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&second));
    assert!(!setup.pins_of(&second).is_empty());
}

#[test]
fn a_repair_that_renames_holds_both_locks_and_finishes_the_archive_under_the_dated_name() {
    let setup = fresh();
    let older = setup.retired_beside_a_newer_workspace();
    let planned = setup.repair_plan();
    let Repaired::Renamed { archive: dated, .. } = &planned else {
        panic!("{planned:?}");
    };
    let dated_name = dated.name.clone();
    let probe = setup.probe();
    let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
    let log = std::rc::Rc::clone(&seen);
    let held_names = [name(), dated_name.clone()];
    set_hook(move |step| {
        let busy: Vec<bool> = held_names
            .iter()
            .map(|held| probe.lock(held).is_err())
            .collect();
        log.borrow_mut().push((step.to_owned(), busy));
        Ok(())
    });

    let outcome = setup.lifecycle().repair(&name(), &planned).unwrap();

    let Repairing::Done(done) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(done.finish.is_some());
    let seen = seen.borrow();
    assert!(!seen.is_empty());
    assert!(
        seen.iter().all(|(_, busy)| busy.iter().all(|held| *held)),
        "{seen:?}"
    );
    let finished = setup.archives.read(&dated_name).unwrap().unwrap();
    assert_eq!(finished.id, older.id);
    assert_eq!(finished.state, ArchiveState::Archived);
    assert_eq!(setup.archive_record(), None);
    assert!(setup.runs.lock(&name()).is_ok());
    assert!(setup.runs.lock(&dated_name).is_ok());
}

#[test]
fn a_repair_of_a_rename_whose_dated_name_was_taken_after_the_question_changes_nothing() {
    let setup = fresh();
    setup.retired_beside_a_newer_workspace();
    let planned = setup.repair_plan();
    let Repaired::Renamed { archive: dated, .. } = &planned else {
        panic!("{planned:?}");
    };
    let dated_name = dated.name.clone();
    // Another run took the dated name with a workspace.
    let taken = setup.workspaces.create(
        &setup.base.join("mxv"),
        &dated_name,
        &setup.fixture.clone,
        Mode::Change,
    );
    assert!(taken.is_ok(), "{taken:?}");

    let outcome = setup.lifecycle().repair(&name(), &planned);

    assert!(matches!(outcome, Ok(Repairing::Changed)), "{outcome:?}");
    assert!(setup.archive_record().is_some());
}

#[test]
fn a_held_deletion_refuses_the_lock_of_another_name_and_deletes_nothing() {
    let setup = fresh();
    setup.create();
    let archive = setup.archive();
    let other: WorkspaceName = "other".parse().unwrap();
    let lock = setup.runs.lock(&other).unwrap();

    let refused = setup
        .lifecycle()
        .delete_archive_held(&lock, &archive, &VolumesAsked::default());

    assert!(
        matches!(&refused, Err(Error::WrongLock { locked, acted_on }) if *locked == other && *acted_on == name()),
        "{refused:?}"
    );
    assert_eq!(
        setup.archive_record().as_ref().map(|a| &a.name),
        Some(&name())
    );
}

#[test]
fn a_held_finish_refuses_the_lock_of_another_name_and_changes_nothing() {
    let setup = fresh();
    setup.create();
    let mut archive = setup.archive();
    let before = archive.clone();
    let other: WorkspaceName = "other".parse().unwrap();
    let lock = setup.runs.lock(&other).unwrap();

    let refused = setup
        .lifecycle()
        .finish_held(&lock, &mut archive, &VolumesAsked::default());

    assert!(
        matches!(&refused, Err(Error::WrongLock { locked, acted_on }) if *locked == other && *acted_on == name()),
        "{refused:?}"
    );
    assert_eq!(archive, before);
}

#[test]
fn a_refused_archive_abandons_its_archive_and_leaves_the_workspace_live() {
    let setup = fresh();
    let workspace = setup.create();
    let checkout = workspace.checkouts[0].path.clone();
    let late = crate::archive::quarantine(&checkout)
        .unwrap()
        .join("late.txt");
    let plan = setup.archive_plan();
    set_hook(move |step| {
        if step == "moved:app" {
            write(&late, "late\n");
        }
        Ok(())
    });

    let outcome = setup
        .lifecycle()
        .archive(plan, &VolumesAsked::default())
        .unwrap();

    HOOK.with(|hook| *hook.borrow_mut() = None);
    assert!(matches!(outcome, Archived::Refused(_)), "{outcome:?}");
    assert_eq!(setup.archive_record(), None);
    assert!(!setup.archives.dir_of(&name()).exists());
    let stored = setup.workspaces.store.read(&name()).unwrap().unwrap();
    assert_eq!(stored.state(), crate::record::WorkspaceState::Live);
    assert!(checkout.join("late.txt").is_file());
    let pins = git(
        &setup.fixture.clone,
        &["for-each-ref", "--format=%(refname)", "refs/mxv/archives/"],
    );
    assert_eq!(pins, "");
    assert!(
        setup
            .workspaces
            .repair(&setup.archives, &name(), Mode::DryRun)
            .is_ok_and(|repaired| matches!(repaired, Repaired::Unchanged { repair: None }))
    );
}

#[test]
fn a_repair_abandons_an_archive_that_stopped_before_it_deleted_a_checkout() {
    let setup = fresh();
    let workspace = setup.create();
    let archive = setup.stopped_archive(setup.archive_plan(), "root");
    assert!(!setup.pins_of(&archive).is_empty());
    let planned = setup.repair_plan();
    assert!(
        matches!(&planned, Repaired::Abandoned { id, removal: Some(_) } if *id == archive.id),
        "{planned:?}"
    );

    let outcome = setup.lifecycle().repair(&name(), &planned).unwrap();

    let Repairing::Done(done) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(matches!(done.repaired, Repaired::Abandoned { .. }));
    assert_eq!(setup.archive_record(), None);
    assert_eq!(setup.pins_of(&archive), "");
    let stored = setup.workspaces.store.read(&name()).unwrap().unwrap();
    assert_eq!(stored.state(), crate::record::WorkspaceState::Live);
    assert_eq!(stored.id, workspace.id);
    assert!(workspace.checkouts[0].path.is_dir());
    assert!(setup.runs.lock(&name()).is_ok());
}

#[test]
fn a_repair_of_an_abandon_whose_checkout_was_deleted_after_the_question_changes_nothing() {
    let setup = fresh();
    let workspace = setup.create();
    let archive = setup.stopped_archive(setup.archive_plan(), "root");
    let planned = setup.repair_plan();
    assert!(matches!(planned, Repaired::Abandoned { .. }), "{planned:?}");
    std::fs::remove_dir_all(&workspace.checkouts[0].path).unwrap();

    let outcome = setup.lifecycle().repair(&name(), &planned).unwrap();

    assert!(matches!(outcome, Repairing::Changed), "{outcome:?}");
    assert_eq!(setup.archive_record().as_ref(), Some(&archive));
    assert!(!setup.pins_of(&archive).is_empty());
    let replanned = setup.repair_plan();
    assert!(
        matches!(
            &replanned,
            Repaired::Unchanged {
                repair: Some(crate::health::Repair::Archive(_))
            }
        ),
        "{replanned:?}"
    );
}
