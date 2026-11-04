//! Git repositories that the end-to-end tests build in temporary
//! directories, and the variables that each run of `mxv` gets.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The `PATH` of each run, which finds git on Linux and macOS.
pub(crate) const PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

/// The variables that keep git from reading the person's own configuration
/// and give commits an author.
pub(crate) const GIT_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_AUTHOR_NAME", "Test"),
    ("GIT_AUTHOR_EMAIL", "test@example.invalid"),
    ("GIT_COMMITTER_NAME", "Test"),
    ("GIT_COMMITTER_EMAIL", "test@example.invalid"),
];

/// Runs git in `dir` with `args` and returns its standard output.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test fixture runs git to build repositories"
    )]
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("PATH", PATH)
        .envs(GIT_ENV.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Creates, in `root`, a bare repository `origin.git` with one commit on
/// `main` and a clone of it named `app` whose `origin` is the relative
/// path `../origin.git`, so a copy of `root` keeps working. Returns the
/// clone's path.
pub(crate) fn repository(root: &Path) -> PathBuf {
    let seed = root.join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &seed,
        &["commit", "--quiet", "--allow-empty", "-m", "Start"],
    );
    git(root, &["clone", "--quiet", "--bare", "seed", "origin.git"]);
    std::fs::remove_dir_all(&seed).unwrap();
    git(root, &["clone", "--quiet", "origin.git", "app"]);
    let clone = root.join("app");
    git(&clone, &["remote", "set-url", "origin", "../origin.git"]);
    clone
}
