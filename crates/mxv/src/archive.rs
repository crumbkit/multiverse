//! The verbs `archive` and `restore`, the archive form of `list` and
//! `remove`, and the documents they print.
//!
//! The library in [`multiverse`] decides what an archive keeps and carries
//! it out. This module holds what only a command can do: the workspace's
//! lock, the question, stopping the workspace's processes, the check for
//! outside processes, and the docker volumes.

use std::collections::BTreeSet;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use crumbkit::{Document, DryRun, Listing, Output, Prose, Yes};
use multiverse::phase::host::TableEntry;
use multiverse::{
    Archive, ArchiveOptions, ArchiveOutcome, ArchivePlan, ArchivePlanned, ArchiveRecords,
    ArchiveState, ArchivedCheckout, Archives, Git, Mode, RestoreOutcome, RestorePlanned, Volumes,
    Workspace, WorkspaceName, Workspaces,
};
use schemars::JsonSchema;
use serde::Serialize;

use crate::cli::{ListArgs, RemoveArgs};
use crate::environment::Environment;
use crate::failures::MxvFailure;
use crate::processes;
use crate::reclaim::{user_reason, users};
use crate::rows::{RemovalResult, WorkspaceRow};
use crate::run_failures::RunFailure;
use crate::verbs::{configured, parse_name};

mod failure;
#[cfg(test)]
mod tests;

pub(crate) use failure::CODES;
pub use failure::{
    ARCHIVE_CHECKOUT_UNUSABLE, ARCHIVE_IN_USE, ARCHIVE_MANIFEST_DISPOSABLE, ARCHIVE_MIRROR_MISSING,
    ARCHIVE_NAME_TAKEN, ARCHIVE_NOT_FOUND, ARCHIVE_SOURCE_CHANGED, ARCHIVE_UNFINISHED,
    ARCHIVE_UNSAVED_WORK, RESTORE_BRANCH_MOVED, RESTORE_COMMIT_MISSING, RESTORE_NAME_TAKEN,
    RESTORE_NOT_FOUND, RESTORE_ROOT_FILE_CHANGED, RESTORE_UNFINISHED,
};
use failure::{ArchiveFailure, archive_refusal, restore_refusal};

/// The document that `mxv archive` prints.
pub(crate) const ARCHIVE: Document<Prose<ArchiveResult>> = Document::new(
    "mxv",
    "archive",
    1,
    "What the archive kept and where each checkout's commit is pinned, or what it would do on a dry run.",
);

/// The document that `mxv restore` prints.
pub(crate) const RESTORE: Document<Prose<RestoreResult>> = Document::new(
    "mxv",
    "restore",
    1,
    "The workspace that the restore rebuilt and each port that changed, or what it would do on a dry run.",
);

/// The arguments of `mxv archive`.
#[derive(Debug, Args)]
pub(crate) struct ArchiveArgs {
    /// The workspace, which defaults to the one that holds the current directory
    #[arg(short, long, value_name = "WORKSPACE")]
    pub(crate) workspace: Option<String>,

    /// Commit uncommitted changes and untracked files onto the branch first; ignored files stay untouched
    #[arg(long, conflicts_with = "force")]
    pub(crate) wip: bool,

    /// Lose what the archive cannot keep, such as uncommitted changes, ignored files such as .env and stashes, and list it in the archive
    #[arg(long)]
    pub(crate) force: bool,

    /// Delete the workspace's docker volumes and the data in them; without this flag the volumes stay for the restore
    #[arg(long)]
    pub(crate) delete_volumes: bool,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Archive the workspace without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// The arguments of `mxv restore`.
#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    /// The name of the archived workspace
    pub(crate) name: String,

    /// Print what the command would do, and change nothing.
    #[command(flatten)]
    pub(crate) dry_run: DryRun,

    /// Restore the workspace without asking.
    #[command(flatten)]
    pub(crate) yes: Yes,
}

/// Where one checkout's commit is kept.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PinRow {
    /// The checkout's folder.
    folder: String,
    /// The branch of the checkout, or null for a detached `HEAD`.
    branch: Option<String>,
    /// The commit that the checkout's `HEAD` named.
    commit: String,
    /// The repository or mirror that holds the reference.
    repository: String,
    /// The reference that keeps the commit.
    reference: String,
}

/// What `mxv archive` did, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ArchiveResult {
    /// The workspace's name.
    name: String,
    /// The workspace's directory.
    path: String,
    /// Where each checkout's commit is kept.
    pins: Vec<PinRow>,
    /// Each commit that exists only on this machine, with the checkout's
    /// folder in front. The archive pushes nothing.
    local_only: Vec<String>,
    /// The paths that `--wip` committed, or would commit.
    wip: Vec<String>,
    /// What `--force` lost, or would lose.
    lost: Vec<String>,
    /// The workspace's processes that the run stopped, or would stop.
    stopped: Vec<String>,
    /// The ignored build directories that the run deleted, or would delete,
    /// because a build makes them again.
    disposable: Vec<String>,
    /// The extra entries of the workspace's directory that the archive
    /// holds.
    root_files: Vec<String>,
    /// The docker volumes that `--delete-volumes` deleted.
    volumes: Vec<String>,
    /// What the run could not read or finish, one sentence for each.
    warnings: Vec<String>,
    /// The command that brings the workspace back.
    restore: String,
    /// The directory to change to, when the current directory was inside
    /// the workspace, or null.
    cd: Option<String>,
    /// Whether the result reports a dry run, which changed nothing.
    #[serde(skip)]
    planned: bool,
}

impl fmt::Display for ArchiveResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let (archived, stops, deletes, commits, loses, keeps, is) = if self.planned {
            (
                "would archive",
                "would stop",
                "would delete",
                "would commit",
                "would lose",
                "would keep",
                "would stay",
            )
        } else {
            (
                "archived",
                "stopped",
                "deleted",
                "committed",
                "lost",
                "kept",
                "stays",
            )
        };
        for process in &self.stopped {
            writeln!(formatter, "mxv {stops} the process {process}.")?;
        }
        for path in &self.wip {
            writeln!(formatter, "mxv {commits} {path}.")?;
        }
        for path in &self.lost {
            writeln!(formatter, "mxv {loses} {path}.")?;
        }
        for path in &self.disposable {
            writeln!(
                formatter,
                "mxv {deletes} the ignored build directory {path}."
            )?;
        }
        for volume in &self.volumes {
            writeln!(formatter, "mxv {deletes} the docker volume {volume}.")?;
        }
        for path in &self.root_files {
            writeln!(formatter, "mxv {keeps} the file {path} in the archive.")?;
        }
        for pin in &self.pins {
            writeln!(
                formatter,
                "The commit {} of {} {is} in {} as {}.",
                pin.commit, pin.folder, pin.repository, pin.reference
            )?;
        }
        for commit in &self.local_only {
            writeln!(
                formatter,
                "The commit {commit} exists only on this machine."
            )?;
        }
        writeln!(formatter, "mxv {archived} the workspace {}.", self.name)?;
        writeln!(formatter, "`{}` brings it back.", self.restore)?;
        if let Some(dir) = &self.cd {
            write!(
                formatter,
                "The current directory was inside the workspace. Change to {dir}."
            )?;
        }
        Ok(())
    }
}

/// One port that a restore could not keep.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PortRow {
    /// The port's name in the manifest.
    name: String,
    /// The port that the workspace held.
    from: u16,
    /// The port that it holds now.
    to: u16,
}

/// One checkout that a restore rebuilt.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RestoredCheckout {
    /// The checkout's folder.
    folder: String,
    /// The checkout's directory.
    path: String,
    /// The checkout's branch, or null for a detached `HEAD`.
    branch: Option<String>,
    /// The commit that the checkout is at.
    commit: String,
}

/// What `mxv restore` did, or would do on a dry run.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct RestoreResult {
    /// The workspace's name.
    name: String,
    /// The workspace's directory.
    path: String,
    /// The checkouts that the restore rebuilt.
    checkouts: Vec<RestoredCheckout>,
    /// Each port that the restore gave a new number.
    ports: Vec<PortRow>,
    /// The folder of each clone whose mirror mxv clones again from its
    /// origin.
    missing_mirrors: Vec<String>,
    /// What the run could not finish, one sentence for each.
    warnings: Vec<String>,
    /// Whether the result reports a dry run, which changed nothing.
    #[serde(skip)]
    planned: bool,
}

impl fmt::Display for RestoreResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for warning in &self.warnings {
            writeln!(formatter, "Warning: {warning}.")?;
        }
        let (restored, gives, clones) = if self.planned {
            ("would restore", "would give", "would clone")
        } else {
            ("restored", "gave", "cloned")
        };
        for folder in &self.missing_mirrors {
            writeln!(formatter, "mxv {clones} the mirror of {folder} again.")?;
        }
        for checkout in &self.checkouts {
            let place = checkout.branch.as_ref().map_or_else(
                || format!("detached at {}", checkout.commit),
                |branch| format!("on {branch} at {}", checkout.commit),
            );
            writeln!(formatter, "The checkout {} is {place}.", checkout.path)?;
        }
        for port in &self.ports {
            writeln!(
                formatter,
                "mxv {gives} {} the port {}, because {} is taken.",
                port.name, port.to, port.from
            )?;
        }
        write!(
            formatter,
            "mxv {restored} the workspace {} at {}.",
            self.name, self.path
        )
    }
}

/// Returns the archives that `environment` names.
fn archives(environment: &Environment) -> Result<Archives, MxvFailure> {
    Ok(Archives::new(environment.archives()?))
}

/// Returns the pids of the ancestors of `own` in `table`, which walks the
/// parents with the stop rules of [`crate::reclaim::owned`]: a parent of 1
/// or 0 belongs to the system, and a walk longer than the table has met a
/// loop.
pub(crate) fn ancestors(table: &[TableEntry], own: u32) -> BTreeSet<u32> {
    let parents: std::collections::BTreeMap<u32, u32> = table
        .iter()
        .map(|entry| (entry.pid, entry.parent))
        .collect();
    let mut found = BTreeSet::new();
    let mut pid = own;
    for _ in 0..=parents.len() {
        match parents.get(&pid) {
            Some(parent) if *parent > 1 && *parent != pid => {
                found.insert(*parent);
                pid = *parent;
            }
            _ => break,
        }
    }
    found
}

/// Returns why a process outside phase keeps `workspace` from being
/// archived, or `None`. The shell or agent that started this command may
/// stand in the workspace, so the ancestors of this process do not count.
/// A background job of that shell is no ancestor, and it counts.
fn outside_user(workspace: &Workspace) -> Result<Option<String>, MxvFailure> {
    let table = multiverse::process_table().map_err(MxvFailure::from)?;
    let own = std::process::id();
    let ancestors = ancestors(&table, own);
    let mut found = users(&table, &workspace.path, &[], own);
    found.retain(|(pid, _)| !ancestors.contains(pid));
    Ok(user_reason(&found))
}

/// Returns the name of the workspace that `args` names, or else of the one
/// that holds the current directory.
fn chosen(environment: &Environment, args: &ArchiveArgs) -> Result<WorkspaceName, crumbkit::Error> {
    if let Some(text) = &args.workspace {
        return Ok(parse_name(text)?);
    }
    let store = multiverse::Store::new(environment.records()?);
    match processes::current(&store, environment)? {
        Some(workspace) => Ok(workspace.name),
        None => Err(RunFailure::NotCurrent.into()),
    }
}

/// Returns the directory to change to after an archive of a workspace at
/// `path`, when the current directory is inside it.
fn cd_target(environment: &Environment, path: &std::path::Path) -> Option<String> {
    let current = environment.current_dir()?;
    let current = current
        .canonicalize()
        .unwrap_or_else(|_| current.to_path_buf());
    let inside = current.starts_with(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    inside.then(|| path.parent().unwrap_or(path).display().to_string())
}

/// What an archive did beyond the record, for its result.
#[derive(Default)]
struct Done {
    /// The processes that the run stopped.
    stopped: Vec<String>,
    /// The disposable paths.
    disposable: Vec<String>,
    /// The volumes that the run deleted.
    volumes: Vec<String>,
    /// The warnings.
    warnings: Vec<String>,
    /// The directory to change to.
    cd: Option<String>,
}

/// Returns the result of the archive of the checkouts of `archive`.
fn result_of(archive: &Archive, wip: Vec<String>, done: Done, planned: bool) -> ArchiveResult {
    ArchiveResult {
        name: archive.name.to_string(),
        path: archive.path.display().to_string(),
        pins: pin_rows(&archive.checkouts),
        local_only: local_only(&archive.checkouts),
        wip,
        lost: archive.lost.clone(),
        stopped: done.stopped,
        disposable: done.disposable,
        root_files: archive
            .root
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
        volumes: done.volumes,
        warnings: done.warnings,
        restore: format!("mxv restore {}", archive.name),
        cd: done.cd,
        planned,
    }
}

/// Returns where each checkout's commit is kept.
fn pin_rows(checkouts: &[ArchivedCheckout]) -> Vec<PinRow> {
    checkouts
        .iter()
        .map(|checkout| PinRow {
            folder: checkout.folder.clone(),
            branch: (!checkout.detached).then(|| checkout.branch.clone()),
            commit: checkout.commit.clone(),
            repository: checkout.pin_repository().display().to_string(),
            reference: checkout.pin.clone(),
        })
        .collect()
}

/// Returns each commit that exists only on this machine, with the
/// checkout's folder in front.
fn local_only(checkouts: &[ArchivedCheckout]) -> Vec<String> {
    checkouts
        .iter()
        .flat_map(|checkout| {
            checkout
                .local_only
                .iter()
                .map(|commit| format!("{}: {commit}", checkout.folder))
        })
        .collect()
}

/// Returns the result of a dry run of `plan`.
fn dry_result(plan: &ArchivePlan, running: Vec<String>, cd: Option<String>) -> ArchiveResult {
    let workspace = &plan.removal.workspace;
    ArchiveResult {
        name: workspace.name.to_string(),
        path: workspace.path.display().to_string(),
        pins: pin_rows(&plan.checkouts),
        local_only: local_only(&plan.checkouts),
        wip: plan.wip.clone(),
        lost: plan.lost.clone(),
        stopped: running,
        disposable: plan.removal.disposable.clone(),
        root_files: plan.root.clone(),
        volumes: Vec::new(),
        warnings: plan.warnings.clone(),
        restore: format!("mxv restore {}", workspace.name),
        cd,
        planned: true,
    }
}

/// Returns the question that an archive of `plan` asks.
fn question(plan: &ArchivePlan, running: &[String], delete_volumes: bool) -> String {
    let workspace = &plan.removal.workspace;
    let mut text = format!(
        "Archive the workspace {}? It keeps each checkout's commit in a pin and deletes {}.",
        workspace.name,
        workspace.path.display()
    );
    if !running.is_empty() {
        text.push_str(&format!(" It stops {}.", running.join(", ")));
    }
    if !plan.removal.manifest.is_empty() {
        text.push_str(&format!(
            " It deletes {}, which only the repository's mxv.toml declares disposable.",
            plan.removal.manifest.join(", ")
        ));
    }
    if !plan.lost.is_empty() {
        text.push_str(&format!(" It loses {}.", plan.lost.join(", ")));
    }
    if delete_volumes {
        text.push_str(" It deletes the workspace's docker volumes and the data in them.");
    }
    text
}

/// Returns the failure when a process outside phase uses `workspace`.
fn refuse_outsider(workspace: &Workspace) -> Result<(), crumbkit::Error> {
    match outside_user(workspace)? {
        Some(reason) => Err(ArchiveFailure::InUse {
            name: workspace.name.to_string(),
            reason,
        }
        .into()),
        None => Ok(()),
    }
}

/// Finishes the archive of `archive`, whose workspace is gone: deletes the
/// run files and the volumes that the run asked to delete, and marks it.
fn finish(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    records: &Archives,
    archive: &mut Archive,
    volumes: Option<Volumes>,
) -> Result<Vec<String>, crumbkit::Error> {
    processes::forget(environment, &archive.name, archive.workspace_id.as_ref())?;
    let mut names = Vec::new();
    if let Some(volumes) = volumes {
        names = volumes.names().to_vec();
        processes::delete_volumes(environment, volumes)?;
    }
    workspaces
        .finish_archive(records, archive)
        .map_err(MxvFailure::from)?;
    Ok(names)
}

/// Archives the workspace that `args` names.
pub(crate) fn archive(
    output: &mut Output,
    environment: &Environment,
    args: &ArchiveArgs,
) -> Result<Prose<ArchiveResult>, crumbkit::Error> {
    let name = chosen(environment, args)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let runs = environment.runs()?;
    let options = ArchiveOptions::new(args.wip, args.force, runs.containers_ran(&name));
    if output.is_dry_run() {
        return dry_run(environment, &workspaces, &records, &name, options);
    }
    let lock = processes::lock(environment, &name)?;
    let plan = match workspaces
        .plan_archive(&records, &name, options, Mode::Change)
        .map_err(MxvFailure::from)?
    {
        ArchivePlanned::Ready(plan) => *plan,
        ArchivePlanned::Refused(refusal) => return Err(archive_refusal(&name, refusal).into()),
        ArchivePlanned::Finish(mut archive) => {
            let mut done = Done {
                cd: cd_target(environment, &archive.path),
                ..Done::default()
            };
            let volumes = stale_volumes(environment, &archive, args.delete_volumes)?;
            done.volumes = finish(environment, &workspaces, &records, &mut archive, volumes)?;
            return Ok(Prose::new(result_of(&archive, Vec::new(), done, false)));
        }
    };
    let workspace = plan.removal.workspace.clone();
    let running = processes::running(environment, &workspace)?;
    if running.is_empty() {
        refuse_outsider(&workspace)?;
    }
    let cd = cd_target(environment, &workspace.path);
    let passed_yes = args.yes != Yes::default();
    if !args.force && !plan.removal.manifest.is_empty() && passed_yes {
        return Err(manifest_disposable(&name, &plan).into());
    }
    output.confirm(args.yes, &question(&plan, &running, args.delete_volumes))?;
    if !running.is_empty() {
        processes::stop_all_locked(environment, &workspace, &lock)?;
        refuse_outsider(&workspace)?;
    }
    let volumes = if args.delete_volumes {
        Some(processes::volumes(environment, &workspace)?)
    } else {
        None
    };
    let wip = plan.wip.clone();
    let mut warnings = plan.warnings.clone();
    let (mut archive, removal) = match workspaces
        .archive(&records, plan)
        .map_err(MxvFailure::from)?
    {
        ArchiveOutcome::Retired { archive, removal } => (*archive, *removal),
        ArchiveOutcome::Refused(refusal) => return Err(archive_refusal(&name, refusal).into()),
    };
    warnings.extend(removal.warnings.iter().cloned());
    let volumes = finish(environment, &workspaces, &records, &mut archive, volumes)?;
    drop(lock);
    let done = Done {
        stopped: running,
        disposable: removal.disposable,
        volumes,
        warnings,
        cd,
    };
    Ok(Prose::new(result_of(&archive, wip, done, false)))
}

/// Returns what an archive of the workspace `name` would do, and changes
/// nothing, so it takes no lock.
fn dry_run(
    environment: &Environment,
    workspaces: &Workspaces<Git>,
    records: &Archives,
    name: &WorkspaceName,
    options: ArchiveOptions,
) -> Result<Prose<ArchiveResult>, crumbkit::Error> {
    let plan = match workspaces
        .plan_archive(records, name, options, Mode::DryRun)
        .map_err(MxvFailure::from)?
    {
        ArchivePlanned::Ready(plan) => *plan,
        ArchivePlanned::Refused(refusal) => return Err(archive_refusal(name, refusal).into()),
        ArchivePlanned::Finish(archive) => {
            let done = Done {
                cd: cd_target(environment, &archive.path),
                ..Done::default()
            };
            return Ok(Prose::new(result_of(&archive, Vec::new(), done, true)));
        }
    };
    let workspace = &plan.removal.workspace;
    let running = processes::running(environment, workspace)?;
    if running.is_empty() {
        refuse_outsider(workspace)?;
    }
    let cd = cd_target(environment, &workspace.path);
    Ok(Prose::new(dry_result(&plan, running, cd)))
}

/// Returns the failure of `--yes` with the paths of `plan` that only a
/// repository's own `mxv.toml` declares disposable.
fn manifest_disposable(name: &WorkspaceName, plan: &ArchivePlan) -> ArchiveFailure {
    ArchiveFailure::ManifestDisposable {
        name: name.to_string(),
        paths: plan.removal.manifest.clone(),
    }
}

/// Returns the volumes of an archive that an earlier run stopped before it
/// deleted them, when the person asks again.
fn stale_volumes(
    environment: &Environment,
    archive: &Archive,
    delete: bool,
) -> Result<Option<Volumes>, crumbkit::Error> {
    match (&archive.workspace_id, delete && archive.containers_ran) {
        (Some(id), true) => Ok(Some(
            environment
                .runs()?
                .archived_volumes(&archive.name, id)
                .map_err(MxvFailure::from)?,
        )),
        _ => Ok(None),
    }
}

/// Restores the workspace that `args` names.
pub(crate) fn restore(
    output: &mut Output,
    environment: &Environment,
    args: &RestoreArgs,
) -> Result<Prose<RestoreResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let runs = environment.runs()?;
    let plan = match workspaces
        .plan_restore(&records, &name)
        .map_err(MxvFailure::from)?
    {
        RestorePlanned::Ready(plan) => *plan,
        RestorePlanned::Refused(refusal) => return Err(restore_refusal(&name, refusal).into()),
    };
    let store = multiverse::Store::new(environment.records()?);
    if output.is_dry_run() {
        let ports = runs
            .planned_ports(&plan.archive.workspace(), &store)
            .map_err(MxvFailure::from)?;
        return Ok(Prose::new(RestoreResult {
            name: name.to_string(),
            path: plan.archive.path.display().to_string(),
            checkouts: restored_checkouts(&plan.archive),
            ports: port_rows(ports),
            missing_mirrors: plan.missing_mirrors,
            warnings: Vec::new(),
            planned: true,
        }));
    }
    output.confirm(
        args.yes,
        &format!(
            "Restore the workspace {name} at {}?",
            plan.archive.path.display()
        ),
    )?;
    // The lock comes after the question, so a prompt holds up nobody, and
    // the library reads the archive again under it.
    let lock = processes::lock(environment, &name)?;
    let archive = plan.archive.clone();
    let missing_mirrors = plan.missing_mirrors.clone();
    let restored = match workspaces
        .restore(&records, plan, &runs)
        .map_err(MxvFailure::from)?
    {
        RestoreOutcome::Restored(restored) => *restored,
        RestoreOutcome::Refused(refusal) => return Err(restore_refusal(&name, refusal).into()),
    };
    drop(lock);
    Ok(Prose::new(RestoreResult {
        name: name.to_string(),
        path: archive.path.display().to_string(),
        checkouts: restored_checkouts(&archive),
        ports: port_rows(restored.ports),
        missing_mirrors,
        warnings: restored.warnings,
        planned: false,
    }))
}

/// Returns the checkouts of `archive` as the rows of a restore.
fn restored_checkouts(archive: &Archive) -> Vec<RestoredCheckout> {
    archive
        .checkouts
        .iter()
        .map(|checkout| RestoredCheckout {
            folder: checkout.folder.clone(),
            path: checkout.path.display().to_string(),
            branch: (!checkout.detached).then(|| checkout.branch.clone()),
            commit: checkout.commit.clone(),
        })
        .collect()
}

/// Returns `changes` as rows.
fn port_rows(changes: Vec<multiverse::PortChange>) -> Vec<PortRow> {
    changes
        .into_iter()
        .map(|change| PortRow {
            name: change.name,
            from: change.from,
            to: change.to,
        })
        .collect()
}

/// The problem of a row whose archive has not finished.
fn unfinished_problem(name: &str) -> String {
    format!("archiving stopped partway; `mxv archive -w {name}` finishes it")
}

/// Returns the age of `archive`.
fn age(archive: &Archive) -> crumbkit::Age {
    let now = SystemTime::now();
    crumbkit::Age::since(UNIX_EPOCH + Duration::from_secs(archive.time), now)
}

/// Returns the row of `archive`.
fn row(archive: &Archive) -> WorkspaceRow {
    let local = archive
        .checkouts
        .iter()
        .map(|checkout| checkout.local_only.len())
        .sum();
    let row = WorkspaceRow::from(&archive.workspace()).archived(age(archive), local);
    if archive.state == ArchiveState::Retiring {
        row.with_problem(unfinished_problem(archive.name.as_str()))
    } else {
        row
    }
}

/// Returns each archive as a row, the degraded rows first.
pub(crate) fn list(
    environment: &Environment,
    args: &ListArgs,
) -> Result<Listing<WorkspaceRow>, MxvFailure> {
    let records: ArchiveRecords = archives(environment)?.list()?;
    let (found, problems) = (records.archives, records.problems);
    let mut rows: Vec<WorkspaceRow> = problems.iter().map(WorkspaceRow::unreadable).collect();
    let (retiring, finished): (Vec<&Archive>, Vec<&Archive>) = found
        .iter()
        .partition(|archive| archive.state == ArchiveState::Retiring);
    rows.extend(retiring.into_iter().map(row));
    rows.extend(finished.into_iter().map(row));
    Ok(args.page.first_of(rows).described("archives").next(
        crumbkit::Call::new(&["restore"]).arg("name"),
        "rebuilds an archived workspace.",
    ))
}

/// Puts the row of each archive that has not finished at the front of
/// `rows`. A workspace whose record is still there keeps one row, with the
/// problem added, so a listing never shows a name twice.
pub(crate) fn mark_unfinished(
    environment: &Environment,
    rows: &mut Vec<WorkspaceRow>,
) -> Result<(), MxvFailure> {
    let found = archives(environment)?.list()?.archives;
    for archive in found
        .iter()
        .rev()
        .filter(|archive| archive.state == ArchiveState::Retiring)
    {
        let name = archive.name.as_str();
        let front = match rows.iter().position(|row| row.name() == name) {
            Some(index) => rows.remove(index).with_problem(unfinished_problem(name)),
            None => row(archive),
        };
        rows.insert(0, front);
    }
    Ok(())
}

/// Deletes the archive that `args` names.
pub(crate) fn remove(
    output: &mut Output,
    environment: &Environment,
    args: &RemoveArgs,
) -> Result<Prose<RemovalResult>, crumbkit::Error> {
    let name = parse_name(&args.name)?;
    let workspaces = configured(environment)?;
    let records = archives(environment)?;
    let Some(archive) = records.read(&name).map_err(MxvFailure::from)? else {
        return Err(ArchiveFailure::NotFound {
            name: name.to_string(),
        }
        .into());
    };
    if archive.state == ArchiveState::Retiring {
        return Err(ArchiveFailure::Unfinished {
            name: name.to_string(),
        }
        .into());
    }
    let (volumes, mut warnings) = archive_volumes(environment, &archive, args.delete_volumes);
    let names = volumes
        .as_ref()
        .map(|volumes| volumes.names().to_vec())
        .unwrap_or_default();
    if output.is_dry_run() {
        return Ok(Prose::new(RemovalResult::of_archive(
            &archive, names, warnings, true,
        )));
    }
    let mut text = format!(
        "Delete the archive {name} and its pins? `mxv restore` cannot bring the workspace back afterwards."
    );
    if !names.is_empty() {
        text.push_str(" It deletes the workspace's docker volumes and the data in them.");
    }
    output.confirm(args.yes, &text)?;
    let lock = processes::lock(environment, &name)?;
    let removed = workspaces
        .remove_archive(&records, archive, Mode::Change)
        .map_err(MxvFailure::from)?;
    warnings.extend(removed.warnings);
    if let Some(volumes) = volumes {
        processes::delete_volumes(environment, volumes)?;
    }
    drop(lock);
    Ok(Prose::new(RemovalResult::of_archive(
        &removed.archive,
        names,
        warnings,
        false,
    )))
}

/// Returns the volumes of `archive` that `delete` asks to delete, and the
/// warning about volumes that stay. A docker that does not answer becomes
/// a warning when no volume is to be deleted, because the removal of the
/// archive does not depend on it.
fn archive_volumes(
    environment: &Environment,
    archive: &Archive,
    delete: bool,
) -> (Option<Volumes>, Vec<String>) {
    let Some(id) = archive
        .workspace_id
        .as_ref()
        .filter(|_| archive.containers_ran)
    else {
        return (None, Vec::new());
    };
    let found = environment
        .runs()
        .map_err(|failure| failure.to_string())
        .and_then(|runs| {
            runs.archived_volumes(&archive.name, id)
                .map_err(|error| error.to_string())
        });
    match found {
        Ok(volumes) if delete => (Some(volumes), Vec::new()),
        Ok(volumes) if volumes.is_empty() => (None, Vec::new()),
        Ok(volumes) => (
            None,
            vec![format!(
                "the docker volumes {} stay; `--delete-volumes` deletes them",
                volumes.names().join(", ")
            )],
        ),
        Err(reason) => (
            None,
            vec![format!(
                "mxv could not read the workspace's docker volumes, which stay: {reason}"
            )],
        ),
    }
}
