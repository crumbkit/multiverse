//! One table of every pair of records that an unfinished or finished
//! archive and the workspace of its name can be in, and what each of the
//! three readers of that pair answers: `health::classify` for the
//! listings, `Workspaces::plan_archive` for `mxv archive` and
//! `mxv archive -w`, and `Workspaces::repair` for `mxv repair`. The three
//! take their case from one rule, so a row that changes for one reader
//! and not for the others is a regression.

use std::time::Duration;

use super::*;
use crate::health::{self, Broken, Problem, Repair, Repaired};
use crate::workspaces::tests::Scripted;

/// Whether the readers can read the pins of a row's archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pins {
    /// The readers read each pin through git.
    Readable,
    /// Every read of a pin fails, as a `git rev-parse` that cannot start
    /// does.
    Unreadable,
}

/// Returns the backend through which the readers of a row read: git, or
/// git whose every read of a pin fails.
fn backend(pins: Pins) -> Scripted {
    let mut backend = Scripted::plain();
    if pins == Pins::Unreadable {
        backend.before_resolve = |dir| {
            Some(Error::TimedOut {
                operation: "rev-parse",
                dir: dir.to_path_buf(),
                deadline: Duration::from_secs(1),
            })
        };
    }
    backend
}

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
    /// The kind of the workspace that the row archives.
    kind: Kind,
    /// Whether the readers can read the archive's pins.
    pins: Pins,
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

/// Archives the workspace and stops after `step`.
fn stopped_at(setup: &Setup, step: &'static str) {
    stop_at(step);
    let plan = setup.plan(ArchiveOptions::default());
    let stopped = setup.workspaces.archive(&setup.archives, plan);
    clear_hook();
    assert!(stopped.is_err(), "the archive did not stop at {step}");
}

/// Writes `archive` as the record of its name, at the version it holds.
fn write_record(setup: &Setup, archive: &Archive) {
    let path = setup.archives.dir_of(&archive.name).join("archive.json");
    write(&path, serde_json::to_vec_pretty(archive).unwrap());
}

/// Archives the workspace and stops just before the archive is marked
/// finished, so the archive is retired and no workspace record is left.
fn retired_alone(setup: &Setup) -> Archive {
    setup.create();
    stopped_at(setup, "removed");
    assert!(setup.workspaces.store.read(&name()).unwrap().is_none());
    let archive = setup.record().unwrap();
    assert_eq!(archive.state, ArchiveState::Retired);
    archive
}

/// Leaves a retired archive with no workspace record, rewritten as an
/// older multiverse wrote it: retiring, at version 2.
fn older_alone(setup: &Setup) -> Archive {
    let mut archive = retired_alone(setup);
    archive.state = ArchiveState::Retiring;
    archive.version = OLDER_ARCHIVE_VERSION;
    write_record(setup, &archive);
    archive
}

/// Archives the workspace, stops after the archive record, and deletes the
/// workspace's record, as `mxv remove` of a workspace whose archive
/// stopped does: a retiring archive at this version with no workspace.
fn retiring_alone(setup: &Setup) -> Archive {
    setup.create();
    stopped_at(setup, "record");
    setup.workspaces.store.delete(&name()).unwrap();
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
/// so the archive is retiring beside the workspace it archives, whose
/// state names it.
fn retiring_beside_its_workspace(setup: &Setup) -> WorkspaceName {
    setup.create();
    stopped_at(setup, "record");
    assert_eq!(setup.record().unwrap().state, ArchiveState::Retiring);
    name()
}

/// Removes the state from the workspace record, and without `keep_id`
/// its stable id too, as an older mxv that rewrote the record leaves it.
fn without_state(setup: &Setup, keep_id: bool) {
    let path = setup.workspaces.store.path(&name());
    let mut record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let fields = record.as_object_mut().unwrap();
    fields.remove("state");
    if !keep_id {
        fields.remove("id");
    }
    write(&path, serde_json::to_vec_pretty(&record).unwrap());
}

/// Leaves a retiring archive beside its workspace, with no stable id in
/// either record. The workspace's state still names the archive.
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

fn retiring_beside_its_workspace_without_its_state(setup: &Setup) -> WorkspaceName {
    retiring_beside_its_workspace(setup);
    without_state(setup, true);
    name()
}

/// Archives the workspace and stops after the archive is written retired,
/// before the workspace record is deleted.
fn retired_beside_its_workspace(setup: &Setup) -> WorkspaceName {
    setup.create();
    stopped_at(setup, "retired");
    assert_eq!(setup.record().unwrap().state, ArchiveState::Retired);
    name()
}

/// Leaves a retiring archive of version 2 beside its workspace, whose
/// record has no state, as an older mxv left them. Without `ids`, neither
/// record holds a stable id.
fn older_beside_its_workspace(setup: &Setup, ids: bool) -> WorkspaceName {
    retiring_beside_its_workspace(setup);
    let mut archive = setup.record().unwrap();
    archive.version = OLDER_ARCHIVE_VERSION;
    if !ids {
        archive.workspace_id = None;
    }
    write_record(setup, &archive);
    without_state(setup, ids);
    name()
}

fn older_beside_its_workspace_with_ids(setup: &Setup) -> WorkspaceName {
    older_beside_its_workspace(setup, true)
}

fn older_beside_its_workspace_without_ids(setup: &Setup) -> WorkspaceName {
    older_beside_its_workspace(setup, false)
}

/// Leaves a retiring archive of this version and a workspace record
/// without a state, neither with a stable id, which are two workspaces.
fn retiring_beside_a_record_without_state_or_ids(setup: &Setup) -> WorkspaceName {
    retiring_beside_its_workspace(setup);
    let mut archive = setup.record().unwrap();
    archive.workspace_id = None;
    write_record(setup, &archive);
    without_state(setup, false);
    name()
}

fn retired_with_pins(setup: &Setup) -> WorkspaceName {
    retired_alone(setup);
    name()
}

fn retired_without_pins(setup: &Setup) -> WorkspaceName {
    delete_pins(&retired_alone(setup));
    name()
}

fn retiring_whose_workspace_was_removed(setup: &Setup) -> WorkspaceName {
    retiring_alone(setup);
    name()
}

fn older_with_pins(setup: &Setup) -> WorkspaceName {
    older_alone(setup);
    name()
}

fn older_without_pins(setup: &Setup) -> WorkspaceName {
    delete_pins(&older_alone(setup));
    name()
}

fn retired_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    retired_alone(setup);
    setup.create();
    name()
}

fn retired_without_pins_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    delete_pins(&retired_alone(setup));
    setup.create();
    name()
}

fn older_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    older_alone(setup);
    setup.create();
    name()
}

fn retiring_whose_workspace_was_removed_beside_a_newer_workspace(setup: &Setup) -> WorkspaceName {
    retiring_alone(setup);
    fs::remove_dir_all(setup.root.join("triage")).unwrap();
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

/// Leaves the two records of a rename that a repair stopped: the retired
/// archive under its name and a copy under its dated name. With `newer`,
/// a newer workspace holds the old name.
fn stopped_rename(setup: &Setup, newer: bool) -> Archive {
    let archive = retired_alone(setup);
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
        Problem::ArchiveWorkspaceRemoved => "workspace removed",
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
        // A plan that resumes an earlier run keeps that run's archive id,
        // so its pins and its record stay the same.
        Ok(ArchivePlanned::Ready(plan)) => match &plan.resuming {
            Some(resumed) if resumed.id == plan.id => "resume the existing archive".to_owned(),
            Some(_) => "resume under another archive id".to_owned(),
            None => "start a new archive".to_owned(),
        },
        Ok(ArchivePlanned::Finish(_)) => "finish".to_owned(),
        Ok(ArchivePlanned::Unchanged(_)) => "unchanged".to_owned(),
        Ok(ArchivePlanned::Refused(refusal)) => match refusal {
            ArchiveRefusal::NotFound => "refuse: not found".to_owned(),
            ArchiveRefusal::NameTaken(_) => "refuse: name taken".to_owned(),
            ArchiveRefusal::WorkspaceRemoved(_) => "refuse: workspace removed".to_owned(),
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
            // A renamed archive is always one that `mxv archive -w`
            // finishes, at this version.
            assert_eq!(archive.state, ArchiveState::Retired);
            assert_eq!(archive.version, ARCHIVE_VERSION);
            "rename to the dated name".to_owned()
        }
        Ok(Repaired::Deleted(_)) => "delete".to_owned(),
    }
}

/// The row of an archive that `mxv archive` resumes beside its workspace.
const fn resumed(case: &'static str, build: fn(&Setup) -> WorkspaceName) -> Row {
    Row {
        case,
        kind: Kind::Clone,
        pins: Pins::Readable,
        build,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "resume the existing archive",
        repair: "unchanged, archive the name",
    }
}

const ROWS: &[Row] = &[
    resumed(
        "retiring beside the workspace it archives",
        retiring_beside_its_workspace,
    ),
    resumed(
        "retiring beside its workspace, neither with a stable id, the state naming the archive",
        retiring_beside_its_workspace_without_ids,
    ),
    resumed(
        "retiring beside its workspace, whose record an older mxv rewrote without the state",
        retiring_beside_its_workspace_without_its_state,
    ),
    resumed(
        "retired beside the workspace it archives",
        retired_beside_its_workspace,
    ),
    resumed(
        "retiring at version 2 beside its workspace without a state, both with a stable id",
        older_beside_its_workspace_with_ids,
    ),
    resumed(
        "retiring at version 2 beside its workspace without a state, neither with a stable id",
        older_beside_its_workspace_without_ids,
    ),
    // Two missing ids count as one workspace only for an archive of
    // version 2.
    Row {
        case: "retiring at this version beside a record without a state, neither with a stable id",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retiring_beside_a_record_without_state_or_ids,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "move aside",
    },
    Row {
        case: "retired with no workspace and its pins there",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retired_with_pins,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "finish",
        repair: "unchanged, archive the name",
    },
    Row {
        case: "retired with no workspace and its pins there, in the worktree's repository",
        kind: Kind::Worktree,
        pins: Pins::Readable,
        build: retired_with_pins,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "finish",
        repair: "unchanged, archive the name",
    },
    // `mxv archive -w` fails when it cannot read a pin, so it never
    // finishes an archive whose pins may be gone. The listing and the
    // repair count the pin as there and name `mxv archive -w`.
    Row {
        case: "retired with no workspace and pins that cannot be read",
        kind: Kind::Clone,
        pins: Pins::Unreadable,
        build: retired_with_pins,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "error",
        repair: "unchanged, archive the name",
    },
    Row {
        case: "retired with no workspace and its pins gone",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retired_without_pins,
        archives: "pins gone, repair the name",
        workspaces: "pins gone, repair the name",
        subject: "refuse: pins gone",
        repair: "move aside",
    },
    Row {
        case: "retired with no workspace and its pins gone from the worktree's repository",
        kind: Kind::Worktree,
        pins: Pins::Readable,
        build: retired_without_pins,
        archives: "pins gone, repair the name",
        workspaces: "pins gone, repair the name",
        subject: "refuse: pins gone",
        repair: "move aside",
    },
    Row {
        case: "retiring at this version with no workspace, which mxv remove removed",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retiring_whose_workspace_was_removed,
        archives: "workspace removed, repair the name",
        workspaces: "workspace removed, repair the name",
        subject: "refuse: workspace removed",
        repair: "move aside",
    },
    Row {
        case: "retiring at this version with no worktree workspace, which mxv remove removed",
        kind: Kind::Worktree,
        pins: Pins::Readable,
        build: retiring_whose_workspace_was_removed,
        archives: "workspace removed, repair the name",
        workspaces: "workspace removed, repair the name",
        subject: "refuse: workspace removed",
        repair: "move aside",
    },
    // No reader reads the pins of an archive that is never finished.
    Row {
        case: "retiring at this version with no workspace, and pins that cannot be read",
        kind: Kind::Clone,
        pins: Pins::Unreadable,
        build: retiring_whose_workspace_was_removed,
        archives: "workspace removed, repair the name",
        workspaces: "workspace removed, repair the name",
        subject: "refuse: workspace removed",
        repair: "move aside",
    },
    Row {
        case: "retiring at version 2 with no workspace and its pins there",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: older_with_pins,
        archives: "unfinished, archive the name",
        workspaces: "unfinished, archive the name",
        subject: "finish",
        repair: "unchanged, archive the name",
    },
    Row {
        case: "retiring at version 2 with no workspace and its pins gone",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: older_without_pins,
        archives: "pins gone, repair the name",
        workspaces: "pins gone, repair the name",
        subject: "refuse: pins gone",
        repair: "move aside",
    },
    Row {
        case: "retired beside a newer workspace, its pins there",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retired_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "rename to the dated name",
    },
    Row {
        case: "retired beside a newer workspace, its pins gone",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retired_without_pins_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "move aside",
    },
    // The repair reads the pins of an archive that it may rename, and
    // fails when it cannot, while the listing names `mxv repair`.
    Row {
        case: "retired beside a newer workspace, and pins that cannot be read",
        kind: Kind::Clone,
        pins: Pins::Unreadable,
        build: retired_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "error",
    },
    Row {
        case: "retiring at version 2 beside a newer workspace, its pins there",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: older_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "rename to the dated name",
    },
    Row {
        case: "retiring at this version, removed, beside a newer workspace",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: retiring_whose_workspace_was_removed_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "move aside",
    },
    Row {
        case: "retiring at this version, removed, beside a newer workspace, pins that cannot be read",
        kind: Kind::Clone,
        pins: Pins::Unreadable,
        build: retiring_whose_workspace_was_removed_beside_a_newer_workspace,
        archives: "beside a newer workspace, repair the name",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "move aside",
    },
    Row {
        case: "archived with no workspace",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: archived_alone,
        archives: "healthy",
        workspaces: "healthy",
        subject: "unchanged",
        repair: "unchanged, no repair",
    },
    Row {
        case: "archived beside a newer workspace",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: archived_beside_a_newer_workspace,
        archives: "healthy",
        workspaces: "healthy",
        subject: "refuse: name taken",
        repair: "unchanged, no repair",
    },
    Row {
        case: "archived beside the whole workspace a restore rebuilt",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: archived_beside_its_restore,
        archives: "restore unfinished, repair the name",
        workspaces: "restore unfinished, repair the name",
        subject: "refuse: name taken",
        repair: "delete",
    },
    Row {
        case: "archived beside its rebuilt workspace whose checkouts are gone",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: archived_beside_its_restore_without_checkouts,
        archives: "healthy",
        workspaces: "checkouts gone, remove the name",
        subject: "refuse: name taken",
        repair: "unchanged, remove the name",
    },
    Row {
        case: "a stopped rename read by its old name",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: stopped_rename_read_by_its_old_name,
        archives: "rename unfinished, repair the name",
        workspaces: "rename unfinished, repair the name",
        subject: "refuse: rename unfinished with the dated name",
        repair: "rename to the dated name",
    },
    Row {
        case: "a stopped rename read by its dated name",
        kind: Kind::Clone,
        pins: Pins::Readable,
        build: stopped_rename_read_by_its_dated_name,
        archives: "rename unfinished, repair another name",
        workspaces: "rename unfinished, repair another name",
        subject: "refuse: rename unfinished with the old name",
        repair: "rename to the dated name",
    },
    Row {
        case: "a stopped rename whose old name a newer workspace holds",
        kind: Kind::Clone,
        pins: Pins::Readable,
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
    for row in ROWS {
        let setup = setup(row.kind);
        let read = (row.build)(&setup);
        let old = read.as_str().to_owned();
        // The readers share the setup's records and mirrors, and read
        // through the row's backend.
        let mut readers = Workspaces::new(Store::new(setup.base.join("state")), backend(row.pins));
        if row.kind == Kind::Clone {
            readers = readers.with_mirrors(setup.base.join("mirrors"));
        }
        let health = health::classify(
            &readers.backend,
            &readers.store.list().unwrap(),
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
                planned_text(readers.plan_archive(
                    &setup.archives,
                    &read,
                    ArchiveOptions::default(),
                    Mode::DryRun,
                )),
                row.subject,
            ),
            (
                "repair",
                repaired_text(readers.repair(&setup.archives, &read, Mode::DryRun), &old),
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
