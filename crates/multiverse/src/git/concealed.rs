//! What `git status` does not show about a checkout, and the size of the
//! files of its uncommitted work. `mxv archive` deletes the checkout, so it
//! reads all of this first.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{
    CHECKOUT_DEADLINE, Git, LOCAL_DEADLINE, MOST_LISTING_BYTES, ORIGIN_REFS, lines, module_dirs,
    run, run_keeping, trimmed,
};
use crate::backend::{
    Backend, Concealed, Hidden, LargeFile, LocalNames, Operation, Paused, SubmoduleWork,
};
use crate::error::Error;

/// The most levels of nested submodules that mxv reads.
const MOST_SUBMODULE_DEPTH: usize = 8;

/// The file mode that git gives a gitlink, an entry that names a commit of
/// a submodule.
const GITLINK: &str = "160000";

/// The file mode that git gives a symbolic link.
const SYMLINK: &str = "120000";

/// The name of the fallback identity of an archive's commit of uncommitted
/// work, which is used when git has no identity of the person.
const FALLBACK_NAME: &str = "mxv";

/// The email of the fallback identity of an archive's commit of uncommitted
/// work.
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
pub(super) fn nested(checkout: &Path) -> Result<Vec<String>, Error> {
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
fn hidden(checkout: &Path, entries: &[IndexEntry]) -> Result<Vec<Hidden>, Error> {
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
            found.push(Hidden {
                path: entry.path.clone(),
                skip_worktree: entry.tag == "S",
            });
        }
    }
    Ok(found)
}

/// Returns the absolute path of the git directory of the checkout at `dir`,
/// which holds the state of a paused operation and the submodules' git
/// directories.
fn git_dir(dir: &Path) -> Result<PathBuf, Error> {
    let printed = run(
        dir,
        ["rev-parse", "--absolute-git-dir"],
        "rev-parse",
        LOCAL_DEADLINE,
    )?
    .ok("rev-parse", dir)?;
    Ok(PathBuf::from(trimmed(&printed)))
}

/// Returns each name that git accounts for below the checkout at `checkout`,
/// whose index holds `entries`: tracked files, untracked files and ignored
/// paths. A directory that git reports as a whole, an ignored one or a
/// nested repository, ends with `/`.
///
/// Git runs with `core.precomposeunicode` off, so on macOS it prints a
/// name as the disk holds it. Each name is folded with
/// [`listing_key`].
fn accounted(checkout: &Path, entries: &[IndexEntry]) -> Result<HashSet<String>, Error> {
    const RAW_NAMES: &str = "core.precomposeunicode=false";
    let untracked = run_keeping(
        checkout,
        [
            "-c",
            RAW_NAMES,
            "ls-files",
            "-z",
            "-o",
            "--exclude-standard",
        ],
        "ls-files",
        CHECKOUT_DEADLINE,
        MOST_LISTING_BYTES,
    )?
    .ok("ls-files", checkout)?;
    let status = run_keeping(
        checkout,
        [
            "-c",
            RAW_NAMES,
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "--untracked-files=normal",
            "--ignored=matching",
            "-z",
        ],
        "status",
        LOCAL_DEADLINE,
        MOST_LISTING_BYTES,
    )?
    .ok("status", checkout)?;
    let tracked = entries
        .iter()
        .filter(|entry| entry.mode != GITLINK)
        .map(|entry| entry.path.as_str());
    Ok(tracked
        .chain(untracked.split('\0'))
        .chain(
            status
                .split('\0')
                .filter_map(|entry| entry.strip_prefix("!! ")),
        )
        .map(listing_key)
        .collect())
}

/// Returns the form of `path` that the comparison of the disk with git's
/// listings uses. Letters are folded to lowercase because a case-insensitive
/// file system can hold a tracked file under another case than the index,
/// and git counts that as the same file. The folding stops at ASCII, and
/// a file that differs from a listed one only in case would count as
/// listed. Nothing is tracked below a submodule directory that is not
/// initialised, so that file cannot be one this comparison exists to find.
fn listing_key(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// Adds to `found` each file below `dir` that is not in `listed`. A
/// directory in `listed`, or one of `submodules`, is left alone, because
/// git reported it as a whole or its own reading covers it. An empty
/// directory holds no work and is not reported. `root` is the checkout.
fn walk(
    root: &Path,
    dir: &Path,
    listed: &HashSet<String>,
    submodules: &BTreeSet<String>,
    found: &mut Vec<String>,
) -> Result<(), Error> {
    let unreadable = |source| Error::Inaccessible {
        path: dir.to_path_buf(),
        source,
    };
    for entry in fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if relative == ".git" {
            continue;
        }
        let key = listing_key(&relative);
        if entry.file_type().map_err(unreadable)?.is_dir() {
            if !listed.contains(&format!("{key}/")) && !submodules.contains(&relative) {
                walk(root, &path, listed, submodules, found)?;
            }
        } else if !listed.contains(&key) {
            found.push(relative);
        }
    }
    Ok(())
}

/// The operation that each file or directory of a git directory says
/// stands paused. A cherry-pick or a revert of one commit writes its own
/// file beside `sequencer`, so the sequence counts only without one.
const PAUSED: [(&str, Operation); 7] = [
    ("rebase-merge", Operation::Rebase),
    ("rebase-apply", Operation::Rebase),
    ("MERGE_HEAD", Operation::Merge),
    ("CHERRY_PICK_HEAD", Operation::CherryPick),
    ("REVERT_HEAD", Operation::Revert),
    ("sequencer", Operation::Sequence),
    ("BISECT_LOG", Operation::Bisect),
];

/// Returns the operations that stand paused in the git directory `dir`,
/// each once.
fn paused_in(dir: &Path) -> Vec<Operation> {
    let mut found: Vec<Operation> = Vec::new();
    for (marker, operation) in PAUSED {
        if fs::symlink_metadata(dir.join(marker)).is_ok() && !found.contains(&operation) {
            found.push(operation);
        }
    }
    let single = found.contains(&Operation::CherryPick) || found.contains(&Operation::Revert);
    if single {
        found.retain(|operation| *operation != Operation::Sequence);
    }
    found
}

/// What the walk over a checkout and its submodules found.
#[derive(Default)]
struct Found {
    /// The work of each initialised submodule.
    submodules: Vec<SubmoduleWork>,
    /// Each file that git does not account for.
    unlisted: Vec<String>,
    /// Each paused operation.
    paused: Vec<Paused>,
    /// The git directory of the checkout and of each initialised submodule.
    dirs: BTreeSet<PathBuf>,
}

/// Adds to `found` what the checkout at `checkout`, whose index holds
/// `entries`, does not show, and the work of each initialised submodule
/// and of the submodules inside it. `prefix` is the checkout's path below
/// the top checkout. Past [`MOST_SUBMODULE_DEPTH`] levels mxv reads no
/// further submodule, so the walk reports every file below one as unlisted.
fn read_checkout(
    checkout: &Path,
    entries: &[IndexEntry],
    prefix: &str,
    depth: usize,
    found: &mut Found,
) -> Result<(), Error> {
    let dir = git_dir(checkout)?;
    found
        .paused
        .extend(paused_in(&dir).into_iter().map(|operation| Paused {
            at: prefix.to_owned(),
            operation,
        }));
    found.dirs.insert(dir);
    let initialised: BTreeSet<String> = entries
        .iter()
        .filter(|entry| entry.mode == GITLINK && depth <= MOST_SUBMODULE_DEPTH)
        .filter(|entry| fs::symlink_metadata(checkout.join(&entry.path).join(".git")).is_ok())
        .map(|entry| entry.path.clone())
        .collect();
    let listed = accounted(checkout, entries)?;
    let mut unlisted = Vec::new();
    walk(checkout, checkout, &listed, &initialised, &mut unlisted)?;
    found
        .unlisted
        .extend(unlisted.into_iter().map(|path| format!("{prefix}{path}")));
    for submodule in &initialised {
        let dir = checkout.join(submodule);
        let path = format!("{prefix}{submodule}");
        found.submodules.push(SubmoduleWork {
            path: path.clone(),
            changes: Git.changes(&dir)?,
            stashes: Git.stashes(&dir)?,
            commits: Git.unpushed_commits(&dir)?,
        });
        read_checkout(&dir, &index(&dir)?, &format!("{path}/"), depth + 1, found)?;
    }
    Ok(())
}

/// Returns the stashes and commits of each git directory below `top`, the
/// git directory of a checkout, that `read` does not name. Git keeps a
/// submodule's git directory after `git submodule deinit` takes its files.
fn orphaned(top: &Path, read: &BTreeSet<PathBuf>) -> Result<Vec<SubmoduleWork>, Error> {
    let modules = top.join("modules");
    let mut found = Vec::new();
    for dir in module_dirs(&modules)? {
        if read.contains(&dir) {
            continue;
        }
        let work = SubmoduleWork {
            path: dir
                .strip_prefix(&modules)
                .unwrap_or(&dir)
                .to_string_lossy()
                .into_owned(),
            stashes: Git.stashes(&dir)?,
            commits: Git.unpushed_commits(&dir)?,
            ..SubmoduleWork::default()
        };
        if !work.stashes.is_empty() || !work.commits.is_empty() {
            found.push(work);
        }
    }
    Ok(found)
}

/// Returns each commit in the git directory `dir` that only a reference
/// outside the branches, tags, notes and remote branches keeps, each with
/// that reference, leaving out what `HEAD` reaches, which the pin keeps. A
/// stash has its own report, and mxv's own references copy what `origin`
/// holds. `refs/prefetch` holds git's copies of remote branches.
fn kept_by_other_references(dir: &Path) -> Result<Vec<String>, Error> {
    let listed = run(
        dir,
        [
            "log",
            "--no-show-signature",
            "--source",
            "--format=%H (%S)",
            "--exclude=refs/heads/*",
            "--exclude=refs/tags/*",
            "--exclude=refs/notes/*",
            "--exclude=refs/remotes/*",
            "--exclude=refs/prefetch/*",
            "--exclude=refs/mxv/*",
            "--exclude=refs/stash",
            "--all",
            "--not",
            "HEAD",
            "--branches",
            "--tags",
            "--glob=refs/notes/*",
            "--remotes=origin",
            ORIGIN_REFS,
            "--",
        ],
        "log",
        LOCAL_DEADLINE,
    )?
    .ok("log", dir)?;
    Ok(lines(&listed))
}

/// Returns what `git status` does not show about the checkout at
/// `checkout`.
pub(super) fn concealed(checkout: &Path) -> Result<Concealed, Error> {
    let entries = index(checkout)?;
    let mut found = Found::default();
    read_checkout(checkout, &entries, "", 0, &mut found)?;
    let top = git_dir(checkout)?;
    // The checkout's own other references are read by `orphans`, whose
    // commits the archive keeps, and a worktree's stay in its repository.
    // A submodule's commits live in its own git directory, which the
    // archive deletes with the checkout and cannot pin.
    let mut module_referenced = Vec::new();
    for dir in module_dirs(&top.join("modules"))? {
        module_referenced.extend(kept_by_other_references(&dir)?);
    }
    Ok(Concealed {
        nested: nested(checkout)?,
        hidden: hidden(checkout, &entries)?,
        orphaned: orphaned(&top, &found.dirs)?,
        submodules: found.submodules,
        unlisted: found.unlisted,
        paused: found.paused,
        module_referenced,
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

/// Returns each path that `args`, a listing of `git ls-files` or `git diff`
/// with `-z`, prints in the checkout at `checkout`.
fn listed_paths(checkout: &Path, args: &[&str]) -> Result<Vec<String>, Error> {
    let listed = run_keeping(
        checkout,
        args,
        "ls-files",
        CHECKOUT_DEADLINE,
        MOST_LISTING_BYTES,
    )?
    .ok("ls-files", checkout)?;
    Ok(listed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Returns each file of the uncommitted work of the checkout at
/// `checkout` that is larger than `most` bytes: a changed or staged
/// tracked file, or an untracked file that no ignore rule matches. A file
/// that is gone holds nothing to keep, and a nested repository has its own
/// report.
pub(super) fn large_files(checkout: &Path, most: u64) -> Result<Vec<LargeFile>, Error> {
    let mut paths: BTreeMap<String, bool> = BTreeMap::new();
    for path in listed_paths(checkout, &["ls-files", "-z", "--modified"])? {
        paths.insert(path, false);
    }
    for path in listed_paths(
        checkout,
        &[
            "diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "-z",
            "HEAD",
            "--",
        ],
    )? {
        paths.insert(path, false);
    }
    for path in listed_paths(
        checkout,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )? {
        paths.entry(path).or_insert(true);
    }
    let mut found = Vec::new();
    for (path, untracked) in paths {
        if path.ends_with('/') {
            continue;
        }
        let file = checkout.join(&path);
        let metadata = match fs::symlink_metadata(&file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(Error::Inaccessible { path: file, source }),
        };
        if metadata.is_file() && metadata.len() > most {
            found.push(LargeFile {
                path,
                bytes: metadata.len(),
                untracked,
            });
        }
    }
    Ok(found)
}

/// Returns the path of each submodule that the index of the checkout at
/// `checkout` names.
pub(super) fn submodule_paths(checkout: &Path) -> Result<Vec<String>, Error> {
    Ok(index(checkout)?
        .into_iter()
        .filter(|entry| entry.mode == GITLINK)
        .map(|entry| entry.path)
        .collect())
}

/// Returns the directory of each linked worktree that the repository of
/// the clone at `checkout` lists, leaving out the clone itself.
fn linked_worktrees(checkout: &Path) -> Result<Vec<String>, Error> {
    let listed = run(
        checkout,
        ["worktree", "list", "--porcelain"],
        "worktree list",
        LOCAL_DEADLINE,
    )?
    .ok("worktree list", checkout)?;
    Ok(listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .skip(1)
        .map(str::to_owned)
        .collect())
}

/// Reports whether a new clone writes the local configuration key `key`, or
/// another report names what it holds, as for a remote or a branch.
fn rebuilt_key(key: &str) -> bool {
    key.starts_with("remote.")
        || key.starts_with("branch.")
        || matches!(
            key,
            "core.repositoryformatversion"
                | "core.filemode"
                | "core.bare"
                | "core.logallrefupdates"
                | "core.ignorecase"
                | "core.precomposeunicode"
                | "core.symlinks"
        )
}

/// Returns each hook in the git directory `dir`. Git's own examples end
/// with `.sample` and do nothing.
fn hooks(dir: &Path) -> Result<Vec<String>, Error> {
    let hooks = dir.join("hooks");
    let entries = match fs::read_dir(&hooks) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Inaccessible {
                path: hooks,
                source,
            });
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Inaccessible {
            path: hooks.clone(),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".sample") {
            names.push(name);
        }
    }
    names.sort_unstable();
    Ok(names)
}

/// Returns the rules of the exclude file at `path`, without its comments.
fn exclude_rules(path: &Path) -> Result<Vec<String>, Error> {
    let text = match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Inaccessible {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    Ok(lines(&text)
        .into_iter()
        .filter(|rule| !rule.starts_with('#'))
        .collect())
}

/// Returns what the clone at `checkout` holds that a clone rebuilt from its
/// mirror at `mirror` and the person's repository at `source` would not.
pub(super) fn local_names(checkout: &Path, source: &Path) -> Result<LocalNames, Error> {
    let remotes = run(checkout, ["remote"], "remote", LOCAL_DEADLINE)?.ok("remote", checkout)?;
    let config = run(
        checkout,
        ["config", "--local", "--name-only", "--list"],
        "config",
        LOCAL_DEADLINE,
    )?
    .ok("config", checkout)?;
    let mut config: Vec<String> = lines(&config)
        .into_iter()
        .filter(|key| !rebuilt_key(key))
        .collect();
    config.sort_unstable();
    config.dedup();
    let copied = if source.is_dir() {
        exclude_rules(&Git.exclude_file(source)?)?
    } else {
        Vec::new()
    };
    let excludes = exclude_rules(&Git.exclude_file(checkout)?)?
        .into_iter()
        .filter(|rule| !copied.contains(rule))
        .collect();
    Ok(LocalNames {
        hooks: hooks(&git_dir(checkout)?)?,
        config,
        excludes,
        worktrees: linked_worktrees(checkout)?,
        remotes: lines(&remotes)
            .into_iter()
            .filter(|remote| remote != "origin")
            .collect(),
    })
}
