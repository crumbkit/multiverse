//! The git commands that only `mxv archive` and `mxv restore` run.

use std::ffi::OsStr;
use std::path::Path;

use super::{
    CHECKOUT_DEADLINE, FETCH_DEADLINE, LOCAL_DEADLINE, ORIGIN_REFS, concealed, lines, run, trimmed,
};
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

/// Fetches the `HEAD` of `checkout` into `reference` of `mirror`, durably,
/// and returns the commit that the reference names.
pub(super) fn fetch_head_into_mirror(
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

/// Deletes `reference` of `repository`.
pub(super) fn delete_ref(repository: &Path, reference: &str) -> Result<(), Error> {
    run(
        repository,
        ["update-ref", "-d", reference],
        "update-ref -d",
        LOCAL_DEADLINE,
    )?
    .ok("update-ref -d", repository)
    .map(drop)
}

/// Returns the commit that `revision` names in `repository`, or `None`
/// when it names none.
pub(super) fn resolve_commit(repository: &Path, revision: &str) -> Result<Option<String>, Error> {
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

/// Commits everything in `checkout` with `message`.
pub(super) fn commit_all(checkout: &Path, message: &str) -> Result<(), Error> {
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
