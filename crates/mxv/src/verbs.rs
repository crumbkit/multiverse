//! The workspace verbs `new`, `list`, `show`, `cd` and `remove`, and the
//! documents they print. `clean` lives in its own module.

use crumbkit::{Batch, Call, Document, Listing, Output, Prose};
use multiverse::{
    Allow, Cloned, DEFAULT_CLONE_ON_NEW, Git, Manifest, Mode, NewOutcome, PathGlob, RemovalOutcome,
    RemovalPlan, Store, Workspace, WorkspaceName, Workspaces,
};

use crate::cli::{CdArgs, ListArgs, NewArgs, RemoveArgs, ShowArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::rows::{NewRow, RemovalResult, ShowRow, WorkspaceRow};
use crate::run_failures::RunFailure;

/// The document that `mxv new` prints.
pub(crate) const NEW: Document<NewRow> = Document::new(
    "mxv",
    "new",
    1,
    "The workspace that the run created, or would create on a dry run, and the ignored directories that it cloned into the checkout.",
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
pub(crate) const SHOW: Document<Batch<ShowRow, MxvFailure>> = Document::new(
    "mxv",
    "show",
    1,
    "The workspaces with the given names and their sizes on disk, and a failure for each name that no workspace has.",
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

/// Returns the workspaces whose records `environment` names, made by git,
/// with the disposable globs of the config file.
pub(crate) fn configured(environment: &Environment) -> Result<Workspaces<Git>, MxvFailure> {
    Ok(workspaces(environment)?.with_disposable(environment.config()?.disposable))
}

/// Returns the globs of the ignored directories that `new` clones from the
/// repository at `source`: the config file's `clone_on_new`, or else
/// [`DEFAULT_CLONE_ON_NEW`], and the `clone_on_new` of the repository's
/// manifest. A manifest that does not parse adds nothing, and its error
/// becomes the warning that the result returns.
fn clone_globs(
    environment: &Environment,
    source: &std::path::Path,
) -> Result<(Vec<PathGlob>, Option<String>), MxvFailure> {
    let mut globs = match environment.config()?.clone_on_new {
        Some(globs) => globs,
        None => DEFAULT_CLONE_ON_NEW
            .iter()
            .filter_map(|text| PathGlob::parse_clone(text).ok())
            .collect(),
    };
    match Manifest::read(source) {
        Ok(manifest) => globs.extend(
            manifest
                .map(|manifest| manifest.clone_on_new)
                .unwrap_or_default(),
        ),
        Err(error) => {
            return Ok((
                globs,
                Some(format!(
                    "mxv cloned only the directories that the config file names, because the repository's manifest does not parse: {error}"
                )),
            ));
        }
    }
    Ok((globs, None))
}

/// Creates the workspace that `args` names, or plans it on a dry run.
pub(crate) fn new(
    output: &Output,
    environment: &Environment,
    args: &NewArgs,
) -> Result<NewRow, MxvFailure> {
    let name = parse_name(&args.name)?;
    let mode = if output.is_dry_run() {
        Mode::DryRun
    } else {
        Mode::Change
    };
    let root = environment.root()?;
    let workspaces = workspaces(environment)?;
    match workspaces.create(&root.path, &name, &args.repo, mode)? {
        NewOutcome::Created(workspace) | NewOutcome::Planned(workspace) => {
            let cloned = if args.no_clone {
                Cloned::default()
            } else {
                clone_into(environment, &workspaces, &workspace, mode)?
            };
            Ok(NewRow::new(&workspace, &root, cloned))
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

/// Clones the ignored directories of the repository of `workspace` that
/// the config file and the manifest name into its checkout, or names them
/// on a dry run. The workspace exists by then, so a failure of the cloning
/// becomes the warning of the result.
fn clone_into(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    workspace: &Workspace,
    mode: Mode,
) -> Result<Cloned, MxvFailure> {
    let Some(source) = workspace
        .checkouts
        .first()
        .map(|checkout| checkout.source.clone())
    else {
        return Ok(Cloned::default());
    };
    let (globs, warning) = clone_globs(environment, &source)?;
    let mut cloned = workspaces
        .clone_ignored(workspace, &globs, mode)
        .unwrap_or_else(|error| {
            let mut failed = Cloned::default();
            failed.warning = Some(format!("mxv cloned no ignored directory: {error}"));
            failed
        });
    if cloned.warning.is_none() {
        cloned.warning = warning;
    }
    Ok(cloned)
}

/// Returns the first page of the workspaces: a row for each record that
/// mxv cannot read, first, and then the workspaces in name order. With
/// `--sizes`, each workspace's row on the page holds its sizes, and only
/// the workspaces on the page are measured.
pub(crate) fn list(
    environment: &Environment,
    args: &ListArgs,
) -> Result<Listing<WorkspaceRow>, MxvFailure> {
    let records = store(environment)?.list()?;
    let measure = if args.sizes {
        Some(configured(environment)?)
    } else {
        None
    };
    let shown = args.page.limit().unwrap_or(usize::MAX);
    let mut rows: Vec<WorkspaceRow> = records
        .problems
        .iter()
        .map(WorkspaceRow::unreadable)
        .collect();
    for workspace in &records.workspaces {
        let row = WorkspaceRow::from(workspace);
        rows.push(match &measure {
            Some(workspaces) if rows.len() < shown => {
                row.with_usage(&workspaces.disk_usage(workspace)?)
            }
            _ => row,
        });
    }
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
) -> Result<Batch<ShowRow, MxvFailure>, MxvFailure> {
    let workspaces = configured(environment)?;
    let root = environment.root()?;
    let mut batch = Batch::new(&["show"], "names", &args.names);
    for text in &args.names {
        let found = parse_name(text).and_then(|name| {
            let workspace = workspaces.find(&name)?.ok_or(MxvFailure::NotFound {
                name: name.to_string(),
            })?;
            let usage = workspaces.disk_usage(&workspace)?;
            Ok(ShowRow::new(&workspace, &root, &usage))
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
    let workspaces = configured(environment)?;
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
