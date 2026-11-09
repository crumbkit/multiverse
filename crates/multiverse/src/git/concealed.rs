//! What `git status` does not show about a checkout, and the check that a
//! `--wip` commit can run. `mxv archive` deletes the checkout, so it reads
//! all of this first.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;

use super::{
    CHECKOUT_DEADLINE, Git, LOCAL_DEADLINE, MOST_LISTING_BYTES, lines, run, run_keeping, trimmed,
};
use crate::backend::{Backend, Concealed, LocalNames, SubmoduleWork};
use crate::error::Error;

/// The most levels of nested submodules that mxv reads.
const MOST_SUBMODULE_DEPTH: usize = 8;

/// The file mode that git gives a gitlink, an entry that names a commit of
/// a submodule.
const GITLINK: &str = "160000";

/// The file mode that git gives a symbolic link.
const SYMLINK: &str = "120000";

/// The name of the fallback identity of a `--wip` commit, which is used
/// when git has no identity of the person.
const FALLBACK_NAME: &str = "mxv";

/// The email of the fallback identity of a `--wip` commit.
const FALLBACK_EMAIL: &str = "mxv@localhost.invalid";

/// One entry of git's index.
struct IndexEntry {
    /// The status letter that `git ls-files -v` prints.
    tag: String,
    /// The file mode.
    mode: String,
    /// The object that the index holds.
    object: String,
    /// The path.
    path: String,
}

impl IndexEntry {
    /// Reports whether `git status` does not compare the file with the
    /// index: skip-worktree has the tag `S`, and assume-unchanged has a
    /// lowercase tag.
    fn is_hidden(&self) -> bool {
        self.tag == "S" || self.tag.chars().all(|letter| letter.is_ascii_lowercase())
    }
}

/// Returns each entry of the index of the checkout at `checkout`.
fn index(checkout: &Path) -> Result<Vec<IndexEntry>, Error> {
    let listed = run_keeping(
        checkout,
        ["ls-files", "-z", "-v", "-s"],
        "ls-files",
        CHECKOUT_DEADLINE,
        MOST_LISTING_BYTES,
    )?
    .ok("ls-files", checkout)?;
    Ok(listed
        .split('\0')
        .filter_map(|entry| {
            let (fields, path) = entry.split_once('\t')?;
            let mut fields = fields.split(' ');
            Some(IndexEntry {
                tag: fields.next()?.to_owned(),
                mode: fields.next()?.to_owned(),
                object: fields.next()?.to_owned(),
                path: path.to_owned(),
            })
        })
        .collect())
}

/// Returns each repository inside the checkout at `checkout` that git does
/// not track. `git ls-files` ends the name of such a directory with `/`,
/// and lists every other untracked path as a file.
fn nested(checkout: &Path) -> Result<Vec<String>, Error> {
    let listed = run_keeping(
        checkout,
        ["ls-files", "-z", "-o", "--exclude-standard"],
        "ls-files",
        CHECKOUT_DEADLINE,
        MOST_LISTING_BYTES,
    )?
    .ok("ls-files", checkout)?;
    Ok(listed
        .split('\0')
        .filter(|path| path.ends_with('/'))
        .map(str::to_owned)
        .collect())
}

/// Reports whether the file at `path` in the checkout, which `entry`
/// describes, holds what the index holds.
fn matches_index(checkout: &Path, entry: &IndexEntry) -> Result<bool, Error> {
    if entry.mode == SYMLINK {
        let target = run(
            checkout,
            ["cat-file", "blob", entry.object.as_str()],
            "cat-file",
            LOCAL_DEADLINE,
        )?
        .ok("cat-file", checkout)?;
        return Ok(fs::read_link(checkout.join(&entry.path))
            .is_ok_and(|found| found.as_os_str() == OsStr::new(&target)));
    }
    let named = format!("--path={}", entry.path);
    let written = run(
        checkout,
        ["hash-object", named.as_str(), "--", entry.path.as_str()],
        "hash-object",
        CHECKOUT_DEADLINE,
    )?;
    Ok(written.status.success() && trimmed(&written.stdout) == entry.object)
}

/// Returns each path among `entries` that the index hides from `git status`
/// and whose file differs from the index. A file that is absent from the
/// disk, as in a sparse checkout, holds no work.
fn hidden(checkout: &Path, entries: &[IndexEntry]) -> Result<Vec<String>, Error> {
    let mut found = Vec::new();
    for entry in entries.iter().filter(|entry| entry.is_hidden()) {
        if entry.mode == GITLINK {
            continue;
        }
        let path = checkout.join(&entry.path);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::Inaccessible { path, source }),
        }
        if !matches_index(checkout, entry)? {
            found.push(entry.path.clone());
        }
    }
    Ok(found)
}

/// Adds the work of each initialised submodule among `entries` of the
/// checkout at `checkout` to `found`, and of the submodules inside it.
/// `prefix` is the checkout's path below the top checkout.
fn read_submodules(
    checkout: &Path,
    entries: &[IndexEntry],
    prefix: &str,
    depth: usize,
    found: &mut Vec<SubmoduleWork>,
) -> Result<(), Error> {
    for entry in entries.iter().filter(|entry| entry.mode == GITLINK) {
        let dir = checkout.join(&entry.path);
        if fs::symlink_metadata(dir.join(".git")).is_err() {
            continue;
        }
        let path = format!("{prefix}{}", entry.path);
        found.push(SubmoduleWork {
            path: path.clone(),
            changes: Git.changes(&dir)?,
            stashes: Git.stashes(&dir)?,
            commits: Git.unpushed_commits(&dir)?,
        });
        if depth < MOST_SUBMODULE_DEPTH {
            read_submodules(&dir, &index(&dir)?, &format!("{path}/"), depth + 1, found)?;
        }
    }
    Ok(())
}

/// Returns what `git status` does not show about the checkout at
/// `checkout`.
pub(super) fn concealed(checkout: &Path) -> Result<Concealed, Error> {
    let entries = index(checkout)?;
    let mut submodules = Vec::new();
    read_submodules(checkout, &entries, "", 0, &mut submodules)?;
    Ok(Concealed {
        nested: nested(checkout)?,
        hidden: hidden(checkout, &entries)?,
        submodules,
    })
}

/// Returns the configuration that gives a commit in the checkout at
/// `checkout` an identity: nothing when git has one, or else mxv's own, as
/// on a machine or in a CI runner where nobody set `user.name`.
pub(super) fn identity(checkout: &Path) -> Result<Vec<String>, Error> {
    for variable in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
        if !run(checkout, ["var", variable], "var", LOCAL_DEADLINE)?
            .status
            .success()
        {
            return Ok(vec![
                "-c".to_owned(),
                format!("user.name={FALLBACK_NAME}"),
                "-c".to_owned(),
                format!("user.email={FALLBACK_EMAIL}"),
            ]);
        }
    }
    Ok(Vec::new())
}

/// Returns the failure when `git commit` could not run in the checkout at
/// `checkout`, and changes nothing but a commit object that nothing names.
pub(super) fn check_commit_all(checkout: &Path) -> Result<(), Error> {
    let lock = run(
        checkout,
        ["rev-parse", "--git-path", "index.lock"],
        "rev-parse",
        LOCAL_DEADLINE,
    )?
    .ok("rev-parse", checkout)?;
    let lock = checkout.join(trimmed(&lock));
    if fs::symlink_metadata(&lock).is_ok() {
        return Err(Error::CommandFailed {
            operation: "add",
            dir: checkout.to_path_buf(),
            code: None,
            message: format!(
                "git's index is locked by {}, so mxv could not commit the work",
                lock.display()
            ),
        });
    }
    run(
        checkout,
        ["add", "--all", "--dry-run"],
        "add",
        CHECKOUT_DEADLINE,
    )?
    .ok("add", checkout)?;
    let mut probe = identity(checkout)?;
    probe.extend(
        [
            "commit-tree",
            "-m",
            "mxv probe",
            "-p",
            "HEAD",
            "HEAD^{tree}",
        ]
        .map(str::to_owned),
    );
    run(checkout, probe, "commit-tree", LOCAL_DEADLINE)?
        .ok("commit-tree", checkout)
        .map(drop)
}

/// Returns the references of the kind `prefix` in `repository`, each
/// without the prefix.
fn names_under(repository: &Path, prefix: &str) -> Result<Vec<String>, Error> {
    let listed = run(
        repository,
        ["for-each-ref", "--format=%(refname)", prefix],
        "for-each-ref",
        LOCAL_DEADLINE,
    )?
    .ok("for-each-ref", repository)?;
    Ok(lines(&listed)
        .iter()
        .filter_map(|name| name.strip_prefix(prefix))
        .map(str::to_owned)
        .collect())
}

/// Returns what the clone at `checkout` names and its mirror at `mirror`
/// does not.
pub(super) fn local_names(checkout: &Path, mirror: &Path) -> Result<LocalNames, Error> {
    let absent = |prefix: &str| -> Result<Vec<String>, Error> {
        let held = names_under(mirror, prefix)?;
        Ok(names_under(checkout, prefix)?
            .into_iter()
            .filter(|name| !held.contains(name))
            .collect())
    };
    let remotes = run(checkout, ["remote"], "remote", LOCAL_DEADLINE)?.ok("remote", checkout)?;
    Ok(LocalNames {
        branches: absent("refs/heads/")?,
        tags: absent("refs/tags/")?,
        remotes: lines(&remotes)
            .into_iter()
            .filter(|remote| remote != "origin")
            .collect(),
    })
}
