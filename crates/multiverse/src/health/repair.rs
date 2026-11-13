//! The repairs of the broken records whose repair is
//! [`Repair::Repair`](super::Repair::Repair).

use std::path::PathBuf;

use phase::calendar::civil_from_days;

use crate::archive::{Archive, ArchiveRecords, ArchivedRemoval, Archives};
use crate::backend::Backend;
use crate::error::Error;
use crate::name::{MOST_NAME_BYTES, WorkspaceName};
use crate::store::Records;
use crate::workspaces::{Mode, Workspaces};

use super::{ArchiveCase, Repair, archive_case, classify, count_as_there, pins_gone, rename_pair};

/// The most dated names that [`Workspaces::repair`] tries for an older
/// archive: `NAME-<date>`, then `NAME-<date>-2` up to this suffix. When
/// every one is taken, the repair fails with [`Error::DatedNamesTaken`]
/// and changes nothing.
const MOST_DATED: u32 = 99;

/// The bytes that `-<date>-99` takes after the stem of a dated name.
const DATED_SUFFIX_BYTES: usize = 14;

/// What [`Workspaces::repair`] did with the records of one name, or would
/// do in [`Mode::DryRun`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Repaired {
    /// Nothing of the name is broken, or what is broken is another
    /// command's to repair, which `repair` names.
    Unchanged {
        /// The command that repairs what is broken, or `None` when nothing
        /// is.
        repair: Option<Repair>,
    },
    /// The directory of an archive whose pins are gone moved to `kept`,
    /// with its record and its copies of the root files, so the archive
    /// no longer claims that a restore brings the workspace back. Nothing
    /// is deleted, and nothing in multiverse deletes `kept` or the private
    /// tar later, so they stay until a person deletes them.
    MovedAside {
        /// The archive as it was.
        archive: Box<Archive>,
        /// The directory that now holds the archive's record and files.
        kept: PathBuf,
        /// The private tar of the archive's ignored files, which stays
        /// where it is, or `None` when the archive has none.
        ignored: Option<PathBuf>,
    },
    /// An older archive, whose name a newer workspace holds or held, now
    /// has the name of `archive` and is retired, so the caller
    /// finishes it as `mxv archive -w` finishes an archive.
    Renamed {
        /// The name the archive had.
        from: WorkspaceName,
        /// The archive under its new name.
        archive: Box<Archive>,
    },
    /// The archive that a restore had already rebuilt is deleted, with its
    /// pins and its private tar.
    Deleted(Box<ArchivedRemoval>),
}

impl<B: Backend> Workspaces<B> {
    /// Repairs the records of the workspace `name` whose repair is
    /// [`Repair::Repair`]: an archive whose pins are gone, or whose
    /// workspace was removed before it retired every checkout, moves
    /// aside, an unfinished archive beside a newer workspace takes a dated name,
    /// or moves aside when its pins are gone or it had not retired every
    /// checkout, a rename that an earlier repair
    /// started is finished, and an archive that a restore already rebuilt
    /// is deleted. `name` may be either name of a rename that stopped. Each
    /// repair deletes only what the workspace already holds again. In
    /// [`Mode::DryRun`] it changes nothing and returns what it would do.
    /// The caller holds the workspace's lock.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the records and the pins, and of the
    /// moves and deletions, and [`Error::DatedNamesTaken`] when every dated
    /// name is taken.
    pub fn repair(
        &self,
        archives: &Archives,
        name: &WorkspaceName,
        mode: Mode,
    ) -> Result<Repaired, Error> {
        let all = archives.list()?;
        let Some(archive) = all.archives.iter().find(|archive| archive.name == *name) else {
            // `list` keeps a record that it cannot read in `problems`, so
            // the record of the name is read again here, and the error of
            // an unreadable one returns instead of "nothing is broken".
            archives.read(name)?;
            return self.repair_workspace(name);
        };
        // A rename that stopped is finished first, whichever of its two
        // names the person gave and whether or not a workspace holds the
        // older name now.
        if let Some((old, dated)) = rename_pair(archive, &all.archives) {
            return self.finish_rename(archives, old, &dated.name, mode);
        }
        let live = self.store.read(name)?;
        let Ok(reading) = archive_case(
            archive,
            live.as_ref(),
            &all.archives,
            count_as_there(&self.backend),
        );
        let repair = reading.broken(name).map(|(_, repair)| repair);
        let case = reading.case;
        let archive = archive.clone();
        match case {
            ArchiveCase::PinsGone | ArchiveCase::WorkspaceRemoved => {
                move_aside(archives, archive, mode)
            }
            // An archive that stopped before it retired every checkout,
            // beside a newer workspace, lost its own workspace before it
            // finished, so it moves aside without its pins being read.
            ArchiveCase::BesideNewerWorkspace if !archive.counts_as_retired() => {
                move_aside(archives, archive, mode)
            }
            ArchiveCase::BesideNewerWorkspace if pins_gone(&self.backend, &archive)? => {
                move_aside(archives, archive, mode)
            }
            ArchiveCase::BesideNewerWorkspace => {
                let to = self.dated_name(archives, &archive)?;
                self.finish_rename(archives, &archive, &to, mode)
            }
            ArchiveCase::RestoreUnfinished => Ok(Repaired::Deleted(Box::new(
                self.remove_archive(archives, archive, mode)?,
            ))),
            ArchiveCase::Finishable | ArchiveCase::Resumable(_) | ArchiveCase::Renaming(..) => {
                Ok(Repaired::Unchanged { repair })
            }
            ArchiveCase::ArchivedBesideWorkspace | ArchiveCase::Archived => {
                self.repair_workspace(name)
            }
        }
    }

    /// Returns the repair that the workspace record of `name` needs, which
    /// is never this module's, when no archive of the name is broken.
    fn repair_workspace(&self, name: &WorkspaceName) -> Result<Repaired, Error> {
        let records = Records {
            workspaces: self.store.read(name)?.into_iter().collect(),
            problems: Vec::new(),
        };
        let health = classify(&self.backend, &records, &ArchiveRecords::default());
        Ok(Repaired::Unchanged {
            repair: health
                .workspaces
                .into_iter()
                .find_map(|broken| broken.repair),
        })
    }

    /// Moves the older archive `old` to the name `to`, or finishes a move
    /// that an earlier run started, and returns it for the caller to
    /// finish. When its pins are gone, the renamed archive moves aside
    /// instead, so it is never finished.
    fn finish_rename(
        &self,
        archives: &Archives,
        old: &Archive,
        to: &WorkspaceName,
        mode: Mode,
    ) -> Result<Repaired, Error> {
        let renamed = match mode {
            Mode::Change => archives.rename(old, to)?,
            Mode::DryRun => old.renamed(to),
        };
        if pins_gone(&self.backend, &renamed)? {
            return move_aside(archives, renamed, mode);
        }
        Ok(Repaired::Renamed {
            from: old.name.clone(),
            archive: Box::new(renamed),
        })
    }

    /// Returns the name that the older archive `archive` takes: the first
    /// of [`dated_candidates`] that is free. A name is free when no
    /// workspace, no archive and no directory of the workspace root holds
    /// it, or when an archive of the same id holds it, which an earlier run
    /// of this repair that stopped partway left.
    fn dated_name(&self, archives: &Archives, archive: &Archive) -> Result<WorkspaceName, Error> {
        let parent = archive.path.parent();
        let mut last = String::new();
        for candidate in dated_candidates(archive) {
            match archives.read(&candidate)? {
                Some(existing) if existing.id == archive.id => return Ok(candidate),
                Some(_) => {}
                None => {
                    let taken = archives.dir_of(&candidate).exists()
                        || self.store.read(&candidate)?.is_some()
                        || parent.is_some_and(|parent| parent.join(candidate.as_str()).exists());
                    if !taken {
                        return Ok(candidate);
                    }
                }
            }
            last = candidate.to_string();
        }
        Err(Error::DatedNamesTaken {
            name: archive.name.clone(),
            last,
        })
    }
}

/// Returns the dated names that an older archive may take, in the order
/// that a repair tries them: `NAME-<date>`, from the day the archive
/// started, in UTC, then `NAME-<date>-2` up to [`MOST_DATED`]. A long name
/// is cut to leave room for the date, and a `-` that the cut leaves at the
/// end is dropped. A text that is not a valid name is left out.
pub(crate) fn dated_candidates(archive: &Archive) -> impl Iterator<Item = WorkspaceName> {
    // Seconds since the epoch in a u64, divided into days, always fit in
    // an i64, so the fallback is never taken.
    let days = i64::try_from(archive.time / 86_400).unwrap_or_default();
    let (year, month, day) = civil_from_days(days);
    let date = format!("{year:04}-{month:02}-{day:02}");
    let most = MOST_NAME_BYTES.saturating_sub(DATED_SUFFIX_BYTES);
    let cut: String = archive.name.as_str().chars().take(most).collect();
    let stem = cut.trim_end_matches('-').to_owned();
    (1..=MOST_DATED).filter_map(move |suffix| {
        let text = if suffix == 1 {
            format!("{stem}-{date}")
        } else {
            format!("{stem}-{date}-{suffix}")
        };
        text.parse().ok()
    })
}

/// Moves the directory of `archive` aside, or reports where it would go in
/// [`Mode::DryRun`].
fn move_aside(archives: &Archives, archive: Archive, mode: Mode) -> Result<Repaired, Error> {
    let kept = match mode {
        Mode::Change => archives.move_aside(&archive)?,
        Mode::DryRun => archives.aside_of(&archive),
    };
    Ok(Repaired::MovedAside {
        ignored: archives.ignored_tar(&archive),
        archive: Box::new(archive),
        kept,
    })
}
