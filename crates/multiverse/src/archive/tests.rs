use std::cell::RefCell;
use std::fs;
use std::io;
use std::net::TcpListener;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;
use crate::backend::{Backend, RefHolder};
use crate::disk::DisposablePath;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::runs::Runs;
use crate::store::Store;
use crate::testing::{Fixture, fixture, fixture_named, git, write};
use crate::workspaces::{Mode, NewOutcome, Repo, Workspaces};

mod edges;

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

fn clear_hook() {
    HOOK.with(|hook| *hook.borrow_mut() = None);
}

/// Returns the permission bits of `path` itself, without following a symlink.
fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
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
    workspaces.checkpoint = at_checkpoint;
    Setup {
        fixture: fixture(),
        root: base.join("mxv"),
        archives: Archives::new(base.join("archives"), base.join("data/archives")),
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
        self.restore_plan_with(false)
    }

    fn restore_plan_with(&self, force: bool) -> RestorePlan {
        match self
            .workspaces
            .plan_restore(&self.archives, &name(), force)
            .unwrap()
        {
            RestorePlanned::Ready(plan) => *plan,
            RestorePlanned::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn restore(&self) -> Restored {
        self.restore_with(false)
    }

    fn restore_with(&self, force: bool) -> Restored {
        match self.restore_outcome(force) {
            RestoreOutcome::Restored(restored) => *restored,
            RestoreOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn restore_outcome(&self, force: bool) -> RestoreOutcome {
        self.workspaces
            .restore(&self.archives, self.restore_plan_with(force), &self.runs)
            .unwrap()
    }

    fn restore_refusal(&self) -> RestoreRefusal {
        match self
            .workspaces
            .plan_restore(&self.archives, &name(), false)
            .unwrap()
        {
            RestorePlanned::Refused(refusal) => refusal,
            RestorePlanned::Ready(plan) => panic!("{plan:?}"),
        }
    }

    fn record(&self) -> Option<Archive> {
        self.archives.read(&name()).unwrap()
    }

    /// Returns the tar file of ignored entries of the archive `archive`.
    fn ignored_tar(&self, archive: &Archive) -> PathBuf {
        self.archives.data_of(&archive.id).join("ignored.tar")
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

fn force() -> ArchiveOptions {
    ArchiveOptions::new(true, false)
}

/// Returns the staged changes of `checkout`, as `git diff --cached` names
/// them with their status.
fn staged(checkout: &Path) -> String {
    git(checkout, &["diff", "--cached", "--name-status"])
}

/// Returns each line of `git status --porcelain` of `checkout`, sorted.
fn status(checkout: &Path) -> Vec<String> {
    let mut lines: Vec<String> = git(checkout, &["status", "--porcelain"])
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

/// Stages a change to a committed file, leaves an unstaged change to
/// another, and writes an untracked file, in `checkout`.
fn leave_uncommitted_work(checkout: &Path) {
    write(checkout.join("staged.txt"), "one\n");
    write(checkout.join("edited.txt"), "one\n");
    git(checkout, &["add", "staged.txt", "edited.txt"]);
    git(checkout, &["commit", "--quiet", "-m", "Two files"]);
    write(checkout.join("staged.txt"), "staged\n");
    git(checkout, &["add", "staged.txt"]);
    write(checkout.join("edited.txt"), "unstaged\n");
    fs::create_dir_all(checkout.join("notes")).unwrap();
    write(checkout.join("notes/todo.md"), "untracked\n");
}

/// Makes a commit on a detached `HEAD` of `checkout` and goes back to the
/// branch `triage`, so only the reflog of `HEAD` holds the commit.
fn leave_detached_commit(checkout: &Path) -> String {
    git(checkout, &["switch", "--quiet", "--detach"]);
    git(
        checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Detached"],
    );
    let commit = git(checkout, &["rev-parse", "HEAD"]).trim().to_owned();
    git(checkout, &["switch", "--quiet", "triage"]);
    commit
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
fn a_socket_in_the_workspace_root_refuses_the_archive_and_names_itself() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write(checkout.join("draft.txt"), "draft\n");
    write(checkout.join(".env"), "SECRET=1\n");
    let socket = UnixListener::bind(setup.root.join("triage/s")).unwrap();
    let ArchiveRefusal::Unkeepable(found) = setup.refusal(ArchiveOptions::default()) else {
        panic!("the archive was not refused for work it cannot keep");
    };
    let kinds: Vec<&UnkeptKind> = found.items.iter().map(|item| &item.kind).collect();
    assert_eq!(
        kinds,
        vec![&UnkeptKind::Root {
            entry: "s".to_owned()
        }],
        "an archive keeps draft.txt and .env, so only the socket refuses"
    );
    assert_eq!(found.items[0].to_string(), "root: s");
    assert!(checkout.join("draft.txt").exists());
    assert!(
        setup.record().is_none(),
        "a refused archive writes no record"
    );
    drop(socket);
    fs::remove_file(setup.root.join("triage/s")).unwrap();
    assert_eq!(
        setup.plan(ArchiveOptions::default()).ignored,
        vec![IgnoredSource::new("app".to_owned(), ".env".to_owned(), 9)],
        "without the socket the archive goes ahead and keeps .env"
    );
}

/// Writes `text` at `path` with the permission bits `mode`.
fn write_with_mode(path: &Path, text: &str, mode: u32) {
    write(path, text);
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn an_archive_keeps_a_dot_env_in_a_private_tarball_and_restore_writes_it_back_with_its_mode() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write_with_mode(&checkout.join(".env"), "SECRET=1\n", 0o640);
    write(checkout.join("draft.txt"), "draft\n");
    let digest = crate::store::file_digest(&checkout.join(".env")).unwrap();

    let archive = setup.archive();

    assert_eq!(
        archive.ignored,
        vec![IgnoredEntry {
            path: "app/.env".to_owned(),
            kind: IgnoredKind::File,
            size: 9,
            mode: 0o640,
            digest,
        }]
    );
    assert_eq!(archive.ignored[0].folder(), "app");
    assert_eq!(
        setup.record().as_ref().map(|record| &record.ignored),
        Some(&archive.ignored),
        "the record lists the entry"
    );
    assert!(setup.ignored_tar(&archive).is_file());
    assert!(!setup.root.join("triage").exists());

    let restored = setup.restore();

    assert_eq!(
        fs::read_to_string(checkout.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(mode_of(&checkout.join(".env")), 0o640);
    assert_eq!(
        fs::read_to_string(checkout.join("draft.txt")).unwrap(),
        "draft\n"
    );
    assert_eq!(restored.put_back[0].ignored, ["app/.env"]);
    assert!(restored.lost.is_empty(), "{:?}", restored.lost);
    assert!(
        !setup.archives.data_of(&archive.id).exists(),
        "the restore deletes the tar and its directory with the archive"
    );
    assert!(setup.record().is_none());
}

#[test]
fn a_damaged_tarball_refuses_restore_and_names_the_entries() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write(checkout.join(".env"), "SECRET=1\n");
    write(checkout.join("draft.txt"), "draft\n");
    let archive = setup.archive();
    let tar = setup.ignored_tar(&archive);
    let mut bytes = fs::read(&tar).unwrap();
    let secret = bytes
        .windows(8)
        .position(|window| window == b"SECRET=1")
        .expect("the tar holds the bytes of .env");
    bytes[secret + 7] = b'2';
    write(&tar, &bytes);

    assert_eq!(
        refused(setup.restore_outcome(false)),
        RestoreRefusal::IgnoredDamaged {
            paths: vec!["app/.env".to_owned()]
        }
    );
    assert!(
        !setup.root.join("triage").exists(),
        "the refused restore left its files"
    );
    assert_eq!(setup.record().as_ref(), Some(&archive));

    let restored = setup.restore_with(true);

    assert_eq!(restored.lost, ["app/.env"]);
    assert!(restored.put_back[0].ignored.is_empty());
    assert!(
        !checkout.join(".env").exists(),
        "the forced restore wrote the damaged entry"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("draft.txt")).unwrap(),
        "draft\n"
    );
    assert!(!setup.archives.data_of(&archive.id).exists());
}

#[test]
fn restore_refuses_to_overwrite_an_ignored_path_the_checkout_already_holds() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write(checkout.join(".env"), "SECRET=1\n");
    let archive = setup.archive();
    let planted = checkout.join(".env");
    set_hook(move |step| {
        if step == "apply" {
            write(&planted, "PLANTED=1\n");
        }
        Ok(())
    });
    let outcome = setup.restore_outcome(false);
    clear_hook();

    assert_eq!(
        refused(outcome),
        RestoreRefusal::IgnoredTaken {
            paths: vec!["app/.env".to_owned()]
        }
    );
    assert_eq!(setup.record().as_ref(), Some(&archive), "the archive stays");
    assert!(setup.ignored_tar(&archive).is_file());
    assert!(
        !setup.root.join("triage").exists(),
        "the refused restore left its files"
    );
}

/// Returns the refusal of `outcome`.
fn refused(outcome: RestoreOutcome) -> RestoreRefusal {
    match outcome {
        RestoreOutcome::Refused(refusal) => refusal,
        RestoreOutcome::Restored(restored) => panic!("{restored:?}"),
    }
}

#[test]
fn the_ignored_tarball_and_its_directory_are_readable_only_by_the_user() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    write(setup.checkout().join(".env"), "SECRET=1\n");

    let archive = setup.archive();

    assert_eq!(mode_of(&setup.archives.data_of(&archive.id)), 0o700);
    assert_eq!(mode_of(&setup.ignored_tar(&archive)), 0o600);
}

#[test]
fn an_ignored_file_over_archive_max_file_is_refused_and_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write(checkout.join(".env"), "x".repeat(2048));
    write(checkout.join("draft.txt"), "draft\n");

    let ArchiveRefusal::Unkeepable(found) =
        setup.refusal(ArchiveOptions::default().with_max_file(1024))
    else {
        panic!("the archive was not refused for the large ignored file");
    };
    assert_eq!(
        found.items.first().map(|item| &item.kind),
        Some(&UnkeptKind::TooLarge {
            path: ".env".to_owned(),
            bytes: 2048,
            most: 1024,
            status: FileStatus::Ignored,
        }),
        "{:?}",
        found.items
    );
    assert_eq!(
        found.items[0].to_string(),
        "app: the file .env is 2048 bytes, over the limit of 1024 bytes"
    );

    let archive = setup.archive_with(force().with_max_file(1024));

    assert_eq!(
        archive.lost,
        vec!["app: the file .env is 2048 bytes, over the limit of 1024 bytes".to_owned()]
    );
    assert!(archive.ignored.is_empty(), "{:?}", archive.ignored);
    setup.restore();
    assert!(
        !checkout.join(".env").exists(),
        "the forced archive lost .env"
    );
    assert!(checkout.join("draft.txt").exists());
}

#[test]
fn a_rerun_after_a_partial_tar_writes_it_again_and_finishes() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    write(setup.checkout().join(".env"), "SECRET=1\n");
    stop_at("ignored");
    let plan = setup.plan(ArchiveOptions::default());
    let stopped = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "the run did not stop after the tar");
    let archive = setup.record().expect("the record stays");
    let tar = setup.ignored_tar(&archive);
    let whole = fs::read(&tar).unwrap();
    write(&tar, &whole[..whole.len() / 2]);

    let mut archive = match setup.planned(ArchiveOptions::default()) {
        ArchivePlanned::Ready(plan) => {
            match setup.workspaces.archive(&setup.archives, *plan).unwrap() {
                ArchiveOutcome::Retired { archive, .. } => *archive,
                ArchiveOutcome::Refused(refusal) => panic!("{refusal:?}"),
            }
        }
        other => panic!("{other:?}"),
    };
    setup
        .workspaces
        .finish_archive(&setup.archives, &mut archive)
        .unwrap();

    assert_eq!(
        fs::read(&tar).unwrap(),
        whole,
        "the rerun writes the tar again"
    );
    assert_eq!(archive.ignored.len(), 1, "{:?}", archive.ignored);
    assert!(!setup.root.join("triage").exists());
}

#[test]
fn an_ignored_symbolic_link_is_kept_as_a_link() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    write(checkout.join(".gitignore"), "link\n");
    git(&checkout, &["add", ".gitignore"]);
    git(&checkout, &["commit", "--quiet", "-m", "Ignore"]);
    write(checkout.join("draft.txt"), "draft\n");
    std::os::unix::fs::symlink("draft.txt", checkout.join("link")).unwrap();

    let archive = setup.archive();

    assert_eq!(
        archive
            .ignored
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind))
            .collect::<Vec<_>>(),
        vec![("app/link", IgnoredKind::Link)]
    );
    assert!(setup.ignored_tar(&archive).is_file());

    let restored = setup.restore();

    assert_eq!(
        fs::read_link(checkout.join("link")).unwrap(),
        Path::new("draft.txt")
    );
    assert_eq!(
        fs::read_to_string(checkout.join("link")).unwrap(),
        "draft\n",
        "the link reads through to the untracked file"
    );
    assert_eq!(restored.put_back[0].ignored, ["app/link"]);
}

/// Commits a `.gitignore` that names `target/` and `node_modules/` in the
/// checkout and writes a file into each.
fn leave_build_output(checkout: &Path) {
    write(checkout.join(".gitignore"), "target/\nnode_modules/\n");
    git(checkout, &["add", ".gitignore"]);
    git(
        checkout,
        &["commit", "--quiet", "-m", "Ignore build output"],
    );
    fs::create_dir_all(checkout.join("target")).unwrap();
    fs::create_dir_all(checkout.join("node_modules/x")).unwrap();
    write(checkout.join("target/out.bin"), "object code\n");
    write(
        checkout.join("node_modules/x/index.js"),
        "module.exports = 1\n",
    );
}

/// Returns each disposable path of `found` as its checkout and path, in
/// order of the two.
fn named(found: &[DisposablePath]) -> Vec<(&str, &str)> {
    let mut names: Vec<(&str, &str)> = found
        .iter()
        .map(|path| (path.checkout.as_str(), path.path.as_str()))
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn an_archive_deletes_target_and_node_modules_and_names_each_directory_it_deleted() {
    let setup = setup(Kind::Clone);
    setup.create();
    leave_build_output(&setup.checkout());

    let plan = setup.plan(ArchiveOptions::default());

    assert_eq!(
        named(&plan.disposable),
        [("app", "node_modules/"), ("app", "target/")]
    );
    assert!(
        plan.disposable.iter().all(|path| path.bytes > 0),
        "{:?}",
        plan.disposable
    );
    assert!(plan.ignored.is_empty(), "{:?}", plan.ignored);

    let outcome = setup.workspaces.archive(&setup.archives, plan).unwrap();

    let ArchiveOutcome::Retired {
        archive,
        disposable,
        late,
        ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        named(&disposable),
        [("app", "node_modules/"), ("app", "target/")],
        "the result names what the archive deleted"
    );
    assert!(late.is_empty(), "{late:?}");
    assert!(disposable.iter().all(|path| path.bytes > 0));
    assert!(archive.ignored.is_empty(), "{:?}", archive.ignored);
    assert!(!setup.root.join("triage").exists());
}

#[test]
fn a_file_written_into_target_after_the_move_is_named_as_deleted() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    leave_build_output(&checkout);
    let late = quarantine(&checkout).unwrap().join("target/late.o");
    set_hook(move |step| {
        if step == "moved:app" {
            write(&late, "late\n");
        }
        Ok(())
    });
    let plan = setup.plan(ArchiveOptions::default());
    let outcome = setup.workspaces.archive(&setup.archives, plan).unwrap();
    clear_hook();

    let ArchiveOutcome::Retired {
        disposable, late, ..
    } = outcome
    else {
        panic!("{outcome:?}");
    };
    assert!(
        disposable
            .iter()
            .any(|path| path.checkout == "app" && path.path == "target/"),
        "{disposable:?}"
    );
    assert_eq!(late, ["app: target/late.o"], "the late file is named");
    assert!(!setup.root.join("triage").exists());
}

#[test]
fn a_disposable_directory_deleted_after_the_move_puts_the_checkout_back_and_refuses() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    leave_build_output(&checkout);
    let target = quarantine(&checkout).unwrap().join("target");
    set_hook(move |step| {
        if step == "moved:app" {
            fs::remove_dir_all(&target).unwrap();
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
        differences.iter().any(|item| item.contains("target")),
        "{differences:?}"
    );
    assert!(checkout.join("node_modules/x/index.js").is_file());
    assert_eq!(setup.record().unwrap().state, ArchiveState::Retiring);
}

#[test]
fn an_ignored_file_edited_after_the_tar_puts_the_checkout_back_and_refuses() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    let checkout = setup.checkout();
    write(checkout.join(".env"), "SECRET=1\n");
    let edited = quarantine(&checkout).unwrap().join(".env");
    set_hook(move |step| {
        if step == "moved:app" {
            write(&edited, "SECRET=2\n");
        }
        Ok(())
    });
    let plan = setup.plan(ArchiveOptions::default());
    let outcome = setup.workspaces.archive(&setup.archives, plan).unwrap();
    clear_hook();

    assert_eq!(
        refusal_of(outcome),
        ArchiveRefusal::SourceChanged {
            folder: "app".to_owned(),
            differences: vec!["the ignored file .env changed after the archive kept it".to_owned()],
        }
    );
    assert_eq!(
        fs::read_to_string(checkout.join(".env")).unwrap(),
        "SECRET=2\n",
        "the checkout is back with the edited file"
    );
    assert_eq!(setup.record().unwrap().state, ArchiveState::Retiring);

    let archive = setup.archive();

    assert_eq!(archive.ignored.len(), 1, "{:?}", archive.ignored);
    setup.restore();
    assert_eq!(
        fs::read_to_string(checkout.join(".env")).unwrap(),
        "SECRET=2\n"
    );
}

/// Returns the refusal of `outcome`.
fn refusal_of(outcome: ArchiveOutcome) -> ArchiveRefusal {
    match outcome {
        ArchiveOutcome::Refused(refusal) => refusal,
        ArchiveOutcome::Retired { archive, .. } => panic!("{archive:?}"),
    }
}

fn keeps_uncommitted_work(kind: Kind) {
    let setup = setup(kind);
    setup.create();
    let checkout = setup.checkout();
    leave_uncommitted_work(&checkout);
    let head = setup.head();
    let before = status(&checkout);
    let plan = setup.plan(ArchiveOptions::default());
    assert_eq!(plan.uncommitted.len(), 1);
    assert_eq!(
        plan.uncommitted[0].changes.len(),
        3,
        "{:?}",
        plan.uncommitted
    );
    let archive = setup.archive();
    assert!(archive.lost.is_empty());
    let wip = archive.checkouts[0].kept.wip.clone().expect("no work kept");
    let restored = setup.restore();
    assert_eq!(setup.head(), head, "the work comes back uncommitted");
    assert_eq!(status(&checkout), before);
    assert_eq!(
        fs::read_to_string(checkout.join("edited.txt")).unwrap(),
        "unstaged\n"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("notes/todo.md")).unwrap(),
        "untracked\n"
    );
    assert_eq!(staged(&checkout), "M\tstaged.txt\n");
    let put_back = &restored.put_back[0];
    assert_eq!(
        (put_back.staged, put_back.unstaged, put_back.untracked),
        (1, 1, 1),
        "{put_back:?}"
    );
    let holder = archive.checkouts[0].pin_holder();
    for commit in [&wip.index, &wip.worktree] {
        assert!(
            Git.resolve_commit(holder, commit).unwrap().is_some(),
            "the restore keeps the objects of the work"
        );
    }
    let pins = Git.refs_under(holder, PIN_NAMESPACE).unwrap();
    assert!(pins.is_empty(), "the restore deletes every pin: {pins:?}");
}

#[test]
fn an_archive_keeps_staged_unstaged_and_untracked_changes_and_restore_brings_them_back_uncommitted()
{
    keeps_uncommitted_work(Kind::Worktree);
    keeps_uncommitted_work(Kind::Clone);
}

#[test]
fn an_archive_never_moves_the_workspace_branch_or_its_index() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        let checkout = setup.checkout();
        leave_uncommitted_work(&checkout);
        let head = setup.head();
        let index = staged(&checkout);
        let plan = setup.plan(ArchiveOptions::default());
        let ArchiveOutcome::Retired { archive, .. } =
            setup.workspaces.archive(&setup.archives, plan).unwrap()
        else {
            panic!("the archive was refused");
        };
        assert_eq!(archive.checkouts[0].commit, head);
        if kind == Kind::Worktree {
            let tip = Git
                .branch_tip(&setup.fixture.clone, "triage")
                .unwrap()
                .unwrap();
            assert_eq!(tip, head, "the branch stays at HEAD");
        }
        let mut archive = *archive;
        setup
            .workspaces
            .finish_archive(&setup.archives, &mut archive)
            .unwrap();
        setup.restore();
        assert_eq!(setup.head(), head);
        assert_eq!(branch(&checkout), "triage");
        assert_eq!(staged(&checkout), index);
    }
}

#[test]
fn an_archive_keeps_each_stash_and_restore_gives_back_the_same_stash_list() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    for (file, message) in [("first.txt", "first stash"), ("second.txt", "second stash")] {
        write(checkout.join(file), "stash\n");
        git(&checkout, &["add", file]);
        git(&checkout, &["stash", "push", "--quiet", "-m", message]);
    }
    let list = |checkout: &Path| git(checkout, &["stash", "list", "--format=%H %gs"]);
    let before = list(&checkout);
    let archive = setup.archive();
    assert_eq!(archive.checkouts[0].kept.stashes.len(), 2);
    let restored = setup.restore();
    assert_eq!(list(&checkout), before);
    assert_eq!(restored.put_back[0].stashes, 2);
}

#[test]
fn an_archive_keeps_other_local_branches_and_tags_that_origin_lacks() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    git(&checkout, &["switch", "--quiet", "-c", "spike-cache"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Spike"],
    );
    let spike = setup.head();
    git(&checkout, &["tag", "-a", "-m", "Candidate", "v0.3.0-rc1"]);
    let tag = git(&checkout, &["rev-parse", "v0.3.0-rc1"])
        .trim()
        .to_owned();
    git(&checkout, &["switch", "--quiet", "triage"]);
    let archive = setup.archive();
    let kept = &archive.checkouts[0].kept;
    assert_eq!(kept.branches.get("spike-cache"), Some(&spike));
    assert_eq!(kept.tags.get("v0.3.0-rc1"), Some(&tag));
    assert!(
        !kept.branches.contains_key("triage"),
        "the checkout's own branch is the pin of HEAD"
    );
    let restored = setup.restore();
    assert_eq!(git(&checkout, &["rev-parse", "spike-cache"]).trim(), spike);
    assert_eq!(git(&checkout, &["rev-parse", "v0.3.0-rc1"]).trim(), tag);
    assert_eq!(
        restored.put_back[0].branches,
        vec!["spike-cache".to_owned()]
    );
    assert_eq!(restored.put_back[0].tags, vec!["v0.3.0-rc1".to_owned()]);
}

#[test]
fn an_archive_keeps_a_per_worktree_reference() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        let checkout = setup.checkout();
        let head = setup.head();
        git(
            &checkout,
            &["update-ref", "refs/worktree/bisect-good", &head],
        );
        let archive = setup.archive();
        assert_eq!(
            archive.checkouts[0].kept.worktree.get("bisect-good"),
            Some(&head),
            "{kind:?}"
        );
        setup.restore();
        assert_eq!(
            git(&checkout, &["rev-parse", "refs/worktree/bisect-good"]).trim(),
            head,
            "{kind:?}"
        );
    }
}

#[test]
fn an_archive_keeps_a_reflog_commit_that_no_ref_reaches_as_a_kept_reference() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        let checkout = setup.checkout();
        let detached = leave_detached_commit(&checkout);
        let archive = setup.archive();
        assert_eq!(
            archive.checkouts[0].kept.orphans,
            vec![detached.clone()],
            "{kind:?}"
        );
        let restored = setup.restore();
        let reference = format!("{KEPT_NAMESPACE}{detached}");
        assert_eq!(
            git(&checkout, &["rev-parse", &reference]).trim(),
            detached,
            "{kind:?}"
        );
        assert_eq!(restored.put_back[0].kept, vec![reference], "{kind:?}");
    }
}

#[test]
fn a_paused_merge_is_refused_and_the_refusal_names_merge_abort() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    let head = setup.head();
    write(checkout.join(".git/MERGE_HEAD"), format!("{head}\n"));
    let ArchiveRefusal::Unkeepable(found) = setup.refusal(ArchiveOptions::default()) else {
        panic!("the archive was not refused");
    };
    assert_eq!(
        found.items[0].kind,
        UnkeptKind::Paused {
            at: String::new(),
            operation: crate::backend::Operation::Merge,
        }
    );
    assert_eq!(found.items[0].to_string(), "app: a merge is paused");
    assert_eq!(found.items[0].dir, checkout);
}

#[test]
fn a_file_over_archive_max_file_is_refused_and_named() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let checkout = setup.checkout();
    write(checkout.join("dump.sql"), vec![b'x'; 2048]);
    write(checkout.join("small.txt"), "small\n");
    let options = ArchiveOptions::default().with_max_file(1024);
    let ArchiveRefusal::Unkeepable(found) = setup.refusal(options) else {
        panic!("the archive was not refused");
    };
    assert_eq!(
        found
            .items
            .iter()
            .map(|item| &item.kind)
            .collect::<Vec<_>>(),
        vec![&UnkeptKind::TooLarge {
            path: "dump.sql".to_owned(),
            bytes: 2048,
            most: 1024,
            status: FileStatus::Untracked,
        }]
    );
    let archive = setup.archive_with(force().with_max_file(1024));
    assert!(
        archive.lost.iter().any(|item| item.contains("dump.sql")),
        "{:?}",
        archive.lost
    );
    setup.restore();
    assert!(
        !checkout.join("dump.sql").exists(),
        "a file the archive lists as lost does not come back"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("small.txt")).unwrap(),
        "small\n"
    );
}

#[test]
fn force_on_a_merge_that_stopped_on_a_conflict_keeps_the_conflicted_file_and_loses_the_merge() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    write(checkout.join("shared.txt"), "base\n");
    git(&checkout, &["add", "shared.txt"]);
    git(&checkout, &["commit", "--quiet", "-m", "Base"]);
    git(&checkout, &["switch", "--quiet", "-c", "other"]);
    write(checkout.join("shared.txt"), "theirs\n");
    git(&checkout, &["commit", "--quiet", "-am", "Theirs"]);
    git(&checkout, &["switch", "--quiet", "triage"]);
    write(checkout.join("shared.txt"), "ours\n");
    git(&checkout, &["commit", "--quiet", "-am", "Ours"]);
    let head = setup.head();
    // The merge stops on the conflict and exits 1, which the alias's shell
    // turns into success for the test's git helper.
    git(
        &checkout,
        &[
            "-c",
            "alias.conflict=!git merge --quiet other; true",
            "conflict",
        ],
    );
    assert!(
        !git(&checkout, &["ls-files", "--unmerged"]).is_empty(),
        "the merge left unmerged entries"
    );
    assert!(matches!(
        setup.refusal(ArchiveOptions::default()),
        ArchiveRefusal::Unkeepable(_)
    ));
    let archive = setup.archive_with(force());
    assert_eq!(archive.lost, vec!["app: a merge is paused".to_owned()]);
    setup.restore();
    assert_eq!(setup.head(), head);
    assert!(!checkout.join(".git/MERGE_HEAD").exists());
    let text = fs::read_to_string(checkout.join("shared.txt")).unwrap();
    assert!(
        text.contains("<<<<<<<") && text.contains("theirs"),
        "{text}"
    );
    assert_eq!(status(&checkout), vec![" M shared.txt".to_owned()]);
}

#[test]
fn force_loses_what_the_archive_cannot_keep_and_lists_it_in_the_record() {
    let setup = setup(Kind::Worktree);
    setup.create();
    setup.ignore_env();
    write(setup.checkout().join(".env"), "SECRET=1\n");
    write(setup.checkout().join("draft.txt"), "draft\n");
    let _socket = UnixListener::bind(setup.root.join("triage/s")).unwrap();
    let archive = setup.archive_with(force());
    assert_eq!(
        archive.lost,
        vec!["root: s".to_owned()],
        "an archive keeps draft.txt and .env, so only the socket is lost"
    );
    assert!(setup.record().unwrap().lost == archive.lost);
    assert_eq!(archive.ignored.len(), 1, "{:?}", archive.ignored);
    setup.restore();
    assert!(!setup.root.join("triage/s").exists());
    assert!(setup.checkout().join("draft.txt").is_file());
}

#[test]
fn a_file_written_after_the_plan_is_kept_with_the_uncommitted_work() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let plan = setup.plan(ArchiveOptions::default());
    write(setup.checkout().join("late.txt"), "late\n");
    let ArchiveOutcome::Retired { mut archive, .. } =
        setup.workspaces.archive(&setup.archives, plan).unwrap()
    else {
        panic!("the archive was refused");
    };
    setup
        .workspaces
        .finish_archive(&setup.archives, &mut archive)
        .unwrap();
    setup.restore();
    assert_eq!(
        fs::read_to_string(setup.checkout().join("late.txt")).unwrap(),
        "late\n"
    );
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
    setup.ignore_env();
    write(setup.checkout().join(".env"), "SECRET=1\n");
    write(setup.root.join("triage/notes.md"), "keep me\n");
    leave_uncommitted_work(&setup.checkout());
    assert_ne!(setup.head(), commit);
    let commit = setup.head();
    let before = status(&setup.checkout());
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
    assert_eq!(archive.ignored.len(), 1, "{step}: {:?}", archive.ignored);
    assert!(setup.ignored_tar(&archive).is_file(), "{step}");
    setup.restore();
    assert_eq!(setup.head(), commit, "{step}");
    assert_eq!(status(&setup.checkout()), before, "{step}");
    assert_eq!(
        fs::read_to_string(setup.checkout().join(".env")).unwrap(),
        "SECRET=1\n",
        "{step}"
    );
    assert_eq!(
        fs::read_to_string(setup.root.join("triage/notes.md")).unwrap(),
        "keep me\n",
        "{step}"
    );
}

const ARCHIVE_STEPS: [&str; 11] = [
    "record",
    "wip",
    "pins",
    "ignored",
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
fn a_rerun_after_the_wip_pins_rewrites_them_and_finishes() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        let checkout = setup.checkout();
        write(checkout.join("draft.txt"), "first\n");
        stop_at("pins");
        let plan = setup.plan(ArchiveOptions::default());
        assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
        clear_hook();
        let stopped = setup.record().unwrap().checkouts[0].clone();
        let first = stopped
            .kept
            .wip
            .clone()
            .expect("the first run kept no work");
        write(checkout.join("draft.txt"), "second\n");
        let archive = setup.archive();
        let second = archive.checkouts[0].kept.wip.clone().unwrap();
        assert_ne!(first, second, "{kind:?}: the rerun writes the new work");
        let pinned = Git
            .resolve_commit(
                archive.checkouts[0].pin_holder(),
                &format!("{}/wip", archive.checkouts[0].pin_dir()),
            )
            .unwrap();
        assert_eq!(pinned.as_ref(), Some(&second.worktree), "{kind:?}");
        setup.restore();
        assert_eq!(
            fs::read_to_string(checkout.join("draft.txt")).unwrap(),
            "second\n",
            "{kind:?}"
        );
    }
}

#[test]
fn the_same_work_gives_the_same_commits_on_a_rerun() {
    let setup = setup(Kind::Worktree);
    setup.create();
    leave_uncommitted_work(&setup.checkout());
    stop_at("wip");
    let plan = setup.plan(ArchiveOptions::default());
    assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
    clear_hook();
    let first = setup.record().unwrap().checkouts[0].kept.wip.clone();
    let archive = setup.archive();
    assert!(first.is_some());
    assert_eq!(archive.checkouts[0].kept.wip, first);
}

fn restore_stopped_leaves_the_archive_whole(kind: Kind, step: &'static str) {
    let setup = setup(kind);
    setup.create();
    let commit = setup.commit_locally();
    setup.ignore_env();
    write(setup.checkout().join(".env"), "SECRET=1\n");
    write(setup.root.join("triage/notes.md"), "keep me\n");
    let detached = leave_detached_commit(&setup.checkout());
    leave_uncommitted_work(&setup.checkout());
    assert_ne!(setup.head(), commit);
    let commit = setup.head();
    let before = status(&setup.checkout());
    let archive = setup.archive();
    assert_eq!(archive.checkouts[0].kept.orphans, vec![detached.clone()]);
    assert_eq!(archive.ignored.len(), 1, "{step}: {:?}", archive.ignored);
    let kept = format!("{KEPT_NAMESPACE}{detached}");
    stop_at(step);
    let result = setup
        .workspaces
        .restore(&setup.archives, setup.restore_plan(), &setup.runs);
    clear_hook();
    assert!(result.is_err(), "{step}: the run did not stop");
    assert_eq!(setup.record().as_ref(), Some(&archive), "{step}");
    assert!(
        setup.ignored_tar(&archive).is_file(),
        "{step}: the failed run took the tar"
    );
    assert!(
        !setup.root.join("triage").exists(),
        "{step}: the failed run left its files"
    );
    let source = RefHolder::Repository(&setup.fixture.clone);
    assert_eq!(
        Git.resolve_commit(source, &kept).unwrap(),
        None,
        "{step}: the failed run left {kept} in the repository"
    );
    let restored = setup.restore();
    assert_eq!(setup.head(), commit, "{step}");
    assert_eq!(status(&setup.checkout()), before, "{step}");
    assert_eq!(
        fs::read_to_string(setup.checkout().join(".env")).unwrap(),
        "SECRET=1\n",
        "{step}"
    );
    assert_eq!(restored.put_back[0].ignored, ["app/.env"], "{step}");
    assert!(
        !setup.archives.data_of(&archive.id).exists(),
        "{step}: the restore left the tar"
    );
}

const RESTORE_STEPS: [&str; 8] = [
    "mirrors",
    "directory",
    "checkout:app",
    "apply",
    "ignored",
    "kept",
    "root",
    "marker",
];

#[test]
fn archive_does_not_count_the_marker_as_an_extra_entry_and_restore_writes_it_again() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        let marker = setup.root.join("triage").join(crate::marker::MARKER_FILE);
        assert!(marker.is_file(), "{kind:?}: new wrote no marker");

        let archive = setup.archive();

        assert_eq!(archive.root, [], "{kind:?}");
        assert!(!setup.root.join("triage").exists(), "{kind:?}");
        setup.restore();
        let metadata = fs::symlink_metadata(&marker).unwrap();
        assert!(metadata.is_file(), "{kind:?}");
        assert_eq!(metadata.len(), 0, "{kind:?}");
    }
}

#[test]
fn a_marker_that_holds_bytes_is_archived_as_the_persons_file_and_comes_back() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let marker = setup.root.join("triage").join(crate::marker::MARKER_FILE);
    write(&marker, "a person's note\n");

    let archive = setup.archive();

    assert_eq!(archive.root.len(), 1, "{:?}", archive.root);
    setup.restore();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "a person's note\n");
}

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
    let refused = setup
        .workspaces
        .plan_restore(&setup.archives, &name(), false);
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
    let checkout = setup.checkout();
    write(checkout.join("stashed.txt"), "stash\n");
    git(&checkout, &["add", "stashed.txt"]);
    git(&checkout, &["stash", "push", "--quiet", "-m", "kept"]);
    leave_detached_commit(&checkout);
    leave_uncommitted_work(&checkout);
    let archive = setup.archive();
    let pin = archive.checkouts[0].pin.clone();
    let dir = pin.strip_suffix("head").unwrap().to_owned();
    let mirror = archive.checkouts[0].mirror.clone().unwrap();
    let pins = git(&mirror, &["for-each-ref", "--format=%(refname)", &dir]);
    for kind in ["/head", "/wip", "/wip-index", "/stash/", "/orphans/"] {
        assert!(pins.contains(kind), "no {kind} pin in {pins}");
    }
    setup
        .workspaces
        .remove_archive(&setup.archives, archive, Mode::Change)
        .unwrap();
    assert!(setup.record().is_none());
    assert_eq!(git(&mirror, &["for-each-ref", &dir]), "");
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
    assert_eq!(pin.matches('/').count(), 5, "{pin}");
    assert!(pin.ends_with("/head"), "{pin}");
}

/// Returns the text of the refusal of `planned`, and fails when the archive
/// was not refused.
fn refusal_text(planned: ArchivePlanned) -> String {
    match planned {
        ArchivePlanned::Refused(ArchiveRefusal::Unkeepable(found)) => found
            .items
            .iter()
            .map(|item| format!("{item} {item:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
        ArchivePlanned::Refused(refusal) => format!("{refusal:?}"),
        other => panic!("the archive was not refused: {other:?}"),
    }
}

/// Carries out `plan` and returns the text of its refusal, and fails when
/// the archive was not refused.
fn outcome_refusal(setup: &Setup, plan: ArchivePlan) -> String {
    match setup.workspaces.archive(&setup.archives, plan).unwrap() {
        ArchiveOutcome::Refused(refusal) => format!("{refusal:?}"),
        ArchiveOutcome::Retired { archive, .. } => {
            panic!("the archive was not refused: {archive:?}")
        }
    }
}

fn writes_late_file(kind: Kind, step: &'static str) {
    let setup = setup(kind);
    setup.create();
    let checkout = setup.checkout();
    let late = checkout.join("late.txt");
    let plan = setup.plan(ArchiveOptions::default());
    set_hook(move |at| {
        if at == step {
            write(&late, "late\n");
        }
        Ok(())
    });
    let refusal = outcome_refusal(&setup, plan);
    clear_hook();
    assert!(refusal.contains("SourceChanged"), "{step}: {refusal}");
    assert!(refusal.contains("late.txt"), "{step}: {refusal}");
    assert!(checkout.join("late.txt").is_file(), "{step}");
}

#[test]
fn a_file_written_after_the_pin_of_a_clone_refuses_the_archive_and_is_kept() {
    writes_late_file(Kind::Clone, "pins");
}

#[test]
fn a_file_written_after_the_pin_of_a_worktree_refuses_the_archive_and_is_kept() {
    writes_late_file(Kind::Worktree, "pins");
}

#[test]
fn a_file_written_after_the_record_of_a_clone_refuses_the_archive_and_is_kept() {
    writes_late_file(Kind::Clone, "record");
}

#[test]
fn a_commit_made_after_the_pin_refuses_the_archive_and_stays_in_the_checkout() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    let at = checkout.clone();
    let plan = setup.plan(ArchiveOptions::default());
    set_hook(move |step| {
        if step == "pins" {
            git(&at, &["commit", "--quiet", "--allow-empty", "-m", "Late"]);
        }
        Ok(())
    });
    let refusal = outcome_refusal(&setup, plan);
    clear_hook();
    assert!(refusal.contains("SourceChanged"), "{refusal}");
    assert_eq!(git(&checkout, &["log", "-1", "--format=%s"]).trim(), "Late");
}

#[test]
fn a_rerun_after_head_moved_pins_the_new_commit_and_finishes_the_archive() {
    let setup = setup(Kind::Clone);
    setup.create();
    stop_at("record");
    let plan = setup.plan(ArchiveOptions::default());
    assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
    clear_hook();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    assert_eq!(archive.checkouts[0].commit, commit);
    setup.restore();
    assert_eq!(setup.head(), commit);
}

fn add_nested_repository(setup: &Setup) {
    let nested = setup.checkout().join("nested");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(nested.join("work.txt"), "important\n");
    git(&nested, &["add", "work.txt"]);
    git(&nested, &["commit", "--quiet", "-m", "Nested"]);
}

#[test]
fn an_untracked_nested_repository_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_nested_repository(&setup);
    let head = setup.head();
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("the repository nested/"), "{text}");
    assert!(setup.checkout().join("nested/work.txt").is_file());
    assert_eq!(setup.head(), head, "a refused archive commits nothing");
}

#[test]
fn force_records_an_untracked_nested_repository_as_lost() {
    let setup = setup(Kind::Worktree);
    setup.create();
    add_nested_repository(&setup);
    let archive = setup.archive_with(force());
    assert!(
        archive.lost.iter().any(|item| item.contains("nested")),
        "{:?}",
        archive.lost
    );
}

fn add_submodule(setup: &Setup) {
    let checkout = setup.checkout();
    let origin = setup.fixture.origin.to_string_lossy().into_owned();
    git(
        &checkout,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "--quiet",
            "add",
            &origin,
            "sub",
        ],
    );
    git(&checkout, &["commit", "--quiet", "-m", "Add sub"]);
}

#[test]
fn an_ignored_file_in_a_submodule_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    let checkout = setup.checkout();
    write(checkout.join(".git/modules/sub/info/exclude"), ".env\n");
    write(checkout.join("sub/.env"), "SECRET=1\n");
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("sub") && text.contains(".env"), "{text}");
}

#[test]
fn a_stash_in_a_submodule_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    let sub = setup.checkout().join("sub");
    write(sub.join("stashed.txt"), "stash\n");
    git(&sub, &["add", "stashed.txt"]);
    git(&sub, &["stash", "--quiet"]);
    let stash = git(&sub, &["rev-parse", "stash@{0}"]);
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains(stash.trim()), "{text}");
}

#[test]
fn a_change_in_a_submodule_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    write(setup.checkout().join("sub/draft.txt"), "draft\n");
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("the submodule sub holds"), "{text}");
    assert!(text.contains("draft.txt"), "{text}");
}

#[test]
fn a_clone_that_removed_a_submodule_with_git_rm_is_archived() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    let checkout = setup.checkout();
    git(&checkout, &["rm", "--quiet", "sub"]);
    git(&checkout, &["commit", "--quiet", "-m", "Drop sub"]);
    let commit = setup.head();
    setup.archive();
    setup.restore();
    assert_eq!(setup.head(), commit);
}

#[test]
fn a_restored_clone_names_each_submodule_that_comes_back_uninitialised() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    setup.archive();
    let restored = setup.restore();
    assert_eq!(restored.put_back[0].submodules, vec!["sub".to_owned()]);
}

#[test]
fn a_clean_submodule_in_a_clone_archives_and_restores() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    let commit = setup.head();
    setup.archive();
    setup.restore();
    assert_eq!(setup.head(), commit);
}

#[test]
fn a_worktree_with_an_initialised_submodule_refuses_before_anything_changes() {
    let setup = setup(Kind::Worktree);
    setup.create();
    add_submodule(&setup);
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("Unusable") && text.contains("sub"), "{text}");
    assert!(setup.record().is_none());
    assert!(setup.checkout().join("sub/.git").exists());
}

fn hides_local_edit(flag: &str) {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    write(checkout.join("config.txt"), "shared\n");
    git(&checkout, &["add", "config.txt"]);
    git(&checkout, &["commit", "--quiet", "-m", "Config"]);
    git(&checkout, &["update-index", flag, "config.txt"]);
    write(checkout.join("config.txt"), "local override\n");
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(
        text.contains("the file config.txt is marked"),
        "{flag}: {text}"
    );
    assert_eq!(
        fs::read_to_string(checkout.join("config.txt")).unwrap(),
        "local override\n"
    );
}

#[test]
fn a_skip_worktree_file_with_local_edits_refuses_the_archive_and_is_named() {
    hides_local_edit("--skip-worktree");
}

#[test]
fn an_assume_unchanged_file_with_local_edits_refuses_the_archive_and_is_named() {
    hides_local_edit("--assume-unchanged");
}

#[test]
fn a_skip_worktree_file_without_local_edits_does_not_refuse_the_archive() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    write(checkout.join("config.txt"), "shared\n");
    git(&checkout, &["add", "config.txt"]);
    git(&checkout, &["commit", "--quiet", "-m", "Config"]);
    git(
        &checkout,
        &["update-index", "--skip-worktree", "config.txt"],
    );
    setup.archive();
}

#[test]
fn an_edited_agents_file_comes_back_from_the_archive() {
    let setup = setup(Kind::Clone);
    setup.create();
    let agents = setup.root.join("triage/AGENTS.md");
    let mut text = fs::read_to_string(&agents).unwrap();
    text.push_str("\nMY NOTES\n");
    write(&agents, &text);
    let archive = setup.archive();
    assert!(archive.root.iter().any(|entry| entry.path == "AGENTS.md"));
    setup.restore();
    assert_eq!(fs::read_to_string(&agents).unwrap(), text);
}

#[test]
fn an_agents_file_that_mxv_wrote_is_generated_again_and_not_carried() {
    let setup = setup(Kind::Clone);
    setup.create();
    let agents = setup.root.join("triage/AGENTS.md");
    let text = fs::read_to_string(&agents).unwrap();
    let archive = setup.archive();
    assert!(archive.root.is_empty(), "{:?}", archive.root);
    setup.restore();
    assert_eq!(fs::read_to_string(&agents).unwrap(), text);
}

fn edit_during_retirement(path: &str, first: &'static str, second: &'static str) {
    let setup = setup(Kind::Clone);
    setup.create();
    let file = setup.root.join("triage").join(path);
    write(&file, first);
    let at = file.clone();
    let plan = setup.plan(ArchiveOptions::default());
    set_hook(move |step| {
        if step == "retired:app" {
            write(&at, second);
        }
        Ok(())
    });
    let refusal = outcome_refusal(&setup, plan);
    clear_hook();
    assert!(refusal.contains("SourceChanged"), "{refusal}");
    assert_eq!(fs::read_to_string(&file).unwrap(), second);
    setup.archive();
    setup.restore();
    assert_eq!(fs::read_to_string(&file).unwrap(), second);
}

#[test]
fn a_root_file_edited_during_retirement_refuses_and_the_rerun_keeps_the_edit() {
    edit_during_retirement("notes.md", "v1\n", "v2 edited\n");
}

#[test]
fn a_manifest_edited_during_retirement_refuses_and_the_rerun_keeps_the_edit() {
    edit_during_retirement("mxv.toml", "# v1\n", "# v2\n");
}

#[test]
fn a_root_file_that_appears_during_retirement_refuses_and_the_rerun_keeps_it() {
    let setup = setup(Kind::Clone);
    setup.create();
    let file = setup.root.join("triage/late.md");
    let at = file.clone();
    let plan = setup.plan(ArchiveOptions::default());
    set_hook(move |step| {
        if step == "retired:app" {
            write(&at, "late\n");
        }
        Ok(())
    });
    let refusal = outcome_refusal(&setup, plan);
    clear_hook();
    assert!(refusal.contains("late.md"), "{refusal}");
    setup.archive();
    setup.restore();
    assert_eq!(fs::read_to_string(&file).unwrap(), "late\n");
}

#[test]
fn a_record_over_the_size_limit_refuses_before_anything_changes() {
    let setup = setup(Kind::Clone);
    setup.create();
    let dir = setup.root.join("triage/many");
    fs::create_dir_all(&dir).unwrap();
    for index in 0..12_000 {
        write(dir.join(format!("file-{index:05}.txt")), "x");
    }
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("RecordTooLarge"), "{text}");
    assert!(setup.record().is_none());
    assert!(setup.checkout().is_dir());
}

#[test]
fn an_unreachable_origin_leaves_no_empty_mirror_behind() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = setup.commit_locally();
    git(&setup.checkout(), &["push", "--quiet", "origin", "triage"]);
    let archive = setup.archive();
    let mirror = archive.checkouts[0].mirror.clone().unwrap();
    fs::remove_dir_all(&mirror).unwrap();
    let origin = setup.fixture.origin.clone();
    let hidden = origin.with_extension("hidden");
    crate::store::move_aside(&origin, &hidden).unwrap();
    assert!(
        setup
            .workspaces
            .restore(&setup.archives, setup.restore_plan(), &setup.runs)
            .is_err()
    );
    assert!(
        !mirror.join("HEAD").exists(),
        "the failed run left a mirror"
    );
    crate::store::move_aside(&hidden, &origin).unwrap();
    setup.restore();
    assert_eq!(setup.head(), commit);
}

#[test]
fn a_clone_without_an_origin_comes_back_from_its_mirror_without_an_origin() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = setup.commit_locally();
    git(&setup.checkout(), &["remote", "remove", "origin"]);
    setup.archive();
    setup.restore();
    assert_eq!(setup.head(), commit);
    assert_eq!(git(&setup.checkout(), &["remote"]).trim(), "");
}

#[test]
fn a_clone_with_no_upstream_comes_back_with_no_upstream() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.archive();
    setup.restore();
    let config = git(&setup.checkout(), &["config", "--list", "--local"]);
    assert!(!config.contains("branch.triage.merge"), "{config}");
}

#[test]
fn an_error_after_the_rename_puts_the_checkout_back_under_its_own_name() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    let held = quarantine(&checkout).unwrap();
    let head = held.join(".git/HEAD");
    let plan = setup.plan(ArchiveOptions::default());
    set_hook(move |step| {
        if step == "moved:app" {
            write(&head, "not a head\n");
        }
        Ok(())
    });
    let result = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(result.is_err(), "{result:?}");
    assert!(checkout.join(".git").is_dir());
    assert!(!quarantine(&checkout).unwrap().exists());
}

#[test]
fn a_password_leaves_an_ssh_origin_and_its_user_name_stays() {
    assert_eq!(
        crate::mirror::recorded_origin("ssh://git:secret@example.com/app.git"),
        "ssh://git@example.com/app.git"
    );
    assert_eq!(
        crate::mirror::recorded_origin("ssh://git@example.com/app.git"),
        "ssh://git@example.com/app.git"
    );
    assert_eq!(
        crate::mirror::recorded_origin("https://token@example.com/app.git"),
        "https://example.com/app.git"
    );
}

#[test]
fn a_root_directory_that_mxv_cannot_empty_refuses_before_anything_changes() {
    let setup = setup(Kind::Clone);
    setup.create();
    let dir = setup.root.join("triage/ro");
    fs::create_dir_all(&dir).unwrap();
    write(dir.join("a.txt"), "a\n");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let text = refusal_text(setup.planned(force()));
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(text.contains("ro"), "{text}");
    assert!(setup.record().is_none());
}

#[test]
fn a_root_file_that_mxv_cannot_read_refuses_before_anything_changes() {
    let setup = setup(Kind::Clone);
    setup.create();
    let file = setup.root.join("triage/secret.txt");
    write(&file, "a\n");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(text.contains("secret.txt"), "{text}");
    assert!(setup.record().is_none());
}

#[test]
fn a_workspace_manifest_that_mxv_cannot_read_refuses_before_anything_changes() {
    let setup = setup(Kind::Clone);
    setup.create();
    let manifest = setup.root.join("triage/mxv.toml");
    write(&manifest, "# private\n");
    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o000)).unwrap();
    let planned = setup
        .workspaces
        .plan_archive(&setup.archives, &name(), force(), Mode::Change);
    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
    let text = match planned {
        Ok(planned) => refusal_text(planned),
        Err(error) => format!("{error:?}"),
    };
    assert!(text.contains("mxv.toml"), "{text}");
    assert!(setup.record().is_none());
}

#[test]
fn a_root_directory_whose_name_is_not_utf8_refuses_before_anything_changes() {
    let setup = setup(Kind::Clone);
    setup.create();
    let name_bytes = std::ffi::OsStr::from_bytes(b"odd-\xFF");
    // APFS refuses such a name, so only a Linux file system reaches the rest.
    if fs::create_dir(setup.root.join("triage").join(name_bytes)).is_err() {
        return;
    }
    let text = refusal_text(setup.planned(force()));
    assert!(text.contains("Unusable"), "{text}");
    assert!(setup.record().is_none());
}

#[test]
fn a_rerun_copies_a_root_file_again_after_a_second_checkout_stopped_the_first_run() {
    let setup = setup(Kind::Clone);
    let other = fixture_named("web");
    let repos = [Repo::new(&setup.fixture.clone), Repo::new(&other.clone)];
    setup
        .workspaces
        .create_repos(&setup.root, &name(), &repos, Mode::Change, None)
        .unwrap();
    let web = setup.root.join("triage/web");
    let late = web.join("late.txt");
    let held = quarantine(&web).unwrap().join("late.txt");
    let plan = setup.plan(ArchiveOptions::default());
    set_hook(move |step| {
        if step == "moved:web" {
            write(&held, "late\n");
        }
        Ok(())
    });
    let refusal = outcome_refusal(&setup, plan);
    clear_hook();
    assert!(refusal.contains("SourceChanged"), "{refusal}");
    assert!(!setup.checkout().exists(), "the first checkout is retired");
    assert!(late.is_file(), "the second checkout is back in its place");
    fs::remove_file(&late).unwrap();
    let archive = setup.archive();
    assert_eq!(archive.checkouts.len(), 2);
    setup.restore();
    assert!(setup.checkout().join(".git").exists());
    assert!(web.join(".git").exists());
}

#[test]
fn a_restored_manifest_keeps_its_permission_bits() {
    let setup = setup(Kind::Clone);
    setup.create();
    let manifest = setup.root.join("triage/mxv.toml");
    write(&manifest, "# private\n");
    fs::set_permissions(&manifest, fs::Permissions::from_mode(0o600)).unwrap();
    setup.archive();
    setup.restore();
    assert_eq!(
        fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn a_manifest_with_a_second_hard_link_refuses_the_archive() {
    let setup = setup(Kind::Clone);
    setup.create();
    let manifest = setup.root.join("triage/mxv.toml");
    write(&manifest, "# shared\n");
    fs::hard_link(&manifest, setup.base.join("other.toml")).unwrap();
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("mxv.toml"), "{text}");
}

#[test]
fn force_deletes_a_socket_in_the_root_so_the_restore_finds_the_path_free() {
    let setup = setup(Kind::Clone);
    setup.create();
    let _socket = UnixListener::bind(setup.root.join("triage/s")).unwrap();
    let archive = setup.archive_with(force());
    assert!(archive.lost.iter().any(|item| item.contains('s')));
    assert!(!setup.root.join("triage").exists());
    setup.restore();
}

#[test]
fn a_record_whose_root_path_leaves_the_workspace_is_refused_as_corrupt() {
    let setup = setup(Kind::Worktree);
    setup.create();
    write(setup.root.join("triage/notes.md"), "keep\n");
    setup.archive();
    let path = setup.archives.dir_of(&name()).join("archive.json");
    let text = fs::read_to_string(&path).unwrap();
    write(&path, text.replace("\"notes.md\"", "\"../notes.md\""));
    assert!(matches!(
        setup.archives.read(&name()),
        Err(Error::CorruptRecord { .. })
    ));
}

#[test]
fn a_clone_remote_that_the_archive_drops_is_named_in_a_warning_and_a_branch_is_kept() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    git(&checkout, &["branch", "side"]);
    git(
        &checkout,
        &["remote", "add", "fork", "https://example.invalid/app.git"],
    );
    let plan = setup.plan(ArchiveOptions::default());
    let warnings = plan.warnings.join("\n");
    assert!(!warnings.contains("side"), "{warnings}");
    assert!(warnings.contains("fork"), "{warnings}");
    assert!(plan.checkouts[0].kept.branches.contains_key("side"));
}

#[test]
fn a_worktree_made_from_a_clone_is_named_in_a_warning() {
    let setup = setup(Kind::Clone);
    setup.create();
    let linked = setup.base.join("linked");
    git(
        &setup.checkout(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    let plan = setup.plan(ArchiveOptions::default());
    let warnings = plan.warnings.join("\n");
    assert!(
        warnings.contains("made from the clone app") && warnings.contains("linked"),
        "{warnings}"
    );
}

#[test]
fn a_version_one_archive_still_restores() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = setup.commit_locally();
    let archive = setup.archive();
    // A record of version 1 names the pin of HEAD by the folder alone and
    // keeps nothing else.
    let checkout = &archive.checkouts[0];
    let holder = checkout.pin_holder();
    let old_pin = checkout.pin_dir().to_owned();
    Git.delete_ref(holder, &checkout.pin).unwrap();
    git(
        checkout.pin_repository(),
        &["update-ref", &old_pin, &checkout.commit],
    );
    let path = setup.archives.dir_of(&name()).join(RECORD_FILE);
    let text = fs::read_to_string(&path).unwrap();
    let old = text
        .replace("\"version\": 2", "\"version\": 1")
        .replace(&checkout.pin, &old_pin);
    write(&path, old);
    let read = setup.record().unwrap();
    assert_eq!(read.version, 1);
    assert!(read.checkouts[0].kept.is_empty());
    setup.restore();
    assert_eq!(setup.head(), commit);
    assert_eq!(Git.resolve_commit(holder, &old_pin).unwrap(), None);
}

#[test]
fn an_unborn_head_refuses_as_a_checkout_that_cannot_be_read() {
    let setup = setup(Kind::Clone);
    setup.create();
    git(
        &setup.checkout(),
        &["switch", "--quiet", "--orphan", "empty"],
    );
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("Unusable"), "{text}");
}

#[test]
fn a_mirror_that_is_gone_refuses_the_archive() {
    let setup = setup(Kind::Clone);
    let workspace = setup.create();
    let mirror = workspace.checkouts[0].mirror.clone().unwrap();
    fs::remove_dir_all(&mirror).unwrap();
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("MirrorMissing"), "{text}");
}

#[test]
fn a_locked_worktree_refuses_as_a_checkout_that_cannot_be_read() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let path = setup.checkout().to_string_lossy().into_owned();
    git(
        &setup.fixture.clone,
        &["worktree", "lock", "--reason", "mine", &path],
    );
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("Unusable"), "{text}");
}
