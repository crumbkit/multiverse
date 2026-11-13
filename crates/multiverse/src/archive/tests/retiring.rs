//! The tests of the stored states of an archive: the workspace's
//! `retiring`, which names its archive, and the archive's `retired`, which
//! says that every checkout was retired.

use std::rc::Rc;

use super::*;
use crate::health::{self, Problem, Repaired};
use crate::record::WorkspaceState;
use crate::workspaces::{Allow, RemovalOutcome, RemovalPlan};

/// Runs the archive of the setup's workspace until it stops at `step`.
fn stopped_at(setup: &Setup, step: &'static str) {
    stop_at(step);
    let plan = setup.plan(ArchiveOptions::default());
    let stopped = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "{step}: the run did not stop");
}

/// Plans and runs the archive of the setup's workspace again, and returns
/// its record once the workspace is gone, before it is marked archived.
fn resumed(setup: &Setup) -> Archive {
    match setup.planned(ArchiveOptions::default()) {
        ArchivePlanned::Ready(plan) => {
            match setup.workspaces.archive(&setup.archives, *plan).unwrap() {
                ArchiveOutcome::Retired { archive, .. } => *archive,
                ArchiveOutcome::Refused(refusal) => panic!("{refusal:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

/// Returns the workspace record of the setup's workspace.
fn workspace(setup: &Setup) -> Workspace {
    setup.workspaces.store.read(&name()).unwrap().unwrap()
}

/// Rewrites the workspace record of the setup's workspace through `edit`,
/// as an older mxv or a person's editor would leave it.
fn edit_workspace(
    setup: &Setup,
    edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) {
    let path = setup.workspaces.store.path(&name());
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(record.as_object_mut().unwrap());
    write(&path, serde_json::to_vec_pretty(&record).unwrap());
}

/// Rewrites the archive record of the setup's workspace through `edit`.
fn edit_archive(setup: &Setup, edit: impl FnOnce(&mut Archive)) {
    let mut archive = setup.record().unwrap();
    edit(&mut archive);
    let path = setup.archives.dir_of(&name()).join("archive.json");
    write(&path, serde_json::to_vec_pretty(&archive).unwrap());
}

/// The id that `state` names.
fn named(state: &WorkspaceState) -> ArchiveId {
    state.archive().cloned().unwrap()
}

#[test]
fn an_archive_stopped_after_the_retiring_state_reuses_its_archive_id_when_it_runs_again() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        stopped_at(&setup, "state");
        assert!(setup.record().is_none(), "{kind:?}");
        let id = named(&workspace(&setup).state());
        let plan = setup.plan(ArchiveOptions::default());
        assert_eq!(plan.id, id, "{kind:?}");
        let archive = resumed(&setup);
        assert_eq!(archive.id, id, "{kind:?}");
        assert_eq!(archive.state, ArchiveState::Retired, "{kind:?}");
    }
}

#[test]
fn an_archive_writes_the_retiring_state_before_its_record_and_retired_before_the_deletion() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        stopped_at(&setup, "record");
        let archive = setup.record().unwrap();
        assert_eq!(archive.state, ArchiveState::Retiring, "{kind:?}");
        assert_eq!(archive.version, ARCHIVE_VERSION, "{kind:?}");
        assert_eq!(
            workspace(&setup).state(),
            WorkspaceState::retiring(archive.id.clone()),
            "{kind:?}"
        );
        let setup = setup_stopped(kind, "retired");
        assert_eq!(
            setup.record().unwrap().state,
            ArchiveState::Retired,
            "{kind:?}"
        );
        assert!(
            setup.workspaces.store.read(&name()).unwrap().is_some(),
            "{kind:?}"
        );
    }
}

/// Returns a setup whose archive stopped at `step`.
fn setup_stopped(kind: Kind, step: &'static str) -> Setup {
    let setup = setup(kind);
    setup.create();
    stopped_at(&setup, step);
    setup
}

#[test]
fn a_resumed_retired_archive_is_never_written_retiring() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup_stopped(kind, "retired");
        let archives = setup.archives.clone();
        let seen = Rc::new(RefCell::new(Vec::new()));
        let found = Rc::clone(&seen);
        set_hook(move |step| {
            if let Some(archive) = archives.read(&name()).unwrap() {
                found.borrow_mut().push((step.to_owned(), archive.state));
            }
            Ok(())
        });
        let archive = resumed(&setup);
        clear_hook();
        assert_eq!(archive.state, ArchiveState::Retired, "{kind:?}");
        let seen = seen.borrow();
        assert!(!seen.is_empty(), "{kind:?}");
        assert!(
            seen.iter()
                .all(|(_, state)| *state == ArchiveState::Retired),
            "{kind:?}: {seen:?}"
        );
    }
}

/// Removes the setup's workspace as `mxv remove --force` does.
fn remove_workspace(setup: &Setup) {
    let allow = Allow {
        discard_changes: true,
        delete_unmerged: true,
    };
    let RemovalPlan::Ready(removal) = setup.workspaces.plan_removal(&name(), allow).unwrap() else {
        panic!("the removal is not ready");
    };
    let outcome = setup.workspaces.remove(removal).unwrap();
    assert!(matches!(outcome, RemovalOutcome::Done(_)), "{outcome:?}");
    assert!(setup.workspaces.store.read(&name()).unwrap().is_none());
}

#[test]
fn an_archive_stopped_half_way_whose_workspace_is_removed_is_refused_and_moved_aside() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup_stopped(kind, "pins");
        remove_workspace(&setup);
        let refusal = setup.refusal(ArchiveOptions::default());
        assert!(
            matches!(refusal, ArchiveRefusal::WorkspaceRemoved(_)),
            "{kind:?}: {refusal:?}"
        );
        let health = health::classify(
            &setup.workspaces.backend,
            &setup.workspaces.store.list().unwrap(),
            &setup.archives.list().unwrap(),
        );
        assert_eq!(
            health.archives[0].problem,
            Problem::ArchiveWorkspaceRemoved,
            "{kind:?}"
        );
        let repaired = setup
            .workspaces
            .repair(&setup.archives, &name(), Mode::Change)
            .unwrap();
        assert!(
            matches!(repaired, Repaired::MovedAside { .. }),
            "{kind:?}: {repaired:?}"
        );
        assert!(setup.record().is_none(), "{kind:?}");
    }
}

#[test]
fn a_retired_archive_whose_workspace_record_is_gone_is_finished() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup_stopped(kind, "removed");
        let planned = setup.planned(ArchiveOptions::default());
        assert!(
            matches!(&planned, ArchivePlanned::Finish(archive) if archive.state == ArchiveState::Retired),
            "{kind:?}: {planned:?}"
        );
    }
}

/// Archives a workspace of the fixture and a second repository `web`, and
/// stops once the workspace's record is gone, so the archive is retired.
/// The fixture of `web` comes back too, because a worktree's pin lies in
/// its repository.
fn retired_with_two_checkouts(kind: Kind) -> (Setup, Fixture) {
    let setup = setup(kind);
    let other = fixture_named("web");
    let repos = [Repo::new(&setup.fixture.clone), Repo::new(&other.clone)];
    setup
        .workspaces
        .create_repos(&setup.root, &name(), &repos, Mode::Change, None)
        .unwrap();
    stopped_at(&setup, "removed");
    (setup, other)
}

#[test]
fn a_retired_archive_with_one_of_two_head_pins_deleted_is_refused_as_pins_gone() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let (setup, _web) = retired_with_two_checkouts(kind);
        let archive = setup.record().unwrap();
        let web = &archive.checkouts[1];
        git(web.pin_repository(), &["update-ref", "-d", &web.pin]);
        let refusal = setup.refusal(ArchiveOptions::default());
        assert!(
            matches!(refusal, ArchiveRefusal::PinsGone(_)),
            "{kind:?}: {refusal:?}"
        );
    }
}

#[test]
fn a_retired_archive_with_both_head_pins_is_finished() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let (setup, _web) = retired_with_two_checkouts(kind);
        let planned = setup.planned(ArchiveOptions::default());
        assert!(
            matches!(planned, ArchivePlanned::Finish(_)),
            "{kind:?}: {planned:?}"
        );
    }
}

#[test]
fn a_retiring_record_without_its_state_key_beside_its_archive_resumes() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup_stopped(kind, "record");
        let id = setup.record().unwrap().id;
        edit_workspace(&setup, |record| {
            record.remove("state");
        });
        assert_eq!(workspace(&setup).state(), WorkspaceState::Live, "{kind:?}");
        let health = health::classify(
            &setup.workspaces.backend,
            &setup.workspaces.store.list().unwrap(),
            &setup.archives.list().unwrap(),
        );
        assert_eq!(
            health.workspaces[0].problem,
            Problem::ArchiveUnfinished,
            "{kind:?}"
        );
        let archive = resumed(&setup);
        assert_eq!(archive.id, id, "{kind:?}");
    }
}

#[test]
fn a_state_that_names_another_archive_than_the_record_is_repointed_and_the_archive_resumes() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup_stopped(kind, "record");
        let id = setup.record().unwrap().id;
        edit_workspace(&setup, |record| {
            record.insert(
                "state".to_owned(),
                serde_json::json!({ "retiring": { "archive": "b".repeat(32) } }),
            );
        });
        stop_at("record");
        let plan = setup.plan(ArchiveOptions::default());
        assert_eq!(plan.id, id, "{kind:?}");
        let stopped = setup.workspaces.archive(&setup.archives, plan);
        clear_hook();
        assert!(stopped.is_err(), "{kind:?}");
        assert_eq!(
            workspace(&setup).state(),
            WorkspaceState::retiring(id.clone()),
            "{kind:?}"
        );
        assert_eq!(resumed(&setup).id, id, "{kind:?}");
    }
}

/// Leaves the pair that an older mxv left when its archive stopped after
/// the record: a retiring archive of version 2 and a workspace record
/// without a state. Without `ids`, neither record holds a stable id.
fn older_pair(kind: Kind, ids: bool) -> Setup {
    let setup = setup_stopped(kind, "record");
    edit_archive(&setup, |archive| {
        archive.version = OLDER_ARCHIVE_VERSION;
        if !ids {
            archive.workspace_id = None;
        }
    });
    edit_workspace(&setup, |record| {
        record.remove("state");
        if !ids {
            record.remove("id");
            record.insert("version".to_owned(), 2.into());
        }
    });
    setup
}

#[test]
fn an_older_retiring_archive_beside_a_record_without_a_state_resumes_as_version_three() {
    for kind in [Kind::Worktree, Kind::Clone] {
        for ids in [true, false] {
            let setup = older_pair(kind, ids);
            let id = setup.record().unwrap().id;
            // A run that stops between the advance and the archive's write
            // leaves a record that names the archive, beside the older one.
            stopped_at(&setup, "state");
            let held = workspace(&setup);
            assert!(held.id.is_some(), "{kind:?}, ids {ids}");
            assert_eq!(held.state(), WorkspaceState::retiring(id.clone()));
            assert_eq!(setup.record().unwrap().version, OLDER_ARCHIVE_VERSION);
            stopped_at(&setup, "record");
            let archive = setup.record().unwrap();
            assert_eq!(archive.version, ARCHIVE_VERSION, "{kind:?}, ids {ids}");
            assert_eq!(archive.workspace_id, held.id, "{kind:?}, ids {ids}");
            assert_eq!(archive.id, id, "{kind:?}, ids {ids}");
            assert_eq!(resumed(&setup).id, id, "{kind:?}, ids {ids}");
        }
    }
}

#[test]
fn a_record_and_an_archive_of_this_version_that_both_lack_ids_are_two_workspaces() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup_stopped(kind, "record");
        edit_archive(&setup, |archive| archive.workspace_id = None);
        edit_workspace(&setup, |record| {
            record.remove("state");
            record.remove("id");
            record.insert("version".to_owned(), 2.into());
        });
        let refusal = setup.refusal(ArchiveOptions::default());
        assert!(
            matches!(refusal, ArchiveRefusal::NameTaken(_)),
            "{kind:?}: {refusal:?}"
        );
    }
}

#[test]
fn a_write_of_a_record_without_an_id_beside_an_older_pair_without_ids_mints_none() {
    let setup = older_pair(Kind::Clone, false);
    let mut held = workspace(&setup);
    held.pinned = true;
    setup.workspaces.store.update(&mut held).unwrap();
    let read = workspace(&setup);
    assert!(read.pinned);
    assert_eq!(read.id, None);
}

#[test]
fn a_restore_writes_the_workspace_record_as_live() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        setup.archive();
        setup.restore();
        assert_eq!(workspace(&setup).state(), WorkspaceState::Live, "{kind:?}");
        let text = fs::read_to_string(setup.workspaces.store.path(&name())).unwrap();
        assert!(text.contains("\"state\": \"live\""), "{kind:?}: {text}");
    }
}

#[test]
fn an_archive_of_a_workspace_without_a_stable_id_carries_the_id_that_it_mints() {
    for kind in [Kind::Worktree, Kind::Clone] {
        let setup = setup(kind);
        setup.create();
        edit_workspace(&setup, |record| {
            record.remove("id");
        });
        assert_eq!(workspace(&setup).id, None);
        stopped_at(&setup, "record");
        let minted = workspace(&setup).id;
        assert!(minted.is_some(), "{kind:?}");
        assert_eq!(setup.record().unwrap().workspace_id, minted, "{kind:?}");
    }
}
