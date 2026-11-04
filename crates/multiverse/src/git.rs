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
//!   error and discards the rest. A fetch has [`FETCH_DEADLINE`],
//!   `worktree add` has [`CHECKOUT_DEADLINE`], and every other command has
//!   [`LOCAL_DEADLINE`].
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

use crate::backend::{Backend, Changes, Listed, Repository};
use crate::error::Error;

#[cfg(test)]
mod tests;

/// The deadline of `git fetch`, which talks to the network.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(120);

/// The deadline of `git worktree add`, which writes every file of the
/// checkout and runs the repository's `post-checkout` hook, and may fetch
/// large files through a filter such as Git LFS.
pub const CHECKOUT_DEADLINE: Duration = Duration::from_secs(600);

/// The deadline of every other git command, which reads or writes only
/// local files.
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
            message: explanation(self.status, &self.stderr),
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

/// The most lines of standard error that a failure quotes when git printed
/// no `fatal:` or `error:` line.
const MOST_QUOTED_LINES: usize = 5;

/// Returns the part of git's standard error that explains a failure: the
/// last line that starts with `fatal:` or `error:`, or else the last
/// [`MOST_QUOTED_LINES`] lines that are not empty and are not `hint:`
/// lines, joined with ` / `.
fn message(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if let Some(line) = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
    {
        return (*line).to_owned();
    }
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.starts_with("hint:"))
        .collect();
    let start = kept.len().saturating_sub(MOST_QUOTED_LINES);
    kept.get(start..).unwrap_or_default().join(" / ")
}

/// Returns why a command with exit status `status` failed: [`message`]
/// of its standard error, followed by the signal that ended it, if a signal
/// did. A command that a signal ended may print nothing, so naming the
/// signal keeps the failure from reading as empty.
pub(crate) fn explanation(status: ExitStatus, stderr: &str) -> String {
    let explained = message(stderr);
    match signal(status) {
        None => explained,
        Some(signal) if explained.is_empty() => format!("git was ended by signal {signal}"),
        Some(signal) => format!("{explained} (git was ended by signal {signal})"),
    }
}

/// Returns the signal that ended a command, or `None` when it exited.
#[cfg(unix)]
fn signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// Returns `None`, because only Unix ends a command with a signal.
#[cfg(not(unix))]
fn signal(_status: ExitStatus) -> Option<i32> {
    None
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
                dir: dir.to_path_buf(),
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
            ["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
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
                return Ok(reference);
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

    fn branch_tip(&self, repository: &Path, branch: &str) -> Result<Option<String>, Error> {
        let reference = format!("refs/heads/{branch}");
        let found = run(
            repository,
            ["rev-parse", "--verify", "--quiet", reference.as_str()],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        Ok(found
            .status
            .success()
            .then(|| trimmed(&found.stdout))
            .filter(|tip| !tip.is_empty()))
    }

    fn create_branch(
        &self,
        repository: &Path,
        branch: &str,
        base: &str,
    ) -> Result<Option<String>, Error> {
        let target = format!("{base}^{{commit}}");
        let commit = run(
            repository,
            ["rev-parse", "--verify", "--quiet", target.as_str()],
            "rev-parse",
            LOCAL_DEADLINE,
        )?
        .ok("rev-parse", repository)?;
        let commit = trimmed(&commit);
        let reference = format!("refs/heads/{branch}");
        // An empty old value makes `update-ref` refuse when the reference
        // already exists, so creating the branch and checking that it is
        // new happen in one step.
        let created = run(
            repository,
            ["update-ref", reference.as_str(), commit.as_str(), ""],
            "update-ref",
            LOCAL_DEADLINE,
        )?;
        if created.status.success() {
            return Ok(Some(commit));
        }
        if self.branch_tip(repository, branch)?.is_some() {
            return Ok(None);
        }
        Err(created.failure("update-ref", repository))
    }

    fn add_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        branch: &str,
    ) -> Result<String, Error> {
        run(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--quiet"),
                checkout.as_os_str(),
                OsStr::new(branch),
            ],
            "worktree add",
            CHECKOUT_DEADLINE,
        )?
        .ok("worktree add", repository)?;
        let commit = run(checkout, ["rev-parse", "HEAD"], "rev-parse", LOCAL_DEADLINE)?
            .ok("rev-parse", checkout)?;
        Ok(trimmed(&commit))
    }

    fn listed_checkout(&self, repository: &Path, checkout: &Path) -> Result<Option<Listed>, Error> {
        let listed = run(
            repository,
            ["worktree", "list", "--porcelain"],
            "worktree list",
            LOCAL_DEADLINE,
        )?
        .ok("worktree list", repository)?;
        Ok(find_listed(&listed, checkout))
    }

    fn unlock_checkout(&self, repository: &Path, checkout: &Path) -> Result<(), Error> {
        run(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("unlock"),
                checkout.as_os_str(),
            ],
            "worktree unlock",
            LOCAL_DEADLINE,
        )?
        .ok("worktree unlock", repository)
        .map(drop)
    }

    fn changes(&self, checkout: &Path) -> Result<Changes, Error> {
        // `--ignored=matching` names each path that an ignore rule
        // matches, and names a matched directory once, such as
        // `web/node_modules/`, so a large build directory costs one line
        // and keeps the name that marks it disposable.
        let status = run(
            checkout,
            [
                // A status refreshes the index when it can, which would
                // count as a change of the checkout. `--no-optional-locks`
                // keeps it from writing anything.
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--untracked-files=normal",
                "--ignored=matching",
                // `-z` ends each entry with NUL and never quotes a path,
                // so a path with a space or a non-ASCII character keeps
                // its name.
                "-z",
            ],
            "status",
            LOCAL_DEADLINE,
        )?
        .ok("status", checkout)?;
        let mut changes = Changes::default();
        let mut entries = status.split('\0').filter(|entry| !entry.is_empty());
        while let Some(entry) = entries.next() {
            if let Some(path) = entry.strip_prefix("!! ") {
                changes.ignored.push(path.to_owned());
                continue;
            }
            // A rename or a copy names its source in the next entry.
            if entry.starts_with(['R', 'C']) {
                entries.next();
            }
            changes.changed.push(entry.to_owned());
        }
        Ok(changes)
    }

    fn tracks(&self, checkout: &Path, path: &str) -> Result<bool, Error> {
        // `:(literal)` keeps git from reading the path's `*` or `?` as a
        // pattern, so the listing names the path itself.
        let pathspec = format!(":(literal){path}");
        let listed = run(
            checkout,
            ["ls-files", "-z", "--cached", "--", pathspec.as_str()],
            "ls-files",
            LOCAL_DEADLINE,
        )?
        .ok("ls-files", checkout)?;
        Ok(!listed.is_empty())
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

    fn unmerged_commits(
        &self,
        repository: &Path,
        revision: &str,
        base: &str,
    ) -> Result<Option<u32>, Error> {
        let target = format!("{base}^{{commit}}");
        let found = run(
            repository,
            ["rev-parse", "--verify", "--quiet", target.as_str()],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        if !found.status.success() {
            return Ok(None);
        }
        let range = format!("{base}..{revision}");
        let counted = run(
            repository,
            ["rev-list", "--count", range.as_str(), "--"],
            "rev-list",
            LOCAL_DEADLINE,
        )?
        .ok("rev-list", repository)?;
        Ok(Some(count(&counted)))
    }

    fn stray_commits(&self, dir: &Path, revision: &str) -> Result<u32, Error> {
        let counted = run(
            dir,
            [
                "rev-list",
                "--count",
                revision,
                "--not",
                "--branches",
                "--tags",
                "--remotes",
                "--",
            ],
            "rev-list",
            LOCAL_DEADLINE,
        )?
        .ok("rev-list", dir)?;
        Ok(count(&counted))
    }

    fn delete_branch_at(&self, repository: &Path, branch: &str, commit: &str) -> Result<(), Error> {
        let reference = format!("refs/heads/{branch}");
        run(
            repository,
            ["update-ref", "-d", reference.as_str(), commit],
            "update-ref -d",
            LOCAL_DEADLINE,
        )?
        .ok("update-ref -d", repository)
        .map(drop)
    }
}

/// Returns the entry for `checkout` in `listed`, the output of
/// `git worktree list --porcelain`, which holds one block of lines for each
/// checkout, separated by empty lines.
fn find_listed(listed: &str, checkout: &Path) -> Option<Listed> {
    let wanted = same_path(checkout);
    listed.split("\n\n").find_map(|block| {
        let mut lines = block.lines();
        let path = lines.next()?.strip_prefix("worktree ")?;
        if same_path(Path::new(path)) != wanted {
            return None;
        }
        let mut entry = Listed::default();
        for line in lines {
            if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = Some(head.to_owned());
            } else if line == "detached" {
                entry.detached = true;
            } else if line == "locked" {
                entry.locked = Some(String::new());
            } else if let Some(reason) = line.strip_prefix("locked ") {
                entry.locked = Some(reason.to_owned());
            }
        }
        Some(entry)
    })
}

/// Returns the number that `git rev-list --count` printed, or `u32::MAX`
/// when git printed something else, so a caller that guards commits treats
/// an unreadable count as commits that would be lost.
fn count(printed: &str) -> u32 {
    printed.trim().parse().unwrap_or(u32::MAX)
}

/// Returns `path` with the symbolic links of its parent directories
/// resolved, so a path that git prints and the path that a record holds
/// compare equal on macOS, where `/var` leads to `/private/var`. The last
/// component stays as it is, so a symbolic link in place of a checkout
/// never compares equal to the checkout it leads to.
fn same_path(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map_or_else(|_| path.to_path_buf(), |parent| parent.join(name)),
        _ => path.to_path_buf(),
    }
}
