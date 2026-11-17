//! A restore that a kill stopped, whose archive record holds the mark of
//! what it made. A panic in a checkpoint stands for the kill, because it
//! unwinds without the undo that a failed step runs.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::archive::RestoreMark;
use crate::health::Repaired;

/// The checkpoints of a restore after which a kill leaves something made,
/// and `mirrors`, before which it leaves nothing.
const KILL_STEPS: [&str; 8] = [
    "mirrors",
    "directory",
    "checkout:app",
    "apply",
    "ignored",
    "kept",
    "root",
    "marker",
];

/// An archive of a workspace with an ignored file, uncommitted work, a root
/// file and a commit that no branch reaches, which a restore puts back
/// as a kept reference.
struct Archived {
    setup: Setup,
    /// The status of the checkout before the archive.
    before: Vec<String>,
    archive: Archive,
    kept: String,
}

fn archived(kind: Kind) -> Archived {
    let setup = setup(kind);
    setup.create();
    setup.ignore_env();
    write(setup.checkout().join(".env"), "SECRET=1\n");
    write(setup.root.join("triage/notes.md"), "keep me\n");
    let detached = leave_detached_commit(&setup.checkout());
    leave_uncommitted_work(&setup.checkout());
    let before = status(&setup.checkout());
    let archive = setup.archive();
    Archived {
        before,
        setup,
        archive,
        kept: format!("{KEPT_NAMESPACE}{detached}"),
    }
}

impl Archived {
    /// Runs a restore that panics at `step`.
    fn kill_at(&self, step: &'static str) {
        set_hook(move |at| {
            assert_ne!(at, step, "killed at {step}");
            Ok(())
        });
        let plan = self.setup.restore_plan();
        let result = catch_unwind(AssertUnwindSafe(|| {
            drop(
                self.setup
                    .workspaces
                    .restore(&self.setup.archives, plan, &self.setup.runs),
            );
        }));
        clear_hook();
        assert!(result.is_err(), "{step}: the run was not killed");
    }

    fn marked(&self) -> Archive {
        self.setup.record().expect("the archive is gone")
    }

    /// Returns the reference of the kept commit in the person's repository.
    fn kept_in_source(&self) -> Option<String> {
        Git.resolve_commit(RefHolder::Repository(&self.setup.fixture.clone), &self.kept)
            .unwrap()
    }

    /// Returns the number of worktrees that the person's repository lists,
    /// its own included.
    fn worktrees(&self) -> usize {
        git(
            &self.setup.fixture.clone,
            &["worktree", "list", "--porcelain"],
        )
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
    }

    fn repair(&self, mode: Mode) -> Repaired {
        self.setup
            .workspaces
            .repair(&self.setup.archives, &name(), mode)
            .unwrap()
    }
}

fn a_killed_restore_is_undone_by_the_next_restore(kind: Kind) {
    for step in KILL_STEPS {
        let made = archived(kind);
        let commit = made.archive.checkouts[0].commit.clone();
        made.kill_at(step);
        let stopped = made.marked();
        assert_eq!(
            stopped.restoring.is_some(),
            step != "mirrors",
            "{kind:?} {step}"
        );
        let restored = made.setup.restore();
        assert_eq!(made.setup.head(), commit, "{kind:?} {step}");
        assert_eq!(
            status(&made.setup.checkout()),
            made.before,
            "{kind:?} {step}"
        );
        assert_eq!(
            restored.put_back[0].ignored,
            ["app/.env"],
            "{kind:?} {step}"
        );
        assert_eq!(
            fs::read_to_string(made.setup.root.join("triage/notes.md")).unwrap(),
            "keep me\n",
            "{kind:?} {step}"
        );
        assert!(made.setup.record().is_none(), "{kind:?} {step}");
        let expected_worktrees = if kind == Kind::Worktree { 2 } else { 1 };
        assert_eq!(made.worktrees(), expected_worktrees, "{kind:?} {step}");
        assert_eq!(
            made.kept_in_source().is_some(),
            kind == Kind::Worktree,
            "{kind:?} {step}: the kept reference"
        );
    }
}

#[test]
fn a_worktree_restore_killed_at_each_step_is_undone_and_finished_by_the_next_restore() {
    a_killed_restore_is_undone_by_the_next_restore(Kind::Worktree);
}

#[test]
fn a_clone_restore_killed_at_each_step_is_undone_and_finished_by_the_next_restore() {
    a_killed_restore_is_undone_by_the_next_restore(Kind::Clone);
}

fn a_killed_restore_is_undone_by_a_repair(kind: Kind) {
    for step in &KILL_STEPS[1..] {
        let made = archived(kind);
        made.kill_at(step);
        let stopped = made.marked();
        let planned = made.repair(Mode::DryRun);
        assert!(
            matches!(&planned, Repaired::Undone(archive) if archive.id == stopped.id),
            "{kind:?} {step}: {planned:?}"
        );
        assert_eq!(made.marked(), stopped, "{kind:?} {step}: a dry run wrote");
        assert!(made.setup.root.join("triage").exists(), "{kind:?} {step}");
        let done = made.repair(Mode::Change);
        assert!(matches!(done, Repaired::Undone(_)), "{kind:?} {step}");
        assert_eq!(made.marked(), made.archive, "{kind:?} {step}: the archive");
        assert!(!made.setup.root.join("triage").exists(), "{kind:?} {step}");
        assert_eq!(made.kept_in_source(), None, "{kind:?} {step}");
        assert_eq!(made.worktrees(), 1, "{kind:?} {step}");
        assert!(
            made.setup.ignored_tar(&made.archive).is_file(),
            "{kind:?} {step}: the tar"
        );
        let again = made.repair(Mode::Change);
        assert!(
            matches!(again, Repaired::Unchanged { repair: None }),
            "{kind:?} {step}: {again:?}"
        );
        made.setup.restore();
    }
}

#[test]
fn a_worktree_restore_killed_at_each_step_is_undone_by_a_repair() {
    a_killed_restore_is_undone_by_a_repair(Kind::Worktree);
}

#[test]
fn a_clone_restore_killed_at_each_step_is_undone_by_a_repair() {
    a_killed_restore_is_undone_by_a_repair(Kind::Clone);
}

#[test]
fn a_repair_that_stopped_after_it_deleted_the_work_runs_again() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let made = archived(kind);
        made.kill_at("root");
        let stopped = made.marked();
        made.repair(Mode::Change);
        // A kill between the undo and the write that clears the mark leaves
        // the mark over what is already gone.
        made.setup.archives.write(&stopped).unwrap();
        assert!(matches!(made.repair(Mode::Change), Repaired::Undone(_)));
        assert_eq!(made.marked(), made.archive, "{kind:?}");
        assert_eq!(made.worktrees(), 1, "{kind:?}");
        made.setup.restore();
    }
}

#[test]
fn a_mark_names_the_kept_reference_before_the_run_makes_it() {
    let made = archived(Kind::Worktree);
    made.kill_at("kept");
    let RestoreMark(mark) = made.marked().restoring.unwrap();
    assert!(
        mark.refs
            .iter()
            .any(|(_, reference)| *reference == made.kept),
        "{mark:?}"
    );
}

#[test]
fn a_kept_reference_that_the_repository_held_before_survives_an_undone_restore() {
    let made = archived(Kind::Worktree);
    let source = made.setup.fixture.clone.clone();
    let commit = made.kept.trim_start_matches(KEPT_NAMESPACE).to_owned();
    git(&source, &["update-ref", &made.kept, &commit]);
    made.kill_at("kept");
    made.repair(Mode::Change);
    assert_eq!(made.kept_in_source(), Some(commit));
}

#[test]
fn a_directory_that_the_person_made_is_not_touched_by_a_restore() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let archive = setup.archive();
    let theirs = setup.root.join("triage");
    fs::create_dir_all(&theirs).unwrap();
    write(theirs.join("mine.txt"), "mine\n");
    assert!(matches!(
        setup.restore_refusal(),
        RestoreRefusal::NameTaken { record: false, .. }
    ));
    // A mark that says the run had not made the directory yet does not
    // cover a directory that holds files.
    let mut marked = archive;
    marked.restoring = Some(RestoreMark::default());
    setup.archives.write(&marked).unwrap();
    assert!(matches!(
        setup.restore_refusal(),
        RestoreRefusal::NameTaken { record: false, .. }
    ));
    assert_eq!(
        fs::read_to_string(theirs.join("mine.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn an_empty_directory_left_before_the_mark_said_so_is_removed_and_the_restore_finishes() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let mut archive = setup.archive();
    archive.restoring = Some(RestoreMark::default());
    setup.archives.write(&archive).unwrap();
    fs::create_dir_all(setup.root.join("triage")).unwrap();
    setup.restore();
    assert!(setup.checkout().is_dir());
    assert!(setup.record().is_none());
}

#[test]
fn a_directory_that_the_mark_says_the_run_made_is_deleted_with_its_files() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let mut archive = setup.archive();
    let mut mark = RestoreMark::default();
    mark.0.directory = true;
    archive.restoring = Some(mark);
    setup.archives.write(&archive).unwrap();
    let left = setup.root.join("triage");
    fs::create_dir_all(&left).unwrap();
    write(left.join("partial.txt"), "left\n");
    setup.restore();
    assert!(!left.join("partial.txt").exists());
    assert!(setup.checkout().is_dir());
}

#[test]
fn a_record_without_a_mark_has_no_restoring_key_and_a_mark_round_trips() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let mut archive = setup.archive();
    let file = setup
        .archives
        .dir_of(&name())
        .join(crate::archive::RECORD_FILE);
    assert!(!fs::read_to_string(&file).unwrap().contains("restoring"));
    let mut mark = RestoreMark::default();
    mark.0.directory = true;
    mark.0
        .refs
        .push((PathBuf::from("/repo"), "refs/mxv/kept/abc".to_owned()));
    archive.restoring = Some(mark);
    setup.archives.write(&archive).unwrap();
    assert_eq!(setup.record(), Some(archive));
}
