//! The workspace verbs `new`, `list`, `show`, `cd` and `remove`, and the
//! documents they print.

use crumbkit::{Batch, Call, Document, Listing, Output, Prose};
use multiverse::{Git, Mode, NewOutcome, RemovalPlan, Store, WorkspaceName, Workspaces};

use crate::cli::{CdArgs, ListArgs, NewArgs, RemoveArgs, ShowArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::rows::{RemovalResult, WorkspaceRow};

/// The document that `mxv new` prints.
pub(crate) const NEW: Document<WorkspaceRow> = Document::new(
    "mxv",
    "new",
    1,
    "The workspace that the run created, or would create on a dry run.",
);

/// The document that `mxv list` prints.
pub(crate) const LIST: Document<Listing<WorkspaceRow>> = Document::new(
    "mxv",
    "list",
    1,
    "One page of the workspaces, in name order, and the number of workspaces.",
)
.when_empty("no workspace exists yet, and `mxv new <NAME> --repo <PATH>` creates one");

/// The document that `mxv show` prints.
pub(crate) const SHOW: Document<Batch<WorkspaceRow, MxvFailure>> = Document::new(
    "mxv",
    "show",
    1,
    "The workspaces with the given names, and a failure for each name that no workspace has.",
);

/// The document that `mxv cd` prints.
pub(crate) const CD: Document<Prose<String>> = Document::new(
    "mxv",
    "cd",
    1,
    "The directory of the workspace, for `cd \"$(mxv cd <NAME>)\"`.",
);

/// The document that `mxv remove` prints.
pub(crate) const REMOVE: Document<Prose<RemovalResult>> = Document::new(
    "mxv",
    "remove",
    1,
    "What the run removed, deleted and kept, or would on a dry run.",
);

/// Returns `text` as a workspace name.
fn parse_name(text: &str) -> Result<WorkspaceName, MxvFailure> {
    text.parse().map_err(|reason| MxvFailure::BadName {
        name: text.to_owned(),
        reason,
    })
}

/// Returns the store of the workspace records that `environment` names.
fn store(environment: &Environment) -> Result<Store, MxvFailure> {
    Ok(Store::new(environment.records()?))
}

/// Returns the workspaces whose records `environment` names, made by git.
fn workspaces(environment: &Environment) -> Result<Workspaces<Git>, MxvFailure> {
    Ok(Workspaces::new(store(environment)?, Git))
}

/// Creates the workspace that `args` names, or plans it on a dry run.
pub(crate) fn new(
    output: &Output,
    environment: &Environment,
    args: &NewArgs,
) -> Result<WorkspaceRow, MxvFailure> {
    let name = parse_name(&args.name)?;
    let mode = if output.is_dry_run() {
        Mode::DryRun
    } else {
        Mode::Change
    };
    let root = environment.root()?;
    match workspaces(environment)?.create(&root, &name, &args.repo, mode)? {
        NewOutcome::Created(workspace) | NewOutcome::Planned(workspace) => {
            Ok(WorkspaceRow::from(&workspace))
        }
        NewOutcome::NameTaken(_) => Err(MxvFailure::Exists {
            name: name.to_string(),
        }),
        NewOutcome::PathTaken(path) => Err(MxvFailure::DirectoryTaken { path }),
        NewOutcome::BranchTaken { repository, branch } => {
            Err(MxvFailure::BranchExists { repository, branch })
        }
    }
}

/// Returns the first page of the workspaces, in name order.
pub(crate) fn list(
    environment: &Environment,
    args: &ListArgs,
) -> Result<Listing<WorkspaceRow>, MxvFailure> {
    let rows = store(environment)?
        .list()?
        .iter()
        .map(WorkspaceRow::from)
        .collect();
    Ok(args.page.first_of(rows).described("workspaces").next(
        Call::new(&["show"]).arg("names"),
        "takes up to 100 names in one call.",
    ))
}

/// Returns the workspaces that `args` names, in the order given, with a
/// failure for each name that no workspace has.
pub(crate) fn show(
    environment: &Environment,
    args: &ShowArgs,
) -> Result<Batch<WorkspaceRow, MxvFailure>, MxvFailure> {
    let store = store(environment)?;
    let mut batch = Batch::new(&["show"], "names", &args.names);
    for text in &args.names {
        let found = parse_name(text).and_then(|name| {
            store
                .read(&name)?
                .map(|workspace| WorkspaceRow::from(&workspace))
                .ok_or(MxvFailure::NotFound {
                    name: name.to_string(),
                })
        });
        batch.push(found);
    }
    Ok(batch)
}

/// Returns the directory of the workspace that `args` names.
pub(crate) fn cd(environment: &Environment, args: &CdArgs) -> Result<Prose<String>, MxvFailure> {
    let name = parse_name(&args.name)?;
    let workspace = store(environment)?
        .read(&name)?
        .ok_or(MxvFailure::NotFound {
            name: name.to_string(),
        })?;
    Ok(Prose::new(workspace.path.display().to_string()))
}

/// Removes the workspace that `args` names, after the person confirms, or
/// reports what removing it would do on a dry run.
pub(crate) fn remove(
    output: &mut Output,
    environment: &Environment,
    args: &RemoveArgs,
) -> Result<Prose<RemovalResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = workspaces(environment)?;
    let removal = match workspaces
        .plan_removal(&name, args.force)
        .map_err(MxvFailure::from)?
    {
        RemovalPlan::Ready(removal) => removal,
        RemovalPlan::NotFound => {
            return Err(MxvFailure::NotFound {
                name: name.to_string(),
            }
            .into());
        }
        RemovalPlan::Dirty { changes, .. } => {
            return Err(MxvFailure::Dirty {
                name: name.to_string(),
                changes,
            }
            .into());
        }
        RemovalPlan::Unowned { checkout, .. } => {
            return Err(MxvFailure::Unowned {
                name: name.to_string(),
                path: checkout,
            }
            .into());
        }
    };
    if output.is_dry_run() {
        return Ok(Prose::new(RemovalResult::new(&removal, true)));
    }
    output.confirm(
        args.yes,
        &format!("Remove the workspace {name}, its checkouts and its record?"),
    )?;
    let removed = workspaces.remove(removal).map_err(MxvFailure::from)?;
    Ok(Prose::new(RemovalResult::new(&removed, false)))
}
