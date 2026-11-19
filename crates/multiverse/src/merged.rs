//! Whether a workspace's change has merged, and the board mounts to
//! advance when it has.
//!
//! A checkout's branch counts as merged when its commits are all in the
//! branch that it started from, after a fetch of origin, which a merge
//! commit or a fast-forward gives, or when GitHub holds a merged pull
//! request whose head is the checkout's `HEAD`, which a squash merge gives.
//! A workspace counts as merged when every checkout that holds commits of
//! its own is merged, no checkout holds uncommitted changes or stashes, and
//! at least one checkout holds a commit of its own.

use std::path::Path;
use std::time::Duration;

use crate::backend::Backend;
use crate::error::Error;
use crate::process::{Ran, run_command};
use crate::record::{BoardMount, Workspace, WorkspaceState};
use crate::workspaces::{Workspaces, default_branch};

#[cfg(test)]
mod tests;

/// How long `gh` and `board` may run before mxv stops them: 30 seconds.
pub const HELPER_DEADLINE: Duration = Duration::from_secs(30);

/// A source that says whether a branch's pull request merged.
pub trait PullRequests {
    /// Returns whether a merged pull request of the repository that the
    /// checkout in `checkout` belongs to has the branch `branch` with the
    /// head commit `head`, and merged into origin's branch `base`, such as
    /// `main`. A source that cannot tell answers `false`.
    fn merged(&self, checkout: &Path, branch: &str, base: &str, head: &str) -> bool;
}

/// The pull requests of GitHub, which the `gh` program lists.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitHub;

impl PullRequests for GitHub {
    fn merged(&self, checkout: &Path, branch: &str, base: &str, head: &str) -> bool {
        let words: Vec<String> = [
            "gh",
            "pr",
            "list",
            "--head",
            branch,
            "--base",
            base,
            "--state",
            "merged",
            "--json",
            "headRefOid",
            "--limit",
            "20",
        ]
        .iter()
        .map(|word| (*word).to_owned())
        .collect();
        let Ran::Exited {
            success: true,
            stdout,
        } = run_command(&words, checkout, HELPER_DEADLINE, true)
        else {
            return false;
        };
        let Ok(listed) = serde_json::from_slice::<Vec<serde_json::Value>>(&stdout) else {
            return false;
        };
        listed
            .iter()
            .any(|pull| pull.get("headRefOid").and_then(serde_json::Value::as_str) == Some(head))
    }
}

/// Whether a workspace's change has merged.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// Every checkout that holds commits of its own is merged.
    Merged,
    /// The workspace stays, for the reason in a sentence.
    Kept(String),
}

impl<B: Backend> Workspaces<B> {
    /// Decides whether the change of `workspace` has merged, fetching
    /// origin into each checkout that holds commits of its own and asking
    /// `pulls` about a branch that origin's base does not hold.
    ///
    /// # Errors
    ///
    /// Returns the failure of reading a checkout.
    pub fn merge_verdict(
        &self,
        workspace: &Workspace,
        pulls: &dyn PullRequests,
    ) -> Result<Verdict, Error> {
        if workspace.state() != WorkspaceState::Live {
            return Ok(Verdict::Kept("it is not a live workspace".to_owned()));
        }
        let mut merged_any = false;
        for checkout in &workspace.checkouts {
            let folder = &checkout.name;
            let head = self.backend.head(&checkout.path)?;
            let (Some(commit), Some(branch)) = (head.commit, head.branch) else {
                return Ok(Verdict::Kept(format!(
                    "the checkout {folder} has no branch or no commit checked out"
                )));
            };
            if !self.backend.changes(&checkout.path)?.changed.is_empty() {
                return Ok(Verdict::Kept(format!(
                    "the checkout {folder} has uncommitted changes"
                )));
            }
            if !self.backend.stashes(&checkout.path)?.is_empty() {
                return Ok(Verdict::Kept(format!("the checkout {folder} has stashes")));
            }
            if checkout.commit.as_deref() == Some(commit.as_str()) {
                // No commit of its own, so there is no change to merge.
                continue;
            }
            if let Err(error) = self.backend.fetch(&checkout.path) {
                return Ok(Verdict::Kept(format!(
                    "mxv could not fetch origin in {folder}: {error}"
                )));
            }
            let unmerged = self
                .backend
                .unmerged_commits(&checkout.path, "HEAD", &checkout.base)?;
            // A branch that origin's base holds counts as merged only when
            // origin also held the branch under its own name, so a branch
            // that nobody published and that only moved along its base,
            // such as after `git pull`, is not taken for finished work.
            let published = self.backend.unmerged_commits(
                &checkout.path,
                "HEAD",
                &format!("refs/remotes/origin/{branch}"),
            )? == Some(0);
            if !(unmerged == Some(0) && published)
                && !pulls.merged(
                    &checkout.path,
                    &branch,
                    &default_branch(&checkout.base),
                    &commit,
                )
            {
                return Ok(Verdict::Kept(format!(
                    "the branch {branch} of {folder} is not merged"
                )));
            }
            merged_any = true;
        }
        Ok(if merged_any {
            Verdict::Merged
        } else {
            Verdict::Kept("no checkout holds a commit of its own".to_owned())
        })
    }
}

impl<B: Backend> Workspaces<B> {
    /// Records that `workspace` follows the board mounts `mounts`, which
    /// the sweep advances when it archives the workspace, and returns the
    /// workspace as the record now holds it.
    ///
    /// # Errors
    ///
    /// Returns the failure of writing the record.
    pub fn record_board_mounts(
        &self,
        workspace: &Workspace,
        mounts: Vec<BoardMount>,
    ) -> Result<Workspace, Error> {
        let mut updated = workspace.clone();
        updated.board_mounts = mounts;
        self.store.update(&mut updated)?;
        Ok(updated)
    }
}

/// Advances the board mount `mount` to the commit that it follows, with
/// `board -C <board> mount update <path>`.
///
/// # Errors
///
/// Returns a sentence that says why the update did not happen, such as a
/// board program that is not installed or a command that exited with an
/// error. The sentence holds no output of the program.
pub fn update_board_mount(mount: &BoardMount) -> Result<(), String> {
    let words: Vec<String> = [
        "board",
        "-C",
        mount.board.to_string_lossy().as_ref(),
        "mount",
        "update",
        mount.path.as_str(),
    ]
    .iter()
    .map(|word| (*word).to_owned())
    .collect();
    match run_command(&words, &mount.board, HELPER_DEADLINE, false) {
        Ran::Exited { success: true, .. } => Ok(()),
        Ran::Exited { success: false, .. } => Err(format!(
            "`board -C {} mount update {}` exited with an error",
            mount.board.display(),
            mount.path
        )),
        Ran::Unstartable => Err("the board program could not be started".to_owned()),
        Ran::TimedOut => Err(format!(
            "`board -C {} mount update {}` did not finish in {} seconds",
            mount.board.display(),
            mount.path,
            HELPER_DEADLINE.as_secs()
        )),
    }
}
