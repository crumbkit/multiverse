use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use phase::WorkspaceId;

use super::{Broken, Health, Problem, Repair};
use crate::archive::{
    ARCHIVE_VERSION, Archive, ArchiveRecords, ArchiveState, ArchivedCheckout, Kept,
};
use crate::error::Error;
use crate::git::Git;
use crate::name::WorkspaceName;
use crate::record::{Checkout, Workspace, WorkspaceKind};
use crate::store::Records;
use crate::testing::{fixture, git};

/// Classifies `records` and `archives`, reading pins through git.
fn classify(records: &Records, archives: &ArchiveRecords) -> Health {
    super::classify(&Git, records, archives)
}

/// Returns `archive` with one checkout `app` whose pin lies in
/// `repository` and names `commit`.
fn with_checkout(mut archive: Archive, repository: &Path, commit: &str) -> Archive {
    archive.kind = WorkspaceKind::Worktree;
    archive.checkouts = vec![ArchivedCheckout {
        folder: "app".to_owned(),
        path: archive.path.join("app"),
        source: repository.to_path_buf(),
        origin: None,
        branch: archive.name.to_string(),
        detached: false,
        base: "refs/remotes/origin/main".to_owned(),
        upstream: None,
        upstream_remote: None,
        commit: commit.to_owned(),
        mirror: None,
        pin: format!("refs/mxv/archives/{}/app/head", archive.id),
        local_only: Vec::new(),
        kept: Kept::default(),
    }];
    archive
}

fn name(text: &str) -> WorkspaceName {
    text.parse().unwrap()
}

fn id(digit: char) -> WorkspaceId {
    WorkspaceId::try_from(std::iter::repeat_n(digit, 32).collect::<String>()).unwrap()
}

/// Returns the record of a workspace `text` under `root` with the id
/// `digit` and the checkouts `folders`, and makes each checkout's
/// directory.
fn workspace(root: &Path, text: &str, digit: char, folders: &[&str]) -> Workspace {
    let path = root.join(text);
    let checkouts = folders
        .iter()
        .map(|folder| {
            let checkout = path.join(folder);
            fs::create_dir_all(&checkout).unwrap();
            Checkout::new(
                (*folder).to_owned(),
                &root.join("src").join(folder),
                checkout,
                text.to_owned(),
                "refs/remotes/origin/main".to_owned(),
            )
        })
        .collect();
    let mut workspace = Workspace::new(name(text), path, checkouts);
    workspace.id = Some(id(digit));
    workspace
}

/// Returns an archive of the workspace `text` with the id `digit`, in
/// `state`. The archive's own id is made of `digit` too.
fn archive(text: &str, digit: char, state: ArchiveState) -> Archive {
    Archive {
        version: ARCHIVE_VERSION,
        id: std::iter::repeat_n(digit, 32).collect(),
        state,
        time: 0,
        name: name(text),
        workspace_id: Some(id(digit)),
        kind: WorkspaceKind::Clone,
        path: PathBuf::from("/home/agent/mxv").join(text),
        ports: BTreeMap::new(),
        pinned: false,
        containers_ran: false,
        checkouts: Vec::new(),
        lost: Vec::new(),
        root: Vec::new(),
        manifest: None,
        manifest_mode: None,
        ignored: Vec::new(),
    }
}

fn records(workspaces: Vec<Workspace>, problems: Vec<Error>) -> Records {
    Records {
        workspaces,
        problems,
    }
}

fn archives(archives: Vec<Archive>, problems: Vec<Error>) -> ArchiveRecords {
    ArchiveRecords { archives, problems }
}

fn corrupt(path: &Path) -> Error {
    Error::CorruptRecord {
        path: path.to_path_buf(),
        source: serde_json::from_str::<u32>("{").unwrap_err(),
    }
}

#[test]
fn a_whole_workspace_and_a_finished_archive_are_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let health = classify(
        &records(vec![workspace(dir.path(), "triage", '1', &["app"])], vec![]),
        &archives(vec![archive("docs", '2', ArchiveState::Archived)], vec![]),
    );
    assert!(health.workspaces.is_empty(), "{health:?}");
    assert!(health.archives.is_empty(), "{health:?}");
}

#[test]
fn an_unfinished_archive_with_no_workspace_is_finished_by_archiving_it_again() {
    let health = classify(
        &records(vec![], vec![]),
        &archives(
            vec![archive("old-api", '1', ArchiveState::Retiring)],
            vec![],
        ),
    );
    let expected = Broken {
        name: "old-api".to_owned(),
        problem: Problem::ArchiveUnfinished,
        repair: Some(Repair::Archive(name("old-api"))),
    };
    assert_eq!(health.archives, std::slice::from_ref(&expected));
    assert_eq!(health.workspaces, [expected]);
}

#[test]
fn a_workspace_whose_archive_stopped_after_retiring_its_checkouts_is_finished_by_archiving_it() {
    let dir = tempfile::tempdir().unwrap();
    let triage = workspace(dir.path(), "triage", '1', &["app"]);
    fs::remove_dir_all(dir.path().join("triage/app")).unwrap();
    let health = classify(
        &records(vec![triage], vec![]),
        &archives(vec![archive("triage", '1', ArchiveState::Retiring)], vec![]),
    );
    let expected = Broken {
        name: "triage".to_owned(),
        problem: Problem::ArchiveUnfinished,
        repair: Some(Repair::Archive(name("triage"))),
    };
    assert_eq!(health.workspaces, std::slice::from_ref(&expected));
    assert_eq!(health.archives, [expected]);
}

#[test]
fn an_unfinished_archive_beside_a_newer_workspace_is_named_on_the_archive_alone() {
    let dir = tempfile::tempdir().unwrap();
    let health = classify(
        &records(vec![workspace(dir.path(), "docs", '2', &["app"])], vec![]),
        &archives(vec![archive("docs", '1', ArchiveState::Retiring)], vec![]),
    );
    assert!(health.workspaces.is_empty(), "{health:?}");
    assert_eq!(
        health.archives,
        [Broken {
            name: "docs".to_owned(),
            problem: Problem::ArchiveBesideNewerWorkspace,
            repair: Some(Repair::Repair(name("docs"))),
        }]
    );
}

#[test]
fn a_dated_copy_that_a_stopped_repair_left_is_finished_by_repairing_the_old_name() {
    let dir = tempfile::tempdir().unwrap();
    let old = archive("docs", '1', ArchiveState::Retiring);
    let mut dated = old.clone();
    dated.name = name("docs-2026-11-02");
    let health = classify(
        &records(vec![workspace(dir.path(), "docs", '2', &["app"])], vec![]),
        &archives(vec![old, dated], vec![]),
    );
    assert_eq!(
        health.archives,
        [
            Broken {
                name: "docs".to_owned(),
                problem: Problem::ArchiveBesideNewerWorkspace,
                repair: Some(Repair::Repair(name("docs"))),
            },
            Broken {
                name: "docs-2026-11-02".to_owned(),
                problem: Problem::ArchiveUnfinished,
                repair: Some(Repair::Repair(name("docs"))),
            },
        ]
    );
}

#[test]
fn an_unfinished_archive_whose_pins_are_gone_is_repaired_and_not_finished() {
    let fixture = fixture();
    let commit = git(&fixture.clone, &["rev-parse", "HEAD"]);
    let pinless = with_checkout(
        archive("old-api", '1', ArchiveState::Retiring),
        &fixture.clone,
        commit.trim(),
    );
    let health = classify(&records(vec![], vec![]), &archives(vec![pinless], vec![]));
    let expected = Broken {
        name: "old-api".to_owned(),
        problem: Problem::ArchivePinsGone,
        repair: Some(Repair::Repair(name("old-api"))),
    };
    assert_eq!(health.archives, std::slice::from_ref(&expected));
    assert_eq!(health.workspaces, [expected]);
}

#[test]
fn an_unfinished_archive_whose_pin_is_there_is_finished_by_archiving_it() {
    let fixture = fixture();
    let commit = git(&fixture.clone, &["rev-parse", "HEAD"]);
    let pinned = with_checkout(
        archive("old-api", '1', ArchiveState::Retiring),
        &fixture.clone,
        commit.trim(),
    );
    git(
        &fixture.clone,
        &["update-ref", &pinned.checkouts[0].pin, commit.trim()],
    );
    let health = classify(&records(vec![], vec![]), &archives(vec![pinned], vec![]));
    assert_eq!(health.archives[0].problem, Problem::ArchiveUnfinished);
    assert_eq!(
        health.archives[0].repair,
        Some(Repair::Archive(name("old-api")))
    );
}

#[test]
fn a_finished_archive_beside_the_workspace_it_holds_is_a_restore_that_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let health = classify(
        &records(vec![workspace(dir.path(), "docs", '1', &["app"])], vec![]),
        &archives(vec![archive("docs", '1', ArchiveState::Archived)], vec![]),
    );
    let expected = Broken {
        name: "docs".to_owned(),
        problem: Problem::RestoreUnfinished,
        repair: Some(Repair::Repair(name("docs"))),
    };
    assert_eq!(health.archives, std::slice::from_ref(&expected));
    assert_eq!(health.workspaces, [expected]);
}

#[test]
fn a_finished_archive_and_a_workspace_without_ids_are_not_a_restore_that_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut docs = workspace(dir.path(), "docs", '1', &["app"]);
    docs.id = None;
    let mut archived = archive("docs", '1', ArchiveState::Archived);
    archived.workspace_id = None;
    let health = classify(
        &records(vec![docs], vec![]),
        &archives(vec![archived], vec![]),
    );
    assert!(health.workspaces.is_empty(), "{health:?}");
    assert!(health.archives.is_empty(), "{health:?}");
}

#[test]
fn a_workspace_whose_checkout_directories_are_all_gone_is_repaired_by_removing_it() {
    let dir = tempfile::tempdir().unwrap();
    let triage = workspace(dir.path(), "triage", '1', &["api", "web"]);
    fs::remove_dir_all(dir.path().join("triage")).unwrap();
    let health = classify(&records(vec![triage], vec![]), &archives(vec![], vec![]));
    assert_eq!(
        health.workspaces,
        [Broken {
            name: "triage".to_owned(),
            problem: Problem::CheckoutsGone,
            repair: Some(Repair::Remove(name("triage"))),
        }]
    );
}

#[test]
fn a_workspace_with_one_checkout_directory_left_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let triage = workspace(dir.path(), "triage", '1', &["api", "web"]);
    fs::remove_dir_all(dir.path().join("triage/web")).unwrap();
    let health = classify(&records(vec![triage], vec![]), &archives(vec![], vec![]));
    assert!(health.workspaces.is_empty(), "{health:?}");
}

#[test]
fn a_workspace_whose_root_is_gone_too_is_not_named_for_removal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("unmounted");
    let triage = workspace(&root, "triage", '1', &["app"]);
    fs::remove_dir_all(&root).unwrap();
    let health = classify(&records(vec![triage], vec![]), &archives(vec![], vec![]));
    assert!(health.workspaces.is_empty(), "{health:?}");
}

#[test]
fn a_bare_workspace_with_no_checkouts_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let bare = workspace(dir.path(), "notes", '1', &[]);
    let health = classify(&records(vec![bare], vec![]), &archives(vec![], vec![]));
    assert!(health.workspaces.is_empty(), "{health:?}");
}

#[test]
fn an_unreadable_record_is_named_by_its_file_and_moved_aside_by_hand() {
    let workspace_record = PathBuf::from("/state/mxv/workspaces/broken.json");
    let archive_record = PathBuf::from("/state/mxv/archives/gone/archive.json");
    let health = classify(
        &records(vec![], vec![corrupt(&workspace_record)]),
        &archives(vec![], vec![corrupt(&archive_record)]),
    );
    assert_eq!(
        health.workspaces,
        [Broken {
            name: "broken".to_owned(),
            problem: Problem::Unreadable {
                reason: corrupt(&workspace_record).to_string(),
            },
            repair: Some(Repair::MoveAside(workspace_record)),
        }]
    );
    assert_eq!(
        health.archives,
        [Broken {
            name: "gone".to_owned(),
            problem: Problem::Unreadable {
                reason: corrupt(&archive_record).to_string(),
            },
            repair: Some(Repair::MoveAside(archive_record)),
        }]
    );
}

#[test]
fn broken_records_are_listed_unreadable_first_and_then_in_name_order() {
    let dir = tempfile::tempdir().unwrap();
    let zeta = workspace(dir.path(), "zeta", '1', &["app"]);
    fs::remove_dir_all(dir.path().join("zeta")).unwrap();
    let health = classify(
        &records(
            vec![zeta],
            vec![corrupt(Path::new("/state/mxv/workspaces/broken.json"))],
        ),
        &archives(vec![archive("alpha", '2', ArchiveState::Retiring)], vec![]),
    );
    let names: Vec<&str> = health
        .workspaces
        .iter()
        .map(|broken| broken.name.as_str())
        .collect();
    assert_eq!(names, ["broken", "alpha", "zeta"]);
}
