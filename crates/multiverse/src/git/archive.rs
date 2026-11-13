//! The git commands that only `mxv archive` and `mxv restore` run.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    CHECKOUT_DEADLINE, FETCH_DEADLINE, LOCAL_DEADLINE, MOST_OUTPUT_BYTES, ORIGIN_REFS, concealed,
    detached, lines, run, run_in_holder, run_in_mirror, run_with, run_with_input, trimmed,
};
use crate::backend::{RefHolder, StashEntry, WipCommits, WipTrees};
use crate::error::Error;

/// The configuration that makes git sync references and loose objects before
/// it reports success, so a pin survives a power cut. The components add to
/// git's default, which syncs packs and leaves out both of these. The
/// components are named because git documents the aggregate `committed` as
/// covering only objects.
const DURABLE: &str = "core.fsync=reference,loose-object";

/// Points `reference` in `repository` at `commit`, durably.
pub(super) fn write_ref(repository: &Path, reference: &str, commit: &str) -> Result<(), Error> {
    run(
        repository,
        ["-c", DURABLE, "update-ref", reference, commit],
        "update-ref",
        LOCAL_DEADLINE,
    )?
    .ok("update-ref", repository)
    .map(drop)
}

/// Fetches each object of `refs` from `checkout` into its reference of
/// `mirror`, durably, and checks that each reference then names its
/// object. A fetch from a local repository may name an object that no
/// reference of that repository names.
pub(super) fn fetch_into_mirror(
    mirror: &Path,
    checkout: &Path,
    refs: &[(String, String)],
) -> Result<(), Error> {
    if refs.is_empty() {
        return Ok(());
    }
    let mut args: Vec<OsString> = [
        "-c",
        DURABLE,
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-auto-gc",
        "--no-write-fetch-head",
        "--no-recurse-submodules",
    ]
    .map(OsString::from)
    .to_vec();
    args.push(checkout.as_os_str().to_owned());
    args.extend(
        refs.iter()
            .map(|(reference, object)| OsString::from(format!("+{object}:{reference}"))),
    );
    run_in_mirror(mirror, args, "fetch into mirror", FETCH_DEADLINE)?
        .ok("fetch into mirror", mirror)?;
    let mut args = vec!["rev-parse".to_owned()];
    args.extend(refs.iter().map(|(reference, _)| reference.clone()));
    let named =
        lines(&run_in_mirror(mirror, args, "rev-parse", LOCAL_DEADLINE)?.ok("rev-parse", mirror)?);
    for (index, (reference, object)) in refs.iter().enumerate() {
        let found = named.get(index).map_or("", String::as_str);
        if found != object {
            return Err(Error::CommandFailed {
                operation: "fetch into mirror",
                dir: mirror.to_path_buf(),
                code: None,
                message: format!(
                    "the pin {reference} names {found:?}, and the checkout held {object}"
                ),
            });
        }
    }
    Ok(())
}

/// Returns each reference below `prefix` in the repository that `holder`
/// names, without the prefix, with the object that it names.
pub(super) fn refs_under(
    holder: RefHolder<'_>,
    prefix: &str,
) -> Result<Vec<(String, String)>, Error> {
    let listed = run_in_holder(
        holder,
        ["for-each-ref", "--format=%(objectname) %(refname)", prefix],
        "for-each-ref",
        LOCAL_DEADLINE,
    )?
    .ok("for-each-ref", holder.path())?;
    Ok(lines(&listed)
        .iter()
        .filter_map(|line| {
            let (object, name) = line.split_once(' ')?;
            Some((name.strip_prefix(prefix)?.to_owned(), object.to_owned()))
        })
        .collect())
}

/// Returns each stash entry of `checkout`, newest first.
pub(super) fn stash_entries(checkout: &Path) -> Result<Vec<StashEntry>, Error> {
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
        [
            "log",
            "--no-show-signature",
            "--walk-reflogs",
            "--format=%H%x00%gs",
            "refs/stash",
            "--",
        ],
        "log -g",
        LOCAL_DEADLINE,
    )?
    .ok("log -g", checkout)?;
    Ok(lines(&listed)
        .iter()
        .filter_map(|line| {
            let (commit, message) = line.split_once('\0')?;
            Some(StashEntry {
                commit: commit.to_owned(),
                message: message.to_owned(),
            })
        })
        .collect())
}

/// Returns the commits of `checkout` that `origin` does not hold and that
/// neither `HEAD` nor `kept` reaches, each one that no other of them
/// reaches, as [`Backend::orphans`](crate::backend::Backend::orphans)
/// describes.
pub(super) fn orphans(
    checkout: &Path,
    kept: &[String],
    shared: bool,
) -> Result<Vec<String>, Error> {
    let mut held: Vec<String> = vec!["--not".to_owned(), "HEAD".to_owned()];
    held.extend(kept.iter().cloned());
    let mut found = Vec::new();
    if !shared {
        // Every reference counts except the copies of other repositories'
        // branches. A stash, a branch, a tag and a per-worktree reference
        // are in `kept`, so what is left is what notes and other references
        // such as `refs/backup/x` keep.
        let mut args: Vec<String> = [
            "rev-list",
            "--exclude=refs/remotes/*",
            "--exclude=refs/prefetch/*",
            "--exclude=refs/mxv/origin/*",
            "--all",
        ]
        .map(str::to_owned)
        .to_vec();
        args.extend(held.iter().cloned());
        args.extend(["--remotes=origin", ORIGIN_REFS, "--"].map(str::to_owned));
        found.extend(lines(
            &run(checkout, &args, "rev-list", LOCAL_DEADLINE)?.ok("rev-list", checkout)?,
        ));
    }
    let detached = detached::detached_commits(checkout)?;
    if !detached.is_empty() {
        let mut args: Vec<String> = vec!["rev-list".to_owned(), "--ignore-missing".to_owned()];
        args.extend(detached.iter().cloned());
        args.extend(held.iter().cloned());
        args.push("--".to_owned());
        let unreached =
            lines(&run(checkout, &args, "rev-list", LOCAL_DEADLINE)?.ok("rev-list", checkout)?);
        for commit in detached {
            if unreached.contains(&commit) && !found.contains(&commit) {
                found.push(commit);
            }
        }
    }
    if found.len() > 1 {
        // Each commit that no other found commit reaches is one that the
        // parents of the found commits do not reach. The list goes on
        // standard input, because a long local history would not fit on a
        // command line.
        let mut input = String::new();
        for commit in &found {
            input.push_str(commit);
            input.push_str("\n^");
            input.push_str(commit);
            input.push_str("^@\n");
        }
        found = lines(
            &run_with_input(
                checkout,
                ["rev-list", "--stdin"],
                input.into_bytes(),
                "rev-list",
                LOCAL_DEADLINE,
            )?
            .ok("rev-list", checkout)?,
        );
    }
    found.sort_unstable();
    Ok(found)
}

/// Returns the path of the index file of `checkout`.
fn index_file(checkout: &Path) -> Result<PathBuf, Error> {
    let path = run(
        checkout,
        ["rev-parse", "--git-path", "index"],
        "rev-parse",
        LOCAL_DEADLINE,
    )?
    .ok("rev-parse", checkout)?;
    // git prints the path relative to the directory it ran in, unless it
    // is absolute.
    Ok(checkout.join(trimmed(&path)))
}

/// Runs `args` in `checkout` with `index` as git's index file, and returns
/// what it printed.
fn with_index(
    checkout: &Path,
    index: &Path,
    args: &[&str],
    operation: &'static str,
    deadline: Duration,
) -> Result<String, Error> {
    run_with(
        checkout,
        args,
        &[("GIT_INDEX_FILE", index.as_os_str())],
        operation,
        deadline,
        MOST_OUTPUT_BYTES,
    )?
    .ok(operation, checkout)
}

/// Returns each path of `checkout` that `index` holds unmerged, once.
fn unmerged_paths(checkout: &Path, index: &Path) -> Result<Vec<String>, Error> {
    let listed = with_index(
        checkout,
        index,
        &["ls-files", "--unmerged", "-z"],
        "ls-files",
        LOCAL_DEADLINE,
    )?;
    let mut paths: Vec<String> = listed
        .split('\0')
        .filter_map(|entry| entry.split_once('\t').map(|(_, path)| path.to_owned()))
        .collect();
    paths.dedup();
    Ok(paths)
}

/// Sets each of `paths` in `index` to its entry in `HEAD`, or takes it out
/// when `HEAD` lacks it.
fn reset_to_head(checkout: &Path, index: &Path, paths: &[String]) -> Result<(), Error> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut args = vec!["--literal-pathspecs", "reset", "--quiet", "HEAD", "--"];
    args.extend(paths.iter().map(String::as_str));
    with_index(checkout, index, &args, "reset", LOCAL_DEADLINE).map(drop)
}

/// Returns the trees of the index and the working files of `checkout`,
/// written through `index`, which starts as a copy of the checkout's own
/// index file and is deleted afterwards. Each of `left_out` keeps its
/// entry in `HEAD` in both trees, so its uncommitted content is not
/// written. A path that a paused merge, rebase, cherry-pick or revert left
/// unmerged takes its entry in `HEAD` in the tree of the index, and its
/// working file, conflict markers included, in the tree of the working
/// files. The objects are synced before git reports success.
pub(super) fn wip_trees(
    checkout: &Path,
    index: &Path,
    left_out: &[String],
) -> Result<WipTrees, Error> {
    let own = index_file(checkout)?;
    match fs::remove_file(index) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(Error::Unwritable {
                path: index.to_path_buf(),
                source,
            });
        }
    }
    // A checkout without an index file has nothing staged.
    if own.is_file() {
        crate::store::copy_file(&own, index, 0o600)?;
    }
    // A repository inside the checkout that git does not track would be
    // added as a commit id without its files. The archive refuses it, and
    // with `--force` loses it, so the commit of the working files leaves it
    // out. A pathspec on `add` itself would make git refuse a checkout with
    // skip-worktree entries, so the commit ids are taken out afterwards.
    let nested = concealed::nested(checkout)?;
    let mut unstage = vec![
        "--literal-pathspecs",
        "rm",
        "--cached",
        "--force",
        "-r",
        "--quiet",
        "--ignore-unmatch",
        "--",
    ];
    unstage.extend(nested.iter().map(|path| path.trim_end_matches('/')));
    let read = || -> Result<WipTrees, Error> {
        // write-tree refuses an index with unmerged entries.
        let mut back = unmerged_paths(checkout, index)?;
        back.extend(left_out.iter().cloned());
        reset_to_head(checkout, index, &back)?;
        let staged = with_index(
            checkout,
            index,
            &["-c", DURABLE, "write-tree"],
            "write-tree",
            LOCAL_DEADLINE,
        )?;
        with_index(
            checkout,
            index,
            &["-c", DURABLE, "add", "--all"],
            "add",
            CHECKOUT_DEADLINE,
        )?;
        if !nested.is_empty() {
            with_index(checkout, index, &unstage, "rm", CHECKOUT_DEADLINE)?;
        }
        reset_to_head(checkout, index, left_out)?;
        let worktree = with_index(
            checkout,
            index,
            &["-c", DURABLE, "write-tree"],
            "write-tree",
            LOCAL_DEADLINE,
        )?;
        Ok(WipTrees {
            index: trimmed(&staged),
            worktree: trimmed(&worktree),
        })
    };
    let trees = read();
    // The copy holds nothing that the trees do not, so a failure to delete
    // it leaves only a file that the next run replaces.
    let _ = fs::remove_file(index);
    trees
}

/// Writes the commit of `tree` with `parents` and `message` in
/// `checkout`, dated `time`, and returns it. `identity` gives the commit an
/// author and a committer when git has none.
fn commit_tree(
    checkout: &Path,
    identity: &[String],
    tree: &str,
    parents: &[&str],
    message: &str,
    time: u64,
) -> Result<String, Error> {
    let mut args: Vec<String> = identity.to_vec();
    // A signature can wait for a passphrase that nobody is there to give,
    // and a signed commit would differ on a rerun.
    // The commit is synced, because for a worktree nothing copies it
    // before the checkout is deleted.
    args.extend(
        [
            "-c",
            "commit.gpgsign=false",
            "-c",
            DURABLE,
            "commit-tree",
            tree,
        ]
        .map(str::to_owned),
    );
    for parent in parents {
        args.extend(["-p".to_owned(), (*parent).to_owned()]);
    }
    args.extend(["-m".to_owned(), message.to_owned()]);
    let date = format!("@{time} +0000");
    let written = run_with(
        checkout,
        &args,
        &[
            ("GIT_AUTHOR_DATE", OsStr::new(&date)),
            ("GIT_COMMITTER_DATE", OsStr::new(&date)),
        ],
        "commit-tree",
        LOCAL_DEADLINE,
        MOST_OUTPUT_BYTES,
    )?
    .ok("commit-tree", checkout)?;
    Ok(trimmed(&written))
}

/// Writes the commits of the uncommitted work of `checkout`, as
/// [`Backend::write_wip`](crate::backend::Backend::write_wip) describes.
pub(super) fn write_wip(
    checkout: &Path,
    index: &Path,
    left_out: &[String],
    time: u64,
    message: &str,
) -> Result<WipCommits, Error> {
    let trees = wip_trees(checkout, index, left_out)?;
    let head = trimmed(
        &run(
            checkout,
            ["rev-parse", "--verify", "HEAD"],
            "rev-parse",
            LOCAL_DEADLINE,
        )?
        .ok("rev-parse", checkout)?,
    );
    let identity = concealed::identity(checkout)?;
    let staged = commit_tree(checkout, &identity, &trees.index, &[&head], message, time)?;
    let worktree = commit_tree(
        checkout,
        &identity,
        &trees.worktree,
        &[&head, &staged],
        message,
        time,
    )?;
    Ok(WipCommits {
        index: staged,
        worktree,
    })
}

/// Returns the tree of `commit` in the repository at `dir`.
pub(super) fn tree_of(dir: &Path, commit: &str) -> Result<String, Error> {
    let target = format!("{commit}^{{tree}}");
    let tree = run(
        dir,
        ["rev-parse", "--verify", "--quiet", target.as_str()],
        "rev-parse",
        LOCAL_DEADLINE,
    )?
    .ok("rev-parse", dir)?;
    Ok(trimmed(&tree))
}

/// Puts the work of `wip` back into the clean checkout at `checkout`.
pub(super) fn apply_wip(checkout: &Path, wip: &WipCommits) -> Result<(), Error> {
    // The first read writes the working files and the index of the commit
    // of the working files, and the second puts back the index alone, so a
    // file that was only in the working files reads as unstaged or
    // untracked again.
    run(
        checkout,
        ["read-tree", "-u", "--reset", wip.worktree.as_str()],
        "read-tree",
        CHECKOUT_DEADLINE,
    )?
    .ok("read-tree", checkout)?;
    run(
        checkout,
        ["read-tree", wip.index.as_str()],
        "read-tree",
        LOCAL_DEADLINE,
    )?
    .ok("read-tree", checkout)
    .map(drop)
}

/// Adds `commit` to the stash of `checkout` as its newest entry.
pub(super) fn store_stash(checkout: &Path, commit: &str, message: &str) -> Result<(), Error> {
    run(
        checkout,
        ["stash", "store", "--quiet", "-m", message, commit],
        "stash store",
        LOCAL_DEADLINE,
    )?
    .ok("stash store", checkout)
    .map(drop)
}

/// Creates `reference` at `object` in `dir` unless it exists, and reports
/// whether it created it.
pub(super) fn create_ref(dir: &Path, reference: &str, object: &str) -> Result<bool, Error> {
    // An empty old value makes `update-ref` refuse when the reference
    // already exists, so the check and the creation are one step.
    let created = run(
        dir,
        ["-c", DURABLE, "update-ref", reference, object, ""],
        "update-ref",
        LOCAL_DEADLINE,
    )?;
    if created.status.success() {
        return Ok(true);
    }
    let exists = run(
        dir,
        ["rev-parse", "--verify", "--quiet", reference],
        "rev-parse",
        LOCAL_DEADLINE,
    )?;
    if exists.status.success() {
        return Ok(false);
    }
    Err(created.failure("update-ref", dir))
}

/// Deletes `reference` of the repository that `holder` names.
pub(super) fn delete_ref(holder: RefHolder<'_>, reference: &str) -> Result<(), Error> {
    run_in_holder(
        holder,
        ["update-ref", "-d", reference],
        "update-ref -d",
        LOCAL_DEADLINE,
    )?
    .ok("update-ref -d", holder.path())
    .map(drop)
}

/// Returns the commit that `revision` names in the repository that
/// `holder` names, or `None` when it names none.
pub(super) fn resolve_commit(
    holder: RefHolder<'_>,
    revision: &str,
) -> Result<Option<String>, Error> {
    let target = format!("{revision}^{{commit}}");
    let found = run_in_holder(
        holder,
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

/// Deletes the remote `origin` of `checkout`.
pub(super) fn remove_origin(checkout: &Path) -> Result<(), Error> {
    run(
        checkout,
        ["remote", "remove", "origin"],
        "remote remove",
        LOCAL_DEADLINE,
    )?
    .ok("remote remove", checkout)
    .map(drop)
}

/// Returns the commits of `revision` in `dir` that `origin` does not hold.
pub(super) fn local_only_commits(dir: &Path, revision: &str) -> Result<Vec<String>, Error> {
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

/// Returns the value of the configuration key `key` in `checkout`, or
/// `None` when it has none.
fn config_value(checkout: &Path, key: &str) -> Result<Option<String>, Error> {
    let found = run(checkout, ["config", "--get", key], "config", LOCAL_DEADLINE)?;
    Ok(found
        .status
        .success()
        .then(|| trimmed(&found.stdout))
        .filter(|value| !value.is_empty()))
}

/// Returns the remote branch that `branch` of `checkout` tracks.
pub(super) fn upstream(checkout: &Path, branch: &str) -> Result<Option<String>, Error> {
    config_value(checkout, &format!("branch.{branch}.merge"))
}

/// Returns the remote that `branch` of `checkout` tracks a branch of.
pub(super) fn upstream_remote(checkout: &Path, branch: &str) -> Result<Option<String>, Error> {
    config_value(checkout, &format!("branch.{branch}.remote"))
}

/// Makes `branch` of `checkout` track the branch `merge` of `remote`.
pub(super) fn set_upstream(
    checkout: &Path,
    branch: &str,
    remote: &str,
    merge: &str,
) -> Result<(), Error> {
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    for (key, value) in [(remote_key.as_str(), remote), (merge_key.as_str(), merge)] {
        run(checkout, ["config", key, value], "config", LOCAL_DEADLINE)?.ok("config", checkout)?;
    }
    Ok(())
}

/// Points the entry of `repository` for the worktree at `checkout` at that
/// directory again.
pub(super) fn repair_checkout(repository: &Path, checkout: &Path) -> Result<(), Error> {
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

/// Makes a worktree of `repository` at `checkout`, detached at `commit`.
pub(super) fn add_detached_checkout(
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

/// Detaches the `HEAD` of `checkout` at `commit`.
pub(super) fn detach_head(checkout: &Path, commit: &str) -> Result<String, Error> {
    run(
        checkout,
        ["checkout", "--quiet", "--detach", commit, "--"],
        "checkout",
        CHECKOUT_DEADLINE,
    )?
    .ok("checkout", checkout)?;
    Ok(commit.to_owned())
}
