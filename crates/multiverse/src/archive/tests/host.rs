//! The tests of a host's data and files through an archive and a restore,
//! under the profile `example` with an [`ArchiveHook`] that carries the
//! files of an evidence directory outside the workspace.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::host::Host;
use crate::record::{HOST_RECORD_VERSION, HostData, HostKey};
use crate::workspaces::{CreateOptions, Repo};

/// What the test's hook reads and how it behaves.
#[derive(Default)]
struct Evidence {
    /// The directory whose entries `save` copies.
    source: PathBuf,
    /// Whether `save` fails.
    save_fails: AtomicBool,
    /// Whether `check` fails.
    check_fails: AtomicBool,
    /// Each directory that `check` was given.
    checked: Mutex<Vec<PathBuf>>,
}

/// The hook of the tests, which copies the evidence directory as it is,
/// links included.
struct EvidenceHook(Arc<Evidence>);

impl ArchiveHook for EvidenceHook {
    fn save(&self, _: &Workspace, into: &Path) -> Result<(), HookError> {
        if self.0.save_fails.load(Ordering::SeqCst) {
            return Err(HookError::new("the evidence store is offline"));
        }
        copy_tree(&self.0.source, into);
        Ok(())
    }

    fn check(&self, _: &Workspace, restored: &Path) -> Result<(), HookError> {
        self.0.checked.lock().unwrap().push(restored.to_path_buf());
        if self.0.check_fails.load(Ordering::SeqCst) {
            return Err(HookError::new("the evidence names another commit"));
        }
        Ok(())
    }
}

/// Copies each entry below `from` into `to`, keeping a link as a link.
fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(entry.path()).unwrap(), &target).unwrap();
        } else if kind.is_dir() {
            fs::create_dir(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            write(&target, fs::read(entry.path()).unwrap());
        }
    }
}

/// Returns the profile named `example`.
fn example() -> Host {
    Host::named("example").unwrap()
}

/// Returns the data that the tests' host keeps.
fn data() -> HostData {
    let mut values = BTreeMap::new();
    values.insert(HostKey::new("runner").unwrap(), "local".to_owned());
    values.insert(HostKey::new("release").unwrap(), "0.7.0".to_owned());
    HostData::new(example().name().clone(), values)
}

/// A setup under the profile `example`, with the hook's evidence.
struct Hosted {
    setup: Setup,
    evidence: Arc<Evidence>,
}

/// Returns the archives of `setup`'s directories under `example`, with the
/// hook of `evidence` when it is given.
fn archives_of(base: &Path, evidence: Option<&Arc<Evidence>>) -> Archives {
    let archives =
        Archives::new(base.join("archives"), base.join("data/archives")).with_host(example());
    match evidence {
        Some(evidence) => archives.with_archive_hook(Box::new(EvidenceHook(Arc::clone(evidence)))),
        None => archives,
    }
}

fn hosted(kind: Kind) -> Hosted {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let mut workspaces = Workspaces::new(Store::new(base.join("state")), Git).with_host(example());
    if kind == Kind::Clone {
        workspaces = workspaces.with_mirrors(base.join("mirrors"));
    }
    workspaces.checkpoint = at_checkpoint;
    let evidence = Arc::new(Evidence {
        source: base.join("evidence"),
        ..Evidence::default()
    });
    fs::create_dir_all(evidence.source.join("notes")).unwrap();
    write(evidence.source.join("run.log"), "passed 12 of 12\n");
    write(evidence.source.join("notes/task.md"), "fix the parser\n");
    let setup = Setup {
        fixture: fixture(),
        root: base.join("work"),
        archives: archives_of(&base, Some(&evidence)),
        runs: Runs::new(&base.join("runs")).with_host(example()),
        base,
        _dir: dir,
        workspaces,
    };
    Hosted { setup, evidence }
}

impl Hosted {
    /// Creates the workspace with the host's data.
    fn create(&self) -> Workspace {
        let setup = &self.setup;
        let outcome = setup
            .workspaces
            .create_with(
                &setup.root,
                &name(),
                &[Repo::new(&setup.fixture.clone)],
                Mode::Change,
                CreateOptions::new(None, Some(data())),
            )
            .unwrap();
        let NewOutcome::Created(workspace) = outcome else {
            panic!("{outcome:?}");
        };
        workspace
    }

    fn host_dir(&self) -> PathBuf {
        self.setup.root.join("triage/.example-host")
    }

    fn kept_dir(&self, archive: &Archive) -> PathBuf {
        self.setup.archives.data_of(&archive.id).join("host")
    }

    /// Runs an archive that is expected to fail, and returns its error.
    fn archive_error(&self) -> Error {
        let plan = self.setup.plan(ArchiveOptions::default());
        match self.setup.workspaces.archive(&self.setup.archives, plan) {
            Err(error) => error,
            Ok(outcome) => panic!("{outcome:?}"),
        }
    }
}

fn host_data_and_files_round_trip(kind: Kind) {
    let hosted = hosted(kind);
    hosted.create();

    let archive = hosted.setup.archive();

    assert_eq!(archive.host, Some(data()), "{kind:?}");
    assert_eq!(archive.version, HOST_ARCHIVE_VERSION, "{kind:?}");
    let paths: Vec<&str> = archive
        .host_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert_eq!(paths, ["notes/task.md", "run.log"], "{kind:?}");
    assert_eq!(archive.host_files[1].size, 16, "{kind:?}");
    assert_eq!(archive.host_files[1].digest.len(), 64, "{kind:?}");
    assert!(!hosted.setup.root.join("triage").exists(), "{kind:?}");
    assert_eq!(
        hosted.setup.record().as_ref(),
        Some(&archive),
        "{kind:?}: the record on disk differs"
    );

    let restored = hosted.setup.restore();

    assert_eq!(restored.workspace.host, Some(data()), "{kind:?}");
    let stored = hosted.setup.workspaces.find(&name()).unwrap().unwrap();
    assert_eq!(stored.host, Some(data()), "{kind:?}");
    assert_eq!(stored.version, HOST_RECORD_VERSION, "{kind:?}");
    assert_eq!(
        fs::read_to_string(hosted.host_dir().join("run.log")).unwrap(),
        "passed 12 of 12\n",
        "{kind:?}"
    );
    assert_eq!(
        fs::read_to_string(hosted.host_dir().join("notes/task.md")).unwrap(),
        "fix the parser\n",
        "{kind:?}"
    );
    assert_eq!(mode_of(&hosted.host_dir()), 0o700, "{kind:?}");
    assert_eq!(
        *hosted.evidence.checked.lock().unwrap(),
        [hosted.host_dir()],
        "{kind:?}"
    );
    assert!(hosted.setup.record().is_none(), "{kind:?}");
}

#[test]
fn a_host_stores_data_and_a_file_archives_restores_and_finds_both_unchanged() {
    host_data_and_files_round_trip(Kind::Worktree);
    host_data_and_files_round_trip(Kind::Clone);
}

#[test]
fn a_restore_with_a_damaged_host_file_refuses_and_keeps_the_archive() {
    let hosted = hosted(Kind::Worktree);
    hosted.create();
    let archive = hosted.setup.archive();
    write(
        hosted.kept_dir(&archive).join("run.log"),
        "passed 0 of 12\n",
    );

    let outcome = hosted.setup.restore_outcome(false);

    let RestoreOutcome::Refused(RestoreRefusal::HostFilesDamaged { paths }) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(paths, ["run.log"]);
    assert_eq!(hosted.setup.record().as_ref(), Some(&archive));
    assert!(!hosted.setup.root.join("triage").exists());
    assert!(hosted.setup.workspaces.find(&name()).unwrap().is_none());
    assert!(hosted.evidence.checked.lock().unwrap().is_empty());
}

#[test]
fn a_restore_whose_hook_check_fails_is_undone_and_keeps_the_archive() {
    let hosted = hosted(Kind::Clone);
    hosted.create();
    let archive = hosted.setup.archive();
    hosted.evidence.check_fails.store(true, Ordering::SeqCst);

    let outcome = hosted.setup.restore_outcome(false);

    let RestoreOutcome::Refused(RestoreRefusal::HostCheckFailed { message }) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(message, "the evidence names another commit");
    assert_eq!(hosted.setup.record().as_ref(), Some(&archive));
    assert!(!hosted.setup.root.join("triage").exists());
    assert!(hosted.setup.workspaces.find(&name()).unwrap().is_none());
    hosted.evidence.check_fails.store(false, Ordering::SeqCst);
    hosted.setup.restore();
    assert!(hosted.host_dir().join("run.log").is_file());
}

#[test]
fn a_restore_without_a_hook_refuses_an_archive_with_host_files_before_writing() {
    let hosted = hosted(Kind::Worktree);
    hosted.create();
    let archive = hosted.setup.archive();
    let unhooked = archives_of(&hosted.setup.base, None);

    let planned = hosted
        .setup
        .workspaces
        .plan_restore(&unhooked, &name(), RestoreOptions::default())
        .unwrap();

    let RestorePlanned::Refused(RestoreRefusal::HostHookMissing { owner }) = planned else {
        panic!("{planned:?}");
    };
    assert_eq!(owner.as_ref(), Some(example().name()));
    assert_eq!(unhooked.read(&name()).unwrap().as_ref(), Some(&archive));
    assert!(!hosted.setup.root.join("triage").exists());
}

#[test]
fn an_archive_whose_save_fails_leaves_the_workspace_whole_and_the_rerun_finishes() {
    let hosted = hosted(Kind::Clone);
    hosted.create();
    let before = status(&hosted.setup.checkout());
    hosted.evidence.save_fails.store(true, Ordering::SeqCst);

    let error = hosted.archive_error();

    let Error::HostHookFailed {
        name: failed,
        message,
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(*failed, name());
    assert_eq!(message, "the evidence store is offline");
    assert!(error.to_string().contains("before it moved any checkout"));
    assert_eq!(status(&hosted.setup.checkout()), before);
    let stopped = hosted.setup.record().unwrap();
    assert_eq!(stopped.state, ArchiveState::Retiring);
    assert!(stopped.host_files.is_empty());
    let partial = hosted
        .setup
        .archives
        .data_of(&stopped.id)
        .join("host.partial");
    assert!(
        partial.is_dir(),
        "the failed save left no partial directory"
    );

    hosted.evidence.save_fails.store(false, Ordering::SeqCst);
    let archive = hosted.setup.archive();

    assert_eq!(archive.host_files.len(), 2);
    assert!(!partial.exists());
    assert!(hosted.kept_dir(&archive).join("run.log").is_file());
}

#[test]
fn an_archive_refuses_a_host_symlink_and_more_than_1000_host_files_before_quarantine() {
    let hosted = hosted(Kind::Worktree);
    hosted.create();
    let link = hosted.evidence.source.join("latest");
    std::os::unix::fs::symlink("run.log", &link).unwrap();

    let error = hosted.archive_error();
    let Error::HostFileUnkeepable { path, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(path, "latest");
    assert!(hosted.setup.checkout().join(".git").exists());

    fs::remove_file(&link).unwrap();
    fs::create_dir(hosted.evidence.source.join("many")).unwrap();
    for index in 0..MOST_HOST_FILES {
        write(
            hosted.evidence.source.join(format!("many/{index}.txt")),
            "x",
        );
    }

    let error = hosted.archive_error();
    let Error::HostFilesTooMany { count, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(count, MOST_HOST_FILES + 1);
    assert!(hosted.setup.checkout().join(".git").exists());
    let held = quarantine(&hosted.setup.checkout(), &example()).unwrap();
    assert!(!held.exists());
}

#[test]
fn a_later_archive_does_not_copy_the_host_directory_as_a_root_entry() {
    let hosted = hosted(Kind::Worktree);
    hosted.create();
    hosted.setup.archive();
    hosted.setup.restore();
    assert!(hosted.host_dir().join("run.log").is_file());
    write(hosted.setup.root.join("triage/notes.md"), "mine\n");

    let archive = hosted.setup.archive();

    let root: Vec<&str> = archive
        .root
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(root, ["notes.md"]);
    assert_eq!(archive.host_files.len(), 2);
    assert!(!hosted.setup.root.join("triage").exists());
}

/// Stops an archive at `step`, changes the files that the hook's save
/// would write, runs the archive again, and checks that the archive keeps
/// the files of the first run and a restore puts them back.
fn host_files_survive_a_resume_after(kind: Kind, step: &'static str) {
    let hosted = hosted(kind);
    hosted.create();
    stop_at(step);
    let plan = hosted.setup.plan(ArchiveOptions::default());
    let stopped = hosted
        .setup
        .workspaces
        .archive(&hosted.setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "{kind:?}, {step}: the run did not stop");
    let first = hosted.setup.record().unwrap().host_files;
    assert_eq!(first.len(), 2, "{kind:?}, {step}");
    fs::remove_dir_all(&hosted.evidence.source).unwrap();
    fs::create_dir(&hosted.evidence.source).unwrap();
    write(hosted.evidence.source.join("run.log"), "passed 0 of 12\n");

    let mut archive = match hosted.setup.planned(ArchiveOptions::default()) {
        ArchivePlanned::Finish(archive) | ArchivePlanned::Unchanged(archive) => *archive,
        ArchivePlanned::Ready(plan) => {
            match hosted
                .setup
                .workspaces
                .archive(&hosted.setup.archives, *plan)
                .unwrap()
            {
                ArchiveOutcome::Retired { archive, .. } => *archive,
                ArchiveOutcome::Refused(refusal) => panic!("{kind:?}, {step}: {refusal:?}"),
            }
        }
        ArchivePlanned::Refused(refusal) => panic!("{kind:?}, {step}: {refusal:?}"),
    };
    hosted
        .setup
        .workspaces
        .finish_archive(&hosted.setup.archives, &mut archive)
        .unwrap();

    assert_eq!(archive.host_files, first, "{kind:?}, {step}");
    for file in &first {
        let kept = hosted.kept_dir(&archive).join(&file.path);
        assert_eq!(
            crate::store::file_digest(&kept).unwrap(),
            file.digest,
            "{kind:?}, {step}: {}",
            file.path
        );
    }
    hosted.setup.restore();
    assert_eq!(
        fs::read_to_string(hosted.host_dir().join("run.log")).unwrap(),
        "passed 12 of 12\n",
        "{kind:?}, {step}"
    );
    assert!(
        hosted.host_dir().join("notes/task.md").is_file(),
        "{kind:?}, {step}"
    );
}

#[test]
fn an_archive_stopped_before_its_host_step_whose_checkout_is_then_deleted_keeps_the_host_files() {
    for step in ["record", "wip", "pins", "ignored", "root"] {
        let hosted = hosted(Kind::Clone);
        hosted.create();
        stop_at(step);
        let plan = hosted.setup.plan(ArchiveOptions::default());
        let stopped = hosted
            .setup
            .workspaces
            .archive(&hosted.setup.archives, plan);
        clear_hook();
        assert!(stopped.is_err(), "{step}: the run did not stop");
        assert!(
            hosted.setup.record().unwrap().host_files.is_empty(),
            "{step}"
        );
        fs::remove_dir_all(hosted.setup.checkout()).unwrap();

        let archive = hosted.setup.archive();

        let paths: Vec<&str> = archive
            .host_files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        assert_eq!(paths, ["notes/task.md", "run.log"], "{step}");
        assert!(
            hosted.kept_dir(&archive).join("run.log").is_file(),
            "{step}"
        );
    }
}

#[test]
fn an_archive_resumed_after_its_host_step_keeps_the_host_files_of_the_first_run() {
    for kind in [Kind::Worktree, Kind::Clone] {
        for step in [
            "host",
            "before-move:app",
            "cleared:app",
            "retired:app",
            "delete",
        ] {
            host_files_survive_a_resume_after(kind, step);
        }
    }
}

#[test]
fn a_restore_of_an_archive_without_host_data_or_files_skips_the_host_step() {
    let hosted = hosted(Kind::Worktree);
    fs::remove_dir_all(&hosted.evidence.source).unwrap();
    fs::create_dir(&hosted.evidence.source).unwrap();
    hosted.setup.create();
    let archive = hosted.setup.archive();
    assert!(archive.host.is_none());
    assert!(archive.host_files.is_empty());
    assert_eq!(archive.version, NO_HOST_ARCHIVE_VERSION);
    set_hook(|at| {
        assert_ne!(at, "host", "killed at host");
        Ok(())
    });
    let plan = hosted.setup.restore_plan();
    let result = catch_unwind(AssertUnwindSafe(|| {
        drop(
            hosted
                .setup
                .workspaces
                .restore(&hosted.setup.archives, plan, &hosted.setup.runs),
        );
    }));
    clear_hook();
    assert!(result.is_err(), "the run was not killed");
    let marked = hosted.setup.record().unwrap();
    assert_eq!(marked.version, NO_HOST_ARCHIVE_VERSION);
    let mark = serde_json::to_value(marked.restoring.as_ref().unwrap()).unwrap();
    assert!(mark.get("host").is_none(), "{mark}");

    hosted.setup.restore();

    assert!(!hosted.host_dir().exists());
    assert!(hosted.evidence.checked.lock().unwrap().is_empty());
}

#[test]
fn a_restore_killed_partway_through_the_host_file_copy_is_undone_by_the_next_restore() {
    let hosted = hosted(Kind::Clone);
    hosted.create();
    hosted.setup.archive();
    set_hook(|at| {
        assert_ne!(at, "host:notes/task.md", "killed during the copy");
        Ok(())
    });
    let plan = hosted.setup.restore_plan();
    let result = catch_unwind(AssertUnwindSafe(|| {
        drop(
            hosted
                .setup
                .workspaces
                .restore(&hosted.setup.archives, plan, &hosted.setup.runs),
        );
    }));
    clear_hook();
    assert!(result.is_err(), "the run was not killed");
    assert!(hosted.host_dir().join("notes/task.md").is_file());
    assert!(!hosted.host_dir().join("run.log").exists());

    hosted.setup.restore();

    assert_eq!(
        fs::read_to_string(hosted.host_dir().join("run.log")).unwrap(),
        "passed 12 of 12\n"
    );
    assert_eq!(
        *hosted.evidence.checked.lock().unwrap(),
        [hosted.host_dir()]
    );
    assert!(hosted.setup.record().is_none());
}

#[test]
fn a_restore_killed_during_the_host_step_is_undone_by_the_next_restore() {
    let hosted = hosted(Kind::Worktree);
    hosted.create();
    hosted.setup.archive();
    set_hook(|at| {
        assert_ne!(at, "host", "killed at host");
        Ok(())
    });
    let plan = hosted.setup.restore_plan();
    let result = catch_unwind(AssertUnwindSafe(|| {
        drop(
            hosted
                .setup
                .workspaces
                .restore(&hosted.setup.archives, plan, &hosted.setup.runs),
        );
    }));
    clear_hook();
    assert!(result.is_err(), "the run was not killed");
    let marked = hosted.setup.record().unwrap();
    let mark = serde_json::to_value(marked.restoring.as_ref().unwrap()).unwrap();
    assert_eq!(mark["host"], true, "{mark}");
    assert!(hosted.host_dir().join("run.log").is_file());

    hosted.setup.restore();

    assert_eq!(
        fs::read_to_string(hosted.host_dir().join("run.log")).unwrap(),
        "passed 12 of 12\n"
    );
    assert!(hosted.setup.record().is_none());
}

/// A workspace record, an archive record and the same archive record with
/// a restore's mark, which multiverse 0.6.0 wrote. A test at the v0.6.0
/// tag made them with that version's code. The directories of the test's
/// temporary repository were then replaced with `/work` and `/fixture`, and
/// the percent-encoded fixture directory inside each `mirror` path with
/// `fixture`. Nothing else was changed.
const WORKSPACE_0_6_0: &str = include_str!("v0_6_0/workspace.json");
const ARCHIVE_0_6_0: &str = include_str!("v0_6_0/archive.json");
const RESTORING_ARCHIVE_0_6_0: &str = include_str!("v0_6_0/archive-restoring.json");

#[test]
fn a_0_6_0_record_and_archive_are_byte_identical_after_a_0_7_0_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("state"));
    let record = dir.path().join("state/triage.json");
    fs::create_dir_all(record.parent().unwrap()).unwrap();
    write(&record, WORKSPACE_0_6_0);
    let mut workspace = store.read(&name()).unwrap().unwrap();
    assert!(workspace.host.is_none());
    store.update(&mut workspace).unwrap();
    assert_eq!(fs::read_to_string(&record).unwrap(), WORKSPACE_0_6_0);

    let archives = Archives::new(dir.path().join("archives"), dir.path().join("data"));
    let path = archives.dir_of(&name()).join(RECORD_FILE);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for text in [ARCHIVE_0_6_0, RESTORING_ARCHIVE_0_6_0] {
        write(&path, text);
        let mut archive = archives.read(&name()).unwrap().unwrap();
        assert!(archive.host.is_none());
        assert!(archive.host_files.is_empty());
        archives.write(&mut archive).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(archive.version, NO_HOST_ARCHIVE_VERSION);
    }
    let restoring = archives.read(&name()).unwrap().unwrap();
    let mark = serde_json::to_value(restoring.restoring.as_ref().unwrap()).unwrap();
    assert!(mark.get("host").is_none(), "{mark}");
}
