//! The repairs of the broken records whose repair is
//! [`Repair::Repair`](super::Repair::Repair).

use std::io;
use std::path::PathBuf;

use crate::archive::{Archive, ArchiveRecords, ArchivedRemoval, Archives};
use crate::backend::Backend;
use crate::error::Error;
use crate::name::{MOST_NAME_BYTES, WorkspaceName};
use crate::store::Records;
use crate::workspaces::{Mode, Workspaces};

use super::{Problem, Repair, classify, pins_gone};

/// The most dated names that [`Workspaces::repair`] tries for an older
/// archive: `NAME-<date>`, then `NAME-<date>-2` up to this suffix. When
/// every one is taken, the repair fails and changes nothing.
const MOST_DATED: u32 = 99;

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
    /// is deleted.
    MovedAside {
        /// The archive as it was.
        archive: Box<Archive>,
        /// The directory that now holds the archive's record and files.
        kept: PathBuf,
        /// The private tar of the archive's ignored files, which stays
        /// where it is, or `None` when the archive has none.
        ignored: Option<PathBuf>,
    },
    /// An older archive, whose name a newer workspace holds, now has the
    /// name of `archive` and is still retiring, so the caller finishes it
    /// as `mxv archive -w` finishes an archive.
    Renamed {
        /// The name the archive had.
        from: WorkspaceName,
        /// The archive under its new name.
        archive: Box<Archive>,
    },
    /// The archive that a restore had already rebuilt is deleted, with its
    /// pins and its private tar.
    Deleted(ArchivedRemoval),
}

impl<B: Backend> Workspaces<B> {
    /// Repairs the records of the workspace `name` whose repair is
    /// [`Repair::Repair`]: an archive whose pins are gone moves aside, an
    /// older archive beside a newer workspace takes a dated name, or moves
    /// aside when its pins are gone, and an archive that a restore already
    /// rebuilt is deleted. Each repair deletes only what the workspace
    /// already holds again. In [`Mode::DryRun`] it changes nothing and
    /// returns what it would do. The caller holds the workspace's lock.
    ///
    /// # Errors
    ///
    /// Returns the errors of reading the records and the pins, and of the
    /// moves and deletions.
    pub fn repair(
        &self,
        archives: &Archives,
        name: &WorkspaceName,
        mode: Mode,
    ) -> Result<Repaired, Error> {
        let records = Records {
            workspaces: self.store.read(name)?.into_iter().collect(),
            problems: Vec::new(),
        };
        let found = ArchiveRecords {
            archives: archives.read(name)?.into_iter().collect(),
            problems: Vec::new(),
        };
        let health = classify(&self.backend, &records, &found);
        let (Some(broken), Some(archive)) = (
            health.archives.into_iter().next(),
            found.archives.into_iter().next(),
        ) else {
            return Ok(Repaired::Unchanged {
                repair: health
                    .workspaces
                    .into_iter()
                    .find_map(|broken| broken.repair),
            });
        };
        match broken.problem {
            Problem::ArchivePinsGone => move_aside(archives, archive, mode),
            Problem::ArchiveBesideNewerWorkspace if pins_gone(&self.backend, &archive)? => {
                move_aside(archives, archive, mode)
            }
            Problem::ArchiveBesideNewerWorkspace => {
                let to = self.dated_name(archives, &archive)?;
                let renamed = match mode {
                    Mode::Change => archives.rename(&archive, &to)?,
                    Mode::DryRun => {
                        let mut renamed = archive.clone();
                        renamed.name = to;
                        renamed
                    }
                };
                Ok(Repaired::Renamed {
                    from: archive.name,
                    archive: Box::new(renamed),
                })
            }
            Problem::RestoreUnfinished => Ok(Repaired::Deleted(
                self.remove_archive(archives, archive, mode)?,
            )),
            Problem::Unreadable { .. } | Problem::ArchiveUnfinished | Problem::CheckoutsGone => {
                Ok(Repaired::Unchanged {
                    repair: broken.repair,
                })
            }
        }
    }

    /// Returns the name that the older archive `archive` takes:
    /// `NAME-<date>`, from the day the archive started, in UTC, or the
    /// first of `NAME-<date>-2` and on that is free. A name is free when no
    /// workspace, no archive and no directory of the workspace root holds
    /// it, or when an archive of the same id holds it, which an earlier run
    /// of this repair that stopped partway left.
    fn dated_name(&self, archives: &Archives, archive: &Archive) -> Result<WorkspaceName, Error> {
        let (year, month, day) = civil_from_days(archive.time / 86_400);
        let date = format!("{year:04}-{month:02}-{day:02}");
        // `-<date>-99` takes 14 bytes, so a long name is cut to leave room.
        let most = MOST_NAME_BYTES.saturating_sub(14);
        let stem: String = archive.name.as_str().chars().take(most).collect();
        let parent = archive.path.parent();
        for suffix in 1..=MOST_DATED {
            let text = if suffix == 1 {
                format!("{stem}-{date}")
            } else {
                format!("{stem}-{date}-{suffix}")
            };
            let Ok(candidate) = text.parse::<WorkspaceName>() else {
                continue;
            };
            match archives.read(&candidate)? {
                Some(existing) if existing.id == archive.id => return Ok(candidate),
                Some(_) => continue,
                None => {}
            }
            let taken = archives.dir_of(&candidate).exists()
                || self.store.read(&candidate)?.is_some()
                || parent.is_some_and(|parent| parent.join(candidate.as_str()).exists());
            if !taken {
                return Ok(candidate);
            }
        }
        Err(Error::Unwritable {
            path: archives.dir_of(&archive.name),
            source: io::Error::other(format!(
                "every dated name of {} up to the suffix -{MOST_DATED} is taken",
                archive.name
            )),
        })
    }
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

/// Returns the year, month and day of `days` since the Unix epoch, with
/// Howard Hinnant's `civil_from_days`. phase holds the same function for
/// docker's timestamps (`phase/src/engine/container/runner.rs`), which it
/// does not export.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn days_since_the_epoch_give_the_calendar_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_394), (2025, 11, 2));
    }
}
