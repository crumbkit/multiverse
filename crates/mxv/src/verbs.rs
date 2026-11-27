//! The workspace verbs `new`, `list`, `show`, `cd` and `remove`, and the
//! documents they print. `clean` lives in its own module.

use crumbkit::{Batch, Call, Document, Listing, Prose, Run};
use std::path::PathBuf;

use multiverse::Host;
use multiverse::health::{self, Problem};
use multiverse::{
    AddOutcome, Allow, ArchiveState, Checkout, Cloned, DEFAULT_CLONE_ON_NEW, Git, Joined,
    Lifecycle, Manifest, ManifestProblem, Mode, NewOutcome, PathGlob, Removal, RemovalPlan,
    RemoveAsked, Removed, Repo, RepoSource, Store, Workspace, WorkspaceName, WorkspaceState,
    Workspaces, checkout_prefix, prefixed,
};

use crate::cli::{AddArgs, CdArgs, ListArgs, NewArgs, RemoveArgs, ShowArgs};
use crate::clock;
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::rows::{NewRow, RemovalResult, ShowRow, WorkspaceRow};
use crate::run_failures::RunFailure;

#[cfg(test)]
mod tests;

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
    Ok(Workspaces::new(store(environment)?, Git)
        .with_host(Host::mxv())
        .with_mirrors(environment.mirrors()?))
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

/// Returns the repository or origin that the `--repo` value `path` names,
/// as [`RepoSource::from_argument`] reads it, with the folder `folder`, when
/// one is given.
fn repo_of(path: &std::path::Path, folder: Option<&str>) -> Result<Repo, MxvFailure> {
    let repo = Repo::of(RepoSource::from_argument(path));
    match folder {
        Some(folder) => repo
            .with_folder(folder)
            .map_err(|reason| MxvFailure::BadFolder {
                folder: folder.to_owned(),
                reason,
            }),
        None => Ok(repo),
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
        joined.add_checkout(&folder);
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
            .template(&templates, repo, root)
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
    output: &Run,
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
    // The lock stays held until the ignored directories are copied, so a
    // command such as `mxv remove` cannot delete the checkouts while they
    // are made. A name that a workspace already holds is reported before
    // the lock, so it does not wait for another command that holds it.
    let lock = if mode == Mode::Change {
        if let Some(found) = workspaces.find(&name).map_err(MxvFailure::from)? {
            return Err(taken(&name, &found).into());
        }
        Some(processes::lock(environment, &name)?)
    } else {
        None
    };
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
            let workspace = if mode == Mode::Change && !args.board_mount.is_empty() {
                workspaces
                    .record_board_mounts(&workspace, args.board_mount.clone())
                    .map_err(MxvFailure::from)?
            } else {
                workspace
            };
            let row = NewRow::new(&workspace, &root, cloned, from).with_warnings(&warnings);
            // The checks may take a while, so the lock is released first.
            drop(lock);
            if mode == Mode::Change {
                crate::preflight::announce(environment, &workspace);
            }
            Ok(row)
        }
        NewOutcome::NameTaken(found) => Err(taken(&name, &found).into()),
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

/// Returns the failure of `mxv new` for a name that the workspace `found`
/// holds: `creating` when an earlier `mxv new` did not finish it, so the
/// hint names `mxv remove`, and `exists` otherwise.
fn taken(name: &WorkspaceName, found: &Workspace) -> MxvFailure {
    if found.state() == WorkspaceState::Creating {
        MxvFailure::Creating {
            name: name.to_string(),
        }
    } else {
        MxvFailure::Exists {
            name: name.to_string(),
        }
    }
}

/// Clones the ignored directories of the repositories of `checkouts`, which
/// belong to `workspace`, that the config file and the manifests name,
/// into each checkout, or names them on a dry run. The workspace exists by
/// then, so a failure of the cloning becomes the warning of the result. A
/// checkout made from an origin has no repository to clone from, so it
/// receives nothing, and the result notes that once when the manifest names
/// `clone_on_new` globs.
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
        if checkout.working_source().is_none() {
            let shared = match copied {
                Some(copied) => copied.parse().ok(),
                None => workspace.workspace_manifest().ok().flatten(),
            };
            if warning.is_none() && shared.is_some_and(|manifest| !manifest.clone_on_new.is_empty())
            {
                warning = Some(format!(
                    "mxv cloned no ignored directory into {}, because it was cloned from an origin and no repository on this machine holds its ignored directories",
                    checkout.name
                ));
            }
            continue;
        }
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
/// dry run. It holds the workspace's lock, which `up` and `down` take,
/// refuses a workspace that another workspace of the name replaced while
/// it waited for the lock, as [`processes::Context::lock`] does, and
/// refuses while the workspace's units run, because
/// adding a second checkout renames the first checkout's units and ports.
pub(crate) fn add(
    output: &Run,
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
    if mode == Mode::Change {
        processes::touch_named(environment, &name);
    }
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
    // The record under the lock may differ from the one read before the
    // wait, such as in its ports or volumes, so the checks below read it.
    let (_lock, workspace) = processes::lock_workspace(environment, &workspace)?;
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
        AddOutcome::Creating(_) => {
            return Err(MxvFailure::Creating {
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
        AddOutcome::ExistingFolderInvalid { folder, reason, .. } => {
            return Err(MxvFailure::ExistingFolderInvalid {
                name: name.to_string(),
                folder,
                reason,
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

/// Returns the first page of the workspaces: a row for each broken record
/// with its repair first, such as a record that mxv cannot read or an
/// archive that has not finished, and then the other workspaces in name
/// order. With `--sizes`, each workspace's row on the page holds its
/// sizes, and only the workspaces on the page are measured.
pub(crate) fn list(
    environment: &Environment,
    args: &ListArgs,
) -> Result<Listing<WorkspaceRow>, MxvFailure> {
    if args.archived {
        return crate::archive::list(environment, args);
    }
    let records = store(environment)?.list()?;
    let archived = crate::archive::archives(environment)?.list()?;
    let broken = health::classify(&Git, &records, &archived).workspaces;
    let measure = if args.sizes {
        Some(configured(environment)?)
    } else {
        None
    };
    let shown = args.page.limit().unwrap_or(usize::MAX);
    let live = |name: &str| {
        records
            .workspaces
            .iter()
            .find(|workspace| workspace.name.as_str() == name)
    };
    let mut listed: Vec<(Option<&Workspace>, WorkspaceRow)> = broken
        .iter()
        .map(|broken| {
            if matches!(broken.problem, Problem::Unreadable { .. }) {
                return (None, WorkspaceRow::broken(broken));
            }
            if let Some(workspace) = live(&broken.name) {
                return (
                    Some(workspace),
                    WorkspaceRow::from(workspace).with_broken(broken),
                );
            }
            let row = archived
                .archives
                .iter()
                .find(|archive| archive.name.as_str() == broken.name)
                .map_or_else(
                    || WorkspaceRow::broken(broken),
                    |archive| crate::archive::row(archive).with_broken(broken),
                );
            (None, row)
        })
        .collect();
    listed.extend(
        records
            .workspaces
            .iter()
            .filter(|workspace| {
                !broken
                    .iter()
                    .any(|found| found.name == workspace.name.as_str())
            })
            .map(|workspace| (Some(workspace), WorkspaceRow::from(workspace))),
    );
    let rows = listed
        .into_iter()
        .enumerate()
        .map(|(index, (workspace, row))| match (&measure, workspace) {
            // A workspace that mxv cannot measure keeps its row with the
            // failure as its problem, unless it already has one, and the
            // others are still measured.
            (Some(workspaces), Some(workspace)) if index < shown => {
                match workspaces.disk_usage(workspace) {
                    Ok(usage) => row.with_usage(&usage),
                    Err(_) if row.has_problem() => row,
                    Err(error) => row.with_problem(format!("mxv cannot measure it: {error}")),
                }
            }
            _ => row,
        })
        .collect();
    Ok(args.page.first_of(rows).described("workspaces").step(
        crumbkit::Step::new("Show up to 100 workspaces by name in one call.")
            .call(Call::new(&["show"]).arg("names")),
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
            processes::touch_named(environment, &workspace.name);
            let usage = workspaces.disk_usage(&workspace)?;
            // The disk usage already warns about a manifest file that
            // cannot be read, so only a manifest that parses adds its own
            // warnings. Two files of a merged manifest whose names clash
            // each parse, so the disk usage cannot see the clash, and the
            // merge's error is added here.
            let row = ShowRow::new(&workspace, &root, &usage);
            let (row, warnings) = match workspace.manifest() {
                Ok(Some(manifest)) => {
                    let warnings = manifest.warnings.clone();
                    (row.with_services(&workspace, &manifest), warnings)
                }
                Ok(None) => (row, Vec::new()),
                Err(error)
                    if matches!(error.problem, ManifestProblem::NameClash { .. })
                        || (workspace.has_several_checkouts()
                            && matches!(error.problem, ManifestProblem::ServiceClash { .. })) =>
                {
                    let warning = format!(
                        "the merged manifest cannot be used, and `mxv up` fails with mxv::manifest::name_clash: {error}"
                    );
                    (row, vec![warning])
                }
                Err(_) => (row, Vec::new()),
            };
            Ok(row.with_warnings(&warnings))
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
    processes::touch_named(environment, &workspace.name);
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

/// Returns why a process outside phase works in `workspace`, which stops
/// its removal unless `allow` has `--force`. A process that phase did not
/// start loses its working directory when the checkouts go.
fn outside_user(
    name: &WorkspaceName,
    workspace: &Workspace,
    allow: Allow,
) -> Result<Option<String>, crumbkit::Error> {
    let outside = crate::archive::outside_reason(workspace).map_err(MxvFailure::from)?;
    match outside {
        Some(reason) if !allow.discard_changes => Err(MxvFailure::RemoveInUse {
            name: name.to_string(),
            reason,
            allow,
        }
        .into()),
        other => Ok(other),
    }
}

/// Returns the sentence of a removal's question that counts the stash
/// entries of the clones that it deletes, or nothing.
fn stash_clause(removal: &Removal) -> String {
    let stashes: usize = removal
        .checkouts
        .iter()
        .map(|checkout| checkout.stashes().len())
        .sum();
    if stashes == 0 {
        return String::new();
    }
    format!(
        " It deletes {} of the workspace's clones.",
        crate::rows::stashes(u32::try_from(stashes).unwrap_or(u32::MAX))
    )
}

/// Returns the sentence of a removal's question that names the process
/// outside phase that `outside` gives, or nothing.
fn outside_clause(outside: Option<&str>) -> String {
    outside.map_or_else(String::new, |reason| format!(" It is in use: {reason}."))
}

/// Removes the workspace that `args` names, after the person confirms, or
/// reports what removing it would do on a dry run. The removal plans again
/// after the confirmation, and refuses when the workspace changed while
/// mxv waited.
pub(crate) fn remove(
    output: &mut Run,
    environment: &Environment,
    args: &RemoveArgs,
    served: &mut crate::proxy::Served,
) -> Result<Prose<RemovalResult>, crumbkit::Error> {
    if args.archived {
        return crate::archive::remove(output, environment, args);
    }
    let name = parse_name(&args.name)?;
    let workspaces = configured(environment)?;
    // Running processes stop the removal unless it is forced, and a forced
    // removal stops them after the person confirms.
    let allow = Allow {
        discard_changes: args.force,
        delete_unmerged: args.delete_unmerged,
    };
    let mut volumes = None;
    let mut containers = Vec::new();
    let mut outside = None;
    if let Some(workspace) = workspaces.find(&name).map_err(MxvFailure::from)? {
        let running = processes::running(environment, &workspace)?;
        if !running.is_empty() && !args.force {
            return Err(RunFailure::Running {
                workspace: name.to_string(),
                processes: running,
            }
            .into());
        }
        // Phase's own units work in the checkouts, so a workspace with
        // running units has no process outside phase to look for.
        if running.is_empty() {
            outside = outside_user(&name, &workspace, allow)?;
        }
        // A volume may hold the only copy of a database, so removing it
        // needs its own flag, and `--force` alone keeps the workspace.
        let found = processes::volumes(environment, &workspace)?;
        if !found.is_empty() && !args.delete_volumes {
            return Err(RunFailure::Volumes {
                workspace: name.to_string(),
                volumes: found.names(),
                allow,
            }
            .into());
        }
        volumes = Some(found);
        containers = processes::containers(environment, &workspace)?;
    }
    let names: Vec<String> = volumes
        .as_ref()
        .map(|volumes| volumes.names())
        .unwrap_or_default();
    let plan = workspaces
        .plan_removal(&name, allow)
        .map_err(MxvFailure::from)?;
    let planned = output.is_dry_run();
    let gone = || -> Result<Prose<RemovalResult>, crumbkit::Error> {
        let path = environment.root()?.path.join(name.as_str());
        Ok(Prose::new(RemovalResult::unchanged(
            &name, &path, false, planned,
        )))
    };
    let removal = match plan {
        RemovalPlan::Ready(removal) => removal,
        RemovalPlan::NotFound => return gone(),
        refused @ (RemovalPlan::Dirty { .. }
        | RemovalPlan::IgnoredFiles { .. }
        | RemovalPlan::StrayCommits { .. }
        | RemovalPlan::Unpushed { .. }
        | RemovalPlan::Stashed { .. }
        | RemovalPlan::Locked { .. }
        | RemovalPlan::Unowned { .. }) => return Err(refusal(&name, allow, refused).into()),
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
    question.push_str(&stash_clause(&removal));
    question.push_str(&outside_clause(outside.as_deref()));
    if args.delete_volumes {
        question.push_str(&crate::archive::volumes_clause(&names));
    }
    output.confirm(args.yes, &question)?;
    let runs = environment.runs()?;
    let archives = crate::archive::archives(environment)?;
    let asked = RemoveAsked::new(args.force, names.clone());
    let outcome = Lifecycle::new(&workspaces, &archives, &runs)
        .with_in_use(crate::archive::outside_reason)
        .remove(removal, &asked)
        .map_err(MxvFailure::from)?;
    match outcome {
        Removed::Done(done) => {
            let removed = done.removal;
            let mut held = processes::held_sentences(&name, done.held);
            crate::proxy::sync(
                environment,
                &multiverse::Store::new(environment.records()?),
                &removed.workspace,
                multiverse::SyncMode::IfRunning,
                served,
            );
            held.extend(stopped_archive(environment, &removed.workspace)?);
            Ok(Prose::new(
                RemovalResult::new(&removed, false)
                    .with_docker(containers, names)
                    .with_warnings(held),
            ))
        }
        // Another removal finished after the question.
        Removed::Unchanged | Removed::Refused(RemovalPlan::NotFound) => gone(),
        Removed::Changed | Removed::Refused(RemovalPlan::Ready(_)) => Err(MxvFailure::Changed {
            name: name.to_string(),
            allow,
        }
        .into()),
        Removed::Refused(refused) => Err(refusal(&name, allow, refused).into()),
        Removed::Running(processes) => Err(RunFailure::Running {
            workspace: name.to_string(),
            processes,
        }
        .into()),
        Removed::InUse(reason) => Err(MxvFailure::RemoveInUse {
            name: name.to_string(),
            reason,
            allow,
        }
        .into()),
        Removed::Unstoppable(unstoppable) => Err(RunFailure::Unstoppable {
            process: unstoppable.process,
            pid: unstoppable.pid,
            unverified: unstoppable.unverified,
            not_started: Vec::new(),
        }
        .into()),
    }
}

/// Returns the warning of a removal of `workspace`, which an archive had
/// started to retire, about the archive record of its name that stays: a
/// retiring one can no longer be resumed, and a retired one, or a retiring
/// one that an older mxv wrote, is still finished by `mxv archive -w`. The
/// archive is the workspace's when `health::same_workspace` says so: the
/// workspace is retiring into it, or the two records hold the same stable
/// id, which covers a record whose state an older mxv dropped. Any other
/// workspace, or an archive record that does not exist or cannot be read,
/// has no warning, and `mxv list --archived` names a record that cannot
/// be read.
fn stopped_archive(
    environment: &Environment,
    workspace: &Workspace,
) -> Result<Vec<String>, crumbkit::Error> {
    let name = &workspace.name;
    let Ok(Some(archive)) = crate::archive::archives(environment)?.read(name) else {
        return Ok(Vec::new());
    };
    if archive.state == ArchiveState::Archived || !health::same_workspace(&archive, workspace) {
        return Ok(Vec::new());
    }
    Ok(if archive.counts_as_retired() {
        vec![format!(
            "the archive of {name} retired every checkout and did not finish; `mxv archive -w {name}` finishes it"
        )]
    } else {
        vec![format!(
            "the archive of {name} stopped before it retired every checkout, so it can no longer be resumed; `mxv repair {name}` moves it aside"
        )]
    })
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
            checkout,
            commits,
            fetched,
            ..
        } => MxvFailure::Unpushed {
            name,
            path: checkout,
            commits,
            fetched_ago: fetched.map(|time| crumbkit::Age::since(time, clock::wall_now())),
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
