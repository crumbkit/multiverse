//! The process table that mxv's `clean` and `reclaim` read, as phase reads
//! it (PROC-1).

use phase::host::ProcessError;

use crate::error::Error;

/// Returns every process that has not ended, with its parent, its group,
/// its working directory and its resident memory, as phase reads them.
///
/// # Errors
///
/// Returns [`Error::ProcessTable`] when the process table cannot be read,
/// and [`Error::UnsupportedOs`] when this system's structs differ from
/// those phase reads.
pub fn process_table() -> Result<Vec<phase::host::TableEntry>, Error> {
    phase::host::process_table().map_err(process_error)
}

/// Returns mxv's error for `error`, a failure of phase's process table.
pub(crate) fn process_error(error: ProcessError) -> Error {
    match error {
        ProcessError::Unsupported {
            call,
            pid,
            written,
            expected,
            ..
        } => Error::UnsupportedOs {
            message: format!(
                "{call} for the process {pid} wrote {written} bytes, and mxv reads a struct of {expected}"
            ),
        },
        other => Error::ProcessTable {
            message: other.to_string(),
        },
    }
}
