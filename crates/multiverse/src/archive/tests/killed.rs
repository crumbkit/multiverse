//! A restore that a kill stopped, whose archive record holds the mark of
//! what it made. A panic in a checkpoint stands for the kill, because it
//! unwinds without the undo that a failed step runs.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::archive::{KeptRef, RestoreMark};
use crate::health::{self, Repaired};

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
            matches!(&planned, Repaired::Undone { archive, .. } if archive.id == stopped.id),
            "{kind:?} {step}: {planned:?}"
        );
        assert_eq!(made.marked(), stopped, "{kind:?} {step}: a dry run wrote");
        assert!(made.setup.root.join("triage").exists(), "{kind:?} {step}");
        let done = made.repair(Mode::Change);
        assert!(matches!(done, Repaired::Undone { .. }), "{kind:?} {step}");
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
        assert!(matches!(made.repair(Mode::Change), Repaired::Undone { .. }));
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
            .any(|(_, reference)| reference.as_str() == made.kept),
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
fn a_directory_that_the_mark_says_the_run_made_is_deleted_with_the_half_written_files() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let mut archive = setup.archive();
    let mut mark = RestoreMark::default();
    mark.0.directory = true;
    mark.0.agents = true;
    archive.restoring = Some(mark);
    setup.archives.write(&archive).unwrap();
    let left = setup.root.join("triage");
    fs::create_dir_all(&left).unwrap();
    write(left.join("AGENTS.md"), "half\n");
    write(left.join(".AGENTS.md.99999.tmp"), "half\n");
    setup.restore();
    assert!(!left.join(".AGENTS.md.99999.tmp").exists());
    assert!(setup.checkout().is_dir());
}

#[test]
fn a_half_written_file_with_crumbkits_temporary_name_is_deleted_with_the_directory_the_run_made() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let mut archive = setup.archive();
    let mut mark = RestoreMark::default();
    mark.0.directory = true;
    mark.0.agents = true;
    archive.restoring = Some(mark);
    setup.archives.write(&archive).unwrap();
    let left = setup.root.join("triage");
    fs::create_dir_all(&left).unwrap();
    write(left.join(".AGENTS.md.99999.0.tmp"), "half\n");
    setup.restore();
    assert!(!left.join(".AGENTS.md.99999.0.tmp").exists());
    assert!(setup.checkout().is_dir());
}

/// Returns the paths that a refusal of an undo names, or panics with what
/// the undo returned instead.
fn refused_paths<T: std::fmt::Debug>(result: Result<T, Error>) -> Vec<String> {
    match result {
        Err(Error::StoppedRestoreHoldsOtherFiles { paths, .. }) => paths,
        other => panic!("{other:?}"),
    }
}

/// Kills a restore after it wrote the root files and puts a file of the
/// person's into the directory that the restore made.
fn killed_beside_a_file_of_the_persons(kind: Kind) -> (Archived, Archive, PathBuf) {
    let made = archived(kind);
    made.kill_at("root");
    let stopped = made.marked();
    let theirs = made.setup.root.join("triage/mine.txt");
    write(&theirs, "mine\n");
    (made, stopped, theirs)
}

#[test]
fn a_repair_refuses_a_made_directory_that_holds_a_file_the_restore_did_not_make() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let (made, stopped, theirs) = killed_beside_a_file_of_the_persons(kind);
        for mode in [Mode::DryRun, Mode::Change] {
            let result = made
                .setup
                .workspaces
                .repair(&made.setup.archives, &name(), mode);
            assert_eq!(refused_paths(result), ["mine.txt"], "{kind:?} {mode:?}");
        }
        assert_eq!(fs::read_to_string(&theirs).unwrap(), "mine\n", "{kind:?}");
        assert_eq!(made.marked(), stopped, "{kind:?}: the mark");
        assert!(made.setup.checkout().is_dir(), "{kind:?}: the checkout");
        // Once the person moves the file away, the repair undoes the run.
        fs::remove_file(&theirs).unwrap();
        assert!(matches!(made.repair(Mode::Change), Repaired::Undone { .. }));
        assert!(!made.setup.root.join("triage").exists(), "{kind:?}");
        made.setup.restore();
    }
}

#[test]
fn a_restore_refuses_a_made_directory_that_holds_a_file_the_stopped_restore_did_not_make() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let made = archived(kind);
        made.kill_at("root");
        let stopped = made.marked();
        // The plan comes first, so the restore's own second plan refuses.
        let plan = made.setup.restore_plan();
        let theirs = made.setup.root.join("triage/mine.txt");
        write(&theirs, "mine\n");
        let planned = made.setup.workspaces.plan_restore(
            &made.setup.archives,
            &name(),
            RestoreOptions::default(),
        );
        assert_eq!(refused_paths(planned), ["mine.txt"], "{kind:?}");
        let restored = made
            .setup
            .workspaces
            .restore(&made.setup.archives, plan, &made.setup.runs);
        assert_eq!(refused_paths(restored), ["mine.txt"], "{kind:?}");
        assert_eq!(fs::read_to_string(&theirs).unwrap(), "mine\n", "{kind:?}");
        assert_eq!(made.marked(), stopped, "{kind:?}: the mark");
    }
}

#[test]
fn removing_an_archive_refuses_a_made_directory_that_holds_a_file_the_restore_did_not_make() {
    use crate::lifecycle::{Lifecycle, VolumesAsked};
    for kind in [Kind::Worktree, Kind::Clone] {
        let (made, stopped, theirs) = killed_beside_a_file_of_the_persons(kind);
        let result = Lifecycle::new(
            &made.setup.workspaces,
            &made.setup.archives,
            &made.setup.runs,
        )
        .delete_archive(&stopped, &VolumesAsked::default());
        assert_eq!(refused_paths(result), ["mine.txt"], "{kind:?}");
        assert_eq!(fs::read_to_string(&theirs).unwrap(), "mine\n", "{kind:?}");
        assert_eq!(made.marked(), stopped, "{kind:?}: the archive and its mark");
    }
}

#[test]
fn the_refusal_names_each_path_the_mark_does_not_account_for_and_the_directory() {
    let made = archived(Kind::Worktree);
    made.kill_at("root");
    let dir = made.setup.root.join("triage");
    write(dir.join("mine.txt"), "mine\n");
    fs::create_dir_all(dir.join("notes/deep")).unwrap();
    write(dir.join("notes/deep/a.txt"), "a\n");
    // A file inside a checkout's folder belongs to the run.
    write(made.setup.checkout().join("scratch.txt"), "scratch\n");
    let error = made
        .setup
        .workspaces
        .repair(&made.setup.archives, &name(), Mode::Change)
        .unwrap_err();
    let Error::StoppedRestoreHoldsOtherFiles {
        name: of,
        path,
        paths,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(of, &name());
    assert_eq!(path, &dir);
    assert_eq!(paths, &["mine.txt", "notes"]);
    let text = error.to_string();
    assert!(
        text.contains("mine.txt") && text.contains("notes"),
        "{text}"
    );
    assert!(text.contains(&dir.display().to_string()), "{text}");
}

#[test]
fn a_regular_file_where_the_mark_names_a_checkout_or_a_root_directory_is_the_persons() {
    let made = archived(Kind::Worktree);
    made.kill_at("root");
    let dir = made.setup.root.join("triage");
    // The archive gains a root directory, and the person replaces it with a
    // file of the same name.
    let mut stopped = made.marked();
    stopped.root.push(crate::archive::RootEntry {
        path: "docs".to_owned(),
        kind: crate::archive::RootKind::Directory { mode: 0o755 },
    });
    made.setup.archives.write(&stopped).unwrap();
    write(dir.join("docs"), "mine\n");
    // A checkout's folder that the person replaced with a file.
    git(
        &made.setup.fixture.clone,
        &[
            "worktree",
            "remove",
            "--force",
            made.setup.checkout().to_str().unwrap(),
        ],
    );
    write(made.setup.checkout(), "mine\n");
    let paths = refused_paths(made.setup.workspaces.repair(
        &made.setup.archives,
        &name(),
        Mode::Change,
    ));
    assert_eq!(paths, ["app", "docs"]);
    assert_eq!(fs::read_to_string(dir.join("docs")).unwrap(), "mine\n");
    assert_eq!(fs::read_to_string(made.setup.checkout()).unwrap(), "mine\n");
}

#[test]
fn a_file_the_person_puts_inside_a_made_checkout_goes_with_the_undone_restore() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let made = archived(kind);
        made.kill_at("root");
        write(made.setup.checkout().join("scratch.txt"), "scratch\n");
        assert!(matches!(made.repair(Mode::Change), Repaired::Undone { .. }));
        assert!(!made.setup.root.join("triage").exists(), "{kind:?}");
    }
}

#[test]
fn a_root_file_that_the_mark_has_not_reached_is_the_persons() {
    let made = archived(Kind::Worktree);
    // The run made the checkout but had not yet written the root files, so
    // a notes.md there is not the run's.
    made.kill_at("apply");
    write(made.setup.root.join("triage/notes.md"), "mine\n");
    write(made.setup.root.join("triage/AGENTS.md"), "mine\n");
    let paths = refused_paths(made.setup.workspaces.repair(
        &made.setup.archives,
        &name(),
        Mode::Change,
    ));
    assert_eq!(paths, ["AGENTS.md", "notes.md"]);
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
    mark.0.refs.push((
        archive.checkouts[0].source.clone(),
        KeptRef::of_commit("abc").unwrap(),
    ));
    archive.restoring = Some(mark);
    setup.archives.write(&archive).unwrap();
    assert_eq!(setup.record(), Some(archive));
}

#[test]
fn a_removed_archive_that_holds_a_mark_has_what_the_dead_restore_made_deleted() {
    use crate::lifecycle::{ArchiveDeleted, Lifecycle, VolumesAsked};
    for kind in [Kind::Worktree, Kind::Clone] {
        let made = archived(kind);
        made.kill_at("kept");
        assert!(made.marked().restoring.is_some());
        let outcome = Lifecycle::new(
            &made.setup.workspaces,
            &made.setup.archives,
            &made.setup.runs,
        )
        .delete_archive(&made.marked(), &VolumesAsked::default())
        .unwrap();
        assert!(matches!(outcome, ArchiveDeleted::Done(_)), "{kind:?}");
        assert!(made.setup.record().is_none(), "{kind:?}");
        assert!(!made.setup.root.join("triage").exists(), "{kind:?}");
        assert_eq!(made.kept_in_source(), None, "{kind:?}");
        assert_eq!(made.worktrees(), 1, "{kind:?}");
    }
}

#[test]
fn a_killed_restore_deletes_the_branch_it_created_while_it_is_at_the_archived_commit() {
    let made = archived(Kind::Worktree);
    let source = made.setup.fixture.clone.clone();
    git(&source, &["branch", "-D", "triage"]);
    made.kill_at("apply");
    let RestoreMark(mark) = made.marked().restoring.unwrap();
    assert_eq!(mark.branches.len(), 1, "{mark:?}");
    assert!(
        Git.branch_tip(&source, "triage").unwrap().is_some(),
        "the killed restore made the branch"
    );
    made.repair(Mode::Change);
    assert_eq!(Git.branch_tip(&source, "triage").unwrap(), None);
    assert_eq!(made.worktrees(), 1);
    made.setup.restore();
    assert_eq!(branch(&made.setup.checkout()), "triage");
}

#[test]
fn a_killed_restore_keeps_a_branch_that_has_moved_since_it_was_created() {
    let made = archived(Kind::Worktree);
    let source = made.setup.fixture.clone.clone();
    git(&source, &["branch", "-D", "triage"]);
    made.kill_at("apply");
    git(
        &made.setup.checkout(),
        &[
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@b.c",
            "commit",
            "--allow-empty",
            "-m",
            "later",
        ],
    );
    let moved = Git.branch_tip(&source, "triage").unwrap();
    made.repair(Mode::Change);
    assert_eq!(Git.branch_tip(&source, "triage").unwrap(), moved);
}

/// The checkpoints of an archive after which a kill leaves every checkout
/// in place, so a repair abandons the archive.
const UNTOUCHED_STEPS: [&str; 7] = [
    "state",
    "record",
    "wip",
    "pins",
    "ignored",
    "root",
    "before-move:app",
];

/// A workspace with an ignored file, uncommitted work and a root file,
/// whose archive a panic stopped.
struct Stopped {
    setup: Setup,
    /// The status of the checkout before the archive.
    before: Vec<String>,
    /// The head of the checkout before the archive.
    head: String,
}

impl Stopped {
    fn new(kind: Kind, step: &'static str) -> Self {
        let setup = setup(kind);
        setup.create();
        setup.ignore_env();
        write(setup.checkout().join(".env"), "SECRET=1\n");
        write(setup.root.join("triage/notes.md"), "keep me\n");
        leave_uncommitted_work(&setup.checkout());
        let before = status(&setup.checkout());
        let head = setup.head();
        let plan = setup.plan(ArchiveOptions::default());
        kill(step, || {
            drop(setup.workspaces.archive(&setup.archives, plan));
        });
        Self {
            setup,
            before,
            head,
        }
    }

    fn repair(&self, mode: Mode) -> Repaired {
        self.setup
            .workspaces
            .repair(&self.setup.archives, &name(), mode)
            .unwrap()
    }

    fn state(&self) -> crate::record::WorkspaceState {
        self.setup
            .workspaces
            .store
            .read(&name())
            .unwrap()
            .unwrap()
            .state()
    }

    /// Returns every pin of every archive in the repository that holds
    /// the pins of this kind of workspace.
    fn pins(&self) -> String {
        let workspace = self.setup.workspaces.store.read(&name()).unwrap().unwrap();
        let holder = workspace.checkouts[0]
            .mirror()
            .map_or_else(|| self.setup.fixture.clone.clone(), Path::to_path_buf);
        git(
            &holder,
            &["for-each-ref", "--format=%(refname)", "refs/mxv/archives/"],
        )
    }

    /// Checks that the workspace is live and whole, and that no archive
    /// record, pin or tar is left.
    fn assert_abandoned(&self, what: &str) {
        assert_eq!(self.state(), crate::record::WorkspaceState::Live, "{what}");
        assert!(self.setup.record().is_none(), "{what}: the record");
        assert!(
            !self.setup.archives.dir_of(&name()).exists(),
            "{what}: the record's directory"
        );
        assert_eq!(self.pins(), "", "{what}: the pins");
        assert!(
            fs::read_dir(self.setup.base.join("data/archives"))
                .map_or(true, |mut entries| entries.next().is_none()),
            "{what}: the tar"
        );
        assert_eq!(self.setup.head(), self.head, "{what}");
        assert_eq!(status(&self.setup.checkout()), self.before, "{what}");
        assert_eq!(
            fs::read_to_string(self.setup.checkout().join(".env")).unwrap(),
            "SECRET=1\n",
            "{what}"
        );
        assert_eq!(
            fs::read_to_string(self.setup.root.join("triage/notes.md")).unwrap(),
            "keep me\n",
            "{what}"
        );
    }
}

/// Runs `run` with a checkpoint that panics at `step`, as a kill there
/// would stop it.
fn kill(step: &'static str, run: impl FnOnce()) {
    set_hook(move |at| {
        assert_ne!(at, step, "killed at {step}");
        Ok(())
    });
    let result = catch_unwind(AssertUnwindSafe(run));
    clear_hook();
    assert!(result.is_err(), "{step}: the run was not killed");
}

fn a_killed_archive_that_deleted_no_checkout_is_abandoned_by_a_repair(kind: Kind) {
    for step in UNTOUCHED_STEPS {
        let what = format!("{kind:?} {step}");
        let stopped = Stopped::new(kind, step);
        let record = stopped.setup.record();
        assert_eq!(record.is_some(), step != "state", "{what}");
        if let Some(archive) = &record {
            let health = health::classify(
                &stopped.setup.workspaces.backend,
                &stopped.setup.workspaces.store.list().unwrap(),
                &stopped.setup.archives.list().unwrap(),
            );
            assert_eq!(
                health.workspaces[0].problem,
                health::Problem::ArchiveNotStarted,
                "{what}"
            );
            let planned = stopped.repair(Mode::DryRun);
            assert!(
                matches!(&planned, Repaired::Abandoned { id, removal: Some(_) } if *id == archive.id),
                "{what}: {planned:?}"
            );
            assert_eq!(stopped.setup.record().as_ref(), Some(archive), "{what}");
        }
        let done = stopped.repair(Mode::Change);
        assert!(
            matches!(done, Repaired::Abandoned { .. }),
            "{what}: {done:?}"
        );
        stopped.assert_abandoned(&what);
        let again = stopped.repair(Mode::Change);
        assert!(
            matches!(again, Repaired::Unchanged { repair: None }),
            "{what}: {again:?}"
        );
        stopped.setup.archive();
        stopped.setup.restore();
        assert_eq!(stopped.setup.head(), stopped.head, "{what}");
    }
}

#[test]
fn a_worktree_archive_killed_before_it_deleted_a_checkout_is_abandoned_by_a_repair() {
    a_killed_archive_that_deleted_no_checkout_is_abandoned_by_a_repair(Kind::Worktree);
}

#[test]
fn a_clone_archive_killed_before_it_deleted_a_checkout_is_abandoned_by_a_repair() {
    a_killed_archive_that_deleted_no_checkout_is_abandoned_by_a_repair(Kind::Clone);
}

#[test]
fn an_abandon_killed_after_each_step_is_finished_by_a_second_repair() {
    for kind in [Kind::Worktree, Kind::Clone] {
        for step in ["unpinned", "unrecorded", "abandoned"] {
            let what = format!("{kind:?} {step}");
            let stopped = Stopped::new(kind, "root");
            kill(step, || {
                drop(stopped.repair(Mode::Change));
            });
            let left = stopped.setup.record().is_some();
            assert_eq!(left, step == "unpinned", "{what}: the record");
            let retiring = stopped.state() != crate::record::WorkspaceState::Live;
            assert_eq!(retiring, step != "abandoned", "{what}: the state");
            let again = stopped.repair(Mode::Change);
            let expected = match step {
                "unpinned" => matches!(
                    again,
                    Repaired::Abandoned {
                        removal: Some(_),
                        ..
                    }
                ),
                "unrecorded" => matches!(again, Repaired::Abandoned { removal: None, .. }),
                _ => matches!(again, Repaired::Unchanged { repair: None }),
            };
            assert!(expected, "{what}: {again:?}");
            stopped.assert_abandoned(&what);
        }
    }
}

#[test]
fn a_repair_keeps_an_archive_that_quarantined_a_checkout_for_mxv_archive() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let stopped = Stopped::new(kind, "moved:app");
        let archive = stopped.setup.record().unwrap();
        let planned = stopped.repair(Mode::Change);
        assert!(
            matches!(
                planned,
                Repaired::Unchanged {
                    repair: Some(health::Repair::Archive(_))
                }
            ),
            "{kind:?}: {planned:?}"
        );
        assert_eq!(stopped.setup.record(), Some(archive), "{kind:?}");
        assert!(
            stopped
                .setup
                .workspaces
                .abandon_archive(
                    &stopped.setup.archives,
                    stopped.setup.record().unwrap(),
                    Mode::Change
                )
                .unwrap()
                .is_none()
        );
        assert!(stopped.setup.record().is_some(), "{kind:?}");
    }
}

#[test]
fn a_repair_keeps_the_archive_of_a_workspace_without_checkouts_that_deleted_its_root_files() {
    let setup = setup(Kind::Worktree);
    let created = setup
        .workspaces
        .create_repos(&setup.root, &name(), &[], Mode::Change, None)
        .unwrap();
    assert!(matches!(created, NewOutcome::Created(_)), "{created:?}");
    write(setup.root.join("triage/notes.md"), "keep me\n");
    let plan = setup.plan(ArchiveOptions::default());
    kill("delete", || {
        drop(setup.workspaces.archive(&setup.archives, plan));
    });
    assert!(!setup.root.join("triage/notes.md").exists());
    let archive = setup.record().unwrap();
    for mode in [Mode::DryRun, Mode::Change] {
        let repaired = setup
            .workspaces
            .repair(&setup.archives, &name(), mode)
            .unwrap();
        assert!(
            !matches!(repaired, Repaired::Abandoned { .. }),
            "{mode:?}: {repaired:?}"
        );
    }
    assert_eq!(setup.record(), Some(archive));
    setup.archive();
    setup.restore();
    assert_eq!(
        fs::read_to_string(setup.root.join("triage/notes.md")).unwrap(),
        "keep me\n"
    );
}

/// Rewrites the first entry of the stopped restore's mark in the record on
/// disk, which `edit` changes as JSON.
fn edit_mark(made: &Archived, edit: impl FnOnce(&mut serde_json::Value)) {
    edit_record(made, |mark| edit(&mut mark["refs"][0]));
}

/// Rewrites the stopped restore's mark as a whole.
fn edit_record(made: &Archived, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = made.setup.archives.dir_of(&name()).join("archive.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    edit(&mut record["restoring"]);
    write(&path, serde_json::to_string(&record).unwrap());
}

#[test]
fn a_mark_that_names_a_branch_is_refused_as_corrupt_and_the_branch_stays() {
    let made = archived(Kind::Worktree);
    let source = made.setup.fixture.clone.clone();
    git(&source, &["branch", "precious"]);
    made.kill_at("kept");
    edit_mark(&made, |entry| entry[1] = "refs/heads/precious".into());
    let repaired = made
        .setup
        .workspaces
        .repair(&made.setup.archives, &name(), Mode::Change);
    assert!(
        matches!(repaired, Err(Error::CorruptRecord { .. })),
        "{repaired:?}"
    );
    assert!(git(&source, &["branch", "--list", "precious"]).contains("precious"));
}

#[test]
fn a_mark_that_climbs_out_of_the_kept_namespace_is_refused_as_corrupt() {
    let made = archived(Kind::Worktree);
    made.kill_at("kept");
    edit_mark(&made, |entry| {
        entry[1] = "refs/mxv/kept/../../heads/main".into();
    });
    assert!(matches!(
        made.setup.archives.read(&name()),
        Err(Error::CorruptRecord { .. })
    ));
}

#[test]
fn a_mark_that_names_a_repository_that_is_no_checkouts_source_is_refused_as_corrupt() {
    let made = archived(Kind::Worktree);
    made.kill_at("kept");
    edit_mark(&made, |entry| entry[0] = "/somewhere/else".into());
    assert!(matches!(
        made.setup.archives.read(&name()),
        Err(Error::CorruptRecord { .. })
    ));
}

#[test]
fn a_mark_that_names_another_start_for_the_branch_is_refused_and_the_branch_stays() {
    let made = archived(Kind::Worktree);
    let source = made.setup.fixture.clone.clone();
    made.kill_at("kept");
    let branch = made.marked().checkouts[0].branch.clone();
    let start = git(&source, &["rev-parse", &branch]).trim().to_owned();
    git(&source, &["branch", "-f", &branch, "HEAD"]);
    let other = git(&source, &["rev-parse", &branch]).trim().to_owned();
    edit_record(&made, |mark| mark["branches"] = serde_json::json!([other]));
    let repaired = made
        .setup
        .workspaces
        .repair(&made.setup.archives, &name(), Mode::Change);
    assert!(
        matches!(repaired, Err(Error::CorruptRecord { .. })),
        "{repaired:?}"
    );
    assert!(!git(&source, &["branch", "--list", &branch]).is_empty(), "{start}");
}
