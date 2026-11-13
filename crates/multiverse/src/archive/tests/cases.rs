//! One table of every pair of records that an unfinished or finished
//! archive and the workspace of its name can be in today, and what each
//! of the three readers of that pair answers: `health::classify` for the
//! listings, `Workspaces::plan_archive` for `mxv archive` and
//! `mxv archive -w`, and `Workspaces::repair` for `mxv repair`. The three
//! take their case from one rule, so a row that changes for one reader
//! and not for the others is a regression.

use super::*;
use crate::health::{self, Broken, Problem, Repair, Repaired};

/// Returns the first dated name that a repair gives `archive`:
/// `NAME-<date>`, from the day the archive started, in UTC.
fn dated(archive: &Archive) -> WorkspaceName {
    let days = i64::try_from(archive.time / 86_400).unwrap();
    let (year, month, day) = phase::calendar::civil_from_days(days);
    format!("{}-{year:04}-{month:02}-{day:02}", archive.name)
        .parse()
        .unwrap()
}

/// What one row builds and what each reader answers for `name`.
struct Row {
    /// What the records hold.
    case: &'static str,
    /// Builds the records, and returns the name that the readers read.
    build: fn(&Setup) -> WorkspaceName,
    /// What the listing of the archives shows for the name.
    archives: &'static str,
    /// What the listing of the workspaces shows for the name.
    workspaces: &'static str,
    /// What `mxv archive` and `mxv archive -w` plan.
    subject: &'static str,
    /// What `mxv repair` does.
    repair: &'static str,
}

/// Archives the workspace and stops just before the archive is marked
/// finished, so the archive is retiring and no workspace record is left.
fn retiring_alone(setup: &Setup) -> Archive {
    setup.create();
    stop_at("removed");
    let plan = setup.plan(ArchiveOptions::default());
    let stopped = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "the archive did not stop");
    assert!(setup.workspaces.store.read(&name()).unwrap().is_none());
    let archive = setup.record().unwrap();
    assert_eq!(archive.state, ArchiveState::Retiring);
    archive
}

/// Deletes every pin of `archive`.
fn delete_pins(archive: &Archive) {
    for checkout in &archive.checkouts {
        git(
            checkout.pin_repository(),
            &["update-ref", "-d", &checkout.pin],
        );
    }
}

/// Archives the workspace and stops after the archive record is written,
/// so the archive is retiring beside the workspace it archives.
fn retiring_beside_its_workspace(setup: &Setup) -> WorkspaceName {
    setup.create();
    stop_at("record");
    let plan = setup.plan(ArchiveOptions::default());
    let stopped = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "the archive did not stop");
    assert_eq!(setup.record().unwrap().state, ArchiveState::Retiring);
    name()
}

/// Leaves a retiring archive beside its workspace, with no stable id in
/// either record, as an older multiverse wrote them.
fn retiring_beside_its_workspace_without_ids(setup: &Setup) -> WorkspaceName {
    retiring_beside_its_workspace(setup);
    let path = setup.workspaces.store.path(&name());
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    record.as_object_mut().unwrap().remove("id");
    write(&path, serde_json::to_vec_pretty(&record).unwrap());
    assert_eq!(
        setup.workspaces.store.read(&name()).unwrap().unwrap().id,
        None
    );
    let mut archive = setup.record().unwrap();
    archive.workspace_id = None;
    setup.archives.write(&archive).unwrap();
    name()
}

fn retiring_with_pins(setup: &Setup) -> WorkspaceName {
    retiring_alone(setup);
    name()
}

fn retiring_without_pins(setup: &Setup) -> WorkspaceName {
    delete_pins(&retiring_alone(setup));
    name()
}

fn retiring_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    retiring_alone(setup);
    setup.create();
    name()
}

fn retiring_without_pins_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    delete_pins(&retiring_alone(setup));
    setup.create();
    name()
}

fn archived_alone(setup: &Setup) -> WorkspaceName {
    setup.create();
    setup.archive();
    name()
}

fn archived_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    setup.create();
    setup.archive();
    setup.create();
    name()
}

/// Restores the archive and stops after the workspace record is written,
/// so the archive is archived beside the workspace it rebuilt.
fn archived_beside_its_restore(setup: &Setup) -> WorkspaceName {
    setup.create();
    setup.archive();
    stop_at("record");
    let result = setup
        .workspaces
        .restore(&setup.archives, setup.restore_plan(), &setup.runs);
    clear_hook();
    assert!(result.is_err(), "the restore did not stop");
    name()
}

fn archived_beside_its_restore_without_checkouts(setup: &Setup) -> WorkspaceName {
    archived_beside_its_restore(setup);
    fs::remove_dir_all(setup.checkout()).unwrap();
    name()
}

/// Leaves the two records of a rename that a repair stopped: the retiring
/// archive under its name and a copy under its dated name. With
/// `newer`, a newer workspace holds the old name.
fn stopped_rename(setup: &Setup, newer: bool) -> Archive {
    let archive = retiring_alone(setup);
    setup
        .archives
        .write(&archive.renamed(&dated(&archive)))
        .unwrap();
    if newer {
        setup.create();
    }
    archive
}

fn stopped_rename_read_by_its_old_name(setup: &Setup) -> WorkspaceName {
    stopped_rename(setup, false).name
}

fn stopped_rename_read_by_its_dated_name(setup: &Setup) -> WorkspaceName {
    dated(&stopped_rename(setup, false))
}

fn stopped_rename_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    stopped_rename(setup, true).name
}

/// Returns the text of the row of `found` for `name`. A repair of `old`,
/// the name the readers read, is written "the name".
fn listed(found: &[Broken], name: &WorkspaceName, old: &str) -> String {
    let Some(broken) = found.iter().find(|broken| broken.name == name.as_str()) else {
        return "healthy".to_owned();
    };
    let problem = match &broken.problem {
        Problem::Unreadable { .. } => "unreadable",
        Problem::ArchiveUnfinished => "unfinished",
        Problem::ArchivePinsGone => "pins gone",
        Problem::ArchiveBesideNewerWorkspace => "beside a newer workspace",
        Problem::RenameUnfinished => "rename unfinished",
        Problem::RestoreUnfinished => "restore unfinished",
        Problem::CheckoutsGone => "checkouts gone",
    };
    format!("{problem}, {}", repair_text(broken.repair.as_ref(), old))
}

fn repair_text(repair: Option<&Repair>, old: &str) -> String {
    let named = |name: &WorkspaceName| {
        if name.as_str() == old {
            "the name".to_owned()
        } else {
            "another name".to_owned()
        }
    };
    match repair {
        None => "no repair".to_owned(),
        Some(Repair::Archive(name)) => format!("archive {}", named(name)),
        Some(Repair::Remove(name)) => format!("remove {}", named(name)),
        Some(Repair::Repair(name)) => format!("repair {}", named(name)),
        Some(Repair::MoveAside(_)) => "move aside by hand".to_owned(),
    }
}

fn planned_text(planned: Result<ArchivePlanned, Error>) -> String {
    match planned {
        Err(_) => "error".to_owned(),
        Ok(ArchivePlanned::Ready(_)) => "archive the workspace".to_owned(),
        Ok(ArchivePlanned::Finish(_)) => "finish".to_owned(),
        Ok(ArchivePlanned::Unchanged(_)) => "unchanged".to_owned(),
        Ok(ArchivePlanned::Refused(refusal)) => match refusal {
            ArchiveRefusal::NotFound => "refuse: not found".to_owned(),
            ArchiveRefusal::NameTaken(_) => "refuse: name taken".to_owned(),
            ArchiveRefusal::PinsGone(_) => "refuse: pins gone".to_owned(),
            ArchiveRefusal::RenameUnfinished(other) if other.name == name() => {
                "refuse: rename unfinished with the old name".to_owned()
            }
            ArchiveRefusal::RenameUnfinished(_) => {
                "refuse: rename unfinished with the dated name".to_owned()
            }
            other => format!("refuse: {other:?}"),
        },
    }
}

fn repaired_text(repaired: Result<Repaired, Error>, old: &str) -> String {
    match repaired {
        Err(_) => "error".to_owned(),
        Ok(Repaired::Unchanged { repair }) => {
            format!("unchanged, {}", repair_text(repair.as_ref(), old))
        }
        Ok(Repaired::MovedAside { .. }) => "move aside".to_owned(),
        Ok(Repaired::Renamed { from, archive }) => {
            assert_eq!(from.as_str(), "triage");
            assert_eq!(archive.name, dated(&archive.renamed(&from)));
            "rename to the dated name".to_owned()
        }
        Ok(Repaired::Deleted(_)) => "delete".to_owned(),
    }
}

const ROWS: [Row; 13] = [
    Row {
        case: "retiring beside the workspace it archives",
        build: retiring_beside_its_workspace,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "archive the workspace",
        repair: "unchanged, archive the name",
    },
    Row {
        case: "retiring beside its workspace, neither with a stable id",
        build: retiring_beside_its_workspace_without_ids,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "archive the workspace",
        repair: "unchanged, archive the name",
    },
    Row {
        case: "retiring with no workspace and its pins there",
        build: retiring_with_pins,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "finish",
        repair: "unchanged, archive the name",
    },
    Row {
        case: "retiring with no workspace and its pins gone",
        build: retiring_without_pins,
        archives: "pins gone, repair the name",
        workspaces: "pins gone, repair the name",
        subject: "refuse: pins gone",
        repair: "move aside",
    },
    Row {
        case: "retiring beside a newer workspace, its pins there",
        build: retiring_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "rename to the dated name",
    },
    Row {
        case: "retiring beside a newer workspace, its pins gone",
        build: retiring_without_pins_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "move aside",
    },
    Row {
        case: "archived with no workspace",
        build: archived_alone,
        archives: "healthy",
        workspaces: "healthy",
        subject: "unchanged",
        repair: "unchanged, no repair",
    },
    Row {
        case: "archived beside a newer workspace",
        build: archived_beside_a_newer_workspace,
        archives: "healthy",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "unchanged, no repair",
    },
    Row {
        case: "archived beside the whole workspace a restore rebuilt",
        build: archived_beside_its_restore,
        archives: "restore unfinished, repair the name",
        workspaces: "restore unfinished, repair the name",
        subject: "refuse: name taken",
        repair: "delete",
    },
    Row {
        case: "archived beside its rebuilt workspace whose checkouts are gone",
        build: archived_beside_its_restore_without_checkouts,
        archives: "healthy",
        workspaces: "checkouts gone, remove the name",
        subject: "refuse: name taken",
        repair: "unchanged, remove the name",
    },
    Row {
        case: "a stopped rename read by its old name",
        build: stopped_rename_read_by_its_old_name,
        archives: "rename unfinished, repair the name",
        workspaces: "rename unfinished, repair the name",
        subject: "refuse: rename unfinished with the dated name",
        repair: "rename to the dated name",
    },
    Row {
        case: "a stopped rename read by its dated name",
        build: stopped_rename_read_by_its_dated_name,
        archives: "rename unfinished, repair another name",
        workspaces: "rename unfinished, repair another name",
        subject: "refuse: rename unfinished with the old name",
        repair: "rename to the dated name",
    },
    Row {
        case: "a stopped rename whose old name a newer workspace holds",
        build: stopped_rename_beside_a_newer_workspace,
        archives: "rename unfinished, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "rename to the dated name",
    },
];

#[test]
fn the_listings_the_archive_command_and_the_repair_read_every_pair_of_records_as_the_table_says() {
    let mut wrong = Vec::new();
    for row in &ROWS {
        let setup = setup(Kind::Clone);
        let read = (row.build)(&setup);
        let old = read.as_str().to_owned();
        let health = health::classify(
            &Git,
            &setup.workspaces.store.list().unwrap(),
            &setup.archives.list().unwrap(),
        );
        let found = [
            (
                "archives",
                listed(&health.archives, &read, &old),
                row.archives,
            ),
            (
                "workspaces",
                listed(&health.workspaces, &read, &old),
                row.workspaces,
            ),
            (
                "subject",
                planned_text(setup.workspaces.plan_archive(
                    &setup.archives,
                    &read,
                    ArchiveOptions::default(),
                    Mode::DryRun,
                )),
                row.subject,
            ),
            (
                "repair",
                repaired_text(
                    setup
                        .workspaces
                        .repair(&setup.archives, &read, Mode::DryRun),
                    &old,
                ),
                row.repair,
            ),
        ];
        for (reader, got, expected) in found {
            if got != expected {
                wrong.push(format!(
                    "{}: {reader} says {got:?}, the table says {expected:?}",
                    row.case
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
