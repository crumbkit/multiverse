//! The tests that hold `README.md` to the failure codes that mxv defines.

use std::fs;
use std::path::Path;

/// Returns the rows of the first table after the heading `heading` in
/// `text`, one string for each row below the separator, or `None` when
/// `text` has no such heading.
fn table_after<'a>(text: &'a str, heading: &str) -> Option<Vec<&'a str>> {
    let start = text.lines().position(|line| line == heading)?;
    let rows = text
        .lines()
        .skip(start + 1)
        .skip_while(|line| !line.starts_with('|'))
        .take_while(|line| line.starts_with('|'))
        .skip(2)
        .collect();
    Some(rows)
}

#[test]
fn the_restore_table_has_a_row_for_every_restore_code() {
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("README.md can be read");
    let rows =
        table_after(&readme, "### Restoring").expect("README.md has the heading ### Restoring");
    let missing: Vec<&str> = mxv::CODES
        .iter()
        .chain(mxv::ARCHIVE_CODES)
        .map(|code| code.as_str())
        .filter(|code| code.starts_with("mxv::restore::"))
        .filter(|code| {
            let cell = format!("| `{code}` |");
            !rows.iter().any(|row| row.starts_with(&cell))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "the table under ### Restoring in crates/mxv/README.md has no row for {missing:?}"
    );
}
