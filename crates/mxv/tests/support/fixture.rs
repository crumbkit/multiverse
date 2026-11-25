//! The fixture in which the convention tests and the terminal test run the
//! examples of `mxv`: the variables of each run and a directory of git
//! repositories.
//!
//! The module uses `crate::support`, so a test file that includes it must
//! also mount `support/git.rs` as the module `support`.

use tempfile::TempDir;

use crate::support::{PATH, git, repository};

/// The variables of every run: a `PATH` that finds git, and git without
/// the person's configuration. `MXV_ROOT` is unset, so the workspace root
/// comes from the config file of `tests/fixtures/xdg`.
pub(crate) const ENV: &[(&str, &str)] = &[
    ("PATH", PATH),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// The directory of repositories that each run starts in a fresh copy of,
/// which the test deletes when it drops.
#[derive(Debug)]
pub(crate) struct Repositories {
    /// The temporary directory.
    _dir: TempDir,
    /// The directory's canonical path.
    pub(crate) path: String,
}

/// Returns a new directory of repositories: a bare `origin.git`, its clones
/// `app` and `web`, a repository `plain` without an origin, and a clone
/// `fresh` of an origin without commits. The clone `app` also holds a
/// branch named `taken`.
pub(crate) fn repositories() -> Repositories {
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
