//! Blocks of host ports that end-to-end tests claim, so tests that run at
//! once, in one checkout or in two, never allocate the same port.
//!
//! nextest runs each test in a process of its own, so the blocks are
//! claimed through the file system: a test holds an exclusive lock on
//! `<temp>/mxv-test-ports/block-<n>.lock` while it runs. The operating
//! system releases the lock when the test's process ends, even when the
//! test crashes. The blocks lie from 10000 to 19999, below the default
//! range of `mxv up`, so a test that allocates from the default range never
//! takes a port of another test's block.

use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

/// The first port of the first block.
const FIRST: u16 = 10000;

/// The number of ports in one block.
const SIZE: u16 = 40;

/// The number of blocks.
const BLOCKS: u16 = 250;

/// A block of host ports that this test holds until the value drops.
pub(crate) struct PortBlock {
    /// The first port of the block.
    pub(crate) first: u16,
    /// The locked file that claims the block.
    _file: File,
}

impl PortBlock {
    /// Claims the first block that no other running test holds.
    pub(crate) fn claim() -> Self {
        let dir = std::env::temp_dir().join("mxv-test-ports");
        fs::create_dir_all(&dir).unwrap();
        for block in 0..BLOCKS {
            let path: PathBuf = dir.join(format!("block-{block}.lock"));
            #[expect(
                clippy::disallowed_methods,
                reason = "the test fixture opens the lock file that claims a block of ports"
            )]
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            #[expect(
                clippy::disallowed_methods,
                reason = "the test fixture locks a block of ports for the test's lifetime"
            )]
            let locked = file.try_lock();
            if locked.is_ok() {
                return Self {
                    first: FIRST + block * SIZE,
                    _file: file,
                };
            }
        }
        panic!("every block of test ports is claimed");
    }

    /// Returns the config file line that makes `mxv up` allocate from the
    /// block.
    pub(crate) fn config_line(&self) -> String {
        format!(
            "ports_range = [{}, {}]\n",
            self.first,
            self.first + SIZE - 1
        )
    }
}
