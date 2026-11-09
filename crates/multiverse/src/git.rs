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
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::backend::{Backend, Changes, Concealed, Head, Listed, LocalNames, Repository};
use crate::error::Error;
use crate::mirror::redacted;

mod concealed;
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

/// The most bytes of output that a listing of every path of a checkout
/// keeps, because a listing that stops early would hide work.
const MOST_LISTING_BYTES: u64 = 256 * 1024 * 1024;

/// The configuration that makes git sync references and loose objects before
/// it reports success, so a pin survives a power cut. The components add to
/// git's default, which syncs packs and leaves out both of these. The
/// components are named because git documents the aggregate `committed` as
/// covering only objects.
const DURABLE: &str = "core.fsync=reference,loose-object";

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

/// The refspec that copies the tags of a clone's `origin` into a namespace
/// of their own, so a tag counts as pushed only while `origin` holds it.
const ORIGIN_TAGS: &str = "+refs/tags/*:refs/mxv/origin/tags/*";

/// The refspec that copies the notes of a clone's `origin` into a
/// namespace of their own, so a note counts as pushed only while `origin`
/// holds it.
const ORIGIN_NOTES: &str = "+refs/notes/*:refs/mxv/origin/notes/*";

/// The pattern of the copies of the tags and notes of a clone's `origin`.
const ORIGIN_REFS: &str = "--glob=refs/mxv/origin/*";

/// How far before the oldest detached commit [`detached_commits`] looks for
/// a copy of it, in seconds, so a clock that was a little wrong when the
/// copy was made does not hide it.
const COPY_MARGIN: i64 = 24 * 60 * 60;

/// Returns the lines of `text` that are not empty.
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Returns the commits made on a detached `HEAD` in the git directory
/// `dir` that nothing else holds, which deleting the clone loses and git
/// itself warns about when `HEAD` leaves them.
///
/// A commit counts when only the reflog of `HEAD` holds it: the reflog of
/// no local branch holds it or a descendant of it, and no branch, tag,
/// note or copy of `origin` holds it. A commit that was rewritten on a
/// branch, by `git rebase`, `git commit --amend` or `git reset`, is in the
/// branch's reflog and was given up on purpose, so it does not count. A
/// commit whose patch matches a commit that `origin` or a local branch
/// holds, such as one that was cherry-picked onto the branch, does not
/// count either.
fn detached_commits(dir: &Path) -> Result<Vec<String>, Error> {
    let branches = run(
        dir,
        ["for-each-ref", "--format=%(refname)", "refs/heads"],
        "for-each-ref",
        LOCAL_DEADLINE,
    )?
    .ok("for-each-ref", dir)?;
    let branches = lines(&branches);
    let reflog = |refs: &[String]| -> Result<Vec<String>, Error> {
        let mut args: Vec<String> = ["log", "-g", "--format=%H"].map(str::to_owned).to_vec();
        args.extend(refs.iter().cloned());
        args.push("--".to_owned());
        let logged = run(dir, &args, "log -g", LOCAL_DEADLINE)?.ok("log -g", dir)?;
        Ok(lines(&logged))
    };
    let on_branches = if branches.is_empty() {
        Vec::new()
    } else {
        reflog(&branches)?
    };
    let mut only_head: Vec<String> = reflog(&["HEAD".to_owned()])?
        .into_iter()
        .filter(|commit| !on_branches.contains(commit))
        .collect();
    only_head.sort_unstable();
    only_head.dedup();
    if only_head.is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<String> = ["rev-list", "--ignore-missing"].map(str::to_owned).to_vec();
    args.extend(only_head);
    args.extend(
        [
            "--not",
            "--branches",
            "--tags",
            "--glob=refs/notes/*",
            "--remotes=origin",
            ORIGIN_REFS,
        ]
        .map(str::to_owned),
    );
    args.extend(on_branches);
    args.push("--".to_owned());
    let candidates = lines(&run(dir, &args, "rev-list", LOCAL_DEADLINE)?.ok("rev-list", dir)?);
    if candidates.is_empty() {
        return Ok(candidates);
    }
    let mut args: Vec<String> = ["log", "--no-walk", "--format=%ct"]
        .map(str::to_owned)
        .to_vec();
    args.extend(candidates.iter().cloned());
    args.push("--".to_owned());
    let oldest = run(dir, &args, "log", LOCAL_DEADLINE)?
        .ok("log", dir)?
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .min()
        .unwrap_or(0);
    let mut args: Vec<String> = vec!["--no-walk".to_owned()];
    args.extend(candidates.iter().cloned());
    let patches = patch_ids(dir, &args)?;
    let known = patch_ids(
        dir,
        &[
            format!("--max-age={}", oldest.saturating_sub(COPY_MARGIN).max(0)),
            "--branches".to_owned(),
            "--remotes=origin".to_owned(),
            ORIGIN_REFS.to_owned(),
        ],
    )?;
    let known: Vec<&String> = known.values().collect();
    Ok(candidates
        .into_iter()
        .filter(|commit| {
            patches
                .get(commit)
                .is_none_or(|patch| !known.contains(&patch))
        })
        .collect())
}

/// Returns the stable patch ID of each commit that `git log` lists with
/// `revisions` in the git directory `dir`, by commit. A merge and a commit
/// that changes nothing have no patch ID and are left out. `git log -p`
/// writes straight into `git patch-id`, so no patch passes through mxv.
fn patch_ids(
    dir: &Path,
    revisions: &[String],
) -> Result<std::collections::HashMap<String, String>, Error> {
    let git = || {
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/multiverse/src/git.rs is the module that starts processes (PROC-1)"
        )]
        let mut command = Command::new(GIT);
        command
            .arg("-C")
            .arg(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        command
    };
    let mut log = git();
    log.args([
        "log",
        "-p",
        "--no-merges",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--pretty=medium",
    ])
    .args(revisions)
    .arg("--")
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let unstartable = |source| Error::ProgramMissing {
        program: GIT,
        source,
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/git.rs is the module that starts processes (PROC-1)"
    )]
    let spawned = log.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/git.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut producer: std::process::Child = spawned.map_err(unstartable)?;
    let patches = producer.stdout.take().map_or_else(Stdio::null, Stdio::from);
    let mut patch_id = git();
    patch_id
        .args(["patch-id", "--stable"])
        .stdin(patches)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[expect(
        clippy::disallowed_methods,
        reason = "crates/multiverse/src/git.rs is the module that starts processes (PROC-1)"
    )]
    let spawned = patch_id.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/git.rs holds the child and owns its deadline and cleanup (PROC-1)"
    )]
    let mut consumer: std::process::Child = match spawned {
        Ok(child) => child,
        Err(source) => {
            let _ = producer.kill();
            let _ = producer.wait();
            return Err(unstartable(source));
        }
    };
    let stdout = consumer
        .stdout
        .take()
        .map(|stream| drain(stream, MOST_OUTPUT_BYTES));
    let stderr = consumer
        .stderr
        .take()
        .map(|stream| drain(stream, MOST_OUTPUT_BYTES));
    let waited = consumer.wait_timeout(LOCAL_DEADLINE);
    let status = match waited {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            // Both children are killed and reaped whatever the wait
            // reported, so no zombie remains.
            let _ = consumer.kill();
            let _ = consumer.wait();
            let _ = producer.kill();
            let _ = producer.wait();
            return Err(Error::TimedOut {
                operation: "patch-id",
                dir: dir.to_path_buf(),
                deadline: LOCAL_DEADLINE,
            });
        }
    };
    // `git patch-id` read the whole log, so the log has ended or ends now.
    let _ = producer.wait();
    let finished = Finished {
        status,
        stdout: joined(stdout),
        stderr: joined(stderr),
    };
    let listed = finished.ok("patch-id", dir)?;
    Ok(listed
        .lines()
        .filter_map(|line| {
            let (patch, commit) = line.split_once(' ')?;
            Some((commit.to_owned(), patch.to_owned()))
        })
        .collect())
}

/// Returns the git directory of the clone at `checkout`, followed by the
/// git directory of each submodule that the clone has initialised, which
/// git keeps below `.git/modules`, at any depth. A submodule's commits
/// live only in its git directory, so deleting the clone loses them too.
/// A worktree or a submodule has a file named `.git`, so its `.git/modules`
/// is no directory and it adds no submodules here.
fn git_dirs(checkout: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut dirs = vec![checkout.to_path_buf()];
    let mut pending = vec![checkout.join(".git").join("modules")];
    while let Some(dir) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(source) => return Err(Error::Inaccessible { path: dir, source }),
        };
        for entry in entries {
            let entry = entry.map_err(|source| Error::Inaccessible {
                path: dir.clone(),
                source,
            })?;
            let path = entry.path();
            let is_dir = entry
                .file_type()
                .map_err(|source| Error::Inaccessible {
                    path: path.clone(),
                    source,
                })?
                .is_dir();
            if !is_dir {
                continue;
            }
            if path.join("HEAD").is_file() && path.join("objects").is_dir() {
                pending.push(path.join("modules"));
                dirs.push(path);
            } else {
                pending.push(path);
            }
        }
    }
    Ok(dirs)
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
    let explained = redacted(&message(stderr));
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
    run_keeping(dir, args, operation, deadline, MOST_OUTPUT_BYTES)
}

/// Runs `git -C <dir> <args>` as [`run`] does, and keeps the first `kept`
/// bytes of its standard output.
fn run_keeping<I, S>(
    dir: &Path,
    args: I,
    operation: &'static str,
    deadline: Duration,
    kept: u64,
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
    let stdout = child.stdout.take().map(|stream| drain(stream, kept));
    let stderr = child
        .stderr
        .take()
        .map(|stream| drain(stream, MOST_OUTPUT_BYTES));
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

/// Reads `stream` on a thread of its own: the first `most` bytes into a
/// buffer, and the rest into nothing, so the child never blocks on a full
/// pipe.
fn drain<R: Read + Send + 'static>(mut stream: R, most: u64) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut stream).take(most).read_to_end(&mut kept);
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

    fn head(&self, checkout: &Path) -> Result<Head, Error> {
        // A checkout that lost its `.git` inside another repository reads
        // that repository, so the caller compares the top level with the
        // checkout.
        let toplevel = run(
            checkout,
            ["rev-parse", "--show-toplevel"],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        let commit = run(
            checkout,
            ["rev-parse", "--verify", "--quiet", "HEAD"],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        // `symbolic-ref --quiet` exits with 1 and prints nothing for a
        // detached `HEAD`.
        let branch = run(
            checkout,
            ["symbolic-ref", "--quiet", "--short", "HEAD"],
            "symbolic-ref",
            LOCAL_DEADLINE,
        )?;
        let named = |finished: &Finished| {
            finished
                .status
                .success()
                .then(|| trimmed(&finished.stdout))
                .filter(|text| !text.is_empty())
        };
        Ok(Head {
            toplevel: named(&toplevel).map(PathBuf::from),
            branch: named(&branch),
            commit: named(&commit),
        })
    }

    fn changes(&self, checkout: &Path) -> Result<Changes, Error> {
        // `--ignored=matching` names each path that an ignore rule
        // matches, and names a matched directory once, such as
        // `web/node_modules/`, so a large build directory costs one line
        // and keeps the name that marks it disposable.
        let status = run_keeping(
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
            MOST_LISTING_BYTES,
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

    fn origin_url(&self, repository: &Path) -> Result<String, Error> {
        let origin = run(
            repository,
            ["remote", "get-url", "origin"],
            "remote get-url",
            LOCAL_DEADLINE,
        )?;
        if !origin.status.success() {
            return Err(Error::NoOrigin {
                repository: repository.to_path_buf(),
            });
        }
        Ok(trimmed(&origin.stdout))
    }

    fn prepare_mirror(&self, mirror: &Path) -> Result<(), Error> {
        fs::create_dir_all(mirror).map_err(|source| Error::Unwritable {
            path: mirror.to_path_buf(),
            source,
        })?;
        if !mirror.join("HEAD").is_file() {
            // An empty `--template=` copies no template, so the mirror has
            // no hooks.
            run(
                mirror,
                ["init", "--quiet", "--bare", "--template="],
                "init --bare",
                LOCAL_DEADLINE,
            )?
            .ok("init --bare", mirror)?;
        }
        for (key, value) in [
            ("gc.auto", "0"),
            ("gc.pruneExpire", "never"),
            ("maintenance.auto", "false"),
        ] {
            run(mirror, ["config", key, value], "config", LOCAL_DEADLINE)?.ok("config", mirror)?;
        }
        Ok(())
    }

    fn fetch_mirror(&self, mirror: &Path, url: &str, default: &str) -> Result<(), Error> {
        let fetched = run(
            mirror,
            [
                "fetch",
                "--quiet",
                "--prune",
                "--no-auto-gc",
                url,
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
            "fetch mirror",
            FETCH_DEADLINE,
        )?;
        if !fetched.status.success() {
            return Err(Error::MirrorFetchFailed {
                origin: redacted(url),
                mirror: mirror.to_path_buf(),
                message: explanation(fetched.status, &fetched.stderr),
            });
        }
        let head = format!("refs/heads/{default}");
        run(
            mirror,
            ["symbolic-ref", "HEAD", head.as_str()],
            "symbolic-ref",
            LOCAL_DEADLINE,
        )?
        .ok("symbolic-ref", mirror)
        .map(drop)
    }

    fn clone_mirror(&self, mirror: &Path, checkout: &Path, url: &str) -> Result<(), Error> {
        let parent = checkout.parent().unwrap_or(checkout);
        run(
            parent,
            [
                OsStr::new("clone"),
                OsStr::new("--quiet"),
                OsStr::new("--shared"),
                OsStr::new("--no-checkout"),
                OsStr::new("--template="),
                OsStr::new("--"),
                mirror.as_os_str(),
                checkout.as_os_str(),
            ],
            "clone --shared",
            CHECKOUT_DEADLINE,
        )?
        .ok("clone --shared", parent)?;
        run(
            checkout,
            ["remote", "set-url", "origin", url],
            "remote set-url",
            LOCAL_DEADLINE,
        )?
        .ok("remote set-url", checkout)
        .map(drop)
    }

    fn exclude_file(&self, repository: &Path) -> Result<PathBuf, Error> {
        let path = run(
            repository,
            ["rev-parse", "--git-path", "info/exclude"],
            "rev-parse",
            LOCAL_DEADLINE,
        )?
        .ok("rev-parse", repository)?;
        // git prints the path relative to the directory it ran in, unless
        // it is absolute.
        Ok(repository.join(trimmed(&path)))
    }

    fn switch_branch(&self, checkout: &Path, branch: &str) -> Result<String, Error> {
        run(
            checkout,
            ["checkout", "--quiet", branch, "--"],
            "checkout",
            CHECKOUT_DEADLINE,
        )?
        .ok("checkout", checkout)?;
        let commit = run(checkout, ["rev-parse", "HEAD"], "rev-parse", LOCAL_DEADLINE)?
            .ok("rev-parse", checkout)?;
        Ok(trimmed(&commit))
    }

    fn refresh_origin(&self, checkout: &Path) -> Result<(), Error> {
        for dir in git_dirs(checkout)? {
            let fetched = run(
                &dir,
                [
                    "fetch",
                    "--quiet",
                    "--prune",
                    "--no-tags",
                    "--no-auto-gc",
                    "origin",
                    "+refs/heads/*:refs/remotes/origin/*",
                    ORIGIN_TAGS,
                    ORIGIN_NOTES,
                ],
                "fetch origin",
                FETCH_DEADLINE,
            )?;
            if !fetched.status.success() {
                return Err(Error::OriginFetchFailed {
                    checkout: dir,
                    message: explanation(fetched.status, &fetched.stderr),
                });
            }
        }
        Ok(())
    }

    fn unpushed_commits(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        let mut unpushed = Vec::new();
        for dir in git_dirs(checkout)? {
            let held = run(
                &dir,
                [
                    "rev-list",
                    "HEAD",
                    "--branches",
                    "--tags",
                    "--glob=refs/notes/*",
                    "--not",
                    "--remotes=origin",
                    ORIGIN_REFS,
                    "--",
                ],
                "rev-list",
                LOCAL_DEADLINE,
            )?
            .ok("rev-list", &dir)?;
            let held = lines(&held);
            let detached = detached_commits(&dir)?;
            unpushed.extend(detached.into_iter().filter(|commit| !held.contains(commit)));
            unpushed.extend(held);
        }
        Ok(unpushed)
    }

    fn stashes(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        let found = run(
            checkout,
            ["rev-parse", "--verify", "--quiet", "refs/stash"],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        if !found.status.success() {
            return Ok(Vec::new());
        }
        let listed = run(
            checkout,
            ["rev-list", "--walk-reflogs", "refs/stash", "--"],
            "rev-list",
            LOCAL_DEADLINE,
        )?
        .ok("rev-list", checkout)?;
        Ok(listed
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
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

    fn write_ref(&self, repository: &Path, reference: &str, commit: &str) -> Result<(), Error> {
        run(
            repository,
            ["-c", DURABLE, "update-ref", reference, commit],
            "update-ref",
            LOCAL_DEADLINE,
        )?
        .ok("update-ref", repository)
        .map(drop)
    }

    fn fetch_head_into_mirror(
        &self,
        mirror: &Path,
        checkout: &Path,
        reference: &str,
    ) -> Result<String, Error> {
        let refspec = format!("+HEAD:{reference}");
        run(
            mirror,
            [
                OsStr::new("-c"),
                OsStr::new(DURABLE),
                OsStr::new("fetch"),
                OsStr::new("--quiet"),
                OsStr::new("--no-tags"),
                OsStr::new("--no-auto-gc"),
                OsStr::new("--no-recurse-submodules"),
                checkout.as_os_str(),
                OsStr::new(refspec.as_str()),
            ],
            "fetch into mirror",
            FETCH_DEADLINE,
        )?
        .ok("fetch into mirror", mirror)?;
        let commit = run(
            mirror,
            ["rev-parse", "--verify", reference],
            "rev-parse",
            LOCAL_DEADLINE,
        )?
        .ok("rev-parse", mirror)?;
        Ok(trimmed(&commit))
    }

    fn delete_ref(&self, repository: &Path, reference: &str) -> Result<(), Error> {
        run(
            repository,
            ["update-ref", "-d", reference],
            "update-ref -d",
            LOCAL_DEADLINE,
        )?
        .ok("update-ref -d", repository)
        .map(drop)
    }

    fn resolve_commit(&self, repository: &Path, revision: &str) -> Result<Option<String>, Error> {
        let target = format!("{revision}^{{commit}}");
        let found = run(
            repository,
            ["rev-parse", "--verify", "--quiet", target.as_str()],
            "rev-parse",
            LOCAL_DEADLINE,
        )?;
        Ok(found
            .status
            .success()
            .then(|| trimmed(&found.stdout))
            .filter(|commit| !commit.is_empty()))
    }

    fn commit_all(&self, checkout: &Path, message: &str) -> Result<(), Error> {
        run(checkout, ["add", "--all"], "add", CHECKOUT_DEADLINE)?.ok("add", checkout)?;
        // `core.hooksPath` turns off every hook, and `--no-verify` alone
        // would still run `prepare-commit-msg` and `post-commit`. A hook can
        // fail or change files after mxv read the checkout. A signature can
        // wait for a passphrase that nobody is there to give.
        let mut args = concealed::identity(checkout)?;
        args.extend(
            [
                "-c",
                DURABLE,
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--no-verify",
                "-m",
                message,
            ]
            .map(str::to_owned),
        );
        run(checkout, args, "commit", CHECKOUT_DEADLINE)?
            .ok("commit", checkout)
            .map(drop)
    }

    fn check_commit_all(&self, checkout: &Path) -> Result<(), Error> {
        concealed::check_commit_all(checkout)
    }

    fn concealed(&self, checkout: &Path) -> Result<Concealed, Error> {
        concealed::concealed(checkout)
    }

    fn local_names(&self, checkout: &Path, mirror: &Path) -> Result<LocalNames, Error> {
        concealed::local_names(checkout, mirror)
    }

    fn remove_origin(&self, checkout: &Path) -> Result<(), Error> {
        run(
            checkout,
            ["remote", "remove", "origin"],
            "remote remove",
            LOCAL_DEADLINE,
        )?
        .ok("remote remove", checkout)
        .map(drop)
    }

    fn local_only_commits(&self, dir: &Path, revision: &str) -> Result<Vec<String>, Error> {
        let listed = run(
            dir,
            [
                "rev-list",
                revision,
                "--not",
                "--remotes=origin",
                ORIGIN_REFS,
                "--",
            ],
            "rev-list",
            LOCAL_DEADLINE,
        )?
        .ok("rev-list", dir)?;
        Ok(lines(&listed))
    }

    fn upstream(&self, checkout: &Path, branch: &str) -> Result<Option<String>, Error> {
        let key = format!("branch.{branch}.merge");
        let found = run(
            checkout,
            ["config", "--get", key.as_str()],
            "config",
            LOCAL_DEADLINE,
        )?;
        Ok(found
            .status
            .success()
            .then(|| trimmed(&found.stdout))
            .filter(|merge| !merge.is_empty()))
    }

    fn set_upstream(&self, checkout: &Path, branch: &str, merge: &str) -> Result<(), Error> {
        let remote = format!("branch.{branch}.remote");
        let tracked = format!("branch.{branch}.merge");
        for (key, value) in [(remote.as_str(), "origin"), (tracked.as_str(), merge)] {
            run(checkout, ["config", key, value], "config", LOCAL_DEADLINE)?
                .ok("config", checkout)?;
        }
        Ok(())
    }

    fn repair_checkout(&self, repository: &Path, checkout: &Path) -> Result<(), Error> {
        run(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("repair"),
                checkout.as_os_str(),
            ],
            "worktree repair",
            LOCAL_DEADLINE,
        )?
        .ok("worktree repair", repository)
        .map(drop)
    }

    fn add_detached_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        commit: &str,
    ) -> Result<String, Error> {
        run(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--quiet"),
                OsStr::new("--detach"),
                checkout.as_os_str(),
                OsStr::new(commit),
            ],
            "worktree add",
            CHECKOUT_DEADLINE,
        )?
        .ok("worktree add", repository)?;
        Ok(commit.to_owned())
    }

    fn detach_head(&self, checkout: &Path, commit: &str) -> Result<String, Error> {
        run(
            checkout,
            ["checkout", "--quiet", "--detach", commit, "--"],
            "checkout",
            CHECKOUT_DEADLINE,
        )?
        .ok("checkout", checkout)?;
        Ok(commit.to_owned())
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
