//! The comparison of a directory that no workspace record names with the
//! archive whose restore would rebuild it. `mxv repair --orphans` makes it
//! before and after it moves such a directory aside, and deletes the
//! directory only when both find nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::archive::{
    Archive, ArchivedCheckout, Archives, QUARANTINE_SUFFIX, ignored, quarantine, root,
};
use crate::backend::{Backend, RefHolder};
use crate::error::Error;
use crate::record::{Checkout, WorkspaceKind, agents_text};
use crate::workspaces::{ASIDE_SUFFIX, Entry, Workspaces, aside};

impl<B: Backend> Workspaces<B> {
    /// Returns one sentence for each thing in the directory `dir` that a
    /// restore of `archive` would not write again, or nothing when deleting
    /// `dir` loses nothing that the archive does not keep. `dir` is the
    /// archive's own directory, or the place where a sweep moved it.
    ///
    /// A restore that stops partway leaves each checkout it reached at the
    /// archived commit, with either no uncommitted work yet or the work
    /// that the archive keeps, and leaves the ignored files and the root
    /// files that it wrote. Anything else counts: another commit, other
    /// uncommitted work, an ignored file that the archive does not keep or
    /// that differs from its copy, a reference that names an object the
    /// archive's repository lacks, hidden work, and an extra entry of the
    /// directory. A checkout that git cannot read counts too.
    ///
    /// # Errors
    ///
    /// Returns the failures of reading the directory and of git.
    pub(crate) fn rebuild_differences(
        &self,
        archives: &Archives,
        archive: &Archive,
        dir: &Path,
    ) -> Result<Vec<String>, Error> {
        let mut workspace = archive.workspace();
        // The text of `AGENTS.md` names each checkout's path at the
        // archive's own directory and its origin, which the record keeps,
        // so it is the same wherever the directory is now.
        let origins: Vec<String> = archive
            .checkouts
            .iter()
            .map(|checkout| {
                checkout
                    .origin
                    .as_deref()
                    .map_or_else(|| "(none)".to_owned(), crate::mirror::redacted)
            })
            .collect();
        let generated = agents_text(&workspace, &origins);
        workspace.path = dir.to_path_buf();
        for (checkout, archived) in workspace.checkouts.iter_mut().zip(&archive.checkouts) {
            checkout.path = archived
                .path
                .strip_prefix(&archive.path)
                .map_or_else(|_| dir.to_path_buf(), |below| dir.join(below));
        }
        let mut found = root::differences(archive, &workspace, &generated)?;
        found.extend(leftover_entries(dir, &workspace.checkouts)?);
        let index = archives.wip_index(&archive.name)?;
        for (checkout, archived) in workspace.checkouts.iter().zip(&archive.checkouts) {
            let differences = self.checkout_differences(archive, checkout, archived, &index);
            // The temporary index is only read while a checkout is
            // compared, so a failure to delete it changes nothing.
            let _ = fs::remove_file(&index);
            found.extend(
                differences?
                    .into_iter()
                    .map(|difference| format!("{}: {difference}", checkout.name)),
            );
        }
        Ok(found)
    }

    /// Returns what the checkout `checkout` holds that a restore of
    /// `archived` would not write, reading its uncommitted work through
    /// `index`.
    fn checkout_differences(
        &self,
        archive: &Archive,
        checkout: &Checkout,
        archived: &ArchivedCheckout,
        index: &Path,
    ) -> Result<Vec<String>, Error> {
        let path = &checkout.path;
        match Entry::at(path)? {
            Entry::Absent => return Ok(Vec::new()),
            Entry::Link => return Ok(vec!["a symbolic link stands in its place".to_owned()]),
            Entry::Present => {}
        }
        if is_empty_dir(path)? {
            return Ok(Vec::new());
        }
        let head = match self.backend.head(path) {
            Ok(head) => head,
            Err(error) => return Ok(vec![format!("git cannot read it: {error}")]),
        };
        let own = head
            .toplevel
            .as_deref()
            .is_some_and(|top| same_dir(top, path));
        if !own {
            return Ok(vec!["git finds no repository of its own in it".to_owned()]);
        }
        let mut found = Vec::new();
        if head.commit.as_deref() != Some(archived.commit.as_str()) {
            found.push(format!(
                "HEAD names {}, and the archive keeps {}",
                head.commit.as_deref().unwrap_or("no commit"),
                archived.commit
            ));
            return Ok(found);
        }
        if self
            .backend
            .resolve_commit(archived.pin_holder(), &archived.commit)?
            .is_none()
        {
            found
                .push("the repository of the archive's pins no longer holds its commit".to_owned());
            return Ok(found);
        }
        if !archived.detached && head.branch.as_deref() != Some(archived.branch.as_str()) {
            found.push(format!(
                "HEAD is not on the branch {} that the archive keeps",
                archived.branch
            ));
        }
        found.extend(self.work_differences(path, archived, index)?);
        found.extend(ignored_differences(
            archive,
            &checkout.name,
            path,
            &self.backend.changes(path)?.ignored,
        )?);
        found.extend(self.reference_differences(archive.kind, path, archived)?);
        let concealed = self.backend.concealed(path)?;
        let hidden = concealed.nested.len()
            + concealed.hidden.len()
            + concealed.submodules.len()
            + concealed.unlisted.len()
            + concealed.module_referenced.len()
            + concealed.paused.len()
            + concealed.orphaned.len();
        if hidden > 0 {
            found.push(format!(
                "it holds {hidden} pieces of work that git status does not show"
            ));
        }
        Ok(found)
    }

    /// Returns a sentence when the index and the working files of the
    /// checkout at `path` hold neither the tree of the archived commit, as
    /// before the restore put the uncommitted work back, nor the trees of
    /// the uncommitted work that the archive keeps.
    fn work_differences(
        &self,
        path: &Path,
        archived: &ArchivedCheckout,
        index: &Path,
    ) -> Result<Vec<String>, Error> {
        let trees = self.backend.wip_trees(path, index, &[])?;
        // The trees are read in the repository that holds the pins, which
        // holds the kept commits even when the checkout does not yet.
        let holder = archived.pin_repository();
        let head = self.backend.tree_of(holder, &archived.commit)?;
        if trees.index == head && trees.worktree == head {
            return Ok(Vec::new());
        }
        if let Some(wip) = &archived.kept.wip
            && trees.index == self.backend.tree_of(holder, &wip.index)?
            && trees.worktree == self.backend.tree_of(holder, &wip.worktree)?
        {
            return Ok(Vec::new());
        }
        Ok(vec![
            "its uncommitted work differs from what the archive keeps".to_owned(),
        ])
    }

    /// Returns a sentence for each reference of the checkout at `path`
    /// that holds what the archive does not keep. A clone's references
    /// other than its remote branches must name objects that the
    /// repository of the pins holds. A worktree shares its repository's
    /// references, so only its own references below `refs/worktree/`
    /// count, and each must be one that the archive keeps.
    fn reference_differences(
        &self,
        kind: WorkspaceKind,
        path: &Path,
        archived: &ArchivedCheckout,
    ) -> Result<Vec<String>, Error> {
        let own = RefHolder::Repository(path);
        let mut found = Vec::new();
        if kind == WorkspaceKind::Worktree {
            for (name, object) in self.backend.refs_under(own, "refs/worktree/")? {
                if archived.kept.worktree.get(&name) != Some(&object) {
                    found.push(format!(
                        "the reference refs/worktree/{name} is not in the archive"
                    ));
                }
            }
            return Ok(found);
        }
        for (name, object) in self.backend.refs_under(own, "refs/")? {
            if name.starts_with("remotes/") {
                continue;
            }
            if self
                .backend
                .resolve_commit(archived.pin_holder(), &object)?
                .is_none()
            {
                found.push(format!(
                    "the reference refs/{name} names {object}, which the archive does not keep"
                ));
            }
        }
        Ok(found)
    }
}

/// Returns a sentence for each ignored path of the checkout `folder` at
/// `path`, as `ignored` names them relative to it, that the archive does
/// not keep or keeps with other content. An ignored directory counts
/// through each file below it.
fn ignored_differences(
    archive: &Archive,
    folder: &str,
    path: &Path,
    ignored: &[String],
) -> Result<Vec<String>, Error> {
    let mut found = Vec::new();
    for entry in ignored {
        let mut files = Vec::new();
        if entry.ends_with('/') {
            files_below(&path.join(entry.trim_end_matches('/')), entry, &mut files)?;
        } else {
            files.push(entry.clone());
        }
        for relative in files {
            let archived = format!("{folder}/{relative}");
            let kept = archive.ignored.iter().find(|kept| kept.path == archived);
            let same = match kept {
                Some(kept) => ignored::matches(kept, &path.join(&relative))?,
                None => false,
            };
            if !same {
                found.push(format!(
                    "the ignored file {relative} is not what the archive keeps"
                ));
            }
        }
    }
    Ok(found)
}

/// Adds the path of each file and link below the directory `dir`, whose
/// path relative to the checkout is `prefix` with its trailing `/`, to
/// `files`. An empty directory adds itself, so it still counts.
fn files_below(dir: &Path, prefix: &str, files: &mut Vec<String>) -> Result<(), Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    let mut empty = true;
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        empty = false;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().map_err(unreadable)?.is_dir() {
            files_below(&entry.path(), &format!("{prefix}{name}/"), files)?;
        } else {
            files.push(format!("{prefix}{name}"));
        }
    }
    if empty {
        files.push(prefix.to_owned());
    }
    Ok(())
}

/// Returns a sentence for each entry of `dir` that an archive or a removal
/// that stopped partway left beside a checkout, which the comparison of the
/// root files does not count.
fn leftover_entries(dir: &Path, checkouts: &[Checkout]) -> Result<Vec<String>, Error> {
    let unreadable = |source| Error::StateUnreadable {
        path: dir.to_path_buf(),
        source,
    };
    let mut held: Vec<PathBuf> = Vec::new();
    for checkout in checkouts {
        held.extend(quarantine(&checkout.path));
        held.extend(aside(&checkout.path));
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if held.contains(&entry.path())
            || name.ends_with(QUARANTINE_SUFFIX)
            || name.ends_with(ASIDE_SUFFIX)
        {
            found.push(format!(
                "an entry that a stopped archive or removal left: {name}"
            ));
        }
    }
    Ok(found)
}

/// Reports whether `path` is a directory with nothing in it.
fn is_empty_dir(path: &Path) -> Result<bool, Error> {
    match fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(false),
        Err(source) => Err(Error::StateUnreadable {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reports whether `left` and `right` name the same directory, after each
/// resolves its symbolic links.
fn same_dir(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}
