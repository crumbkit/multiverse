//! The convention tests of the `mxv` command, which hold its command tree,
//! tool record, help pages, examples and failures to crumbkit's conventions with
//! crumbkit-test (CLI-6).
//!
//! They live in an integration test so that the example check can run the
//! built binary, whose path cargo sets only for integration tests.
//! `SNAPSHOTS=overwrite cargo test` writes, rewrites and deletes the golden
//! files.
//!
//! Each example runs in a fresh copy of a directory that the test builds
//! with git: a bare `origin.git`, its clone `app`, a repository `plain`
//! without an origin, and a clone `fresh` of an origin without commits. Its
//! XDG state starts as a copy of `tests/fixtures/xdg`, which holds the
//! record of the workspace `reading-list`. That record names directories
//! under `/home/agent`, which do not exist, so the examples of `show`, `cd`
//! and `remove` read a record whose checkout is already gone. `MXV_ROOT` is
//! unset, and the fixture's config file sets `root = "/home/agent/mxv"`, so
//! the examples of `new` and `show` print a root whose length is the same on
//! every machine. The default root under the temporary `XDG_DATA_HOME`
//! would change the width of the table's columns, because crumbkit-test
//! measures them before it replaces that path with its placeholder. `new`
//! runs as a dry run, so the examples fetch nothing.
//!
//! The process verbs `up`, `restart` and `logs` need a live checkout with
//! an `mxv.toml` and start real processes, so their examples do not run
//! here, and `tests/processes.rs` runs them instead. `down` and `status`
//! of the fixture's workspace run, because they read the run files, which
//! the fixture does not hold, and need no checkout.
//!
//! The refusals of `remove` need live checkouts, which a record under
//! `/home/agent` cannot name. The recovery and twin checks therefore also
//! start from [`live_workspaces`], which creates real workspaces with the
//! built binary in a temporary directory and an XDG fixture that holds
//! their records. Each chain of those checks reads the records from a copy
//! of that fixture, and acts on the real checkouts, so each refusal has a
//! workspace of its own.

#![cfg(test)]

use clap::CommandFactory as _;
use crumbkit_test::{
    Allow, Examples, Failing, PathBudgets, Recoveries, Rule, RuntimeFailing, Suite, Twin, Twins,
    Unprovoked,
};
#[path = "support/git.rs"]
mod support;

use crumbkit::codes::CONFIRM_NEEDS_YES;
use mxv::{
    ARCHIVE_CHECKOUT_UNUSABLE, ARCHIVE_CODES, ARCHIVE_IN_USE, ARCHIVE_MANIFEST_DISPOSABLE,
    ARCHIVE_MIRROR_MISSING, ARCHIVE_NAME_TAKEN, ARCHIVE_NOT_FOUND, ARCHIVE_RECORD_TOO_LARGE,
    ARCHIVE_SOURCE_CHANGED, ARCHIVE_UNFINISHED, ARCHIVE_UNSAVED_WORK, ARCHIVE_VOLUMES_IN_USE,
    BRANCH_EXISTS, CHECKOUT_INCOMPLETE, CHECKOUT_INVALID_NAME, CHECKOUT_LOCKED,
    CHECKOUT_NAME_TAKEN, CHECKOUT_NOT_FOUND, CHECKOUT_ORIGIN_UNREACHABLE, CHECKOUT_RESUME_BLOCKED,
    CHECKOUT_STASHES, CHECKOUT_UNOWNED, CHECKOUT_UNPUSHED_COMMITS, CHECKOUT_UNREACHABLE_COMMITS,
    CODES, CONFIG_INVALID, CONFIG_NO_LIMIT, CONFIG_UNREADABLE, Cli, DIRECTORY_TAKEN,
    DOCKER_ADDRESS_POOLS_EXHAUSTED, DOCKER_COMPOSE_MISSING, DOCKER_FAILED, DOCKER_FOREIGN_OBJECT,
    DOCKER_NOT_FOUND, DOCKER_UNAVAILABLE, ENVIRONMENT_NO_DIRECTORY, ENVIRONMENT_RELATIVE_ROOT,
    FILE_UNWRITABLE, GIT_FAILED, GIT_NOT_FOUND, GIT_TIMED_OUT, MANIFEST_INVALID,
    MANIFEST_NAME_CLASH, MANIFEST_NEEDED, MANIFEST_NEWER, MANIFEST_NOT_FOUND, MANIFEST_RESERVED,
    MANIFEST_UNIT_NAME, MANIFEST_UNKNOWN_KEY, MANIFEST_UNREADABLE, MANIFEST_UNRUNNABLE,
    MANIFEST_UNTRUSTED, MIRROR_FETCH_FAILED, MIRROR_LOCKED, PATH_INACCESSIBLE, PHASE_FAILED,
    PORT_EXHAUSTED, PORT_NOT_FOUND, PORT_TAKEN, PROCESS_DETACHED_LISTENER, PROCESS_EXITED,
    PROCESS_FOREIGN_LISTENER, PROCESS_NOT_FOUND, PROCESS_NOT_READY, PROCESS_TABLE_UNREADABLE,
    PROCESS_UNSTARTABLE, PROCESS_UNSTOPPABLE, PROCESS_UNSUPPORTED_OS, RECORD_CORRUPT,
    RECORD_MISMATCHED, RECORD_NEWER, RECORD_UNREADABLE, REPOSITORY_EMPTY_ORIGIN,
    REPOSITORY_NO_DEFAULT_BRANCH, REPOSITORY_NO_ORIGIN, REPOSITORY_NOT_FOUND, RESTORE_BRANCH_MOVED,
    RESTORE_COMMIT_MISSING, RESTORE_NAME_TAKEN, RESTORE_NOT_FOUND, RESTORE_ROOT_FILE_CHANGED,
    RESTORE_UNFINISHED, RUN_CODES, TEMPLATE_AMBIGUOUS, TEMPLATE_INVALID, TOOL, VOLUME_RENAMED,
    WORKSPACE_BAD_NAME, WORKSPACE_BUSY, WORKSPACE_CHANGED, WORKSPACE_DIRTY, WORKSPACE_EXISTS,
    WORKSPACE_IGNORED_FILES, WORKSPACE_NOT_CURRENT, WORKSPACE_NOT_FOUND, WORKSPACE_RUNNING,
    WORKSPACE_VOLUMES, WORKSPACE_WORKTREE_KIND,
};
use tempfile::TempDir;

use crate::support::{git, repository};

/// The reason that a code of the container verbs is unprovoked here.
const DOCKER: &str = "Only a live checkout whose mxv.toml declares containers, with a docker daemon that answers, provokes it; tests/containers.rs provokes it when docker runs.";

/// The reason that a process verb's examples do not run.
const NO_LIVE_CHECKOUT: &str = "The example starts or reads the processes of a live checkout with an mxv.toml, and the fixture's workspace names a checkout under /home/agent, which a test cannot create; tests/processes.rs runs the verb.";

/// The reason that `down` and `restart` ask no question.
const NO_QUESTION: &str = "The command stops only units that mxv up started for the named workspace, and it deletes no file that the person wrote, so it asks no question first.";

/// The reason that a code of `archive` and `restore` is unprovoked here.
const ARCHIVE_STATE: &str = "Only a real workspace or archive in that state provokes it, and the fixture's records name checkouts under /home/agent, which a test cannot create; tests/archive.rs provokes the code, or, for a refusal that needs a change partway through a run, the tests of multiverse's archive module provoke the refusal.";

/// The `mxv` command's opt-outs from crumbkit's conventions, which every
/// check below reads.
const ALLOW: &[Allow] = &[
    Allow::new(Rule::Examples, "up", NO_LIVE_CHECKOUT),
    Allow::new(Rule::Examples, "restart", NO_LIVE_CHECKOUT),
    Allow::new(Rule::Examples, "logs", NO_LIVE_CHECKOUT),
    Allow::new(Rule::Examples, "exec", NO_LIVE_CHECKOUT),
    Allow::new(
        Rule::Yes,
        "exec",
        "The person writes the command after `--`, and mxv cannot tell what it changes, so a question from mxv would ask about every command, harmless or not.",
    ),
    Allow::new(
        Rule::Examples,
        "add",
        "The example adds a checkout to a workspace of clones, and the fixture's only workspace holds a git worktree under /home/agent, which a test cannot create; tests/workspaces.rs runs the verb.",
    ),
    Allow::new(
        Rule::Examples,
        "archive",
        "The example archives a live workspace, and the fixture's workspace names a checkout under /home/agent, which a test cannot create; tests/archive.rs runs the verb.",
    ),
    Allow::new(
        Rule::Examples,
        "self usage",
        "The output names the day the report starts, which changes every day, and tests/usage.rs checks the usage log instead.",
    ),
    Allow::new(
        Rule::Examples,
        "self figures list",
        "The figures come from the usage log of the machine that runs the test, and tests/usage.rs checks the usage log instead.",
    ),
    Allow::new(Rule::Yes, "down", NO_QUESTION),
    Allow::new(Rule::Yes, "restart", NO_QUESTION),
    Allow::new(
        Rule::Yes,
        "reclaim",
        "A load manager runs the command when the machine runs short, and no person is there to answer. It stops only the units that mxv up started for idle workspaces, which mxv up starts again, and deletes only the disposable directories that mxv clean --yes deletes, so it asks no question first.",
    ),
    Allow::new(
        Rule::Yes,
        "ports reset",
        "The command replaces only host ports that mxv allocated, refuses while the workspace's units run, and deletes no file that the person wrote, so it asks no question first.",
    ),
    Allow::new(Rule::Examples, "ports reset", NO_LIVE_CHECKOUT),
    Allow::untwinned(
        NEWER_RECORD,
        "The record that a newer mxv wrote causes the failure, and every word of the command line that names it reads the same record.",
    ),
    Allow::untwinned(
        CORRUPT_RECORD,
        "The corrupt record causes the failure, and every word of the command line that names it reads the same record.",
    ),
    Allow::untwinned(
        MISMATCHED_RECORD,
        "The record that holds another name causes the failure, and every word of the command line that names it reads the same record.",
    ),
    Allow::untwinned(
        ADD_TO_WORKTREES,
        "The kind of the workspace's record causes the failure, and the fixture holds no workspace of clones to name instead.",
    ),
    Allow::untwinned(
        BAD_CONFIG,
        "The config file causes the failure, and no word of the command line names the config file.",
    ),
    Allow::untwinned(
        UNREADABLE_CONFIG,
        "The directory at the config file's path causes the failure, and no word of the command line names the config file.",
    ),
];

/// `restore` of a name that no archive has.
const RESTORE_MISSING: Failing<'static> =
    Failing::new(&RESTORE_NOT_FOUND, &["restore", "missing", "--dry-run"]);

/// `show` of a record that holds another name than its file.
const MISMATCHED_RECORD: Failing<'static> =
    Failing::new(&RECORD_MISMATCHED, &["show", "reading-list"])
        .xdg("tests/fixtures/xdg-mismatched");

/// `new` with a config file that sets an unknown key.
const BAD_CONFIG: Failing<'static> = Failing::new(
    &CONFIG_INVALID,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.xdg("tests/fixtures/xdg-bad-config");

/// `new` with a directory where the config file belongs.
const UNREADABLE_CONFIG: Failing<'static> = Failing::new(
    &CONFIG_UNREADABLE,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.xdg("tests/fixtures/xdg-config-directory");

/// `show` of a record that a newer mxv wrote.
const NEWER_RECORD: Failing<'static> =
    Failing::new(&RECORD_NEWER, &["show", "reading-list"]).xdg("tests/fixtures/xdg-newer");

/// `show` of a record that is not JSON.
const CORRUPT_RECORD: Failing<'static> =
    Failing::new(&RECORD_CORRUPT, &["show", "reading-list"]).xdg("tests/fixtures/xdg-corrupt");

/// The budgets of the paths through the help, which are crumbkit's
/// defaults.
const PATH_BUDGETS: PathBudgets<'_> = PathBudgets::new("tests/golden/examples");
/// The variables of every run: a `PATH` that finds git, and git without
/// the person's configuration. `MXV_ROOT` is unset, so the workspace root
/// comes from the config file of `tests/fixtures/xdg`.
const ENV: &[(&str, &str)] = &[
    ("PATH", support::PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// The directory of repositories that each run starts in a fresh copy of,
/// which the test deletes when it drops.
struct Repositories {
    /// The temporary directory.
    _dir: TempDir,
    /// The directory's path, which [`Examples`] borrows.
    path: String,
}

/// Returns a new directory of the repositories that the module docs name.
/// The clone `app` also holds a branch named `taken`.
fn repositories() -> Repositories {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let app = repository(&root);
    git(&app, &["branch", "taken"]);
    git(&root, &["clone", "--quiet", "origin.git", "web"]);
    git(
        &root.join("web"),
        &["remote", "set-url", "origin", "../origin.git"],
    );
    let plain = root.join("plain");
    std::fs::create_dir(&plain).unwrap();
    git(&plain, &["init", "--quiet"]);
    git(
        &root,
        &[
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            "empty.git",
        ],
    );
    git(&root, &["clone", "--quiet", "empty.git", "fresh"]);
    git(
        &root.join("fresh"),
        &["remote", "set-url", "origin", "../empty.git"],
    );
    let path = root.to_str().unwrap().to_owned();
    Repositories { _dir: dir, path }
}

/// Returns the settings that run the built binary in a fresh copy of the
/// repositories at `path`, with the XDG state of `tests/fixtures/xdg`.
fn examples(path: &str) -> Examples<'_> {
    Examples::new(env!("CARGO_BIN_EXE_mxv"), "tests/golden/examples")
        .env(ENV)
        .xdg("tests/fixtures/xdg")
        .fresh_dir(path)
        .allow(ALLOW)
}

/// `cd` of a name that no workspace has.
const CD_MISSING: Failing<'static> = Failing::new(&WORKSPACE_NOT_FOUND, &["cd", "missing"]);

/// `new` of the name of the workspace in the fixture.
const NEW_TAKEN_NAME: Failing<'static> = Failing::new(
    &WORKSPACE_EXISTS,
    &["new", "reading-list", "--repo", "app", "--dry-run"],
);

/// `new` of a name with a slash.
const NEW_BAD_NAME: Failing<'static> = Failing::new(
    &WORKSPACE_BAD_NAME,
    &["new", "a/b", "--repo", "app", "--dry-run"],
);

/// `new` of the name of a branch that the repository already has.
const NEW_TAKEN_BRANCH: Failing<'static> = Failing::new(
    &BRANCH_EXISTS,
    &["new", "taken", "--repo", "app", "--dry-run"],
);

/// `new` of a path that does not exist.
const NEW_MISSING_REPOSITORY: Failing<'static> = Failing::new(
    &REPOSITORY_NOT_FOUND,
    &["new", "triage", "--repo", "missing", "--dry-run"],
);

/// `new` of a repository without an origin.
const NEW_NO_ORIGIN: Failing<'static> = Failing::new(
    &REPOSITORY_NO_ORIGIN,
    &["new", "triage", "--repo", "plain", "--dry-run"],
);

/// `new` of a repository whose origin has no commits.
const NEW_EMPTY_ORIGIN: Failing<'static> = Failing::new(
    &REPOSITORY_EMPTY_ORIGIN,
    &["new", "triage", "--repo", "fresh", "--dry-run"],
);

/// `new` with a relative `MXV_ROOT`.
const NEW_RELATIVE_ROOT: Failing<'static> = Failing::new(
    &ENVIRONMENT_RELATIVE_ROOT,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.env(&[("MXV_ROOT", Some("work"))]);

/// `new` of a name whose directory already exists under the default root.
const NEW_TAKEN_DIRECTORY: Failing<'static> = Failing::new(
    &DIRECTORY_TAKEN,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.xdg("tests/fixtures/xdg-taken");

/// `new` with a `PATH` that finds no git.
const NEW_NO_GIT: Failing<'static> = Failing::new(
    &GIT_NOT_FOUND,
    &["new", "triage", "--repo", "app", "--dry-run"],
)
.env(&[("PATH", Some("/nonexistent"))]);

/// `clean --over-limit` with a config file that sets no `max_disposable`.
const CLEAN_NO_LIMIT: Failing<'static> =
    Failing::new(&CONFIG_NO_LIMIT, &["clean", "--over-limit", "--dry-run"]);

/// `status` outside every workspace, without a name.
const STATUS_OUTSIDE: Failing<'static> = Failing::new(&WORKSPACE_NOT_CURRENT, &["status"]);

/// `status` of a name that no workspace has.
const STATUS_MISSING: Failing<'static> =
    Failing::new(&WORKSPACE_NOT_FOUND, &["status", "-w", "missing"]);

/// `new` of two checkouts in the folder `app`.
const NEW_TAKEN_FOLDER: Failing<'static> = Failing::new(
    &CHECKOUT_NAME_TAKEN,
    &[
        "new",
        "triage",
        "--repo",
        "app",
        "--repo",
        "app",
        "--as",
        "app",
        "--dry-run",
    ],
);

/// `new` of a checkout in the folder that holds the workspace's
/// `AGENTS.md`.
const NEW_RESERVED_FOLDER: Failing<'static> = Failing::new(
    &CHECKOUT_INVALID_NAME,
    &[
        "new",
        "triage",
        "--repo",
        "app",
        "--as",
        "AGENTS.md",
        "--dry-run",
    ],
);

/// `cd` of a checkout that the fixture's workspace does not have.
const CD_MISSING_CHECKOUT: Failing<'static> =
    Failing::new(&CHECKOUT_NOT_FOUND, &["cd", "reading-list", "missing"]);

/// `add` to the fixture's workspace, whose record of version 1 holds a git
/// worktree.
const ADD_TO_WORKTREES: Failing<'static> = Failing::new(
    &WORKSPACE_WORKTREE_KIND,
    &["add", "-w", "reading-list", "--repo", "app", "--dry-run"],
);

/// The words of `new` that the fixture accepts, which each `new` line above
/// differs from in one word.
const NEW_TRIAGE: &[&str] = &["new", "triage", "--repo", "app", "--dry-run"];

/// The command lines that start the recovery chains, one for each code that
/// a command line provokes.
const STARTS: &[Failing<'static>] = &[
    CD_MISSING,
    NEW_TAKEN_NAME,
    NEW_BAD_NAME,
    NEW_TAKEN_BRANCH,
    NEW_MISSING_REPOSITORY,
    NEW_NO_ORIGIN,
    NEW_EMPTY_ORIGIN,
    NEW_RELATIVE_ROOT,
    NEW_TAKEN_DIRECTORY,
    NEW_NO_GIT,
    NEWER_RECORD,
    CORRUPT_RECORD,
    MISMATCHED_RECORD,
    BAD_CONFIG,
    UNREADABLE_CONFIG,
    Failing::new(&CONFIRM_NEEDS_YES, &["remove", "reading-list"]),
    RESTORE_MISSING,
    STATUS_OUTSIDE,
    STATUS_MISSING,
    CLEAN_NO_LIMIT,
    NEW_TAKEN_FOLDER,
    NEW_RESERVED_FOLDER,
    CD_MISSING_CHECKOUT,
    ADD_TO_WORKTREES,
];

/// The declared codes that no command line in this test provokes, because
/// each needs a state that a fixed fixture cannot hold.
const UNPROVOKED: &[Unprovoked<'static>] = &[
    Unprovoked::new(&ARCHIVE_NOT_FOUND, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_NAME_TAKEN, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_SOURCE_CHANGED, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_MIRROR_MISSING, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_UNSAVED_WORK, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_IN_USE, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_CHECKOUT_UNUSABLE, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_UNFINISHED, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_MANIFEST_DISPOSABLE, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_RECORD_TOO_LARGE, ARCHIVE_STATE),
    Unprovoked::new(&ARCHIVE_VOLUMES_IN_USE, ARCHIVE_STATE),
    Unprovoked::new(&RESTORE_NAME_TAKEN, ARCHIVE_STATE),
    Unprovoked::new(&RESTORE_BRANCH_MOVED, ARCHIVE_STATE),
    Unprovoked::new(&RESTORE_COMMIT_MISSING, ARCHIVE_STATE),
    Unprovoked::new(&RESTORE_UNFINISHED, ARCHIVE_STATE),
    Unprovoked::new(&RESTORE_ROOT_FILE_CHANGED, ARCHIVE_STATE),
    Unprovoked::new(
        &MIRROR_LOCKED,
        "Only a mirror whose lock another mxv command holds for longer than mxv waits provokes it, which is longer than the check waits for a run; the library's workspace tests provoke it with a short wait.",
    ),
    Unprovoked::new(
        &MIRROR_FETCH_FAILED,
        "Only an origin that cannot be fetched provokes it, and the check's fixture keeps its origin; tests/workspaces.rs provokes it with an origin that is gone.",
    ),
    Unprovoked::new(
        &TEMPLATE_INVALID,
        "Only a malformed template in the config directory's templates provokes it, and the check's fixture has no templates; tests/workspaces.rs provokes it with a template that lists the repository's origin and holds a key after its [template] header.",
    ),
    Unprovoked::new(
        &MANIFEST_UNTRUSTED,
        "Only a `--manifest` file inside a git work tree or under the workspaces' root provokes it, and the check's command lines name no manifest file; tests/workspaces.rs provokes it with the repository's own mxv.toml.",
    ),
    Unprovoked::new(
        &TEMPLATE_AMBIGUOUS,
        "Only two templates that list the repository's origin provoke it, and the check's fixture has no templates; tests/workspaces.rs provokes it with two such templates.",
    ),
    Unprovoked::new(
        &CHECKOUT_ORIGIN_UNREACHABLE,
        "Only a clone whose origin cannot be fetched provokes it, and the check's fixture keeps its origin; tests/workspaces.rs provokes it with an origin that is gone.",
    ),
    Unprovoked::new(
        &WORKSPACE_CHANGED,
        "Only a workspace that changes while mxv remove waits for the person's answer provokes it, and the check's runs have no terminal to wait at; the library's workspace tests provoke the change.",
    ),
    Unprovoked::new(
        &PATH_INACCESSIBLE,
        "Only a workspace path below a directory without search permission provokes it, and the check's fixture copies keep the permissions searchable; the library's workspace tests provoke it.",
    ),
    Unprovoked::new(
        &ENVIRONMENT_NO_DIRECTORY,
        "Only a run in which mxv finds no home directory provokes it, and etcetera reads the home directory from the user database when HOME is unset, so every test run has one.",
    ),
    Unprovoked::new(
        &CHECKOUT_UNOWNED,
        "Only a checkout directory that git does not list provokes it, and the fixture's workspace has no checkout directory.",
    ),
    Unprovoked::new(
        &REPOSITORY_NO_DEFAULT_BRANCH,
        "Only a clone whose origin has branches but no default branch provokes it; the library's git tests provoke it.",
    ),
    Unprovoked::new(
        &GIT_FAILED,
        "Only a git command that fails in a working repository provokes it, such as a fetch from an origin that is gone; the library's git tests provoke it.",
    ),
    Unprovoked::new(
        &GIT_TIMED_OUT,
        "Only a git command that runs past its deadline of 30, 120 or 600 seconds provokes it, which is longer than the check waits for a run.",
    ),
    Unprovoked::new(
        &RECORD_UNREADABLE,
        "Only a record directory without read permission provokes it, and the check's fixture copies keep the permissions readable.",
    ),
    Unprovoked::new(
        &FILE_UNWRITABLE,
        "Only a directory without write permission or a full disk provokes it, and the examples write nothing outside their fresh directories.",
    ),
    Unprovoked::new(&WORKSPACE_RUNNING, LIVE),
    Unprovoked::new(&WORKSPACE_VOLUMES, DOCKER),
    Unprovoked::new(
        &DOCKER_NOT_FOUND,
        "Only a live checkout whose mxv.toml declares containers, on a PATH without docker, provokes it; tests/containers.rs provokes it.",
    ),
    Unprovoked::new(
        &DOCKER_UNAVAILABLE,
        "Only a docker daemon that does not answer provokes it, which the fixture cannot hold; tests/containers.rs provokes it with a docker that cannot reach its daemon.",
    ),
    Unprovoked::new(
        &DOCKER_ADDRESS_POOLS_EXHAUSTED,
        "Only a docker daemon whose address pools are used up provokes it, which a test cannot make without changing the daemon; the library's docker tests read docker's message for it.",
    ),
    Unprovoked::new(
        &DOCKER_COMPOSE_MISSING,
        "Only a docker without its compose plugin provokes it; tests/containers.rs provokes it with a docker whose compose command fails.",
    ),
    Unprovoked::new(&DOCKER_FAILED, DOCKER),
    Unprovoked::new(&DOCKER_FOREIGN_OBJECT, DOCKER),
    Unprovoked::new(&MANIFEST_NOT_FOUND, LIVE),
    Unprovoked::new(&MANIFEST_UNIT_NAME, LIVE),
    Unprovoked::new(&MANIFEST_UNRUNNABLE, LIVE),
    Unprovoked::new(
        &VOLUME_RENAMED,
        "Only `mxv add` to a workspace of one clone whose container has run and left a docker volume provokes it, with a docker daemon that answers.",
    ),
    Unprovoked::new(
        &PHASE_FAILED,
        "Only a failure of phase that mxv has no code of its own for provokes it, such as a run record that phase cannot read.",
    ),
    Unprovoked::new(&MANIFEST_UNKNOWN_KEY, LIVE),
    Unprovoked::new(&MANIFEST_NEWER, LIVE),
    Unprovoked::new(&PORT_NOT_FOUND, LIVE),
    Unprovoked::new(&MANIFEST_RESERVED, LIVE),
    Unprovoked::new(&MANIFEST_INVALID, LIVE),
    Unprovoked::new(
        &MANIFEST_NAME_CLASH,
        "Only two checkouts whose mxv.toml files declare names that clash once prefixed provoke it, and the fixture's workspace has one checkout; tests/workspaces.rs provokes it.",
    ),
    Unprovoked::new(
        &MANIFEST_NEEDED,
        "Only `mxv add` to a live workspace of one clone with a workspace manifest provokes it, and the fixture's workspace holds a git worktree; tests/multirepo.rs provokes it.",
    ),
    Unprovoked::new(
        &CHECKOUT_INCOMPLETE,
        "Only `mxv add` to a live workspace of clones whose record names an unfinished checkout provokes it, and the fixture's workspace holds a git worktree; tests/multirepo.rs provokes it.",
    ),
    Unprovoked::new(
        &CHECKOUT_RESUME_BLOCKED,
        "Only `mxv add` that resumes an unfinished clone whose directory holds work provokes it, and the fixture's workspace holds a git worktree; the library's tests provoke it.",
    ),
    Unprovoked::new(
        &MANIFEST_UNREADABLE,
        "Only an mxv.toml without read permission provokes it, in a live checkout that the fixture cannot hold.",
    ),
    Unprovoked::new(&PROCESS_NOT_FOUND, LIVE),
    Unprovoked::new(&PROCESS_EXITED, LIVE),
    Unprovoked::new(&PROCESS_NOT_READY, LIVE),
    Unprovoked::new(&PROCESS_FOREIGN_LISTENER, LIVE),
    Unprovoked::new(&PROCESS_DETACHED_LISTENER, LIVE),
    Unprovoked::new(
        &PROCESS_UNSUPPORTED_OS,
        "Only an operating system whose libproc structs have another size than the ones mxv reads provokes it.",
    ),
    Unprovoked::new(
        &PROCESS_UNSTOPPABLE,
        "Only a process group that outlives SIGKILL provokes it, such as a process stuck in the kernel, which a test cannot make on demand.",
    ),
    Unprovoked::new(&PORT_TAKEN, LIVE),
    Unprovoked::new(
        &PROCESS_UNSTARTABLE,
        "Only a manifest process whose cwd is missing from a live checkout provokes it; the library's process tests provoke it.",
    ),
    Unprovoked::new(
        &PROCESS_TABLE_UNREADABLE,
        "Only a PATH without ps, during a run that has started processes, provokes it, which the fixture cannot hold.",
    ),
    Unprovoked::new(
        &PORT_EXHAUSTED,
        "Only a port range whose every port is held or taken provokes it.",
    ),
    Unprovoked::new(
        &WORKSPACE_BUSY,
        "Only a second mxv command that runs while the first holds the workspace's lock provokes it; tests/processes.rs provokes it.",
    ),
];

/// The reason that a code of the process verbs is unprovoked here.
const LIVE: &str = "Only a live checkout with an mxv.toml provokes it, and the fixture's workspace names a checkout under /home/agent, which a test cannot create; tests/processes.rs provokes it.";
/// The variables of the runs that create the live workspaces, beside the
/// XDG directories: git without the person's configuration, with an author
/// for the commits.
const LIVE_ENV: &[(&str, &str)] = &[
    ("PATH", support::PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
];

/// Real workspaces of the clone `app` in a temporary directory, which
/// [`live_workspaces`] creates, and the XDG fixture that holds their
/// records.
struct Live {
    /// The temporary directory, which the test deletes when it drops.
    _dir: TempDir,
    /// The XDG fixture, whose `state` directory holds the records.
    xdg: std::path::PathBuf,
}

/// Returns real workspaces, each in the state that one refusal of
/// `remove` needs. The clones are `changed`, with an untracked file,
/// `secret`, with an ignored `.env`, `unpushed`, with a commit that origin
/// does not hold, `stashed`, with a stash entry, and `clean`, with nothing
/// that stops a removal. The workspaces of git worktrees, which mxv made
/// before clones, are `detached`, with a commit on a detached `HEAD`, and
/// `locked`, with a checkout that git lists as locked.
fn live_workspaces() -> Live {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let app = repository(&root);
    write_text(&app.join(".git/info/exclude"), ".env\n");
    let xdg = root.join("xdg");
    for name in ["changed", "secret", "unpushed", "stashed", "clean"] {
        #[expect(
            clippy::disallowed_methods,
            reason = "the fixture runs the built binary to create real workspaces"
        )]
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_mxv"))
            .args(["new", name, "--repo"])
            .arg(&app)
            .env_clear()
            .envs(LIVE_ENV.iter().copied())
            .env("HOME", root.join("home"))
            .env("MXV_ROOT", root.join("workspaces"))
            .env("XDG_CONFIG_HOME", xdg.join("config"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .env("XDG_STATE_HOME", xdg.join("state"))
            .env("XDG_CACHE_HOME", xdg.join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "mxv new {name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let checkout = |name: &str| root.join("workspaces").join(name).join("app");
    for name in ["detached", "locked"] {
        worktree_workspace(&app, &root.join("workspaces"), &xdg, name);
    }
    write_text(&checkout("changed").join("notes.md"), "notes");
    write_text(&checkout("secret").join(".env"), "TOKEN=1");
    git(
        &checkout("unpushed"),
        &["commit", "--quiet", "--allow-empty", "-m", "Unpushed"],
    );
    write_text(&checkout("stashed").join("notes.md"), "notes");
    git(
        &checkout("stashed"),
        &["stash", "push", "--quiet", "--include-untracked"],
    );
    git(&checkout("detached"), &["switch", "--quiet", "--detach"]);
    git(
        &checkout("detached"),
        &["commit", "--quiet", "--allow-empty", "-m", "Lost"],
    );
    git(
        &app,
        &[
            "worktree",
            "lock",
            "--reason",
            "in use",
            checkout("locked").to_str().unwrap(),
        ],
    );
    Live { _dir: dir, xdg }
}

/// Makes the workspace `name` below `workspaces` as mxv made workspaces
/// before clones: a git worktree of `app` on a new branch `name`, and a
/// record of version 1 without a `kind` in the state directory of `xdg`.
fn worktree_workspace(
    app: &std::path::Path,
    workspaces: &std::path::Path,
    xdg: &std::path::Path,
    name: &str,
) {
    let workspace = workspaces.join(name);
    let checkout = workspace.join("app");
    std::fs::create_dir_all(&workspace).unwrap();
    git(app, &["branch", "--no-track", name, "origin/main"]);
    git(
        app,
        &[
            "worktree",
            "add",
            "--quiet",
            checkout.to_str().unwrap(),
            name,
        ],
    );
    let commit = git(&checkout, &["rev-parse", "HEAD"]);
    let record = serde_json::json!({
        "version": 1,
        "name": name,
        "path": workspace,
        "checkouts": [{
            "name": "app",
            "backend": "git",
            "source": app,
            "path": checkout,
            "branch": name,
            "base": "refs/remotes/origin/main",
            "commit": commit.trim(),
        }],
    });
    let records = xdg.join("state").join("mxv").join("workspaces");
    std::fs::create_dir_all(&records).unwrap();
    write_text(&records.join(format!("{name}.json")), &record.to_string());
}

/// Writes `text` to the file at `path`, as a person editing a file by hand
/// would.
fn write_text(path: &std::path::Path, text: &str) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the fixture writes a file that a person would write by hand"
    )]
    std::fs::write(path, text).unwrap();
}

impl Live {
    /// Returns the dry run of `remove` that fails with `code` on the
    /// workspace `name`, with the records of the live workspaces.
    fn line(&self, code: crumbkit::Code, name: &str) -> RuntimeFailing {
        RuntimeFailing::new(code, ["remove", name, "--dry-run"]).xdg(&self.xdg)
    }

    /// Returns the refusals of `remove` that the live workspaces provoke.
    fn refusals(&self) -> Vec<RuntimeFailing> {
        vec![
            self.line(WORKSPACE_DIRTY, "changed"),
            self.line(WORKSPACE_IGNORED_FILES, "secret"),
            self.line(CHECKOUT_UNREACHABLE_COMMITS, "detached"),
            self.line(CHECKOUT_LOCKED, "locked"),
            self.line(CHECKOUT_UNPUSHED_COMMITS, "unpushed"),
            self.line(CHECKOUT_STASHES, "stashed"),
        ]
    }
}

/// One twin for each declared code that a command line provokes. Each twin
/// differs from its refused line in one word or one variable (TEST-1).
const PAIRS: &[Twin<'static>] = &[
    Twin::new(CD_MISSING, &["cd", "reading-list"]),
    Twin::new(RESTORE_MISSING, &["restore", "sample", "--dry-run"]),
    Twin::new(NEW_TAKEN_NAME, NEW_TRIAGE),
    Twin::new(NEW_BAD_NAME, NEW_TRIAGE),
    Twin::new(NEW_TAKEN_BRANCH, NEW_TRIAGE),
    Twin::new(NEW_MISSING_REPOSITORY, NEW_TRIAGE),
    Twin::new(NEW_NO_ORIGIN, NEW_TRIAGE),
    Twin::new(NEW_EMPTY_ORIGIN, NEW_TRIAGE),
    Twin::variable(NEW_RELATIVE_ROOT, "MXV_ROOT"),
    Twin::new(
        NEW_TAKEN_DIRECTORY,
        &["new", "sorting", "--repo", "app", "--dry-run"],
    ),
    Twin::variable(NEW_NO_GIT, "PATH"),
    Twin::new(STATUS_OUTSIDE, &["status", "--workspace=reading-list"]),
    Twin::new(CLEAN_NO_LIMIT, &["clean", "--all", "--dry-run"]),
    Twin::new(
        NEW_TAKEN_FOLDER,
        &[
            "new",
            "triage",
            "--repo",
            "app",
            "--repo",
            "app",
            "--as",
            "copy",
            "--dry-run",
        ],
    ),
    Twin::new(
        NEW_RESERVED_FOLDER,
        &["new", "triage", "--repo", "app", "--as", "api", "--dry-run"],
    ),
    Twin::new(CD_MISSING_CHECKOUT, &["cd", "reading-list", "app"]),
];

/// The settings of every check, built in a fresh set of fixtures that the
/// checks' runs leave behind them when the value drops.
///
/// Each test that `conventions!` expands builds its own, so the recovery
/// chains, which act on the live checkouts, never share a checkout with the
/// twin check or with another test.
struct Fixtures {
    /// The suite, whose settings borrow the leaked paths and lists below.
    suite: Suite<'static>,
    /// The temporary directory of the repositories that each example starts
    /// in a copy of, which the test deletes when it drops.
    _repositories: Repositories,
    /// The live workspaces of the recovery check.
    _recovery: Live,
    /// The live workspaces of the twin check.
    _twin: Live,
}

impl std::ops::Deref for Fixtures {
    type Target = Suite<'static>;

    fn deref(&self) -> &Suite<'static> {
        &self.suite
    }
}

/// Returns `value` for the rest of the process. The suite borrows its path
/// and lists, and a test process builds the suite once.
fn leaked<T: ?Sized>(value: Box<T>) -> &'static T {
    Box::leak(value)
}

/// Returns the suite of the `mxv` command and the fixtures it reads.
fn suite() -> Fixtures {
    let repositories = repositories();
    let recovery = live_workspaces();
    let twin = live_workspaces();
    let path: &'static str = leaked(repositories.path.clone().into_boxed_str());
    let examples = examples(path);
    let mut starts = STARTS.to_vec();
    starts.extend(
        recovery
            .refusals()
            .into_iter()
            .map(RuntimeFailing::into_failing),
    );
    let mut pairs = PAIRS.to_vec();
    pairs.extend(
        twin.refusals()
            .into_iter()
            .map(|refused| refused.twin(["remove", "clean", "--dry-run"])),
    );
    let codes: &[&[&crumbkit::Code]] = &[CODES, RUN_CODES, ARCHIVE_CODES];
    let recoveries = Recoveries::new(examples, codes)
        .starts(leaked(starts.into_boxed_slice()))
        .unprovoked(UNPROVOKED);
    let twins = Twins::new(examples, codes)
        .pairs(leaked(pairs.into_boxed_slice()))
        .unprovoked(UNPROVOKED);
    let suite = Suite::new(|| (Cli::command(), TOOL))
        .allow(ALLOW)
        .verbs(&["usage", "reset"])
        .path_budgets(PATH_BUDGETS)
        .examples(examples)
        .recoveries(recoveries)
        .twins(twins);
    Fixtures {
        suite,
        _repositories: repositories,
        _recovery: recovery,
        _twin: twin,
    }
}

crumbkit_test::conventions!(suite());
