//! The tests of workspaces, archives and restores under a host profile
//! other than mxv's.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::archive::{
    Archive, ArchiveOptions, ArchiveOutcome, ArchivePlanned, Archives, RestoreOptions,
    RestoreOutcome, RestorePlanned,
};
use crate::git::Git;
use crate::host::Host;
use crate::lookup::ManifestSource;
use crate::name::WorkspaceName;
use crate::record::Workspace;
use crate::runs::Runs;
use crate::store::Store;
use crate::testing::{Fixture, fixture, git, write};
use crate::workspaces::{Mode, NewOutcome, Workspaces};

/// The workspaces, archives and runs of one host, in a state directory of
/// their own, which may share a directory of mirrors with another host's.
struct Hosted {
    root: PathBuf,
    workspaces: Workspaces<Git>,
    archives: Archives,
    runs: Runs,
}

/// Returns the workspaces, archives and runs of `host` below `base`, whose
/// clones borrow objects from the mirrors in `mirrors`.
fn hosted(base: &Path, mirrors: &Path, host: &Host) -> Hosted {
    Hosted {
        root: base.join("work"),
        workspaces: Workspaces::new(Store::new(base.join("state")), Git)
            .with_mirrors(mirrors.to_path_buf())
            .with_host(host.clone()),
        archives: Archives::new(base.join("archives"), base.join("data")).with_host(host.clone()),
        runs: Runs::new(&base.join("runs")).with_host(host.clone()),
    }
}

/// Returns the profile named `example`.
fn example() -> Host {
    Host::named("example").unwrap()
}

fn name() -> WorkspaceName {
    "triage".parse().unwrap()
}

impl Hosted {
    fn create(&self, fixture: &Fixture) -> Workspace {
        match self
            .workspaces
            .create(&self.root, &name(), &fixture.clone, Mode::Change)
            .unwrap()
        {
            NewOutcome::Created(workspace) => workspace,
            other => panic!("{other:?}"),
        }
    }

    fn checkout(&self) -> PathBuf {
        self.root.join("triage/app")
    }

    fn archive(&self) -> Archive {
        let plan = match self
            .workspaces
            .plan_archive(
                &self.archives,
                &name(),
                ArchiveOptions::default(),
                Mode::Change,
            )
            .unwrap()
        {
            ArchivePlanned::Ready(plan) => *plan,
            other => panic!("{other:?}"),
        };
        match self.workspaces.archive(&self.archives, plan).unwrap() {
            ArchiveOutcome::Retired { mut archive, .. } => {
                self.workspaces
                    .finish_archive(&self.archives, &mut archive)
                    .unwrap();
                *archive
            }
            ArchiveOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }

    fn restore(&self) {
        let plan = match self
            .workspaces
            .plan_restore(&self.archives, &name(), RestoreOptions::default())
            .unwrap()
        {
            RestorePlanned::Ready(plan) => *plan,
            planned => panic!("{planned:?}"),
        };
        match self
            .workspaces
            .restore(&self.archives, plan, &self.runs)
            .unwrap()
        {
            RestoreOutcome::Restored(_) => {}
            RestoreOutcome::Refused(refusal) => panic!("{refusal:?}"),
        }
    }
}

/// Leaves a commit that `origin` does not hold and uncommitted work in
/// `checkout`, and an empty `user.name`, so git has no identity of the
/// person and an archive's commit of the work takes the host's.
fn leave_work(checkout: &Path) {
    write(checkout.join("edited.txt"), "one\n");
    git(checkout, &["add", "edited.txt"]);
    git(checkout, &["commit", "--quiet", "-m", "One file"]);
    write(checkout.join("edited.txt"), "two, edited\n");
    git(checkout, &["config", "user.name", ""]);
}

/// Returns every reference of the repository at `dir`.
fn refs(dir: &Path) -> Vec<String> {
    git(dir, &["for-each-ref", "--format=%(refname)"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Returns the path of every file and directory below `dir`, relative to
/// it, or nothing when `dir` does not exist.
fn names_below(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut next = vec![dir.to_path_buf()];
    while let Some(at) = next.pop() {
        let Ok(entries) = fs::read_dir(&at) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            // A git directory's own files are git's names, which no host
            // chooses.
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            found.push(
                path.strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() && !path.is_symlink() {
                next.push(path);
            }
        }
    }
    found
}

#[test]
fn a_workspace_created_archived_and_restored_under_a_second_profile_holds_no_mxv_name_in_its_refs_files_or_commits()
 {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let fixture = fixture();
    let mirrors = base.join("mirrors");
    let example = hosted(&base.join("example"), &mirrors, &example());
    example.create(&fixture);
    let agents = fs::read_to_string(example.root.join("triage/AGENTS.md")).unwrap();
    assert!(!agents.contains("mxv"), "{agents}");
    leave_work(&example.checkout());

    let archive = example.archive();

    let mirror = archive.checkouts[0].pin_repository().to_path_buf();
    let pinned = refs(&mirror);
    assert!(
        pinned
            .iter()
            .any(|name| name.starts_with("refs/example/archives/")),
        "{pinned:?}"
    );
    let wip = archive.checkouts[0].kept.wip.clone().expect("no work kept");
    for commit in [&wip.index, &wip.worktree] {
        let shown = git(&mirror, &["log", "-1", "--format=%an <%ae>|%s", commit]);
        assert_eq!(
            shown.trim(),
            "example <example@localhost.invalid>|wip: archived by example"
        );
    }
    let archived: Vec<String> = names_below(&base.join("example"))
        .into_iter()
        .chain(pinned)
        .collect();
    assert!(
        archived.iter().all(|name| !name.contains("mxv")),
        "{archived:?}"
    );

    example.restore();

    assert_eq!(
        fs::read_to_string(example.checkout().join("edited.txt")).unwrap(),
        "two, edited\n"
    );
    // A removal check counts unpushed commits, which copies the mirror's
    // heads into the clone's origin namespace.
    example
        .workspaces
        .plan_removal(&name(), crate::workspaces::Allow::default())
        .unwrap();
    let cloned = refs(&example.checkout());
    assert!(
        cloned
            .iter()
            .any(|name| name.starts_with("refs/example/origin/")),
        "{cloned:?}"
    );
    let restored: Vec<String> = names_below(&base.join("example"))
        .into_iter()
        .chain(cloned)
        .chain(refs(&mirror))
        .collect();
    assert!(
        restored.iter().all(|name| !name.contains("mxv")),
        "{restored:?}"
    );
}

#[test]
fn two_profiles_archive_into_one_mirror_and_each_restores_its_own() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let fixture = fixture();
    let mirrors = base.join("mirrors");
    let mxv = hosted(&base.join("mxv"), &mirrors, &Host::mxv());
    let example = hosted(&base.join("example"), &mirrors, &example());
    mxv.create(&fixture);
    example.create(&fixture);
    leave_work(&mxv.checkout());
    leave_work(&example.checkout());

    let theirs = mxv.archive();
    let ours = example.archive();

    let mirror = ours.checkouts[0].pin_repository().to_path_buf();
    assert_eq!(theirs.checkouts[0].pin_repository(), mirror);
    let pinned = refs(&mirror);
    for namespace in ["refs/mxv/archives/", "refs/example/archives/"] {
        assert!(
            pinned.iter().any(|name| name.starts_with(namespace)),
            "{namespace}: {pinned:?}"
        );
    }
    example.restore();
    let left = refs(&mirror);
    assert!(
        left.iter()
            .any(|name| name.starts_with("refs/mxv/archives/")),
        "the restore under one profile keeps the other's pins: {left:?}"
    );
    mxv.restore();
    for hosted in [&mxv, &example] {
        assert_eq!(
            fs::read_to_string(hosted.checkout().join("edited.txt")).unwrap(),
            "two, edited\n"
        );
    }
    let left = refs(&mirror);
    assert!(
        left.iter().all(|name| !name.contains("/archives/")),
        "{left:?}"
    );
}

#[test]
fn a_restore_under_another_profile_than_the_archive_s_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let fixture = fixture();
    let mirrors = base.join("mirrors");
    let example = hosted(&base, &mirrors, &example());
    example.create(&fixture);
    example.archive();
    let mxv = hosted(&base, &mirrors, &Host::mxv());

    let planned = mxv
        .workspaces
        .plan_restore(&mxv.archives, &name(), RestoreOptions::default());

    let error = planned.unwrap_err();
    assert!(
        format!("{error:?}").contains("refs/example/archives/"),
        "{error:?}"
    );
    assert!(!example.root.join("triage").exists());
    assert!(mxv.archives.read(&name()).is_err());
    assert!(example.archives.read(&name()).unwrap().is_some());
}

#[test]
fn workspaces_refuse_archives_of_another_host_before_they_write_anything() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let fixture = fixture();
    let example = hosted(&base, &base.join("mirrors"), &example());
    example.create(&fixture);
    leave_work(&example.checkout());
    let record = example.workspaces.store.read(&name()).unwrap();
    let before = names_below(&base);
    let mxv = Archives::new(base.join("archives"), base.join("data"));

    let planned =
        example
            .workspaces
            .plan_archive(&mxv, &name(), ArchiveOptions::default(), Mode::Change);
    let restored = example
        .workspaces
        .plan_restore(&mxv, &name(), RestoreOptions::default());
    let repaired = example.workspaces.repair(&mxv, &name(), Mode::Change);

    for outcome in [
        planned.map(|_| ()),
        restored.map(|_| ()),
        repaired.map(|_| ()),
    ] {
        let error = outcome.unwrap_err();
        assert!(
            matches!(
                &error,
                crate::Error::HostMismatch { workspaces, archives }
                    if workspaces.as_str() == "example" && archives.as_str() == "mxv"
            ),
            "{error:?}"
        );
    }
    assert_eq!(names_below(&base), before);
    assert_eq!(example.workspaces.store.read(&name()).unwrap(), record);
}

#[test]
fn a_workspace_under_a_second_profile_reads_its_own_manifest_file_and_ignores_mxv_toml() {
    let dir = TempDir::new().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let fixture = fixture();
    write(fixture.clone.join("example.toml"), "ports = [\"api\"]\n");
    write(fixture.clone.join("mxv.toml"), "ports = [\"web\"]\n");
    git(&fixture.clone, &["add", "example.toml", "mxv.toml"]);
    git(&fixture.clone, &["commit", "--quiet", "-m", "Manifests"]);
    git(&fixture.clone, &["push", "--quiet", "origin", "main"]);
    let example = hosted(&base, &base.join("mirrors"), &example());

    let workspace = example.create(&fixture);

    let inputs = workspace.manifest_inputs();
    assert_eq!(inputs.source(), ManifestSource::Repository);
    assert_eq!(
        inputs.repository,
        Some(example.checkout().join("example.toml"))
    );
    assert_eq!(workspace.manifest().unwrap().unwrap().ports, ["api"]);
    let read = example.workspaces.store.read(&name()).unwrap().unwrap();
    assert_eq!(
        read.manifest_file(),
        example.root.join("triage/example.toml")
    );
    assert_eq!(read.manifest().unwrap().unwrap().ports, ["api"]);
    // The host keeps the unit names that start with its own prefix, and
    // every host keeps mxv's prefix, because Runs adds its internal apps
    // and ledger ports under it for every host.
    write(read.manifest_file(), "ports = [\"example-api\"]\n");
    let refused = read.manifest().unwrap_err();
    assert!(refused.to_string().contains("`example-`"), "{refused}");
    write(read.manifest_file(), "ports = [\"mxv-aws\"]\n");
    let refused = read.manifest().unwrap_err();
    assert!(refused.to_string().contains("`mxv-`"), "{refused}");
}
