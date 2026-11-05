//! The workspace verbs `new`, `list`, `show`, `cd` and `remove`, and the
//! documents they print. `clean` lives in its own module.

use crumbkit::{Batch, Call, Document, Listing, Output, Prose};
use std::path::PathBuf;

use multiverse::{
    AddOutcome, Allow, Checkout, Cloned, DEFAULT_CLONE_ON_NEW, Git, Joined, Manifest,
    ManifestProblem, Mode, NewOutcome, PathGlob, RemovalOutcome, RemovalPlan, Repo, Store,
    Workspace, WorkspaceName, Workspaces, checkout_prefix, prefixed,
};

use crate::cli::{AddArgs, CdArgs, ListArgs, NewArgs, RemoveArgs, ShowArgs};
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

/// The document that `mxv add` prints.
pub(crate) const ADD: Document<NewRow> = Document::new(
    "mxv",
    "add",
    1,
    "The workspace with the checkout that the run added, or would add on a dry run, and the ignored directories that it cloned into the new checkout.",
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
    "The directory of the workspace or of one of its checkouts, for `cd \"$(mxv cd <NAME>)\"`.",
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
    Ok(Workspaces::new(store(environment)?, Git).with_mirrors(environment.mirrors()?))
}

/// Returns the workspaces whose records `environment` names, made by git,
/// with the disposable globs of the config file.
pub(crate) fn configured(environment: &Environment) -> Result<Workspaces<Git>, MxvFailure> {
    Ok(workspaces(environment)?.with_disposable(environment.config()?.disposable))
}

/// The workspace manifest that `new` copies into a new workspace, and the
/// file it comes from.
struct Copied {
    /// The file: the `--manifest` file or a template.
    from: PathBuf,
    /// The text to write, which parses as a manifest.
    text: String,
    /// The names of the checkouts that the manifest joins, which its units
    /// may refer to in a workspace of several repositories.
    joined: Joined,
}

/// Returns the repositories that `args` names, each with the folder that
/// its `--as` gives.
fn repos(args: &NewArgs) -> Result<Vec<Repo>, MxvFailure> {
    args.repos
        .repos
        .iter()
        .map(|repo| repo_of(&repo.path, repo.folder.as_deref()))
        .collect()
}

/// Returns the repository at `path` with the folder `folder`, when one is
/// given.
fn repo_of(path: &std::path::Path, folder: Option<&str>) -> Result<Repo, MxvFailure> {
    match folder {
        Some(folder) => Repo::named(path, folder).map_err(|reason| MxvFailure::BadFolder {
            folder: folder.to_owned(),
            reason,
        }),
        None => Ok(Repo::new(path)),
    }
}

/// Returns the names that the checkouts of `repos` declare in their own
/// `mxv.toml`, prefixed as a workspace with several checkouts prefixes
/// them, for checking a workspace manifest that refers to them. A
/// repository whose `mxv.toml` does not parse adds none.
fn joined_names(workspaces: &Workspaces<Git>, repos: &[Repo]) -> Result<Joined, MxvFailure> {
    let mut joined = Joined::default();
    for repo in repos {
        let (source, folder) = workspaces.resolve(repo)?;
        if let Ok(Some(own)) = Manifest::read(&source) {
            let prefix = checkout_prefix(&folder);
            joined.add(&own.renamed(&|name| prefixed(&prefix, name)));
        }
    }
    Ok(joined)
}

/// Returns the workspace manifest that `new` copies: the `--manifest`
/// file when `args` names one, or else, for a workspace of one
/// repository, the template in `$XDG_CONFIG_HOME/mxv/templates` that lists
/// the repository's origin, or else none. A workspace of several
/// repositories copies no template, because a template names one origin.
/// It also returns the warnings to report: the templates that it skipped,
/// and the tables of the copied manifest that mxv ignores although their
/// names are close to its own. `root` is the root of the workspaces.
fn copied(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    args: &NewArgs,
    repos: &[Repo],
    root: &std::path::Path,
) -> Result<(Option<Copied>, Vec<String>), crumbkit::Error> {
    let (copied, mut warnings) = if let Some(file) = &args.manifest {
        // A relative path is shown from the current directory, as the
        // shell that typed it reads it.
        let from = match environment.current_dir() {
            Some(dir) if file.is_relative() => dir.join(file),
            _ => file.clone(),
        };
        let joined = if repos.len() > 1 {
            joined_names(workspaces, repos)?
        } else {
            Joined::default()
        };
        // The workspace manifest of several repositories refers to names
        // that their clones declare, so the library checks it against the
        // clones once it has made them.
        let text = if repos.len() > 1 {
            Manifest::read_named_unchecked(&from)
        } else {
            Manifest::read_named(&from)
        }
        .map_err(RunFailure::Manifest)?;
        workspaces
            .check_manifest_source(&from, root)
            .map_err(MxvFailure::from)?;
        (Some(Copied { from, text, joined }), Vec::new())
    } else if let [repo] = repos {
        let templates = environment.templates()?;
        let found = workspaces
            .template(&templates, &repo.path, root)
            .map_err(MxvFailure::from)?;
        let copied = found.template.map(|template| Copied {
            from: template.path,
            text: template.text,
            joined: Joined::default(),
        });
        (copied, found.skipped)
    } else {
        (None, Vec::new())
    };
    if let Some(copied) = &copied
        && let Ok(manifest) = copied.parse()
    {
        warnings.extend(manifest.warnings);
    }
    Ok((copied, warnings))
}

impl Copied {
    /// Returns the copied manifest, parsed with the names of the checkouts
    /// that it joins.
    fn parse(&self) -> Result<Manifest, multiverse::ManifestError> {
        Manifest::parse_joining(&self.text, &self.from, &self.joined)
    }
}

/// Returns the globs of the ignored directories that `new` or `add` clones
/// from the repository of `checkout`: the config file's `clone_on_new`, or
/// else [`DEFAULT_CLONE_ON_NEW`], and the manifest's. In a workspace with
/// one checkout, the manifest is the one that the workspace reads, which
/// is `copied` when `new` copies a workspace manifest, or else the
/// repository's own. In a workspace with several checkouts, it is the
/// repository's own and the globs of the workspace manifest, `copied` or
/// the one the workspace holds, that apply in the checkout. A manifest that
/// does not parse adds nothing, and its error becomes the warning that the
/// result returns.
fn clone_globs(
    environment: &Environment,
    workspace: &Workspace,
    checkout: &Checkout,
    copied: Option<&Copied>,
) -> Result<(Vec<PathGlob>, Option<String>), MxvFailure> {
    let mut globs = match environment.config()?.clone_on_new {
        Some(globs) => globs,
        None => DEFAULT_CLONE_ON_NEW
            .iter()
            .filter_map(|text| PathGlob::parse_clone(text).ok())
            .collect(),
    };
    let failed = |error: multiverse::ManifestError| {
        Some(format!(
            "mxv cloned only the directories that the config file names, because the manifest does not parse: {error}"
        ))
    };
    let shared = match copied {
        Some(copied) => copied.parse().map(Some),
        None => workspace.workspace_manifest(),
    };
    if !workspace.has_several_checkouts() {
        let read = match shared {
            Ok(Some(manifest)) => Ok(Some(manifest)),
            Ok(None) => Manifest::read(&checkout.source),
            Err(error) => Err(error),
        };
        return Ok(match read {
            Ok(manifest) => {
                globs.extend(
                    manifest
                        .map(|manifest| manifest.clone_on_new)
                        .unwrap_or_default(),
                );
                (globs, None)
            }
            Err(error) => (globs, failed(error)),
        });
    }
    let mut warning = None;
    match Manifest::read(&checkout.source) {
        Ok(manifest) => {
            globs.extend(
                manifest
                    .map(|manifest| manifest.clone_on_new)
                    .unwrap_or_default(),
            );
        }
        Err(error) => warning = failed(error),
    }
    match shared {
        Ok(manifest) => globs.extend(
            manifest
                .map(|manifest| manifest.clone_on_new)
                .unwrap_or_default()
                .iter()
                .filter_map(|glob| glob.within(&checkout.name)),
        ),
        Err(error) => warning = failed(error),
    }
    Ok((globs, warning))
}

/// Creates the workspace that `args` names, or plans it on a dry run.
pub(crate) fn new(
    output: &Output,
    environment: &Environment,
    args: &NewArgs,
) -> Result<NewRow, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let mode = if output.is_dry_run() {
        Mode::DryRun
    } else {
        Mode::Change
    };
    let root = environment.root()?;
    let mut workspaces = workspaces(environment)?;
    if output.progress_allowed() {
        workspaces = workspaces.with_mirror_notice(crate::mirror_wait_notice);
    }
    let repos = repos(args)?;
    let (copied, warnings) = copied(environment, &workspaces, args, &repos, &root.path)?;
    let text = copied.as_ref().map(|copied| copied.text.as_str());
    let outcome = workspaces
        .create_repos(&root.path, &name, &repos, mode, text)
        .map_err(MxvFailure::from)?;
    match outcome {
        NewOutcome::Created(workspace) | NewOutcome::Planned(workspace) => {
            let cloned = if args.no_clone {
                Cloned::default()
            } else {
                let checkouts: Vec<&Checkout> = workspace.checkouts.iter().collect();
                clone_into(
                    environment,
                    &workspaces,
                    &workspace,
                    &checkouts,
                    mode,
                    copied.as_ref(),
                )?
            };
            let from = copied.as_ref().map(|copied| copied.from.as_path());
            Ok(NewRow::new(&workspace, &root, cloned, from).with_warnings(&warnings))
        }
        NewOutcome::NameTaken(_) => Err(MxvFailure::Exists {
            name: name.to_string(),
        }
        .into()),
        NewOutcome::PathTaken(path) => Err(MxvFailure::DirectoryTaken { path }.into()),
        NewOutcome::BranchTaken { repository, branch } => {
            Err(MxvFailure::BranchExists { repository, branch }.into())
        }
        NewOutcome::FolderTaken { folder, repository } => {
            Err(MxvFailure::FolderTaken { repository, folder }.into())
        }
        NewOutcome::BadFolder { folder, reason, .. } => {
            Err(MxvFailure::BadFolder { folder, reason }.into())
        }
        NewOutcome::ManifestRefused(error) => Err(RunFailure::Manifest(error).into()),
    }
}

/// Clones the ignored directories of the repositories of `checkouts`, which
/// belong to `workspace`, that the config file and the manifests name,
/// into each checkout, or names them on a dry run. The workspace exists by
/// then, so a failure of the cloning becomes the warning of the result.
fn clone_into(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    workspace: &Workspace,
    checkouts: &[&Checkout],
    mode: Mode,
    copied: Option<&Copied>,
) -> Result<Cloned, MxvFailure> {
    let mut each = Vec::new();
    let mut warning = None;
    for checkout in checkouts {
        let (globs, failed) = clone_globs(environment, workspace, checkout, copied)?;
        warning = warning.or(failed);
        each.push((*checkout, globs));
    }
    let mut cloned = workspaces
        .clone_ignored_each(&each, mode)
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

/// Adds the checkout that `args` names to a workspace, or plans it on a
/// dry run. It holds the workspace's lock, which `up` and `down` take, and
/// refuses while the workspace's processes or containers run, because
/// adding a second checkout renames the first checkout's units and ports.
pub(crate) fn add(
    output: &Output,
    environment: &Environment,
    args: &AddArgs,
) -> Result<NewRow, crumbkit::Error> {
    let mode = if output.is_dry_run() {
        Mode::DryRun
    } else {
        Mode::Change
    };
    let root = environment.root()?;
    let mut workspaces = workspaces(environment)?;
    if output.progress_allowed() {
        workspaces = workspaces.with_mirror_notice(crate::mirror_wait_notice);
    }
    let workspace = match &args.workspace {
        Some(text) => {
            let name = parse_name(text)?;
            workspaces
                .find(&name)
                .map_err(MxvFailure::from)?
                .ok_or_else(|| MxvFailure::NotFound { name: text.clone() })?
        }
        None => {
            processes::current(&store(environment)?, environment)?.ok_or(RunFailure::NotCurrent)?
        }
    };
    let name = workspace.name.clone();
    let repo = repo_of(&args.repo, args.folder.as_deref())?;
    let manifest = match &args.manifest {
        Some(file) => {
            let from = match environment.current_dir() {
                Some(dir) if file.is_relative() => dir.join(file),
                _ => file.clone(),
            };
            let text = Manifest::read_named_unchecked(&from).map_err(RunFailure::Manifest)?;
            workspaces
                .check_manifest_source(&from, &root.path)
                .map_err(MxvFailure::from)?;
            Some(text)
        }
        None => None,
    };
    let _lock = processes::lock(environment, &name)?;
    let running = processes::running(environment, &workspace)?;
    if !running.is_empty() {
        return Err(RunFailure::Running {
            workspace: name.to_string(),
            processes: running,
        }
        .into());
    }
    // Adding a checkout prefixes the names of the first checkout's
    // volumes, so a volume that holds data under its old name would be
    // left behind. `add` refuses before it clones anything.
    let volumes = environment
        .runs()?
        .renamed_volumes(&workspace)
        .map_err(MxvFailure::from)?;
    if !volumes.is_empty() {
        return Err(RunFailure::VolumeRenamed {
            workspace: name.to_string(),
            volumes,
        }
        .into());
    }
    let outcome = workspaces
        .add(&name, &repo, mode, manifest.as_deref())
        .map_err(MxvFailure::from)?;
    let workspace = match outcome {
        AddOutcome::Added(workspace) | AddOutcome::Planned(workspace) => workspace,
        AddOutcome::NotFound => {
            return Err(MxvFailure::NotFound {
                name: name.to_string(),
            }
            .into());
        }
        AddOutcome::Worktrees(_) => {
            return Err(MxvFailure::Worktrees {
                name: name.to_string(),
            }
            .into());
        }
        AddOutcome::FolderTaken {
            folder, repository, ..
        } => return Err(MxvFailure::FolderTaken { repository, folder }.into()),
        AddOutcome::BranchTaken { repository, branch } => {
            return Err(MxvFailure::BranchExists { repository, branch }.into());
        }
        AddOutcome::BadFolder { folder, reason, .. } => {
            return Err(MxvFailure::BadFolder { folder, reason }.into());
        }
        AddOutcome::ManifestNeeded {
            manifest, folder, ..
        } => {
            return Err(MxvFailure::ManifestNeeded {
                name: name.to_string(),
                manifest,
                folder,
            }
            .into());
        }
        AddOutcome::ManifestRefused(error) => return Err(RunFailure::Manifest(error).into()),
        AddOutcome::Incomplete {
            folder, repository, ..
        } => {
            return Err(MxvFailure::Incomplete {
                name: name.to_string(),
                folder,
                repository,
            }
            .into());
        }
        AddOutcome::ResumeBlocked { path, reason, .. } => {
            return Err(MxvFailure::ResumeBlocked {
                name: name.to_string(),
                path,
                reason,
            }
            .into());
        }
    };
    let cloned = match workspace.checkouts.last() {
        Some(added) if !args.no_clone => {
            clone_into(environment, &workspaces, &workspace, &[added], mode, None)?
        }
        _ => Cloned::default(),
    };
    let warnings = workspace
        .manifest()
        .map(|manifest| {
            manifest
                .map(|manifest| manifest.warnings)
                .unwrap_or_default()
        })
        .unwrap_or_else(|error| vec![error.to_string()]);
    Ok(NewRow::new(&workspace, &root, cloned, None).with_warnings(&warnings))
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
            // A workspace that mxv cannot measure keeps its row with the
            // failure as its problem, and the others are still measured.
            Some(workspaces) if rows.len() < shown => match workspaces.disk_usage(workspace) {
                Ok(usage) => row.with_usage(&usage),
                Err(error) => row.with_problem(format!("mxv cannot measure it: {error}")),
            },
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
            // The disk usage already warns about a manifest file that
            // cannot be read, so only a manifest that parses adds its own
            // warnings. Two files of a merged manifest whose names clash
            // each parse, so the disk usage cannot see the clash, and the
            // merge's error is added here.
            let warnings = match workspace.manifest() {
                Ok(manifest) => manifest
                    .map(|manifest| manifest.warnings)
                    .unwrap_or_default(),
                Err(error) if matches!(error.problem, ManifestProblem::NameClash { .. }) => {
                    vec![format!(
                        "the merged manifest cannot be used, and `mxv up` fails with mxv::manifest::name_clash: {error}"
                    )]
                }
                Err(_) => Vec::new(),
            };
            Ok(ShowRow::new(&workspace, &root, &usage).with_warnings(&warnings))
        });
        batch.push(found);
    }
    Ok(batch)
}

/// Returns the directory of the workspace that `args` names, or of its
/// checkout in the folder that `args` names.
pub(crate) fn cd(environment: &Environment, args: &CdArgs) -> Result<Prose<String>, MxvFailure> {
    let name = parse_name(&args.name)?;
    let workspace = store(environment)?
        .read(&name)?
        .ok_or(MxvFailure::NotFound {
            name: name.to_string(),
        })?;
    let Some(folder) = &args.checkout else {
        return Ok(Prose::new(workspace.path.display().to_string()));
    };
    let checkout = workspace
        .checkouts
        .iter()
        .find(|checkout| &checkout.name == folder)
        .ok_or_else(|| MxvFailure::CheckoutNotFound {
            name: name.to_string(),
            checkout: folder.clone(),
            checkouts: workspace
                .checkouts
                .iter()
                .map(|checkout| checkout.name.clone())
                .collect(),
        })?;
    Ok(Prose::new(checkout.path.display().to_string()))
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
    let mut id = None;
    if let Some(workspace) = workspaces.find(&name).map_err(MxvFailure::from)? {
        id.clone_from(&workspace.id);
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
        let found = processes::volumes(environment, &workspace)?;
        if !found.is_empty() && !args.delete_volumes {
            return Err(RunFailure::Volumes {
                workspace: name.to_string(),
                volumes: found.names().to_vec(),
                allow,
            }
            .into());
        }
        volumes = Some(found);
        containers = processes::containers(environment, &workspace)?;
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
    // A repository's own mxv.toml is shared, so the paths that only it
    // makes disposable are deleted only when a person confirms them at the
    // prompt, or when `--force` allows discarding them.
    let from_manifest = !args.force && !removal.manifest.is_empty();
    // `Yes` hides its flag, and only its default holds no `--yes`.
    let passed_yes = args.yes != crumbkit::Yes::default();
    if from_manifest && passed_yes {
        return Err(MxvFailure::ManifestDisposable {
            name: name.to_string(),
            paths: removal.manifest.clone(),
            allow,
        }
        .into());
    }
    // A person may have edited the workspace manifest by hand, so the
    // question names it.
    let parts = match &removal.workspace_manifest {
        Some(path) => format!(
            "its checkouts, its workspace manifest {} and its record",
            path.display()
        ),
        None => "its checkouts and its record".to_owned(),
    };
    let mut question = if from_manifest {
        format!(
            "Remove the workspace {name}, {parts}? It deletes {}, which only the repository's mxv.toml declares disposable.",
            removal.manifest.join(", ")
        )
    } else {
        format!("Remove the workspace {name}, {parts}?")
    };
    let stashes: usize = removal
        .checkouts
        .iter()
        .map(|checkout| checkout.stashes.len())
        .sum();
    if stashes > 0 {
        question.push_str(&format!(
            " It deletes {} of the workspace's clones.",
            crate::rows::stashes(u32::try_from(stashes).unwrap_or(u32::MAX))
        ));
    }
    output.confirm(args.yes, &question)?;
    if !running.is_empty() {
        processes::stop_all(environment, &removal.workspace)?;
    }
    match workspaces.remove(removal).map_err(MxvFailure::from)? {
        RemovalOutcome::Done(removed) => {
            processes::forget(environment, &name, id.as_ref())?;
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
        RemovalPlan::IgnoredFiles {
            paths, warnings, ..
        } => MxvFailure::IgnoredFiles {
            name,
            paths,
            warnings,
            allow,
        },
        RemovalPlan::StrayCommits {
            checkout, commits, ..
        } => MxvFailure::StrayCommits {
            name,
            path: checkout,
            commits,
            allow,
        },
        RemovalPlan::Unpushed {
            checkout, commits, ..
        } => MxvFailure::Unpushed {
            name,
            path: checkout,
            commits,
            allow,
        },
        RemovalPlan::Stashed {
            checkout, stashes, ..
        } => MxvFailure::Stashed {
            name,
            path: checkout,
            stashes,
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
