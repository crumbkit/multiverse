//! The failure of a workspace verb that is declared before it is built.

use std::fmt;

use crumbkit::{Code, Document, Failure, FieldValue, Prose};

/// The code of a verb that is declared and not built yet. The area is
/// `command`, because the failure is about the command itself.
pub const UNBUILT: Code = Code::new("mxv", "command", "unbuilt");

/// The document that an unbuilt verb passes to `crumbkit::finish`. The verb
/// always fails, so the document never prints, and no leaf declares it.
pub(crate) const UNBUILT_RESULT: Document<Prose<String>> = Document::new(
    "mxv",
    "unbuilt",
    1,
    "The result of a verb that is not built yet, which never prints.",
);

/// A workspace verb ran before the pull request that builds it.
#[derive(Debug, Failure)]
#[failure(
    code = UNBUILT,
    hint = "this version of mxv declares the command and does not run it yet, so use a release that builds it",
    field(command = FieldValue::from((*command).to_owned())),
)]
pub(crate) struct Unbuilt {
    /// The verb's name, such as `new`.
    command: &'static str,
}

impl Unbuilt {
    /// Returns the failure of the unbuilt verb `command`.
    pub(crate) const fn new(command: &'static str) -> Self {
        Self { command }
    }
}

impl fmt::Display for Unbuilt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "mxv {} is not built yet", self.command)
    }
}

impl std::error::Error for Unbuilt {}

/// Returns the unbuilt failure of `command`, as the result that
/// `crumbkit::finish` prints.
pub(crate) fn fail(command: &'static str) -> Result<Prose<String>, Unbuilt> {
    Err(Unbuilt::new(command))
}
