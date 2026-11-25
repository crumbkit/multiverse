//! The tests that hold the state diagrams in `crates/mxv/README.md` and
//! `LIFECYCLE.md` to the states that the records store.

#![expect(
    clippy::disallowed_methods,
    reason = "the tests read the files and directories that they and the code under test write"
)]

use std::fs;
use std::path::Path;

use crate::{ArchiveState, WorkspaceState};

/// Returns the name of each [`WorkspaceState`], as its record writes it.
/// The `match` fails to compile when a state is added, until this list
/// names it.
fn workspace_states() -> [&'static str; 3] {
    let _ = |state: &WorkspaceState| match state {
        WorkspaceState::Creating | WorkspaceState::Live | WorkspaceState::Retiring { .. } => {}
    };
    ["creating", "live", "retiring"]
}

/// Returns the name of each [`ArchiveState`], as the documents write it.
/// The `match` fails to compile when a state is added, until this list
/// names it.
fn archive_states() -> [&'static str; 3] {
    let _ = |state: ArchiveState| match state {
        ArchiveState::Retiring | ArchiveState::Retired | ArchiveState::Archived => {}
    };
    ["Retiring", "Retired", "Archived"]
}

/// Returns the first fenced `text` block after the line `heading` of the
/// file at `path`, relative to the repository's root.
fn diagram(path: &str, heading: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = fs::read_to_string(root.join(path))
        .unwrap_or_else(|error| panic!("{path} cannot be read: {error}"));
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| *line == heading)
        .unwrap_or_else(|| panic!("{path} has no heading {heading}"));
    let fence = lines[start..]
        .iter()
        .position(|line| *line == "```text")
        .unwrap_or_else(|| panic!("{path} has no text block after {heading}"))
        + start;
    lines[fence + 1..]
        .iter()
        .take_while(|line| **line != "```")
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns each name of `names` that `diagram` does not hold as a word.
fn missing<'a>(diagram: &str, names: &[&'a str]) -> Vec<&'a str> {
    let words: Vec<&str> = diagram
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .collect();
    names
        .iter()
        .copied()
        .filter(|name| !words.contains(name))
        .collect()
}

#[test]
fn the_diagrams_name_every_workspace_state_and_every_archive_state() {
    let names: Vec<&str> = workspace_states()
        .into_iter()
        .chain(archive_states())
        .chain(["restoring"])
        .collect();
    for (path, heading) in [
        ("crates/mxv/README.md", "## Lifecycle"),
        ("LIFECYCLE.md", "# Lifecycle"),
    ] {
        let missing = missing(&diagram(path, heading), &names);
        assert!(
            missing.is_empty(),
            "the diagram under {heading} in {path} does not name {missing:?}"
        );
    }
}
