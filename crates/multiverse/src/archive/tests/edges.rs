use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;

/// Adds the submodule `sub` and takes its working files away, as
/// `git submodule deinit` does, so git sees only a commit id at `sub`.
fn deinitialised_submodule(setup: &Setup) {
    add_submodule(setup);
    git(
        &setup.checkout(),
        &["submodule", "--quiet", "deinit", "--force", "sub"],
    );
}

fn refuses_and_names_a_file_in_a_submodule_directory_that_is_not_initialised(kind: Kind) {
    let setup = setup(kind);
    setup.create();
    deinitialised_submodule(&setup);
    write(setup.checkout().join("sub/notes.txt"), "mine\n");
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("sub/notes.txt"), "{text}");
    assert!(setup.checkout().join("sub/notes.txt").is_file());
}

#[test]
fn a_file_in_a_clones_uninitialised_submodule_directory_refuses_the_archive_and_is_named() {
    refuses_and_names_a_file_in_a_submodule_directory_that_is_not_initialised(Kind::Clone);
}

#[test]
fn a_file_in_a_worktrees_uninitialised_submodule_directory_refuses_the_archive_and_is_named() {
    refuses_and_names_a_file_in_a_submodule_directory_that_is_not_initialised(Kind::Worktree);
}

fn force_lists_a_file_in_a_submodule_directory_that_is_not_initialised(kind: Kind) {
    let setup = setup(kind);
    setup.create();
    deinitialised_submodule(&setup);
    write(setup.checkout().join("sub/notes.txt"), "mine\n");
    let archive = setup.archive_with(force());
    assert!(
        archive
            .lost
            .iter()
            .any(|item| item.contains("sub/notes.txt")),
        "{:?}",
        archive.lost
    );
}

#[test]
fn force_lists_a_file_in_a_clones_uninitialised_submodule_directory_as_lost() {
    force_lists_a_file_in_a_submodule_directory_that_is_not_initialised(Kind::Clone);
}

#[test]
fn force_lists_a_file_in_a_worktrees_uninitialised_submodule_directory_as_lost() {
    force_lists_a_file_in_a_submodule_directory_that_is_not_initialised(Kind::Worktree);
}

#[test]
fn an_empty_uninitialised_submodule_directory_does_not_refuse_the_archive() {
    for kind in [Kind::Clone, Kind::Worktree] {
        let setup = setup(kind);
        setup.create();
        deinitialised_submodule(&setup);
        git(&setup.checkout(), &["push", "--quiet", "origin", "HEAD"]);
        assert!(matches!(
            setup.planned(ArchiveOptions::default()),
            ArchivePlanned::Ready(_)
        ));
    }
}

#[test]
fn a_git_directory_inside_a_tracked_directory_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join("tracked")).unwrap();
    write(checkout.join("tracked/file.txt"), "tracked\n");
    git(&checkout, &["add", "tracked/file.txt"]);
    git(&checkout, &["commit", "--quiet", "-m", "Tracked"]);
    git(&checkout.join("tracked"), &["init", "--quiet"]);
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("tracked/.git/HEAD"), "{text}");
}

#[test]
fn a_tracked_file_whose_name_differs_only_in_case_on_disk_is_not_reported_as_unlisted() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    git(&checkout, &["config", "core.ignorecase", "true"]);
    write(checkout.join("Mixed.txt"), "tracked\n");
    if !checkout.join("MIXED.TXT").exists() {
        // Only a case-insensitive file system can hold this state.
        return;
    }
    git(&checkout, &["add", "Mixed.txt"]);
    git(&checkout, &["commit", "--quiet", "-m", "Mixed"]);
    git(&checkout, &["push", "--quiet", "origin", "HEAD"]);
    crate::store::move_aside(&checkout.join("Mixed.txt"), &checkout.join("t")).unwrap();
    crate::store::move_aside(&checkout.join("t"), &checkout.join("mixed.txt")).unwrap();
    assert!(matches!(
        setup.planned(ArchiveOptions::default()),
        ArchivePlanned::Ready(_)
    ));
}

/// Makes a commit that only `refs/backup/x` keeps, and returns it.
fn commit_only_a_backup_reference_keeps(setup: &Setup) -> String {
    let commit = setup.commit_locally();
    let checkout = setup.checkout();
    git(&checkout, &["update-ref", "refs/backup/x", &commit]);
    git(&checkout, &["reset", "--quiet", "--hard", "HEAD~1"]);
    commit
}

#[test]
fn a_commit_only_another_reference_keeps_comes_back_as_a_kept_reference() {
    let setup = setup(Kind::Clone);
    setup.create();
    let commit = commit_only_a_backup_reference_keeps(&setup);
    let archive = setup.archive();
    assert_eq!(archive.checkouts[0].kept.orphans, vec![commit.clone()]);
    assert!(archive.lost.is_empty(), "{:?}", archive.lost);
    setup.restore();
    let reference = format!("{KEPT_NAMESPACE}{commit}");
    assert_eq!(
        git(&setup.checkout(), &["rev-parse", &reference]).trim(),
        commit
    );
}

#[test]
fn an_archive_keeps_only_the_tips_of_the_commits_that_other_references_keep() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    let commit = |message: &str| {
        git(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "-m", message],
        );
        setup.head()
    };
    commit("First");
    let chain_tip = commit("Second");
    git(&checkout, &["update-ref", "refs/backup/chain", &chain_tip]);
    git(&checkout, &["reset", "--quiet", "--hard", "HEAD~2"]);
    let single = commit("Single");
    git(&checkout, &["update-ref", "refs/backup/single", &single]);
    git(&checkout, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let archive = setup.archive();
    let mut tips = vec![chain_tip, single];
    tips.sort_unstable();
    assert_eq!(archive.checkouts[0].kept.orphans, tips);
}

#[test]
fn another_reference_that_names_a_commit_origin_holds_does_not_refuse_the_archive() {
    let setup = setup(Kind::Clone);
    setup.create();
    let head = setup.head();
    git(&setup.checkout(), &["update-ref", "refs/backup/x", &head]);
    assert!(matches!(
        setup.planned(ArchiveOptions::default()),
        ArchivePlanned::Ready(_)
    ));
}

#[test]
fn a_worktree_does_not_answer_for_another_reference_of_its_repository() {
    let setup = setup(Kind::Worktree);
    setup.create();
    let checkout = setup.checkout();
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Local"],
    );
    let commit = setup.head();
    git(&checkout, &["update-ref", "refs/backup/x", &commit]);
    git(&checkout, &["reset", "--quiet", "--hard", "HEAD~1"]);
    assert!(matches!(
        setup.planned(ArchiveOptions::default()),
        ArchivePlanned::Ready(_)
    ));
}

#[test]
fn a_stash_in_a_deinitialised_submodule_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    let checkout = setup.checkout();
    let sub = checkout.join("sub");
    write(sub.join("stashed.txt"), "stashed\n");
    git(&sub, &["add", "stashed.txt"]);
    git(&sub, &["stash", "--quiet"]);
    let stash = git(&sub, &["rev-parse", "refs/stash"]).trim().to_owned();
    git(
        &checkout,
        &["submodule", "--quiet", "deinit", "--force", "sub"],
    );
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains(&stash), "{text}");
}

#[test]
fn a_nested_repository_in_a_submodules_disposable_directory_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    add_submodule(&setup);
    let checkout = setup.checkout();
    write(
        checkout.join(".git/modules/sub/info/exclude"),
        "node_modules/\n",
    );
    let nested = checkout.join("sub/node_modules/kept");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("node_modules"), "{text}");
}

#[test]
fn a_clone_in_a_paused_merge_refuses_the_archive_and_is_named() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    let head = setup.head();
    write(checkout.join(".git/MERGE_HEAD"), format!("{head}\n"));
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("merge is paused"), "{text}");
}

#[test]
fn a_clone_in_a_paused_rebase_refuses_the_archive_and_force_lists_it_as_lost() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join(".git/rebase-merge")).unwrap();
    let text = refusal_text(setup.planned(ArchiveOptions::default()));
    assert!(text.contains("rebase is paused"), "{text}");
    let archive = setup.archive_with(force());
    assert!(
        archive
            .lost
            .iter()
            .any(|item| item.contains("rebase is paused")),
        "{:?}",
        archive.lost
    );
}

#[test]
fn a_clone_archived_on_the_default_branch_of_its_mirror_comes_back_on_it() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    git(&checkout, &["switch", "--quiet", "main"]);
    let commit = setup.commit_locally();
    setup.archive();
    setup.restore();
    assert_eq!(setup.head(), commit);
    assert_eq!(branch(&setup.checkout()), "main");
    assert_eq!(git(&setup.checkout(), &["status", "--porcelain"]), "");
}

/// Archives a workspace and replaces `from` in its record with `to`.
fn edited_record(kind: Kind, from: impl Fn(&Archive) -> String, to: &str) -> Setup {
    let setup = setup(kind);
    setup.create();
    let archive = setup.archive();
    let path = setup.archives.dir_of(&name()).join("archive.json");
    let text = fs::read_to_string(&path).unwrap();
    let original = from(&archive);
    assert!(text.contains(&original), "{text}");
    write(&path, text.replace(&original, to));
    setup
}

fn assert_corrupt(setup: &Setup) {
    assert!(matches!(
        setup.archives.read(&name()),
        Err(Error::CorruptRecord { .. })
    ));
}

#[test]
fn a_record_whose_checkout_path_leaves_the_workspace_is_refused_as_corrupt() {
    let quoted =
        |archive: &Archive| format!("\"path\": \"{}\"", archive.checkouts[0].path.display());
    assert_corrupt(&edited_record(Kind::Clone, quoted, "\"path\": \"/etc\""));
    let setup = setup(Kind::Clone);
    setup.create();
    let archive = setup.archive();
    let path = setup.archives.dir_of(&name()).join("archive.json");
    let text = fs::read_to_string(&path).unwrap();
    let escaped = format!("{}/../../outside", archive.path.display());
    write(
        &path,
        text.replace(&quoted(&archive), &format!("\"path\": \"{escaped}\"")),
    );
    assert_corrupt(&setup);
}

#[test]
fn a_record_whose_pin_leaves_the_archive_namespace_is_refused_as_corrupt() {
    let quoted = |archive: &Archive| format!("\"pin\": \"{}\"", archive.checkouts[0].pin);
    for pin in [
        "refs/heads/main",
        "refs/mxv/archives/../../heads/main",
        "refs/mxv/archives/",
    ] {
        let to = format!("\"pin\": \"{pin}\"");
        assert_corrupt(&edited_record(Kind::Worktree, quoted, &to));
    }
}

#[test]
fn a_record_whose_id_is_not_32_lowercase_hex_digits_is_refused_as_corrupt() {
    let quoted = |archive: &Archive| format!("\"id\": \"{}\"", archive.id);
    for id in [
        "",
        "/tmp/outside",
        "..",
        "../0123456789abcdef0123456789abcdef",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef",
    ] {
        let to = format!("\"id\": \"{id}\"");
        assert_corrupt(&edited_record(Kind::Clone, quoted, &to));
    }
}

#[test]
fn a_record_whose_id_is_32_lowercase_hex_digits_still_reads() {
    let quoted = |archive: &Archive| format!("\"id\": \"{}\"", archive.id);
    let id = "0123456789abcdef0123456789abcdef";
    let setup = edited_record(Kind::Clone, quoted, &format!("\"id\": \"{id}\""));
    let archive = setup.archives.read(&name()).unwrap().unwrap();
    assert_eq!(archive.id.as_str(), id);
}

#[test]
fn an_archive_whose_id_is_malformed_lists_as_unreadable() {
    let quoted = |archive: &Archive| format!("\"id\": \"{}\"", archive.id);
    let setup = edited_record(Kind::Clone, quoted, "\"id\": \"../outside\"");
    let listed = setup.archives.list().unwrap();
    assert!(listed.archives.is_empty(), "{:?}", listed.archives);
    assert!(
        matches!(listed.problems.as_slice(), [Error::CorruptRecord { .. }]),
        "{:?}",
        listed.problems
    );
    let health = crate::health::classify(&Git, &setup.workspaces.store.list().unwrap(), &listed);
    assert!(
        matches!(
            health.archives.as_slice(),
            [crate::health::Broken {
                problem: crate::health::Problem::Unreadable { .. },
                ..
            }]
        ),
        "{health:?}"
    );
}

#[test]
fn a_restored_clone_tracks_the_remote_that_it_tracked_when_it_was_archived() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    git(&checkout, &["config", "branch.triage.remote", "fork"]);
    git(
        &checkout,
        &["config", "branch.triage.merge", "refs/heads/feature"],
    );
    setup.archive();
    setup.restore();
    let read = |key: &str| git(&setup.checkout(), &["config", key]).trim().to_owned();
    assert_eq!(read("branch.triage.remote"), "fork");
    assert_eq!(read("branch.triage.merge"), "refs/heads/feature");
}

#[test]
fn a_restored_clone_that_tracked_origin_still_tracks_origin() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    git(&checkout, &["config", "branch.triage.remote", "origin"]);
    git(
        &checkout,
        &["config", "branch.triage.merge", "refs/heads/feature"],
    );
    setup.archive();
    setup.restore();
    let read = |key: &str| git(&setup.checkout(), &["config", key]).trim().to_owned();
    assert_eq!(read("branch.triage.remote"), "origin");
}

#[test]
fn what_an_earlier_forced_run_listed_and_the_rerun_kept_is_not_recorded_as_lost() {
    let setup = setup(Kind::Clone);
    setup.create();
    write(setup.checkout().join("scratch.txt"), "doomed\n");
    let plan = setup.plan(force());
    stop_at("record");
    assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
    clear_hook();
    fs::remove_file(setup.checkout().join("scratch.txt")).unwrap();
    let archive = setup.archive();
    assert!(
        archive
            .lost
            .iter()
            .all(|item| !item.contains("scratch.txt")),
        "{:?}",
        archive.lost
    );
}

#[test]
fn what_a_forced_run_lost_with_a_checkout_it_retired_stays_recorded_as_lost() {
    let setup = setup(Kind::Clone);
    setup.create();
    setup.ignore_env();
    write(setup.checkout().join(".env"), "x".repeat(2048));
    let plan = setup.plan(force().with_max_file(1024));
    stop_at("retired:app");
    assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
    clear_hook();
    let archive = setup.archive_with(ArchiveOptions::default().with_max_file(1024));
    assert!(
        archive.lost.iter().any(|item| item.contains(".env")),
        "{:?}",
        archive.lost
    );
}

#[test]
fn a_run_stopped_after_the_pins_leaves_no_second_pin_when_it_runs_again() {
    let setup = setup(Kind::Clone);
    let workspace = setup.create();
    let plan = setup.plan(ArchiveOptions::default());
    stop_at("pins");
    assert!(setup.workspaces.archive(&setup.archives, plan).is_err());
    clear_hook();
    setup.archive();
    let mirror = workspace.checkouts[0].mirror.clone().unwrap();
    let pins = git(
        &mirror,
        &["for-each-ref", "--format=%(refname)", "refs/mxv/archives"],
    );
    assert_eq!(pins.lines().count(), 1, "{pins}");
}

#[test]
fn a_fifo_named_agents_md_refuses_the_archive_and_does_not_hang_it() {
    let setup = setup(Kind::Clone);
    setup.create();
    let fifo = setup.root.join("triage/AGENTS.md");
    fs::remove_file(&fifo).unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test makes a FIFO, which the standard library cannot"
    )]
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(made.unwrap().success());
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let text = refusal_text(setup.planned(ArchiveOptions::default()));
        sender.send(text).unwrap();
    });
    let text = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("the archive hung on the FIFO");
    assert!(text.contains("AGENTS.md"), "{text}");
}

#[test]
fn hooks_local_config_and_exclude_rules_that_a_clone_drops_are_named_in_a_warning() {
    let setup = setup(Kind::Clone);
    setup.create();
    let checkout = setup.checkout();
    fs::create_dir_all(checkout.join(".git/hooks")).unwrap();
    write(checkout.join(".git/hooks/pre-push"), "#!/bin/sh\n");
    git(&checkout, &["config", "user.signingkey", "ABC123"]);
    write(checkout.join(".git/info/exclude"), "# note\nscratch/\n");
    let plan = setup.plan(ArchiveOptions::default());
    let warnings = plan.warnings.join("\n");
    assert!(warnings.contains("pre-push"), "{warnings}");
    assert!(warnings.contains("user.signingkey"), "{warnings}");
    assert!(!warnings.contains("ABC123"), "{warnings}");
    assert!(warnings.contains("scratch/"), "{warnings}");
}

#[test]
fn a_plain_clone_has_no_warning_about_hooks_or_local_config() {
    let setup = setup(Kind::Clone);
    setup.create();
    let plan = setup.plan(ArchiveOptions::default());
    assert_eq!(plan.warnings, Vec::<String>::new());
}
