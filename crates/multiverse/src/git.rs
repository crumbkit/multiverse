//! The git backend, which is the one module that starts processes (PROC-1).
//!
//! Every operation runs one `git` command, as follows (PROC-2):
//!
//! - Program: `git`, found on the `PATH` that the caller's process
//!   inherited. Whoever controls that `PATH` supplies its bytes.
//! - Arguments: separate values that each operation fixes, with the
//!   caller's paths, branch names and remote branch names as single
//!   values. No shell runs. A branch name comes from a
//!   [`WorkspaceName`](crate::WorkspaceName), so it never starts with `-`.
//! - Environment: the caller's environment, plus `GIT_TERMINAL_PROMPT=0`,
//!   so a fetch that needs a password fails instead of waiting for an
//!   answer that nobody gives, and `LC_ALL=C`, so the messages that the
//!   errors quote are in English.
//! - Streams and deadline: standard input is empty. The module keeps the
//!   first [`MOST_OUTPUT_BYTES`] bytes of standard output and of standard
//!   error and discards the rest. A fetch has [`FETCH_DEADLINE`], and every
//!   other command has [`LOCAL_DEADLINE`].
//! - End: the module waits for the child and reads its exit status. A
//!   child that misses its deadline is killed and reaped, and the
//!   operation returns [`Error::TimedOut`]. The module stops only the
//!   direct child and not processes that git started, such as `ssh`
//!   (PROC-3). A fetch that is stopped can leave git's own lock files,
//!   which git reports on the next fetch. A checkout that `worktree add`
//!   leaves half made is reported by git's own error.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::backend::{Backend, Repository};
use crate::error::Error;

#[cfg(test)]
mod tests;

/// The deadline of `git fetch`, which talks to the network.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(120);

/// The deadline of every git command that reads or writes only local files.
pub const LOCAL_DEADLINE: Duration = Duration::from_secs(30);

/// The most bytes of each output stream that a command keeps.
pub const MOST_OUTPUT_BYTES: u64 = 1024 * 1024;

/// The program that this backend runs.
const GIT: &str = "git";

/// The git backend, which runs the `git` program found on `PATH`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Git;

/// What a finished git command printed and how it exited.
struct Finished {
    /// The command's exit status.
    status: ExitStatus,
    /// The kept bytes of standard output, as text.
    stdout: String,
    /// The kept bytes of standard error, as text.
    stderr: String,
}

impl Finished {
    /// Returns the failure of a command that exited with a failure.
    fn failure(&self, operation: &'static str, dir: &Path) -> Error {
        Error::CommandFailed {
            operation,
            dir: dir.to_path_buf(),
            code: self.status.code(),
            message: self
                .stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .to_owned(),
        }
    }

    /// Returns standard output when the command succeeded, or its failure.
    fn ok(self, operation: &'static str, dir: &Path) -> Result<String, Error> {
        if self.status.success() {
            Ok(self.stdout)
        } else {
            Err(self.failure(operation, dir))
        }
    }
}

/// Runs `git -C <dir> <args>` as the module docs describe, and returns
/// what it printed and how it exited.
fn run<I, S>(
    dir: &Path,
    args: I,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/git.rs is the module that starts processes (PROC-1)"
    )]
    let mut command = Command::new(GIT);
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/git.rs is the module that starts processes (PROC-1)"
    )]
    let spawned = command.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/git.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut child: std::process::Child = spawned.map_err(|source| Error::ProgramMissing {
        program: GIT,
        source,
    })?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let waited = child.wait_timeout(deadline);
    let status = match waited {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            // The child is killed and reaped whatever the wait reported, so
            // no zombie remains. A kill of a child that already exited
            // fails harmlessly.
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::TimedOut {
                operation,
                deadline,
            });
        }
    };
    Ok(Finished {
        status,
        stdout: joined(stdout),
        stderr: joined(stderr),
    })
}

/// Reads `stream` on a thread of its own: the first [`MOST_OUTPUT_BYTES`]
/// bytes into a buffer, and the rest into nothing, so the child never
/// blocks on a full pipe.
fn drain<R: Read + Send + 'static>(mut stream: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut stream).take(MOST_OUTPUT_BYTES).read_to_end(&mut kept);
        let _ = io::copy(&mut stream, &mut io::sink());
        kept
    })
}

/// Returns the text that a drain thread read, or an empty text when the
/// stream was not piped or the thread panicked.
fn joined(handle: Option<thread::JoinHandle<Vec<u8>>>) -> String {
    let bytes = handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Returns `line` without its trailing line break.
fn trimmed(line: &str) -> String {
    line.trim_end_matches(['\n', '\r']).to_owned()
}

impl Backend for Git {
    fn repository(&self, path: &Path) -> Result<Repository, Error> {
        let not_found = || Error::RepositoryNotFound {
            path: path.to_path_buf(),
        };
        if !path.is_dir() {
            return Err(not_found());
        }
        let toplevel = run(
            path,
            ["rev-parse", "--show-toplevel"],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        if !toplevel.status.success() {
            return Err(not_found());
        }
        let root = PathBuf::from(trimmed(&toplevel.stdout));
        if root.as_os_str().is_empty() {
            return Err(not_found());
        }
        let origin = run(
            &root,
            ["remote", "get-url", "origin"],
            "remote get-url",
            LOCAL_DEADLINE,
        )?;
        if !origin.status.success() {
            return Err(Error::NoOrigin { repository: root });
        }
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repository".to_owned());
        Ok(Repository { root, name })
    }

    fn fetch(&self, repository: &Path) -> Result<(), Error> {
        run(
            repository,
            ["fetch", "--quiet", "origin"],
            "fetch origin",
            FETCH_DEADLINE,
        )?
        .ok("fetch origin", repository)
        .map(drop)
    }

    fn base(&self, repository: &Path) -> Result<String, Error> {
        let head = run(
            repository,
            [
                "symbolic-ref",
                "--quiet",
                "--short",
                "refs/remotes/origin/HEAD",
            ],
            "symbolic-ref",
            LOCAL_DEADLINE,
        )?;
        if head.status.success() {
            let base = trimmed(&head.stdout);
            if !base.is_empty() {
                return Ok(base);
            }
        }
        for candidate in ["main", "master"] {
            let reference = format!("refs/remotes/origin/{candidate}");
            let found = run(
                repository,
                ["rev-parse", "--verify", "--quiet", reference.as_str()],
                "rev-parse",
                LOCAL_DEADLINE,
            )?;
            if found.status.success() {
                return Ok(format!("origin/{candidate}"));
            }
        }
        let branches = run(
            repository,
            [
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/remotes/origin/",
            ],
            "for-each-ref",
            LOCAL_DEADLINE,
        )?
        .ok("for-each-ref", repository)?;
        let repository = repository.to_path_buf();
        if branches.trim().is_empty() {
            Err(Error::EmptyOrigin { repository })
        } else {
            Err(Error::NoDefaultBranch { repository })
        }
    }

    fn has_branch(&self, repository: &Path, branch: &str) -> Result<bool, Error> {
        let reference = format!("refs/heads/{branch}");
        let found = run(
            repository,
            ["rev-parse", "--verify", "--quiet", reference.as_str()],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        Ok(found.status.success())
    }

    fn add_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        branch: &str,
        base: &str,
    ) -> Result<String, Error> {
        run(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--quiet"),
                OsStr::new("--no-track"),
                OsStr::new("-b"),
                OsStr::new(branch),
                checkout.as_os_str(),
                OsStr::new(base),
            ],
            "worktree add",
            LOCAL_DEADLINE,
        )?
        .ok("worktree add", repository)?;
        let commit = run(checkout, ["rev-parse", "HEAD"], "rev-parse", LOCAL_DEADLINE)?
            .ok("rev-parse", checkout)?;
        Ok(trimmed(&commit))
    }

    fn owns_checkout(&self, repository: &Path, checkout: &Path) -> Result<bool, Error> {
        let listed = run(
            repository,
            ["worktree", "list", "--porcelain"],
            "worktree list",
            LOCAL_DEADLINE,
        )?
        .ok("worktree list", repository)?;
        let wanted = same_path(checkout);
        Ok(listed
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .any(|listed| same_path(Path::new(listed)) == wanted))
    }

    fn changes(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        let status = run(
            checkout,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            "status",
            LOCAL_DEADLINE,
        )?
        .ok("status", checkout)?;
        Ok(status
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn remove_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        force: bool,
    ) -> Result<(), Error> {
        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(checkout.as_os_str());
        run(repository, args, "worktree remove", LOCAL_DEADLINE)?
            .ok("worktree remove", repository)
            .map(drop)
    }

    fn prune_checkouts(&self, repository: &Path) -> Result<(), Error> {
        run(
            repository,
            ["worktree", "prune"],
            "worktree prune",
            LOCAL_DEADLINE,
        )?
        .ok("worktree prune", repository)
        .map(drop)
    }

    fn is_merged(&self, repository: &Path, branch: &str, base: &str) -> Result<bool, Error> {
        let reference = format!("refs/heads/{branch}");
        let compared = run(
            repository,
            ["merge-base", "--is-ancestor", reference.as_str(), base],
            "merge-base",
            LOCAL_DEADLINE,
        )?;
        match compared.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(compared.failure("merge-base", repository)),
        }
    }

    fn delete_branch(&self, repository: &Path, branch: &str) -> Result<(), Error> {
        run(
            repository,
            ["branch", "--quiet", "-D", branch],
            "branch -D",
            LOCAL_DEADLINE,
        )?
        .ok("branch -D", repository)
        .map(drop)
    }
}

/// Returns `path` with symbolic links resolved when it exists, so a path
/// that git prints and the path that a record holds compare equal on macOS,
/// where `/var` leads to `/private/var`.
fn same_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
