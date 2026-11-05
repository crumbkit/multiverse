//! Git repositories that the unit tests create in temporary directories.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

/// A temporary directory that holds a bare repository `origin.git` with one
/// commit on `main`, and a clone of it, `app`, whose `origin` is that
/// bare repository.
pub(crate) struct Fixture {
    /// The temporary directory, which is deleted when the fixture drops.
    pub(crate) dir: TempDir,
    /// The clone's top-level directory.
    pub(crate) clone: PathBuf,
    /// The bare repository.
    pub(crate) origin: PathBuf,
}

/// Runs git in `dir` with `args` and the test identity, without the
/// person's own git configuration, and returns its standard output.
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test fixture runs git to build repositories"
    )]
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Returns a new fixture whose clone is named `app`.
pub(crate) fn fixture() -> Fixture {
    fixture_named("app")
}

/// Returns a new fixture whose clone is named `name`.
pub(crate) fn fixture_named(name: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let seed = root.join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &seed,
        &["commit", "--quiet", "--allow-empty", "-m", "Start"],
    );
    let origin = root.join("origin.git");
    git(&root, &["clone", "--quiet", "--bare", "seed", "origin.git"]);
    git(&root, &["clone", "--quiet", "origin.git", name]);
    Fixture {
        dir,
        clone: root.join(name),
        origin,
    }
}

/// Adds a commit to `main` in the bare repository of `fixture`, through
/// a second clone, and returns the commit.
pub(crate) fn advance_origin(fixture: &Fixture) -> String {
    let root = fixture.dir.path().canonicalize().unwrap();
    let other = root.join("other");
    if !other.exists() {
        git(&root, &["clone", "--quiet", "origin.git", "other"]);
    }
    git(&other, &["pull", "--quiet"]);
    git(
        &other,
        &["commit", "--quiet", "--allow-empty", "-m", "Advance"],
    );
    git(&other, &["push", "--quiet", "origin", "main"]);
    git(&other, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Writes `text` to the file at `path`, as a person editing a checkout or a
/// record by hand would.
pub(crate) fn write(path: impl AsRef<Path>, text: impl AsRef<[u8]>) {
    #[expect(
        clippy::disallowed_methods,
        reason = "the tests write files that a person would edit by hand"
    )]
    std::fs::write(path, text).unwrap();
}
