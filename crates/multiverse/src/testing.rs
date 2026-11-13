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

/// The SSH URL that the tests give an origin. `.invalid` is a name that no
/// network resolves, so only the fake ssh of [`fake_ssh`] can answer it.
pub(crate) const SSH_ORIGIN: &str = "ssh://git@example.invalid/acme/app.git";

/// A stand-in for ssh that answers at once. Its first word chooses how the
/// host answers: `deny` refuses every key, `unknown` shows a host key that
/// the known hosts file lacks, `changed` shows a host key that differs from
/// the known one, and `accept` serves the repository `ORIGIN` with
/// `git upload-pack`. Each run appends its words and the values of
/// `SSH_ASKPASS` and `SSH_ASKPASS_REQUIRE` to the file `LOG`.
///
/// A real ssh asks a question, and waits, when it lacks a usable key or
/// knows no key for the host, unless `SSH_ASKPASS_REQUIRE=force` sends the
/// question to an askpass program. The stand-in prints ssh's own line and
/// exits with 255 when those variables would send the question to the
/// program, and otherwise prints `fake ssh would prompt` and exits with 97,
/// so a test that fails still fails at once.
const FAKE_SSH: &str = r#"#!/bin/sh
mode=$1
shift
printf 'args: %s %s\nSSH_ASKPASS=%s\nSSH_ASKPASS_REQUIRE=%s\n' "$mode" "$*" "${SSH_ASKPASS-}" "${SSH_ASKPASS_REQUIRE-}" >> 'LOG'
for word in "$@"; do
  [ "$word" = -G ] && exit 0
done
if [ "$mode" = accept ]; then
  exec git upload-pack 'ORIGIN'
fi
if [ "${SSH_ASKPASS_REQUIRE-}" = force ] && [ -n "${SSH_ASKPASS-}" ]; then
  case $mode in
    deny) echo 'git@example.invalid: Permission denied (publickey).' >&2 ;;
    changed)
      echo '@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @' >&2
      echo 'Host key verification failed.' >&2 ;;
    *) echo 'Host key verification failed.' >&2 ;;
  esac
  exit 255
fi
echo 'fake ssh would prompt' >&2
exit 97
"#;

/// A fake ssh written by [`fake_ssh`].
pub(crate) struct FakeSsh {
    /// The script.
    pub(crate) program: PathBuf,
    /// The file that each run of the script appends to.
    pub(crate) log: PathBuf,
}

impl FakeSsh {
    /// Returns the SSH command that runs the script in `mode`, as a person
    /// would write it in `core.sshCommand` or `GIT_SSH_COMMAND`.
    pub(crate) fn command(&self, mode: &str) -> String {
        format!("{} {mode}", self.program.display())
    }

    /// Returns what the runs of the script have logged.
    pub(crate) fn logged(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// Writes the fake ssh of [`FAKE_SSH`] into `dir`, serving `origin` in its
/// `accept` mode.
pub(crate) fn fake_ssh(dir: &Path, origin: &Path) -> FakeSsh {
    use std::os::unix::fs::PermissionsExt as _;
    let program = dir.join("fake-ssh");
    let log = dir.join("fake-ssh.log");
    let script = FAKE_SSH
        .replace("'LOG'", &format!("'{}'", log.display()))
        .replace("'ORIGIN'", &format!("'{}'", origin.display()));
    write(&program, script);
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    FakeSsh { program, log }
}
