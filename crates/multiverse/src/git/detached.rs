//! Finding the commits that only the reflog of a detached `HEAD` holds.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use wait_timeout::ChildExt as _;

use super::{
    Finished, GIT, LOCAL_DEADLINE, MOST_OUTPUT_BYTES, ORIGIN_REFS, drain, joined, lines, run,
};
use crate::error::Error;

/// How far before the oldest detached commit [`detached_commits`] looks for
/// a copy of it, in seconds, so a clock that was a little wrong when the
/// copy was made does not hide it.
const COPY_MARGIN: i64 = 24 * 60 * 60;

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
pub(super) fn detached_commits(dir: &Path) -> Result<Vec<String>, Error> {
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
fn patch_ids(dir: &Path, revisions: &[String]) -> Result<HashMap<String, String>, Error> {
    let git = || {
        #[expect(
            clippy::disallowed_methods,
            reason = "crates/multiverse/src/git/detached.rs belongs to the module that starts processes (PROC-1)"
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
        reason = "crates/multiverse/src/git/detached.rs belongs to the module that starts processes (PROC-1)"
    )]
    let spawned = log.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/git/detached.rs holds the child and owns its deadline and cleanup (PROC-1)"
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
        reason = "crates/multiverse/src/git/detached.rs belongs to the module that starts processes (PROC-1)"
    )]
    let spawned = patch_id.spawn();
    #[expect(
        clippy::disallowed_types,
        reason = "crates/multiverse/src/git/detached.rs holds the child and owns its deadline and cleanup (PROC-1)"
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
