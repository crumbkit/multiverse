use std::cell::RefCell;
use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;
use crate::git::Git;
use crate::runs::Runs;
use crate::store::Store;
use crate::testing::{Fixture, fixture, fixture_named, git, write};
use crate::workspaces::{Mode, NewOutcome, Repo};

type Hook = Box<dyn FnMut(&str) -> Result<(), Error>>;

thread_local! {
    static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

/// Runs the hook of this test's thread at `step`.
pub(super) fn at_checkpoint(step: &str) -> Result<(), Error> {
    HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(run) => run(step),
        None => Ok(()),
    })
}

fn set_hook(run: impl FnMut(&str) -> Result<(), Error> + 'static) {
    HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(run)));
}

fn clear_hook() {
    HOOK.with(|hook| *hook.borrow_mut() = None);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Worktree,
    Clone,
}

struct Setup {
    fixture: Fixture,
    root: PathBuf,
    base: PathBuf,
    _dir: TempDir,
    workspaces: Workspaces<Git>,
    archives: Archives,
    runs: Runs,
}

fn setup(kind: Kind) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut workspaces = Workspaces::new(Store::new(base.join("state")), Git);
    if kind == Kind::Clone {
        workspaces = workspaces.with_mirrors(base.join("mirrors"));
    }
    Setup {
        fixture: fixture(),
        root: base.join("mxv"),
        archives: Archives::new(base.join("archives")),
        runs: Runs::new(&base.join("runs")),
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
            .create(&self.root, &name(), &self.fixture.clone, Mode::Change)
            .unwrap()
        {
            NewOutcome::Created(workspace) => workspace,
            other => panic!("{other:?}"),
        }
    }

    fn checkout(&self) -> PathBuf {
        self.root.join("triage/app")
    }

    fn head(&self) -> String {
        git(&self.checkout(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned()
    }

    fn planned(&self, options: ArchiveOptions) -> ArchivePlanned {
        self.workspaces
            .plan_archive(&self.archives, &name(), options, Mode::Change)
            .unwrap()
    }

    fn plan(&self, options: ArchiveOptions) -> ArchivePlan {
        match self.planned(options) {
            ArchivePlanned::Ready(plan) => *plan,
            other => panic!("{other:?}"),
        }
    }

    fn refusal(&self, options: ArchiveOptions) -> ArchiveRefusal {
        match self.planned(options) {
            ArchivePlanned::Refused(refusal) => refusal,
            other => panic!("{other:?}"),
        }
    }

    fn archive_with(&self, options: ArchiveOptions) -> Archive {
        match self
            .workspaces
            .archive(&self.archives, self.plan(options))
            .unwrap()
        {
            ArchiveOutcome::Retired { mut archive, .. } => {
                self.workspaces
                    .finish_archive(&self.archives, &mut archive)
                    .unwrap();
                *archive
            }
            ArchiveOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn archive(&self) -> Archive {
        self.archive_with(ArchiveOptions::default())
    }

    fn restore_plan(&self) -> RestorePlan {
        match self
            .workspaces
            .plan_restore(&self.archives, &name())
            .unwrap()
        {
            RestorePlanned::Ready(plan) => *plan,
            RestorePlanned::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn restore(&self) -> Restored {
        match self
            .workspaces
            .restore(&self.archives, self.restore_plan(), &self.runs)
            .unwrap()
        {
            RestoreOutcome::Restored(restored) => *restored,
            RestoreOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn restore_refusal(&self) -> RestoreRefusal {
        match self
            .workspaces
            .plan_restore(&self.archives, &name())
            .unwrap()
        {
            RestorePlanned::Refused(refusal) => refusal,
            RestorePlanned::Ready(plan) => panic!("{plan:?}"),
        }
    }

    fn record(&self) -> Option<Archive> {
        self.archives.read(&name()).unwrap()
    }

    fn ignore_env(&self) {
        write(self.checkout().join(".gitignore"), ".env\n");
        git(&self.checkout(), &["add", ".gitignore"]);
        git(&self.checkout(), &["commit", "--quiet", "-m", "Ignore"]);
    }

    fn commit_locally(&self) -> String {
        write(self.checkout().join("local.txt"), "only here\n");
        git(&self.checkout(), &["add", "local.txt"]);
        git(&self.checkout(), &["commit", "--quiet", "-m", "Local"]);
        self.head()
    }
}

fn branch(checkout: &Path) -> String {
    git(checkout, &["branch", "--show-current"])
        .trim()
        .to_owned()
}

fn wip() -> ArchiveOptions {
    ArchiveOptions::new(true, false, false)
}

fn force() -> ArchiveOptions {
    ArchiveOptions::new(false, true, false)
}

fn archives_and_restores_at_the_same_commit_and_branch(kind: Kind) {
    let setup = setup(kind);
    let created = setup.create();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    assert_eq!(archive.state, ArchiveState::Archived);
    assert_eq!(archive.checkouts[0].commit, commit);
    assert_eq!(archive.checkouts[0].local_only, vec![commit.clone()]);
    assert!(!setup.root.join("triage").exists());
    assert!(setup.workspaces.store.read(&name()).unwrap().is_none());
    let restored = setup.restore();
    assert_eq!(setup.head(), commit);
    assert_eq!(branch(&setup.checkout()), "triage");
    assert_eq!(restored.workspace.id, created.id);
    assert!(created.id.is_some());
    assert_eq!(restored.workspace.kind, created.kind);
    assert_eq!(
        fs::read_to_string(setup.checkout().join("local.txt")).unwrap(),
        "only here\n"
    );
    assert!(setup.root.join("triage/AGENTS.md").is_file());
    assert!(setup.record().is_none(), "the restore keeps no archive");
    assert_eq!(
        setup.workspaces.store.read(&name()).unwrap().unwrap().id,
        created.id
    );
}

#[test]
fn a_worktree_workspace_comes_back_at_its_commit_and_branch_with_a_commit_no_remote_holds() {
    archives_and_restores_at_the_same_commit_and_branch(Kind::Worktree);
}

#[test]
fn a_clone_workspace_comes_back_at_its_commit_and_branch_with_a_commit_no_remote_holds() {
    archives_and_restores_at_the_same_commit_and_branch(Kind::Clone);
}

#[test]
fn a_workspace_of_two_repositories_comes_back_with_both_checkouts_and_its_record_version() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        let other = fixture_named("web");
        let repos = [Repo::new(&setup.fixture.clone), Repo::new(&other.clone)];
        let created = match setup
            .workspaces
            .create_repos(&setup.root, &name(), &repos, Mode::Change, None)
            .unwrap()
        {
            NewOutcome::Created(workspace) => workspace,
            other => panic!("{other:?}"),
        };
        let web = setup.root.join("triage/web");
        git(&web, &["commit", "--quiet", "--allow-empty", "-m", "Local"]);
        let commit = git(&web, &["rev-parse", "HEAD"]);
        let record = setup.base.join("state/triage.json");
        let before = fs::read_to_string(&record).unwrap();
        setup.archive();
        assert!(!record.exists());
        setup.restore();
        assert_eq!(git(&web, &["rev-parse", "HEAD"]), commit);
        assert_eq!(branch(&setup.checkout()), "triage");
        let found = setup.workspaces.store.read(&name()).unwrap().unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.checkouts.len(), 2);
        let version = |text: &str| text.lines().nth(1).unwrap().to_owned();
        assert_eq!(
            version(&fs::read_to_string(&record).unwrap()),
            version(&before)
        );
    }
}

#[test]
fn the_workspace_manifest_comes_back_from_the_archive_with_its_bytes() {
    let setup = setup(Kind::Clone);
    setup.create();
    let manifest = setup.root.join("triage/mxv.toml");
    let text = "[processes.web]\nargv = [\"sleep\", \"600\"]\n";
    write(&manifest, text);
    let archive = setup.archive();
    assert!(archive.manifest.is_some());
    assert!(archive.root.is_empty(), "{:?}", archive.root);
    assert!(!manifest.exists());
    setup.restore();
    assert_eq!(fs::read_to_string(&manifest).unwrap(), text);
}

#[test]
fn a_restored_clone_tracks_the_branch_that_it_tracked_when_it_was_archived() {
    let setup = setup(Kind::Clone);
    setup.create();
    git(
        &setup.checkout(),
        &["config", "branch.triage.remote", "origin"],
    );
    git(
        &setup.checkout(),
        &["config", "branch.triage.merge", "refs/heads/triage-remote"],
    );
    let archive = setup.archive();
    assert_eq!(
        archive.checkouts[0].upstream.as_deref(),
        Some("refs/heads/triage-remote")
    );
    setup.restore();
    assert_eq!(
        git(&setup.checkout(), &["config", "branch.triage.merge"]).trim(),
        "refs/heads/triage-remote"
    );
}

#[test]
fn a_detached_checkout_comes_back_detached_at_its_commit() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        let commit = setup.commit_locally();
        git(&setup.checkout(), &["switch", "--quiet", "--detach"]);
        let archive = setup.archive();
        assert!(archive.checkouts[0].detached);
        setup.restore();
        assert_eq!(setup.head(), commit);
        assert_eq!(branch(&setup.checkout()), "");
    }
}

#[test]
fn each_kind_of_unsaved_work_refuses_the_archive_and_names_itself() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write(checkout.join("stashed.txt"), "stash\n");
    git(&checkout, &["add", "stashed.txt"]);
    git(&checkout, &["stash", "--quiet"]);
    git(&checkout, &["switch", "--quiet", "-c", "side"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Side"],
    );
    let side = git(&checkout, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&checkout, &["switch", "--quiet", "triage"]);
    write(checkout.join("draft.txt"), "draft\n");
    write(checkout.join(".env"), "SECRET=1\n");
    let _socket = UnixListener::bind(setup.root.join("triage/s")).unwrap();
    let ArchiveRefusal::Unsaved(unsaved) = setup.refusal(ArchiveOptions::default()) else {
        panic!("the archive was not refused for unsaved work");
    };
    assert!(
        unsaved
            .changes
            .iter()
            .any(|item| item.contains("draft.txt")),
        "{unsaved:?}"
    );
    assert!(
        unsaved.ignored.iter().any(|item| item.contains(".env")),
        "{unsaved:?}"
    );
    assert_eq!(unsaved.stashes.len(), 1, "{unsaved:?}");
    assert!(
        unsaved.commits.iter().any(|item| item.contains(&side)),
        "{unsaved:?}"
    );
    assert_eq!(unsaved.root, vec!["root: s".to_owned()]);
    assert!(checkout.join("draft.txt").exists());
    assert!(
        setup.record().is_none(),
        "a refused archive writes no record"
    );
}

#[test]
fn wip_commits_the_changes_on_the_branch_and_the_restore_brings_them_back() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        write(setup.checkout().join("draft.txt"), "draft\n");
        let refused = setup.refusal(ArchiveOptions::default());
        assert!(matches!(refused, ArchiveRefusal::Unsaved(_)), "{refused:?}");
        let archive = setup.archive_with(wip());
        assert!(archive.lost.is_empty());
        setup.restore();
        assert_eq!(
            fs::read_to_string(setup.checkout().join("draft.txt")).unwrap(),
            "draft\n"
        );
        assert_eq!(
            git(&setup.checkout(), &["log", "-1", "--format=%s"]).trim(),
            WIP_MESSAGE
        );
    }
}

#[test]
fn wip_leaves_an_ignored_file_alone_and_the_archive_still_refuses() {
    let setup = setup(Kind::Worktree);
    setup.create();
    setup.ignore_env();
    let head = setup.head();
    write(setup.checkout().join(".env"), "SECRET=1\n");
    write(setup.checkout().join("draft.txt"), "draft\n");
    let ArchiveRefusal::Unsaved(unsaved) = setup.refusal(wip()) else {
        panic!("the archive was not refused");
    };
    assert!(unsaved.ignored.iter().any(|item| item.contains(".env")));
    assert_eq!(setup.head(), head, "a refused --wip commits nothing");
}

#[test]
fn force_loses_what_the_archive_cannot_keep_and_lists_it_in_the_record() {
    let setup = setup(Kind::Worktree);
    setup.create();
    write(setup.checkout().join("draft.txt"), "draft\n");
    let archive = setup.archive_with(force());
    assert!(
        archive.lost.iter().any(|item| item.contains("draft.txt")),
        "{:?}",
        archive.lost
    );
    assert!(setup.record().unwrap().lost == archive.lost);
    setup.restore();
    assert!(!setup.checkout().join("draft.txt").exists());
}

#[test]
fn a_file_written_after_the_plan_refuses_the_archive_and_keeps_the_checkout() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let plan = setup.plan(ArchiveOptions::default());
    write(setup.checkout().join("late.txt"), "late\n");
    let outcome = setup.workspaces.archive(&setup.archives, plan).unwrap();
    assert!(
        matches!(outcome, ArchiveOutcome::Refused(ArchiveRefusal::Unsaved(_))),
        "{outcome:?}"
    );
    assert!(setup.checkout().join("late.txt").is_file());
}

fn a_file_written_during_retirement_comes_back_in_place(kind: Kind) {
    let setup = setup(kind);
    setup.create();
    let checkout = setup.checkout();
    let held = quarantine(&checkout).unwrap();
    let late = held.join("late.txt");
    set_hook(move |step| {
        if step == "moved:app" {
            write(&late, "late\n");
        }
        Ok(())
    });
    let plan = setup.plan(ArchiveOptions::default());
    let outcome = setup.workspaces.archive(&setup.archives, plan).unwrap();
    clear_hook();
    let ArchiveOutcome::Refused(ArchiveRefusal::SourceChanged {
        folder,
        differences,
    }) = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(folder, "app");
    assert!(
        differences.iter().any(|item| item.contains("late.txt")),
        "{differences:?}"
    );
    assert!(checkout.join("late.txt").is_file());
    assert!(!held.exists());
    assert_eq!(branch(&checkout), "triage");
    assert_eq!(setup.record().unwrap().state, ArchiveState::Retiring);
    fs::remove_file(checkout.join("late.txt")).unwrap();
    let archive = setup.archive();
    assert_eq!(archive.state, ArchiveState::Archived);
    setup.restore();
    assert_eq!(branch(&checkout), "triage");
}

#[test]
fn a_file_written_while_a_worktree_is_moved_aside_puts_the_worktree_back_and_refuses() {
    a_file_written_during_retirement_comes_back_in_place(Kind::Worktree);
}

#[test]
fn a_file_written_while_a_clone_is_moved_aside_puts_the_clone_back_and_refuses() {
    a_file_written_during_retirement_comes_back_in_place(Kind::Clone);
}

fn resumes_after(kind: Kind, step: &'static str) {
    let setup = setup(kind);
    setup.create();
    let commit = setup.commit_locally();
    write(setup.root.join("triage/notes.md"), "keep me\n");
    stop_at(step);
    let plan = setup.plan(ArchiveOptions::default());
    let stopped = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "{step}: the run did not stop");
    let mut archive = match setup.planned(ArchiveOptions::default()) {
        ArchivePlanned::Finish(archive) => *archive,
        ArchivePlanned::Ready(plan) => {
            match setup.workspaces.archive(&setup.archives, *plan).unwrap() {
                ArchiveOutcome::Retired { archive, .. } => *archive,
                ArchiveOutcome::Refused(refusal) => panic!("{step}: {refusal:?}"),
            }
        }
        ArchivePlanned::Refused(refusal) => panic!("{step}: {refusal:?}"),
    };
    setup
        .workspaces
        .finish_archive(&setup.archives, &mut archive)
        .unwrap();
    assert!(!setup.root.join("triage").exists(), "{step}");
    setup.restore();
    assert_eq!(setup.head(), commit, "{step}");
    assert_eq!(
        fs::read_to_string(setup.root.join("triage/notes.md")).unwrap(),
        "keep me\n",
        "{step}"
    );
}

const ARCHIVE_STEPS: [&str; 9] = [
    "pins",
    "record",
    "root",
    "before-move:app",
    "moved:app",
    "cleared:app",
    "retired:app",
    "delete",
    "removed",
];

#[test]
fn a_worktree_archive_stopped_after_each_step_finishes_when_it_runs_again() {
    for step in ARCHIVE_STEPS {
        resumes_after(Kind::Worktree, step);
    }
}

#[test]
fn a_clone_archive_stopped_after_each_step_finishes_when_it_runs_again() {
    for step in ARCHIVE_STEPS {
        resumes_after(Kind::Clone, step);
    }
}

#[test]
fn a_wip_archive_stopped_after_its_commit_finishes_when_it_runs_again() {
    let setup = setup(Kind::Worktree);
    setup.create();
    write(setup.checkout().join("draft.txt"), "draft\n");
    stop_at("wip");
    let plan = setup.plan(wip());
    assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
    clear_hook();
    setup.archive();
    setup.restore();
    assert!(setup.checkout().join("draft.txt").is_file());
}

fn restore_stopped_leaves_the_archive_whole(kind: Kind, step: &'static str) {
    let setup = setup(kind);
    setup.create();
    let commit = setup.commit_locally();
    write(setup.root.join("triage/notes.md"), "keep me\n");
    let archive = setup.archive();
    stop_at(step);
    let result = setup
        .workspaces
        .restore(&setup.archives, setup.restore_plan(), &setup.runs);
    clear_hook();
    assert!(result.is_err(), "{step}: the run did not stop");
    assert_eq!(setup.record().as_ref(), Some(&archive), "{step}");
    assert!(
        !setup.root.join("triage").exists(),
        "{step}: the failed run left its files"
    );
    setup.restore();
    assert_eq!(setup.head(), commit, "{step}");
}

const RESTORE_STEPS: [&str; 4] = ["mirrors", "directory", "checkout:app", "root"];

#[test]
fn a_worktree_restore_stopped_after_each_step_leaves_the_archive_whole() {
    for step in RESTORE_STEPS {
        restore_stopped_leaves_the_archive_whole(Kind::Worktree, step);
    }
}

#[test]
fn a_clone_restore_stopped_after_each_step_leaves_the_archive_whole() {
    for step in RESTORE_STEPS {
        restore_stopped_leaves_the_archive_whole(Kind::Clone, step);
    }
}

#[test]
fn a_restore_stopped_after_its_record_keeps_the_archive_and_refuses_the_name() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let archive = setup.archive();
    stop_at("record");
    let result = setup
        .workspaces
        .restore(&setup.archives, setup.restore_plan(), &setup.runs);
    clear_hook();
    assert!(result.is_err());
    assert_eq!(setup.record().as_ref(), Some(&archive));
    assert!(matches!(
        setup.restore_refusal(),
        RestoreRefusal::NameTaken { record: true, .. }
    ));
}

#[test]
fn a_pin_keeps_a_commit_through_a_mirror_fetch_with_prune() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    let mirror = archive.checkouts[0].mirror.clone().unwrap();
    let origin = setup.fixture.origin.to_string_lossy().into_owned();
    Git.fetch_mirror(&mirror, &origin, "main").unwrap();
    git(&mirror, &["reflog", "expire", "--expire=now", "--all"]);
    git(&mirror, &["gc", "--quiet", "--prune=now"]);
    assert_eq!(
        git(&mirror, &["rev-parse", &archive.checkouts[0].pin]).trim(),
        commit
    );
    setup.restore();
    assert_eq!(setup.head(), commit);
    let refs = git(&mirror, &["for-each-ref", "refs/mxv/archives"]);
    assert_eq!(refs, "", "a restore deletes its pins");
}

#[test]
fn a_pin_keeps_a_worktree_commit_after_its_branch_is_deleted() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    let source = &setup.fixture.clone;
    assert_eq!(
        git(source, &["rev-parse", "triage"]).trim(),
        commit,
        "the branch stays in the repository"
    );
    git(source, &["branch", "--quiet", "-D", "triage"]);
    git(source, &["reflog", "expire", "--expire=now", "--all"]);
    git(source, &["gc", "--quiet", "--prune=now"]);
    assert_eq!(
        git(source, &["rev-parse", &archive.checkouts[0].pin]).trim(),
        commit
    );
    setup.restore();
    assert_eq!(setup.head(), commit);
    assert_eq!(branch(&setup.checkout()), "triage");
}

#[test]
fn the_files_of_the_workspace_root_come_back_with_their_modes_and_links() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let root = setup.root.join("triage");
    fs::create_dir_all(root.join("deep/er")).unwrap();
    write(root.join("deep/er/notes.md"), "notes\n");
    write(root.join("run.sh"), "#!/bin/sh\n");
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o750)).unwrap();
    std::os::unix::fs::symlink("run.sh", root.join("link")).unwrap();
    let archive = setup.archive();
    assert_eq!(archive.root.len(), 5, "{:?}", archive.root);
    setup.restore();
    assert_eq!(
        fs::read_to_string(root.join("deep/er/notes.md")).unwrap(),
        "notes\n"
    );
    assert_eq!(
        fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(
        fs::read_link(root.join("link")).unwrap(),
        Path::new("run.sh")
    );
}

#[test]
fn an_archive_is_private_to_the_person() {
    let setup = setup(Kind::Worktree);
    setup.create();
    setup.archive();
    let dir = setup.archives.dir_of(&name());
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("root")), 0o700);
}

#[test]
fn a_root_file_that_differs_from_its_digest_refuses_the_restore() {
    let setup = setup(Kind::Worktree);
    setup.create();
    write(setup.root.join("triage/notes.md"), "keep me\n");
    setup.archive();
    write(
        setup.archives.dir_of(&name()).join("root/notes.md"),
        "changed\n",
    );
    assert_eq!(
        setup.restore_refusal(),
        RestoreRefusal::RootFileChanged {
            path: "notes.md".to_owned()
        }
    );
}

#[test]
fn an_archive_refuses_a_name_that_an_archive_holds() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.archive();
    setup.create();
    assert!(matches!(
        setup.refusal(ArchiveOptions::default()),
        ArchiveRefusal::NameTaken(_)
    ));
}

#[test]
fn a_restore_refuses_a_name_that_a_workspace_holds() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.archive();
    setup.create();
    assert!(matches!(
        setup.restore_refusal(),
        RestoreRefusal::NameTaken { record: true, .. }
    ));
}

#[test]
fn a_restore_refuses_a_path_that_something_else_holds() {
    let setup = setup(Kind::Worktree);
    setup.create();
    setup.archive();
    fs::create_dir_all(setup.root.join("triage")).unwrap();
    assert!(matches!(
        setup.restore_refusal(),
        RestoreRefusal::NameTaken { record: false, .. }
    ));
}

#[test]
fn a_restore_refuses_a_branch_that_moved_and_leaves_it_alone() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let commit = setup.commit_locally();
    setup.archive();
    let source = &setup.fixture.clone;
    git(source, &["branch", "--quiet", "--force", "triage", "main"]);
    let main = git(source, &["rev-parse", "main"]).trim().to_owned();
    assert_eq!(
        setup.restore_refusal(),
        RestoreRefusal::BranchMoved {
            folder: "app".to_owned(),
            branch: "triage".to_owned(),
            pinned: commit,
            tip: main.clone(),
        }
    );
    assert_eq!(git(source, &["rev-parse", "triage"]).trim(), main);
}

#[test]
fn a_restore_refuses_a_commit_that_is_gone_and_builds_nothing() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    let source = &setup.fixture.clone;
    git(source, &["branch", "--quiet", "-D", "triage"]);
    git(source, &["update-ref", "-d", &archive.checkouts[0].pin]);
    git(source, &["reflog", "expire", "--expire=now", "--all"]);
    git(source, &["gc", "--quiet", "--prune=now"]);
    assert_eq!(
        setup.restore_refusal(),
        RestoreRefusal::CommitMissing {
            folder: "app".to_owned(),
            commit,
        }
    );
    assert!(!setup.root.join("triage").exists());
}

#[test]
fn a_record_from_a_newer_version_is_refused_with_its_version() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let mut archive = setup.archive();
    archive.version = ARCHIVE_VERSION + 1;
    setup.archives.write(&archive).unwrap();
    let refused = setup.workspaces.plan_restore(&setup.archives, &name());
    assert!(
        matches!(refused, Err(Error::NewerRecord { version, .. }) if version == ARCHIVE_VERSION + 1)
    );
}

#[test]
fn a_restore_keeps_a_free_port_and_replaces_a_taken_one() {
    let mut setup = setup(Kind::Worktree);
    let held = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let taken = held.local_addr().unwrap().port();
    let free = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let kept = free.local_addr().unwrap().port();
    drop(free);
    setup.runs = setup.runs.with_port_range(20_000..=20_100);
    setup.create();
    let mut archive = setup.archive();
    archive.ports.insert("web".to_owned(), taken);
    archive.ports.insert("api".to_owned(), kept);
    setup.archives.write(&archive).unwrap();
    let restored = setup.restore();
    assert_eq!(restored.workspace.ports["api"], kept);
    assert_ne!(restored.workspace.ports["web"], taken);
    assert_eq!(
        restored.ports,
        vec![PortChange::new(
            "web".to_owned(),
            taken,
            restored.workspace.ports["web"]
        )]
    );
    assert_eq!(
        setup.workspaces.store.read(&name()).unwrap().unwrap().ports,
        restored.workspace.ports
    );
}

#[test]
fn a_mirror_that_is_gone_is_cloned_again_from_the_origin_for_a_pushed_commit() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = setup.commit_locally();
    git(&setup.checkout(), &["push", "--quiet", "origin", "triage"]);
    let archive = setup.archive();
    fs::remove_dir_all(archive.checkouts[0].mirror.as_ref().unwrap()).unwrap();
    setup.restore();
    assert_eq!(setup.head(), commit);
}

#[test]
fn a_mirror_that_is_gone_with_a_commit_only_it_held_refuses_and_builds_nothing() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    fs::remove_dir_all(archive.checkouts[0].mirror.as_ref().unwrap()).unwrap();
    let outcome = setup
        .workspaces
        .restore(&setup.archives, setup.restore_plan(), &setup.runs)
        .unwrap();
    assert!(
        matches!(
            &outcome,
            RestoreOutcome::Refused(RestoreRefusal::CommitMissing { commit: gone, .. }) if *gone == commit
        ),
        "{outcome:?}"
    );
    assert!(!setup.root.join("triage").exists());
    assert_eq!(setup.record().unwrap().id, archive.id);
}

#[test]
fn removing_an_archive_deletes_its_record_and_pins() {
    let setup = setup(Kind::Clone);
    setup.create();
    let archive = setup.archive();
    let pin = archive.checkouts[0].pin.clone();
    let mirror = archive.checkouts[0].mirror.clone().unwrap();
    setup
        .workspaces
        .remove_archive(&setup.archives, archive, Mode::Change)
        .unwrap();
    assert!(setup.record().is_none());
    assert_eq!(git(&mirror, &["for-each-ref", &pin]), "");
}

#[test]
fn the_listing_reports_a_bad_record_beside_the_good_ones() {
    let setup = setup(Kind::Worktree);
    setup.create();
    setup.archive();
    fs::create_dir_all(setup.base.join("archives/broken")).unwrap();
    write(setup.base.join("archives/broken/archive.json"), "{");
    let records = setup.archives.list().unwrap();
    assert_eq!(records.archives.len(), 1);
    assert_eq!(records.problems.len(), 1);
}

#[test]
fn a_pin_name_stays_one_reference_component_for_an_odd_folder() {
    let pin = pin_ref("abc", "a..b/c d");
    assert!(pin.starts_with("refs/mxv/archives/abc/"));
    assert!(!pin.contains(".."), "{pin}");
    assert_eq!(pin.matches('/').count(), 4, "{pin}");
}
