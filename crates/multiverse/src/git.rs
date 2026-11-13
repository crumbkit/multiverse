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
//!   A command on a bare mirror names the mirror with `--git-dir`, because
//!   git refuses to discover a bare repository when its
//!   `safe.bareRepository` setting is `explicit`. The caller says which
//!   commands are on a mirror. mxv does not infer it from the directory's
//!   contents, because a checkout's tracked files could imitate a git
//!   directory.
//! - Environment: the caller's environment, plus `GIT_TERMINAL_PROMPT=0`,
//!   so a fetch that needs a password fails instead of waiting for an
//!   answer that nobody gives, `SSH_ASKPASS_REQUIRE=force` and
//!   `SSH_ASKPASS=false`, so the ssh that git starts fails where it would
//!   have asked a question, as [`ssh`] describes, and `LC_ALL=C`, so the
//!   messages that the errors quote are in English. The commands that write the commits of
//!   an archive's uncommitted work also get `GIT_INDEX_FILE`, a temporary
//!   index under the archive's directory, so the checkout's own index
//!   stays as it is, and `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE`, the
//!   time of the archive's record, so the same work gives the same
//!   commits.
//! - Streams and deadline: standard input is empty, except for a command
//!   that reads a list of commits from it, which could be longer than a
//!   command line may be. The module keeps the
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

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use wait_timeout::ChildExt as _;

use crate::backend::{
    Backend, Changes, Concealed, Head, LargeFile, Listed, LocalNames, RefHolder, Repository,
    StashEntry, WipCommits, WipTrees,
};
use crate::error::Error;
use crate::mirror::redacted;

mod archive;
mod concealed;
mod detached;
mod ssh;
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

/// The pattern of the copies of the heads and tags of a clone's mirror, as
/// the mirror held them when the clone was last counted. Earlier versions of
/// mxv also kept copies of origin's tags and notes here.
const ORIGIN_REFS: &str = "--glob=refs/mxv/origin/*";

/// Returns the lines of `text` that are not empty.
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Returns the mirror that `checkout` shares objects with, read from its
/// alternates file, or `None` when it shares none.
fn shared_mirror(checkout: &Path) -> Option<PathBuf> {
    let objects = checkout.join(".git").join("objects");
    let alternates = fs::read_to_string(objects.join("info").join("alternates")).ok()?;
    alternates
        .lines()
        .map(|line| objects.join(line))
        .find_map(|path| path.parent().map(Path::to_path_buf))
}

/// Copies the heads and tags that the mirror of `checkout` holds into the
/// checkout's `refs/mxv/origin/`, so that the count of unpushed commits
/// treats them as pushed. The mirror holds origin's heads and tags as of
/// its last fetch, and the copy contacts no remote. Copies of refs that the
/// mirror no longer holds are removed.
fn copy_mirror_refs(checkout: &Path) -> Result<(), Error> {
    let Some(mirror) = shared_mirror(checkout) else {
        return Ok(());
    };
    run(
        checkout,
        [
            OsStr::new("fetch"),
            OsStr::new("--quiet"),
            OsStr::new("--no-tags"),
            OsStr::new("--prune"),
            OsStr::new("--no-auto-gc"),
            OsStr::new("--no-write-fetch-head"),
            OsStr::new("--no-recurse-submodules"),
            mirror.as_os_str(),
            OsStr::new("+refs/heads/*:refs/mxv/origin/heads/*"),
            OsStr::new("+refs/tags/*:refs/mxv/origin/tags/*"),
        ],
        "fetch from mirror",
        LOCAL_DEADLINE,
    )?
    .ok("fetch from mirror", checkout)
    .map(drop)
}

/// Returns the git directory of the clone at `checkout`, followed by the
/// git directory of each submodule that the clone has initialised, which
/// git keeps below `.git/modules`, at any depth. A submodule's commits
/// live only in its git directory, so deleting the clone loses them too.
/// A worktree or a submodule has a file named `.git`, so its `.git/modules`
/// is no directory and it adds no submodules here.
fn git_dirs(checkout: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut dirs = vec![checkout.to_path_buf()];
    dirs.extend(module_dirs(&checkout.join(".git").join("modules"))?);
    Ok(dirs)
}

/// Reports whether `dir` is the git directory of a submodule: a directory
/// with a `HEAD` file below a `modules` directory of a git directory.
fn is_module_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file()
        && dir.ancestors().skip(1).any(|ancestor| {
            ancestor.file_name() == Some(OsStr::new("modules"))
                && ancestor
                    .parent()
                    .is_some_and(|parent| parent.join("HEAD").is_file())
        })
}

/// Returns the git directory of each submodule below `modules`, the
/// `modules` directory of a git directory, at any depth.
fn module_dirs(modules: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut dirs = Vec::new();
    let mut pending = vec![modules.to_path_buf()];
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

/// Runs `git -C <mirror> --git-dir=. <args>` as [`run`] does. A bare mirror
/// has no working tree to discover it from, and git refuses that discovery
/// when its `safe.bareRepository` setting is `explicit`, so the command names
/// the mirror as its git directory.
fn run_in_mirror<I, S>(
    mirror: &Path,
    args: I,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args.into_iter().map(|arg| arg.as_ref().to_os_string());
    run(
        mirror,
        std::iter::once(OsString::from("--git-dir=.")).chain(args),
        operation,
        deadline,
    )
}

/// Runs `args` in `holder`, with [`run_in_mirror`] when the holder is a
/// mirror and with [`run`] otherwise.
fn run_in_holder<I, S>(
    holder: RefHolder<'_>,
    args: I,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    match holder {
        RefHolder::Mirror(mirror) => run_in_mirror(mirror, args, operation, deadline),
        RefHolder::Repository(repository) => run(repository, args, operation, deadline),
    }
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
    run_with(dir, args, &[], operation, deadline, kept)
}

/// Runs `git -C <dir> <args>` as [`run_keeping`] does, with each variable
/// of `env` added to its environment.
fn run_with<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, &OsStr)],
    operation: &'static str,
    deadline: Duration,
    kept: u64,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_fed(dir, args, env, None, operation, deadline, kept)
}

/// Runs `git -C <dir> <args>` as [`run`] does, with `input` on its
/// standard input.
fn run_with_input<I, S>(
    dir: &Path,
    args: I,
    input: Vec<u8>,
    operation: &'static str,
    deadline: Duration,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_fed(
        dir,
        args,
        &[],
        Some(input),
        operation,
        deadline,
        MOST_OUTPUT_BYTES,
    )
}

/// Runs `git -C <dir> <args>` as [`run_with`] does, with `input`, when
/// given, written to its standard input.
fn run_fed<I, S>(
    dir: &Path,
    args: I,
    env: &[(&str, &OsStr)],
    input: Option<Vec<u8>>,
    operation: &'static str,
    deadline: Duration,
    kept: u64,
) -> Result<Finished, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = ssh::git_command(dir);
    if is_module_git_dir(dir) {
        // A submodule's git directory names its work tree in
        // `core.worktree`. After `git rm` deletes the submodule, that
        // directory is gone and git refuses to start, so the git
        // directory stands in as the work tree. The commands that read a
        // submodule's git directory read only its references and objects.
        command.arg("--work-tree=.");
    }
    command
        .args(args)
        .envs(env.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    // The input is written on a thread of its own, so a child that fills
    // its output pipe before it reads all of its input cannot block this
    // one. A child that exits early makes the write fail, and its exit
    // status reports why.
    if let Some((mut stream, bytes)) = child.stdin.take().zip(input) {
        thread::spawn(move || {
            let _ = stream.write_all(&bytes);
        });
    }
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
        let fetched = run(
            repository,
            ["fetch", "--quiet", "origin"],
            "fetch origin",
            FETCH_DEADLINE,
        )?;
        if fetched.status.success() {
            return Ok(());
        }
        // The URL is read only after a failure, so a fetch that succeeds
        // costs no second command. A URL that cannot be read leaves git's
        // own failure as it is.
        let url = run(
            repository,
            ["remote", "get-url", "origin"],
            "remote get-url",
            LOCAL_DEADLINE,
        )
        .ok()
        .filter(|origin| origin.status.success())
        .map(|origin| trimmed(&origin.stdout));
        Err(url
            .and_then(|url| ssh::refused(&url, &fetched.stderr))
            .unwrap_or_else(|| fetched.failure("fetch origin", repository)))
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
            run_in_mirror(
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
            run_in_mirror(mirror, ["config", key, value], "config", LOCAL_DEADLINE)?
                .ok("config", mirror)?;
        }
        Ok(())
    }

    fn fetch_mirror(&self, mirror: &Path, url: &str, default: &str) -> Result<(), Error> {
        let fetched = run_in_mirror(
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
            if let Some(refused) = ssh::refused(url, &fetched.stderr) {
                return Err(refused);
            }
            return Err(Error::MirrorFetchFailed {
                origin: redacted(url),
                mirror: mirror.to_path_buf(),
                message: explanation(fetched.status, &fetched.stderr),
            });
        }
        let head = format!("refs/heads/{default}");
        run_in_mirror(
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

    fn unpushed_commits(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        let mut unpushed = Vec::new();
        copy_mirror_refs(checkout)?;
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
            let detached = detached::detached_commits(&dir)?;
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
        archive::write_ref(repository, reference, commit)
    }

    fn fetch_into_mirror(
        &self,
        mirror: &Path,
        checkout: &Path,
        refs: &[(String, String)],
    ) -> Result<(), Error> {
        archive::fetch_into_mirror(mirror, checkout, refs)
    }

    fn refs_under(
        &self,
        holder: RefHolder<'_>,
        prefix: &str,
    ) -> Result<Vec<(String, String)>, Error> {
        archive::refs_under(holder, prefix)
    }

    fn stash_entries(&self, checkout: &Path) -> Result<Vec<StashEntry>, Error> {
        archive::stash_entries(checkout)
    }

    fn orphans(
        &self,
        checkout: &Path,
        kept: &[String],
        shared: bool,
    ) -> Result<Vec<String>, Error> {
        archive::orphans(checkout, kept, shared)
    }

    fn large_files(&self, checkout: &Path, most: u64) -> Result<Vec<LargeFile>, Error> {
        concealed::large_files(checkout, most)
    }

    fn wip_trees(
        &self,
        checkout: &Path,
        index: &Path,
        left_out: &[String],
    ) -> Result<WipTrees, Error> {
        archive::wip_trees(checkout, index, left_out)
    }

    fn write_wip(
        &self,
        checkout: &Path,
        index: &Path,
        left_out: &[String],
        time: u64,
        message: &str,
    ) -> Result<WipCommits, Error> {
        archive::write_wip(checkout, index, left_out, time, message)
    }

    fn tree_of(&self, dir: &Path, commit: &str) -> Result<String, Error> {
        archive::tree_of(dir, commit)
    }

    fn apply_wip(&self, checkout: &Path, wip: &WipCommits) -> Result<(), Error> {
        archive::apply_wip(checkout, wip)
    }

    fn store_stash(&self, checkout: &Path, commit: &str, message: &str) -> Result<(), Error> {
        archive::store_stash(checkout, commit, message)
    }

    fn create_ref(&self, dir: &Path, reference: &str, object: &str) -> Result<bool, Error> {
        archive::create_ref(dir, reference, object)
    }

    fn submodule_paths(&self, checkout: &Path) -> Result<Vec<String>, Error> {
        concealed::submodule_paths(checkout)
    }

    fn delete_ref(&self, holder: RefHolder<'_>, reference: &str) -> Result<(), Error> {
        archive::delete_ref(holder, reference)
    }

    fn resolve_commit(
        &self,
        holder: RefHolder<'_>,
        revision: &str,
    ) -> Result<Option<String>, Error> {
        archive::resolve_commit(holder, revision)
    }

    fn concealed(&self, checkout: &Path) -> Result<Concealed, Error> {
        concealed::concealed(checkout)
    }

    fn local_names(&self, checkout: &Path, source: &Path) -> Result<LocalNames, Error> {
        concealed::local_names(checkout, source)
    }

    fn remove_origin(&self, checkout: &Path) -> Result<(), Error> {
        archive::remove_origin(checkout)
    }

    fn local_only_commits(&self, dir: &Path, revision: &str) -> Result<Vec<String>, Error> {
        archive::local_only_commits(dir, revision)
    }

    fn upstream(&self, checkout: &Path, branch: &str) -> Result<Option<String>, Error> {
        archive::upstream(checkout, branch)
    }

    fn upstream_remote(&self, checkout: &Path, branch: &str) -> Result<Option<String>, Error> {
        archive::upstream_remote(checkout, branch)
    }

    fn set_upstream(
        &self,
        checkout: &Path,
        branch: &str,
        remote: &str,
        merge: &str,
    ) -> Result<(), Error> {
        archive::set_upstream(checkout, branch, remote, merge)
    }

    fn repair_checkout(&self, repository: &Path, checkout: &Path) -> Result<(), Error> {
        archive::repair_checkout(repository, checkout)
    }

    fn add_detached_checkout(
        &self,
        repository: &Path,
        checkout: &Path,
        commit: &str,
    ) -> Result<String, Error> {
        archive::add_detached_checkout(repository, checkout, commit)
    }

    fn detach_head(&self, checkout: &Path, commit: &str) -> Result<String, Error> {
        archive::detach_head(checkout, commit)
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
