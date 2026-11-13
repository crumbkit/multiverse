use std::fs;

use crate::backend::{Backend as _, Changes};
use crate::error::Error;
use crate::git::{Git, explanation, message};
use crate::testing::{SSH_ORIGIN, advance_origin, fake_ssh, fixture, git, write};

#[test]
fn a_status_longer_than_the_kept_output_of_a_command_names_every_path() {
    let fixture = fixture();
    let padding = "x".repeat(100);
    for index in 0..12_000 {
        write(
            fixture.clone.join(format!("{padding}-{index:05}.txt")),
            "work\n",
        );
    }
    let changes = Git.changes(&fixture.clone).unwrap();
    assert_eq!(changes.changed.len(), 12_000);
}

#[test]
fn a_worktree_has_no_submodule_git_directories_to_read() {
    let fixture = fixture();
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("worktree");
    let path = worktree.to_string_lossy().into_owned();
    git(
        &fixture.clone,
        &["worktree", "add", "--quiet", "-b", "side", &path],
    );
    assert_eq!(
        Git.unpushed_commits(&worktree).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn the_git_directory_of_a_clone_is_not_a_repository() {
    let fixture = fixture();
    let error = Git.repository(&fixture.clone.join(".git")).unwrap_err();
    assert!(
        matches!(error, Error::RepositoryNotFound { .. }),
        "{error:?}"
    );
}

#[test]
fn a_tracked_directory_with_a_junk_head_and_objects_resolves_to_the_enclosing_clone() {
    let fixture = fixture();
    let tracked = fixture.clone.join("tracked");
    fs::create_dir_all(tracked.join("objects")).unwrap();
    write(tracked.join("HEAD"), "junk\n");
    write(tracked.join("objects").join("file"), "junk\n");
    git(&fixture.clone, &["add", "tracked"]);
    git(&fixture.clone, &["commit", "--quiet", "-m", "Tracked"]);
    let repository = Git.repository(&tracked).unwrap();
    assert_eq!(
        repository.root.canonicalize().unwrap(),
        fixture.clone.canonicalize().unwrap()
    );
}

#[test]
fn a_directory_without_a_git_entry_that_imitates_a_git_directory_is_not_read_as_a_checkout() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().canonicalize().unwrap().join("lost-its-git");
    fs::create_dir_all(dir.join("objects")).unwrap();
    fs::create_dir_all(dir.join("refs")).unwrap();
    write(dir.join("HEAD"), "ref: refs/heads/main\n");
    write(dir.join("objects").join("x"), "tracked\n");
    write(dir.join("refs").join("x"), "tracked\n");
    write(
        dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n",
    );
    let head = Git.head(&dir).unwrap();
    assert_ne!(head.toplevel, Some(dir));
}

#[test]
fn a_directory_outside_any_repository_is_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let error = Git.repository(dir.path()).unwrap_err();
    assert!(
        matches!(error, Error::RepositoryNotFound { .. }),
        "{error:?}"
    );
}

#[test]
fn a_path_that_does_not_exist_is_not_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let error = Git.repository(&dir.path().join("missing")).unwrap_err();
    assert!(
        matches!(error, Error::RepositoryNotFound { .. }),
        "{error:?}"
    );
}

#[test]
fn a_repository_without_an_origin_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    let error = Git.repository(dir.path()).unwrap_err();
    assert!(matches!(error, Error::NoOrigin { .. }), "{error:?}");
}

#[test]
fn a_subdirectory_of_a_clone_names_the_clone_and_its_directory_name() {
    let fixture = fixture();
    let sub = fixture.clone.join("sub");
    fs::create_dir(&sub).unwrap();
    let repository = Git.repository(&sub).unwrap();
    assert_eq!(repository.root, fixture.clone);
    assert_eq!(repository.name, "app");
}

#[test]
fn the_base_is_the_default_branch_of_origin() {
    let fixture = fixture();
    assert_eq!(
        Git.base(&fixture.clone).unwrap(),
        "refs/remotes/origin/main"
    );
}

#[test]
fn a_checkout_starts_at_the_fetched_base_on_a_branch_that_tracks_nothing() {
    let fixture = fixture();
    let latest = advance_origin(&fixture);
    Git.fetch(&fixture.clone).unwrap();
    let checkout = fixture.dir.path().join("work");
    let created = Git
        .create_branch(&fixture.clone, "reading-list", "refs/remotes/origin/main")
        .unwrap();
    assert_eq!(created.as_deref(), Some(latest.as_str()));
    let commit = Git
        .add_checkout(&fixture.clone, &checkout, "reading-list")
        .unwrap();
    assert_eq!(commit, latest);
    let listed = Git.listed_checkout(&fixture.clone, &checkout).unwrap();
    assert_eq!(listed.and_then(|listed| listed.head), Some(latest));
    let upstream = git(
        &checkout,
        &[
            "for-each-ref",
            "--format=%(upstream)",
            "refs/heads/reading-list",
        ],
    );
    assert_eq!(upstream.trim(), "");
}

#[test]
fn the_head_names_the_current_branch_and_commit_and_no_branch_when_detached() {
    let fixture = fixture();
    let commit = git(&fixture.clone, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let head = Git.head(&fixture.clone).unwrap();
    assert_eq!(head.branch.as_deref(), Some("main"));
    assert_eq!(head.commit.as_deref(), Some(commit.as_str()));
    git(&fixture.clone, &["switch", "--quiet", "--detach"]);
    let head = Git.head(&fixture.clone).unwrap();
    assert_eq!(head.branch, None);
    assert_eq!(head.commit.as_deref(), Some(commit.as_str()));
}

#[test]
fn a_new_file_in_a_checkout_is_a_change() {
    let fixture = fixture();
    assert_eq!(Git.changes(&fixture.clone).unwrap(), Changes::default());
    write(fixture.clone.join("notes.md"), "notes");
    assert_eq!(
        Git.changes(&fixture.clone).unwrap().changed,
        ["?? notes.md"]
    );
}

#[test]
fn an_ignored_file_and_an_ignored_directory_are_listed_apart_from_the_changes() {
    let fixture = fixture();
    write(fixture.clone.join(".git/info/exclude"), ".env\ntarget/\n");
    write(fixture.clone.join(".env"), "TOKEN=x");
    fs::create_dir_all(fixture.clone.join("target/debug")).unwrap();
    write(fixture.clone.join("target/debug/mxv"), "binary");
    let changes = Git.changes(&fixture.clone).unwrap();
    assert!(changes.changed.is_empty(), "{changes:?}");
    assert_eq!(changes.ignored, [".env", "target/"]);
}

#[test]
fn a_path_with_a_space_or_a_non_ascii_character_is_listed_as_it_is_named() {
    let fixture = fixture();
    write(
        fixture.clone.join(".git/info/exclude"),
        "build output/\n*.log\n",
    );
    fs::create_dir_all(fixture.clone.join("build output")).unwrap();
    write(fixture.clone.join("build output/app"), "binary");
    write(fixture.clone.join("caf\u{e9}.log"), "log");
    write(fixture.clone.join("my notes.md"), "notes");
    let changes = Git.changes(&fixture.clone).unwrap();
    assert_eq!(changes.ignored, ["build output/", "caf\u{e9}.log"]);
    assert_eq!(changes.changed, ["?? my notes.md"]);
}

#[test]
fn a_renamed_file_is_one_change() {
    let fixture = fixture();
    write(fixture.clone.join("README.md"), "readme");
    git(&fixture.clone, &["add", "README.md"]);
    git(&fixture.clone, &["commit", "--quiet", "-m", "Add a readme"]);
    git(&fixture.clone, &["mv", "README.md", "READ ME.md"]);
    let changes = Git.changes(&fixture.clone).unwrap();
    assert_eq!(changes.changed, ["R  READ ME.md"]);
}

#[test]
fn a_branch_counts_the_commits_that_its_base_does_not_hold() {
    let fixture = fixture();
    let base = "refs/remotes/origin/main";
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    let unmerged = || {
        Git.unmerged_commits(&fixture.clone, "refs/heads/topic", base)
            .unwrap()
    };
    assert_eq!(unmerged(), Some(0));
    for _ in 0..2 {
        git(
            &checkout,
            &["commit", "--quiet", "--allow-empty", "-m", "Work"],
        );
    }
    assert_eq!(unmerged(), Some(2));
    git(&fixture.clone, &["update-ref", "-d", base]);
    assert_eq!(unmerged(), None);
}

/// Creates the branch `branch` from `origin/main` and checks it out at
/// `checkout`.
fn work(fixture: &crate::testing::Fixture, checkout: &std::path::Path, branch: &str) {
    Git.create_branch(&fixture.clone, branch, "refs/remotes/origin/main")
        .unwrap();
    Git.add_checkout(&fixture.clone, checkout, branch).unwrap();
}

#[test]
fn creating_a_branch_that_exists_leaves_it_where_it_was() {
    let fixture = fixture();
    git(
        &fixture.clone,
        &["commit", "--quiet", "--allow-empty", "-m", "Mine"],
    );
    let mine = git(&fixture.clone, &["rev-parse", "HEAD"]);
    git(&fixture.clone, &["branch", "topic"]);
    let created = Git
        .create_branch(&fixture.clone, "topic", "refs/remotes/origin/main")
        .unwrap();
    assert_eq!(created, None);
    assert_eq!(git(&fixture.clone, &["rev-parse", "topic"]), mine);
}

#[test]
fn a_branch_is_deleted_at_a_commit_only_while_it_still_points_there() {
    let fixture = fixture();
    let start = Git
        .create_branch(&fixture.clone, "topic", "refs/remotes/origin/main")
        .unwrap()
        .unwrap();
    git(
        &fixture.clone,
        &["commit", "--quiet", "--allow-empty", "-m", "Moved"],
    );
    git(&fixture.clone, &["branch", "--force", "topic", "HEAD"]);
    assert!(
        Git.delete_branch_at(&fixture.clone, "topic", &start)
            .is_err()
    );
    let moved = git(&fixture.clone, &["rev-parse", "HEAD"]);
    assert_eq!(
        Git.branch_tip(&fixture.clone, "topic").unwrap(),
        Some(moved.trim().to_owned())
    );
}

#[test]
fn a_branch_that_does_not_exist_has_no_tip() {
    let fixture = fixture();
    assert_eq!(
        Git.branch_tip(&fixture.clone, "no-such-branch").unwrap(),
        None
    );
}

#[test]
fn a_listed_checkout_reports_its_lock_and_a_detached_head() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    let path = checkout.to_str().unwrap();
    git(
        &fixture.clone,
        &["worktree", "lock", "--reason", "initializing", path],
    );
    git(&checkout, &["switch", "--quiet", "--detach"]);
    let listed = Git
        .listed_checkout(&fixture.clone, &checkout)
        .unwrap()
        .unwrap();
    assert_eq!(listed.locked.as_deref(), Some(crate::backend::INITIALIZING));
    assert!(listed.detached);
    Git.unlock_checkout(&fixture.clone, &checkout).unwrap();
    let listed = Git
        .listed_checkout(&fixture.clone, &checkout)
        .unwrap()
        .unwrap();
    assert_eq!(listed.locked, None);
}

#[test]
fn commits_on_a_detached_head_count_as_stray_and_commits_on_any_branch_do_not() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Work"],
    );
    assert_eq!(Git.stray_commits(&checkout, "HEAD").unwrap(), 0);
    git(&checkout, &["switch", "--quiet", "--detach"]);
    git(
        &checkout,
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    assert_eq!(Git.stray_commits(&checkout, "HEAD").unwrap(), 1);
    let head = git(&checkout, &["rev-parse", "HEAD"]);
    assert_eq!(Git.stray_commits(&fixture.clone, head.trim()).unwrap(), 1);
}

#[test]
fn a_failure_message_prefers_the_last_fatal_or_error_line() {
    let stderr = "hint: try this\nerror: first\nwarning: noise\nfatal: the cause\nhint: more\n";
    assert_eq!(message(stderr), "fatal: the cause");
}

#[cfg(unix)]
#[test]
fn a_failure_of_a_command_that_a_signal_ended_names_the_signal() {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;
    let killed = ExitStatus::from_raw(9);
    assert_eq!(explanation(killed, ""), "git was ended by signal 9");
    assert_eq!(
        explanation(killed, "fatal: the cause\n"),
        "fatal: the cause (git was ended by signal 9)"
    );
    assert_eq!(
        explanation(ExitStatus::from_raw(1 << 8), "fatal: the cause\n"),
        "fatal: the cause"
    );
}

#[test]
fn a_failure_message_without_a_fatal_line_quotes_the_last_lines_that_are_not_hints() {
    let stderr = "one\ntwo\nhint: skip\nthree\nfour\nfive\nsix\n\n";
    assert_eq!(message(stderr), "two / three / four / five / six");
}

#[test]
fn a_symbolic_link_in_place_of_a_checkout_is_not_the_checkout() {
    let fixture = fixture();
    let checkout = fixture.dir.path().join("work");
    work(&fixture, &checkout, "topic");
    let moved = fixture.dir.path().join("moved");
    git(
        &fixture.clone,
        &[
            "worktree",
            "move",
            checkout.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    std::os::unix::fs::symlink(&moved, &checkout).unwrap();
    assert_eq!(
        Git.listed_checkout(&fixture.clone, &checkout).unwrap(),
        None
    );
    assert!(
        Git.listed_checkout(&fixture.clone, &moved)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_failed_command_names_its_operation_and_the_last_line_of_its_error() {
    let fixture = fixture();
    let error = Git
        .delete_branch_at(
            &fixture.clone,
            "no-such-branch",
            "0123456789012345678901234567890123456789",
        )
        .unwrap_err();
    let Error::CommandFailed {
        operation, message, ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(operation, "update-ref -d");
    assert!(message.contains("no-such-branch"), "{message}");
}

#[test]
fn a_fetch_from_a_local_origin_that_is_gone_still_fails_as_a_plain_git_failure() {
    let fixture = fixture();
    fs::remove_dir_all(&fixture.origin).unwrap();
    let error = Git.fetch(&fixture.clone).unwrap_err();
    assert!(
        matches!(
            error,
            Error::CommandFailed {
                operation: "fetch origin",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_clone_of_an_origin_without_commits_has_an_empty_origin() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "origin.git",
        ],
    );
    git(&root, &["clone", "--quiet", "origin.git", "app"]);
    let clone = root.join("app");
    Git.fetch(&clone).unwrap();
    let error = Git.base(&clone).unwrap_err();
    assert!(matches!(error, Error::EmptyOrigin { .. }), "{error:?}");
}

#[test]
fn an_origin_with_branches_but_no_default_branch_has_no_default_branch() {
    let fixture = fixture();
    git(
        &fixture.clone,
        &["remote", "set-head", "origin", "--delete"],
    );
    git(
        &fixture.clone,
        &[
            "update-ref",
            "refs/remotes/origin/work",
            "refs/remotes/origin/main",
        ],
    );
    git(
        &fixture.clone,
        &["update-ref", "-d", "refs/remotes/origin/main"],
    );
    let error = Git.base(&fixture.clone).unwrap_err();
    assert!(matches!(error, Error::NoDefaultBranch { .. }), "{error:?}");
}

#[test]
fn a_path_tracks_a_file_only_when_the_repository_tracks_that_file_or_one_below_it() {
    let fixture = fixture();
    write(fixture.clone.join(".git/info/exclude"), "dist/\ntarget/\n");
    fs::create_dir_all(fixture.clone.join("dist")).unwrap();
    write(fixture.clone.join("dist/kept.txt"), "tracked");
    git(&fixture.clone, &["add", "--force", "dist/kept.txt"]);
    fs::create_dir_all(fixture.clone.join("target")).unwrap();
    write(fixture.clone.join("target/out"), "built");
    write(fixture.clone.join("a*b"), "a file whose name holds a star");
    assert!(Git.tracks(&fixture.clone, "dist/").unwrap());
    assert!(Git.tracks(&fixture.clone, "dist/kept.txt").unwrap());
    assert!(!Git.tracks(&fixture.clone, "target/").unwrap());
    assert!(!Git.tracks(&fixture.clone, "*").unwrap());
}

/// The most time that a fetch which ssh refuses may take: far less than
/// [`crate::FETCH_DEADLINE`], so a fetch that waited on a question until
/// the deadline ended it fails the test.
const AT_ONCE: std::time::Duration = std::time::Duration::from_secs(20);

#[test]
fn a_fetch_over_ssh_that_would_prompt_fails_at_once_and_names_the_remote() {
    let fixture = fixture();
    let ssh = fake_ssh(fixture.dir.path(), &fixture.origin);
    git(&fixture.clone, &["remote", "set-url", "origin", SSH_ORIGIN]);
    git(
        &fixture.clone,
        &["config", "core.sshCommand", &ssh.command("deny")],
    );
    let started = std::time::Instant::now();
    let error = Git.fetch(&fixture.clone).unwrap_err();
    assert!(started.elapsed() < AT_ONCE, "{:?}", started.elapsed());
    let Error::SshRefused {
        ref remote,
        ref host,
        refusal,
        ..
    } = error
    else {
        panic!("{error:?}\n{}", ssh.logged());
    };
    assert_eq!(remote, SSH_ORIGIN);
    assert_eq!(host, "example.invalid");
    assert_eq!(refusal, crate::SshRefusal::AuthenticationFailed);
    let logged = ssh.logged();
    assert!(logged.contains("args: deny "), "{logged}");
    assert!(!logged.contains("BatchMode"), "{logged}");
    assert!(logged.contains("SSH_ASKPASS=false\n"), "{logged}");
    assert!(logged.contains("SSH_ASKPASS_REQUIRE=force\n"), "{logged}");
}

#[test]
fn a_mirror_fetch_over_ssh_whose_host_key_is_unknown_names_the_host() {
    let fixture = fixture();
    let ssh = fake_ssh(fixture.dir.path(), &fixture.origin);
    let mirror = fixture.dir.path().join("mirror.git");
    Git.prepare_mirror(&mirror).unwrap();
    git(
        &mirror,
        &[
            "--git-dir=.",
            "config",
            "core.sshCommand",
            &ssh.command("unknown"),
        ],
    );
    let started = std::time::Instant::now();
    let error = Git.fetch_mirror(&mirror, SSH_ORIGIN, "main").unwrap_err();
    assert!(started.elapsed() < AT_ONCE, "{:?}", started.elapsed());
    let Error::SshRefused {
        ref host,
        refusal,
        ref message,
        ..
    } = error
    else {
        panic!("{error:?}\n{}", ssh.logged());
    };
    assert_eq!(host, "example.invalid");
    assert_eq!(refusal, crate::SshRefusal::HostKeyUnverified);
    assert_eq!(message, "Host key verification failed.");
}

#[test]
fn a_fetch_over_ssh_that_the_fake_server_accepts_succeeds() {
    let fixture = fixture();
    let ssh = fake_ssh(fixture.dir.path(), &fixture.origin);
    let latest = advance_origin(&fixture);
    git(&fixture.clone, &["remote", "set-url", "origin", SSH_ORIGIN]);
    git(
        &fixture.clone,
        &["config", "core.sshCommand", &ssh.command("accept")],
    );
    Git.fetch(&fixture.clone)
        .unwrap_or_else(|error| panic!("{error:?}\n{}", ssh.logged()));
    let fetched = git(&fixture.clone, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(fetched.trim(), latest);
    assert!(ssh.logged().contains("args: accept "), "{}", ssh.logged());
}

#[test]
fn the_patch_id_commands_carry_the_settings_that_stop_ssh_asking() {
    let fixture = fixture();
    for command in super::detached::patch_id_commands(&fixture.clone, &["HEAD".to_owned()]) {
        let envs: Vec<_> = command.get_envs().collect();
        for (name, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("SSH_ASKPASS_REQUIRE", "force"),
            ("SSH_ASKPASS", "false"),
        ] {
            assert!(
                envs.contains(&(
                    std::ffi::OsStr::new(name),
                    Some(std::ffi::OsStr::new(value))
                )),
                "{name}={value} is missing from {envs:?}"
            );
        }
    }
}
