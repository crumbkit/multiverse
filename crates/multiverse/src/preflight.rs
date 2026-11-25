//! The credential preflight: it runs the `check` command of each secret
//! that a manifest declares and reports every secret that fails, with the
//! command that signs in.
//!
//! A check runs the real command that uses the credential, such as `aws
//! sts get-caller-identity` or an `op read` of one item, because a status
//! command such as `op whoami` can say yes while a read fails. The check's
//! standard streams go to nowhere, so the value a read command prints
//! never reaches mxv, and the report holds only names, outcomes and the
//! sign-in command lines that the manifest declares.
//!
//! A check is a command that a repository's manifest declares, so mxv runs
//! it only after a person approved that exact command line at a terminal.
//! [`Approvals`] keeps the approved command lines in mxv's state directory,
//! and a check that no approval covers reports [`Outcome::NotApproved`]
//! without running.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::Error;
use crate::manifest::Secret;
use crate::process::{Ran, run_command};
use crate::store::{self, write_file_with};

#[cfg(test)]
mod tests;

/// How long a check may run before mxv stops it: 20 seconds. A check that
/// is still running then counts as missing, because it is most likely
/// waiting for a prompt.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// What one check showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The command exited with 0.
    Works,
    /// The command exited with another code or a signal.
    Failed,
    /// The program could not be started.
    Unrunnable,
    /// The command was still running at the timeout, and mxv killed it.
    TimedOut,
    /// No person approved the command line, so mxv did not run it.
    NotApproved,
}

/// The check command lines that a person approved, kept one per line as a
/// JSON object of a scope and the command's words in a file of mxv's state
/// directory. The scope names the repositories that the workspace holds, so
/// an approval covers every workspace of those repositories that declares
/// the same command line, and no other repository, whose files the command
/// may read.
#[derive(Clone, Debug)]
pub struct Approvals {
    /// The file, or `None` when mxv has no state directory, in which case
    /// nothing is approved.
    path: Option<PathBuf>,
    scope: String,
}

impl Approvals {
    /// Returns the approvals that `state`, mxv's state directory, holds for
    /// `scope`, which names the repositories of a workspace.
    #[must_use]
    pub fn in_state(state: &Path, scope: &str) -> Self {
        Self {
            path: Some(state.join("preflight-approved")),
            scope: scope.to_owned(),
        }
    }

    /// Returns approvals that cover nothing, for when mxv has no state
    /// directory.
    #[must_use]
    pub fn none() -> Self {
        Self {
            path: None,
            scope: String::new(),
        }
    }

    /// Reads the approved lines. A missing file holds none.
    fn lines(&self) -> Result<Vec<String>, Error> {
        let Some(path) = &self.path else {
            return Ok(Vec::new());
        };
        match store::read_to_string(path) {
            Ok(text) => Ok(text.lines().map(str::to_owned).collect()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(Error::StateUnreadable {
                path: path.clone(),
                source,
            }),
        }
    }

    fn line(&self, secret: &Secret) -> String {
        serde_json::json!({"scope": self.scope, "check": secret.check.words()}).to_string()
    }

    /// Reports whether a person approved the check command line of `secret`
    /// for this scope. A file that cannot be read approves nothing.
    #[must_use]
    pub fn covers(&self, secret: &Secret) -> bool {
        let line = self.line(secret);
        self.lines().is_ok_and(|lines| lines.contains(&line))
    }

    /// Adds the check command lines of `secrets`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::StateUnreadable`] when the file exists and cannot
    /// be read, which keeps the earlier approvals, and [`Error::Unwritable`]
    /// when it cannot be written or mxv has no state directory.
    pub fn approve(&self, secrets: &[&Secret]) -> Result<(), Error> {
        use std::io::Write as _;
        let Some(path) = &self.path else {
            return Err(Error::Unwritable {
                path: PathBuf::from("preflight-approved"),
                source: std::io::Error::other("mxv has no state directory"),
            });
        };
        let mut lines = self.lines()?;
        for secret in secrets {
            let line = self.line(secret);
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Unwritable {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        write_file_with(path, 0o600, |file| {
            for line in &lines {
                writeln!(file, "{line}").map_err(|source| Error::Unwritable {
                    path: path.clone(),
                    source,
                })?;
            }
            Ok(())
        })
    }
}

/// One secret with what its check showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// The secret's name.
    pub name: String,
    /// The command line that signs in.
    pub sign_in: String,
    /// What the check showed.
    pub outcome: Outcome,
}

/// The result of checking every secret of a workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preflight {
    /// Each secret, in name order.
    pub checked: Vec<Checked>,
}

impl Preflight {
    /// Runs the check of each of `secrets`, each in the directory beside
    /// its name, and returns what they showed. Each check may run for
    /// `timeout`, and a check that `approvals` does not cover does not run.
    #[must_use]
    pub fn run(
        secrets: &[(String, Secret, PathBuf)],
        approvals: &Approvals,
        timeout: Duration,
    ) -> Self {
        let checked = secrets
            .iter()
            .map(|(name, secret, dir)| Checked {
                name: name.clone(),
                sign_in: secret.sign_in.clone(),
                outcome: if approvals.covers(secret) {
                    check(secret, dir, timeout)
                } else {
                    Outcome::NotApproved
                },
            })
            .collect();
        Self { checked }
    }

    /// Returns each secret whose check did not succeed.
    #[must_use]
    pub fn missing(&self) -> Vec<&Checked> {
        self.checked
            .iter()
            .filter(|checked| !matches!(checked.outcome, Outcome::Works | Outcome::NotApproved))
            .collect()
    }

    /// Returns each secret whose check did not run for want of approval.
    #[must_use]
    pub fn unapproved(&self) -> Vec<&Checked> {
        self.checked
            .iter()
            .filter(|checked| checked.outcome == Outcome::NotApproved)
            .collect()
    }

    /// Returns one message that names every sign-in the person must do,
    /// each with its exact command, and every check that needs approval,
    /// or `None` when every check worked.
    #[must_use]
    pub fn message(&self) -> Option<String> {
        let missing = self.missing();
        let unapproved = self.unapproved();
        let mut parts: Vec<String> = Vec::new();
        if !missing.is_empty() {
            let plural = if missing.len() == 1 {
                "sign-in is"
            } else {
                "sign-ins are"
            };
            let commands: Vec<String> = missing
                .iter()
                .map(|checked| format!("{}: `{}`", checked.name, checked.sign_in))
                .collect();
            parts.push(format!(
                "{} {plural} needed before work starts. Run {}, then run `mxv preflight` to check again",
                missing.len(),
                commands.join("; and ")
            ));
        }
        if !unapproved.is_empty() {
            let names: Vec<&str> = unapproved
                .iter()
                .map(|checked| checked.name.as_str())
                .collect();
            parts.push(format!(
                "mxv did not run the check of {} because no one approved its command; run `mxv preflight` at a terminal to review and approve it",
                names.join(", ")
            ));
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join(". "))
    }
}

/// Runs the check of `secret` in `dir` and returns what it showed.
fn check(secret: &Secret, dir: &Path, timeout: Duration) -> Outcome {
    match run_command(secret.check.words(), dir, timeout, false) {
        Ran::Exited { success: true, .. } => Outcome::Works,
        Ran::Exited { success: false, .. } => Outcome::Failed,
        Ran::Unstartable => Outcome::Unrunnable,
        Ran::TimedOut => Outcome::TimedOut,
    }
}
