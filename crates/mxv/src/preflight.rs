//! The verb `preflight`, and the check that `new`, `restore` and `up` run
//! first, which lists every sign-in that a workspace's manifest needs.
//!
//! The library in [`multiverse`] runs each secret's check command and
//! words the message. This module holds what only a command can do: find
//! the workspace and print the result. No value of a credential reaches
//! it, because the library discards the check's output.

use std::fmt;

use crumbkit::{Document, Prose};
use multiverse::{Approvals, CHECK_TIMEOUT, Outcome, Preflight, Secret, Workspace};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::PreflightArgs;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::run_failures::RunFailure;

/// The document that `mxv preflight` prints.
pub(crate) const PREFLIGHT: Document<Prose<PreflightResult>> = Document::new(
    "mxv",
    "preflight",
    1,
    "Each secret that the workspace's manifest declares, whether its check command worked, and for each one that did not, the command line that signs in.",
);

/// Whether the workspace can start work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PreflightState {
    /// Every check worked, or the manifest declares no secret.
    Ready,
    /// At least one check did not work or did not run, and the person has
    /// to sign in or approve it.
    SignInNeeded,
}

/// Why a check did not work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    /// The command exited with another code or a signal.
    Failed,
    /// The program does not start.
    Unrunnable,
    /// The command was still running at the timeout.
    TimedOut,
    /// No one approved the command line.
    NotApproved,
}

/// One secret and what its check showed.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SecretRow {
    /// The secret's name in the manifest.
    name: String,
    /// Whether the check command exited with 0.
    works: bool,
    /// Why the check did not work, or null when it did: `failed`,
    /// `unrunnable` when the program does not start, `timed_out` when the
    /// command was still running after 20 seconds and mxv stopped it, or
    /// `not_approved` when no one approved the command and mxv did not run
    /// it.
    reason: Option<Reason>,
    /// The command line that signs in, which the manifest declares.
    sign_in: String,
}

/// What `mxv preflight` found.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PreflightResult {
    /// The workspace's name.
    workspace: String,
    /// Whether the workspace can start work.
    state: PreflightState,
    /// Each secret, in name order.
    secrets: Vec<SecretRow>,
    /// One message that names every sign-in the person must do, each with
    /// its exact command, or null when none is needed.
    message: Option<String>,
}

impl From<(&str, &Preflight)> for PreflightResult {
    fn from((workspace, report): (&str, &Preflight)) -> Self {
        let message = report.message();
        Self {
            workspace: workspace.to_owned(),
            state: if message.is_some() {
                PreflightState::SignInNeeded
            } else {
                PreflightState::Ready
            },
            secrets: report
                .checked
                .iter()
                .map(|checked| SecretRow {
                    name: checked.name.clone(),
                    works: checked.outcome == Outcome::Works,
                    reason: match checked.outcome {
                        Outcome::Works => None,
                        Outcome::Failed => Some(Reason::Failed),
                        Outcome::Unrunnable => Some(Reason::Unrunnable),
                        Outcome::TimedOut => Some(Reason::TimedOut),
                        Outcome::NotApproved => Some(Reason::NotApproved),
                    },
                    sign_in: checked.sign_in.clone(),
                })
                .collect(),
            message,
        }
    }
}

impl fmt::Display for PreflightResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.secrets.is_empty() {
            return writeln!(
                formatter,
                "The manifest of {} declares no secret, so no sign-in is needed.",
                self.workspace
            );
        }
        for secret in &self.secrets {
            match secret.reason {
                None => writeln!(formatter, "{}: works", secret.name)?,
                Some(Reason::NotApproved) => writeln!(
                    formatter,
                    "{}: not checked, because no one approved its command",
                    secret.name
                )?,
                Some(reason) => writeln!(
                    formatter,
                    "{}: sign in with `{}` (the check {})",
                    secret.name,
                    secret.sign_in,
                    match reason {
                        Reason::Unrunnable => "could not start",
                        Reason::TimedOut => "did not finish in time",
                        Reason::Failed | Reason::NotApproved => "failed",
                    }
                )?,
            }
        }
        match &self.message {
            Some(message) => writeln!(formatter, "{message}."),
            None => writeln!(formatter, "Every sign-in works."),
        }
    }
}

/// Runs the checks of `secrets` that a person approved for `workspace`
/// and returns what they showed.
fn check(environment: &Environment, workspace: &Workspace, secrets: &[Checkable]) -> Preflight {
    Preflight::run(secrets, &approvals(environment, workspace), CHECK_TIMEOUT)
}

/// A secret with its name and the directory its check runs in.
type Checkable = (String, Secret, std::path::PathBuf);

/// Returns the approvals that mxv's state directory holds for the
/// repositories of `workspace`. Without a state directory nothing is
/// approved.
fn approvals(environment: &Environment, workspace: &Workspace) -> Approvals {
    environment.state().map_or_else(
        |_| Approvals::none(),
        |state| Approvals::in_state(&state, &workspace.approval_scope()),
    )
}

/// Lists the check commands that no approval covers and asks the person
/// at the terminal whether mxv may run them, and records a yes. Without a
/// terminal, or with `--no-input`, it asks nothing and approves nothing. Each command prints as
/// a list of its quoted words, so escape characters and the boundaries
/// between words show.
fn approve_first(
    environment: &Environment,
    workspace: &Workspace,
    secrets: &[Checkable],
    no_input: bool,
) -> Result<(), crumbkit::Error> {
    if no_input || !crate::person_can_answer() {
        return Ok(());
    }
    let approvals = Approvals::in_state(&environment.state()?, &workspace.approval_scope());
    let new: Vec<&Checkable> = secrets
        .iter()
        .filter(|(_, secret, _)| !approvals.covers(secret))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut text = String::from(
        "The manifest declares these check commands, which mxv runs on this machine:\n",
    );
    for (name, secret, _) in &new {
        text.push_str(&format!("  {name}: {:?}\n", secret.check.words()));
    }
    text.push_str("Run them now and in any workspace of these repositories that declares the same commands? [y/N] ");
    if processes::answer(
        &text,
        &mut crate::stderr_writer(),
        &mut crate::stdin_reader(),
    ) {
        let secrets: Vec<&Secret> = new.iter().map(|(_, secret, _)| secret).collect();
        approvals.approve(&secrets).map_err(MxvFailure::from)?;
    }
    Ok(())
}

/// Checks the secrets of the workspace that `args` names, or of the one
/// that holds the current directory. At a terminal, unless `--no-input` is set, it first asks the person
/// to approve each check command that no approval covers.
pub(crate) fn preflight(
    environment: &Environment,
    args: &PreflightArgs,
    no_input: bool,
) -> Result<Prose<PreflightResult>, crumbkit::Error> {
    let context = processes::context(environment, args.workspace.as_ref(), &[])?;
    let secrets = context
        .workspace
        .secrets()
        .map_err(|error| crumbkit::Error::from(RunFailure::Manifest(error)))?;
    approve_first(environment, &context.workspace, &secrets, no_input)?;
    let report = check(environment, &context.workspace, &secrets);
    Ok(Prose::new(PreflightResult::from((
        context.workspace.name.as_str(),
        &report,
    ))))
}

/// Prints one warning on standard error that names every sign-in that
/// `workspace` needs and every check that no one approved, or nothing when
/// its checks all work. A manifest that cannot be read or declares no
/// secret prints nothing, because the verbs that call this report their
/// own manifest failures.
pub(crate) fn announce(environment: &Environment, workspace: &Workspace) {
    if let Ok(secrets) = workspace.secrets()
        && let Some(message) = check(environment, workspace, &secrets).message()
    {
        crate::warn(&[message]);
    }
}
