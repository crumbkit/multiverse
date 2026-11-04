//! The workspace verbs `new`, `list`, `show`, `cd` and `remove`, and the
//! documents they print.

use crumbkit::{Batch, Call, Document, Listing, Output, Prose};
use multiverse::{
    Allow, Git, Mode, NewOutcome, RemovalOutcome, RemovalPlan, Store, WorkspaceName, Workspaces,
};

use crate::cli::{CdArgs, ListArgs, NewArgs, RemoveArgs, ShowArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::rows::{PlacedRow, RemovalResult, WorkspaceRow};
use crate::run_failures::RunFailure;

/// The document that `mxv new` prints.
pub(crate) const NEW: Document<PlacedRow> = Document::new(
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
pub(crate) const SHOW: Document<Batch<PlacedRow, MxvFailure>> = Document::new(
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
pub(crate) fn parse_name(text: &str) -> Result<WorkspaceName, MxvFailure> {
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
) -> Result<PlacedRow, MxvFailure> {
    let name = parse_name(&args.name)?;
    let mode = if output.is_dry_run() {
        Mode::DryRun
    } else {
        Mode::Change
    };
    let root = environment.root()?;
    match workspaces(environment)?.create(&root.path, &name, &args.repo, mode)? {
        NewOutcome::Created(workspace) | NewOutcome::Planned(workspace) => {
            Ok(PlacedRow::new(&workspace, &root))
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

/// Returns the first page of the workspaces: a row for each record that
/// mxv cannot read, first, and then the workspaces in name order.
pub(crate) fn list(
    environment: &Environment,
    args: &ListArgs,
) -> Result<Listing<WorkspaceRow>, MxvFailure> {
    let records = store(environment)?.list()?;
    let rows = records
        .problems
        .iter()
        .map(WorkspaceRow::unreadable)
        .chain(records.workspaces.iter().map(WorkspaceRow::from))
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
) -> Result<Batch<PlacedRow, MxvFailure>, MxvFailure> {
    let store = store(environment)?;
    let root = environment.root()?;
    let mut batch = Batch::new(&["show"], "names", &args.names);
    for text in &args.names {
        let found = parse_name(text).and_then(|name| {
            store
                .read(&name)?
                .map(|workspace| PlacedRow::new(&workspace, &root))
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
/// reports what removing it would do on a dry run. The removal plans again
/// after the confirmation, and refuses when the workspace changed while
/// mxv waited.
pub(crate) fn remove(
    output: &mut Output,
    environment: &Environment,
    args: &RemoveArgs,
) -> Result<Prose<RemovalResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = workspaces(environment)?;
    // Running processes stop the removal unless it is forced, and a forced
    // removal stops them after the person confirms.
    let allow = Allow {
        discard_changes: args.force,
        delete_unmerged: args.delete_unmerged,
    };
    let mut running = Vec::new();
    let mut volumes = None;
    let mut containers = Vec::new();
    if let Some(workspace) = workspaces.find(&name).map_err(MxvFailure::from)? {
        running = processes::running(environment, &workspace)?;
        if !running.is_empty() && !args.force {
            return Err(RunFailure::Running {
                workspace: name.to_string(),
                processes: running,
            }
            .into());
        }
        // A volume may hold the only copy of a database, so removing it
        // needs its own flag, and `--force` alone keeps the workspace.
        let found = processes::volumes(environment, &name)?;
        if !found.is_empty() && !args.delete_volumes {
            return Err(RunFailure::Volumes {
                workspace: name.to_string(),
                volumes: found.names().to_vec(),
                allow,
            }
            .into());
        }
        volumes = Some(found);
        containers = processes::containers(environment, &name)?;
    }
    let names: Vec<String> = volumes
        .as_ref()
        .map(|volumes| volumes.names().to_vec())
        .unwrap_or_default();
    let plan = workspaces
        .plan_removal(&name, allow)
        .map_err(MxvFailure::from)?;
    let removal = match plan {
        RemovalPlan::Ready(removal) => removal,
        refused => return Err(refusal(&name, allow, refused).into()),
    };
    if output.is_dry_run() {
        return Ok(Prose::new(
            RemovalResult::new(&removal, true).with_docker(containers, names),
        ));
    }
    output.confirm(
        args.yes,
        &format!("Remove the workspace {name}, its checkouts and its record?"),
    )?;
    if !running.is_empty() {
        processes::stop_all(environment, &removal.workspace)?;
    }
    match workspaces.remove(removal).map_err(MxvFailure::from)? {
        RemovalOutcome::Done(removed) => {
            processes::forget(environment, &name)?;
            if let Some(volumes) = volumes {
                processes::delete_volumes(environment, volumes)?;
            }
            Ok(Prose::new(
                RemovalResult::new(&removed, false).with_docker(containers, names),
            ))
        }
        RemovalOutcome::Changed(RemovalPlan::Ready(_)) => Err(MxvFailure::Changed {
            name: name.to_string(),
            allow,
        }
        .into()),
        RemovalOutcome::Changed(refused) => Err(refusal(&name, allow, refused).into()),
    }
}

/// Returns the failure of a removal of the workspace `name` that `plan`
/// refuses, where the removal's flags allowed what `allow` allows. A plan
/// that is ready refuses nothing, and reads as a workspace that changed.
fn refusal(name: &WorkspaceName, allow: Allow, plan: RemovalPlan) -> MxvFailure {
    let name = name.to_string();
    match plan {
        RemovalPlan::Ready(_) => MxvFailure::Changed { name, allow },
        RemovalPlan::NotFound => MxvFailure::NotFound { name },
        RemovalPlan::Dirty { changes, .. } => MxvFailure::Dirty {
            name,
            changes,
            allow,
        },
        RemovalPlan::IgnoredFiles { paths, .. } => MxvFailure::IgnoredFiles { name, paths, allow },
        RemovalPlan::StrayCommits {
            checkout, commits, ..
        } => MxvFailure::StrayCommits {
            name,
            path: checkout,
            commits,
            allow,
        },
        RemovalPlan::Locked {
            workspace,
            checkout,
            reason,
        } => MxvFailure::Locked {
            name,
            repository: workspace
                .checkouts
                .iter()
                .find(|listed| listed.path == checkout)
                .map_or_else(|| checkout.clone(), |listed| listed.source.clone()),
            path: checkout,
            reason,
            allow,
        },
        RemovalPlan::Unowned { checkout, .. } => MxvFailure::Unowned {
            name,
            path: checkout,
        },
    }
}
