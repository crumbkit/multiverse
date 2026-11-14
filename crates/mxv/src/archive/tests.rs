use std::collections::HashSet;
use std::path::PathBuf;

use crumbkit::Code;
use multiverse::phase::host::TableEntry;

use super::failure::{Finisher, Holder};
use super::{ArchiveFailure, ancestors};
use crate::failures::rendered;

fn entry(pid: u32, parent: u32) -> TableEntry {
    TableEntry::new(pid, parent, pid, Some(PathBuf::from("/tmp")), None)
}

#[test]
fn the_walk_collects_each_ancestor_and_stops_before_the_system_parent() {
    let table = [entry(30, 20), entry(20, 10), entry(10, 1), entry(40, 20)];
    assert_eq!(ancestors(&table, 30), [20, 10].into());
}

#[test]
fn a_sibling_that_shares_the_parent_is_no_ancestor() {
    let table = [entry(30, 20), entry(20, 1), entry(40, 20)];
    assert!(!ancestors(&table, 30).contains(&40));
}

#[test]
fn a_loop_in_the_table_ends_the_walk() {
    let table = [entry(30, 20), entry(20, 30)];
    assert_eq!(ancestors(&table, 30), [20, 30].into());
}

#[test]
fn a_process_that_is_its_own_parent_ends_the_walk() {
    let table = [entry(30, 30)];
    assert!(ancestors(&table, 30).is_empty());
}

/// Returns `text` as an owned string.
fn owned(text: &str) -> String {
    text.to_owned()
}

/// Returns a case of each failure of `mxv archive`, with one case for each
/// holder or finisher that changes the recovery. `MirrorLoss` and `Unkept`
/// have no constructor outside the library, so their lists are empty.
fn archive_cases() -> Vec<ArchiveFailure> {
    let name = || owned("triage");
    let mut cases = vec![ArchiveFailure::NotFound { name: name() }];
    cases.extend(
        [Holder::Archived, Holder::Retired, Holder::Retiring].map(|holder| {
            ArchiveFailure::NameTaken {
                name: name(),
                holder,
            }
        }),
    );
    cases.extend([
        ArchiveFailure::SourceChanged {
            name: name(),
            folder: owned("app"),
            differences: vec![owned("HEAD moved"), owned("a file changed")],
        },
        ArchiveFailure::MirrorMissing {
            folder: owned("app"),
            mirror: owned("/mirrors/app.git"),
        },
        ArchiveFailure::Unkeepable {
            name: name(),
            items: Vec::new(),
        },
        ArchiveFailure::InUse {
            name: name(),
            reason: owned("process 42 works in it"),
        },
        ArchiveFailure::Unusable {
            folder: owned("app"),
            reason: owned("its index is locked"),
        },
    ]);
    cases.extend([Finisher::Archive, Finisher::Repair].map(|finisher| {
        ArchiveFailure::Unfinished {
            name: name(),
            finisher,
        }
    }));
    cases.extend([
        ArchiveFailure::PinsGone { name: name() },
        ArchiveFailure::WorkspaceRemoved { name: name() },
        ArchiveFailure::RenameUnfinished {
            name: name(),
            other: owned("triage-20261114"),
        },
        ArchiveFailure::ManifestDisposable {
            name: name(),
            paths: vec![owned("app/target"), owned("app/node_modules")],
        },
        ArchiveFailure::RecordTooLarge {
            name: name(),
            bytes: 2_000_000,
            most: 1_048_576,
        },
        ArchiveFailure::VolumesInUse {
            name: name(),
            workspace: owned("triage-2"),
        },
    ]);
    cases
}

/// Returns a case of each failure of `mxv restore` and `mxv remove
/// --archived`, with one case for each finisher and kind of holder that
/// changes the recovery.
fn restore_cases() -> Vec<ArchiveFailure> {
    let name = || owned("triage");
    let mut cases = vec![
        ArchiveFailure::RestoreMirrorMissing {
            name: name(),
            checkouts: Vec::new(),
        },
        ArchiveFailure::RestoreNotFound { name: name() },
    ];
    cases.extend([Finisher::Archive, Finisher::Repair].map(|finisher| {
        ArchiveFailure::RestoreUnfinished {
            name: name(),
            finisher,
        }
    }));
    cases.extend(
        [true, false].map(|record| ArchiveFailure::RestoreNameTaken {
            name: name(),
            path: owned("/work/triage"),
            record,
        }),
    );
    cases.extend([
        ArchiveFailure::BranchMoved {
            name: name(),
            folder: owned("app"),
            branch: owned("triage"),
            pinned: owned("1111111"),
            tip: owned("2222222"),
        },
        ArchiveFailure::CommitMissing {
            name: name(),
            folder: owned("app"),
            commit: owned("1111111"),
        },
        ArchiveFailure::RootFileChanged {
            name: name(),
            path: owned("mxv.toml"),
        },
        ArchiveFailure::IgnoredDamaged {
            name: name(),
            paths: vec![owned("app/.env")],
        },
        ArchiveFailure::IgnoredTaken {
            name: name(),
            paths: vec![owned("app/.env")],
        },
    ]);
    cases
}

/// What each case of the test below gave when `ArchiveFailure` had a
/// hand-written `Failure` impl, recorded from that impl.
const ARCHIVE_FAILURES: &str = r#"mxv::archive::not_found exits 1
  recovery Run(Call { target: Command { path: ["list"], args: [] }, slot: None })
  field name = Text("triage")
mxv::archive::name_taken exits 1
  recovery Hint("run `mxv restore triage` to bring the archive back, or `mxv remove --archived triage` to delete it")
  field name = Text("triage")
mxv::archive::name_taken exits 1
  recovery Hint("an earlier archive of triage has not finished and a newer workspace holds the name; run `mxv repair triage`, which finishes the earlier archive under a dated name, or moves it aside when one of its pins is gone, then archive triage again")
  field name = Text("triage")
mxv::archive::name_taken exits 1
  recovery Hint("an earlier archive of triage stopped before it retired every checkout and a newer workspace holds the name; run `mxv repair triage`, which moves the earlier archive aside, then archive triage again")
  field name = Text("triage")
mxv::archive::source_changed exits 1
  recovery Hint("look at what changed, then run `mxv archive -w triage` again to finish the archive")
  field name = Text("triage")
  field folder = Text("app")
  field differences = List([Text("HEAD moved"), Text("a file changed")])
mxv::archive::mirror_missing exits 1
  recovery Hint("run `mxv new` for the same repository, which clones the mirror again, then archive the workspace again")
  field folder = Text("app")
  field mirror = Text("/mirrors/app.git")
mxv::archive::unkeepable exits 1
  recovery Hint("Everything else in the workspace would be kept. `mxv archive -w triage --force` archives it and loses only the items above.")
  field name = Text("triage")
  field items = List([])
mxv::archive::in_use exits 1
  recovery Hint("stop that process or move it out of the workspace, then run `mxv archive -w triage` again")
  field name = Text("triage")
  field reason = Text("process 42 works in it")
mxv::archive::checkout_unusable exits 1
  recovery Hint("repair the checkout with git or `mxv add`, then archive the workspace again")
  field folder = Text("app")
  field reason = Text("its index is locked")
mxv::archive::unfinished exits 1
  recovery Hint("run `mxv archive -w triage`, which finishes the archive")
  field name = Text("triage")
mxv::archive::unfinished exits 1
  recovery Hint("run `mxv repair triage`, which moves the archive aside or finishes it under a dated name, as the problem that `mxv list --archived` shows for it says")
  field name = Text("triage")
mxv::archive::pins_gone exits 1
  recovery Hint("run `mxv repair triage`, which moves the archive aside so that it no longer claims a restore brings the workspace back")
  field name = Text("triage")
mxv::archive::workspace_removed exits 1
  recovery Hint("run `mxv repair triage`, which moves the archive aside, because nothing says that it holds every checkout")
  field name = Text("triage")
mxv::archive::rename_unfinished exits 1
  recovery Hint("run `mxv repair triage`, which finishes the move and then the archive")
  field name = Text("triage")
  field other = Text("triage-20261114")
mxv::archive::manifest_disposable exits 1
  recovery Hint("run `mxv archive -w triage` without --yes and answer the prompt, or pass --force")
  field name = Text("triage")
  field paths = List([Text("app/target"), Text("app/node_modules")])
mxv::archive::record_too_large exits 1
  recovery Hint("move or delete some of the extra files in the workspace's directory, then archive again")
  field name = Text("triage")
  field bytes = Text("2000000")
  field most = Text("1048576")
mxv::archive::volumes_in_use exits 1
  recovery Hint("run `mxv remove --archived triage` without --delete-volumes, because the volumes belong to the workspace triage-2")
  field name = Text("triage")
  field workspace = Text("triage-2")
mxv::restore::mirror_missing exits 1
  recovery Hint("`mxv restore triage --from-origin` fetches origin again and restores only what origin holds, and `mxv remove --archived triage` deletes the archive")
  field name = Text("triage")
  field mirrors = List([])
  field lost = List([])
mxv::restore::not_found exits 1
  recovery Run(Call { target: Command { path: ["list"], args: [] }, slot: None })
  field name = Text("triage")
mxv::restore::unfinished exits 1
  recovery Hint("run `mxv archive -w triage`, which finishes the archive")
  field name = Text("triage")
mxv::restore::unfinished exits 1
  recovery Hint("run `mxv repair triage`, which moves the archive aside or finishes it under a dated name, as the problem that `mxv list --archived` shows for it says")
  field name = Text("triage")
mxv::restore::name_taken exits 1
  recovery Hint("run `mxv remove triage` first, which keeps the archive, and then restore again; `mxv remove --archived triage` deletes the archive instead, with any files that only it still holds")
  field name = Text("triage")
  field path = Text("/work/triage")
mxv::restore::name_taken exits 1
  recovery Hint("move or delete /work/triage, then restore again")
  field name = Text("triage")
  field path = Text("/work/triage")
mxv::restore::branch_moved exits 1
  recovery Hint("point the branch back with `git branch --force triage 1111111`, or rename it, then restore again")
  field name = Text("triage")
  field folder = Text("app")
  field branch = Text("triage")
  field pinned = Text("1111111")
  field tip = Text("2222222")
mxv::restore::commit_missing exits 1
  recovery Hint("`mxv remove --archived triage` deletes the archive, because nothing can rebuild this checkout")
  field name = Text("triage")
  field folder = Text("app")
  field commit = Text("1111111")
mxv::restore::root_file_changed exits 1
  recovery Hint("put the file back, or run `mxv remove --archived triage` to delete the archive")
  field name = Text("triage")
  field path = Text("mxv.toml")
mxv::restore::ignored_damaged exits 1
  recovery Hint("`mxv restore triage --force` restores the workspace without these files and lists them as lost")
  field name = Text("triage")
  field paths = List([Text("app/.env")])
mxv::restore::ignored_taken exits 1
  recovery Hint("look at what wrote these paths, then restore again")
  field name = Text("triage")
  field paths = List([Text("app/.env")])
"#;

#[test]
fn archive_failures_give_the_code_exit_recovery_and_fields_recorded_from_their_hand_written_impl() {
    let cases = archive_cases().into_iter().chain(restore_cases());

    let text: String = cases.map(|case| rendered(&case)).collect();

    assert_eq!(text, ARCHIVE_FAILURES, "\n{text}");
}

// This test lives for one commit. It shows that the derived list holds the
// codes of the hand-written list before that list is deleted.
#[test]
fn the_derived_archive_codes_are_the_codes_of_the_hand_written_list() {
    let derived: HashSet<&Code> = ArchiveFailure::CODES.iter().copied().collect();
    let listed: HashSet<&Code> = super::CODES.iter().copied().collect();

    assert_eq!(derived, listed);
    assert_eq!(ArchiveFailure::CODES.len(), super::CODES.len());
}
