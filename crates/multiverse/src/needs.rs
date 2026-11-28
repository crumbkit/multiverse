//! The `[needs.*]` and `[provides.*]` tables of `mxv.toml`, which say what
//! an app needs from other repositories and what it provides to them, and
//! the dependency plan of one start with the checks that it must pass.
//!
//! This module holds the schema and the one parser of both tables. A need
//! names only its provider: the repository that provides it, as an origin
//! such as `github.com/acme/dispatcher`, or a contract, and `from`, the
//! port, route, unit or socket provide of that provider that it uses.
//!
//! ```toml
//! [needs.dispatcher-http]
//! repo = "github.com/acme/dispatcher"
//! from = "http"
//!
//! [provides.deadband-socket]
//! kind = "socket"
//! unit = "daemon"
//! path = "run/deadband.sock"
//! ```
//!
//! The variable that receives the need, and whether the process can start
//! without it, belong to the entry of the process's `starmap.toml` whose
//! bind names the need, so a need takes neither `as` nor `optional`. A
//! table with either is refused with [`RefusalCode::Moved`]. The old
//! spellings `[wants]` and `[offers]` are refused with
//! [`RefusalCode::Renamed`]. A need with neither `repo` nor `contract`,
//! such as `[needs.db] kind = "postgres"`, describes a shared service, and
//! is refused with [`RefusalCode::ServiceKeys`], because a process lists
//! a shared service that it uses in its own `needs` key.
//!
//! [`parse`] reads the text that the manifest passes it. It starts no
//! process and reads no file. A refused table leaves the other tables of
//! the file, and the manifest reports each refusal as a warning.
//!
//! [`Plan`] is the dependency plan of one start, and [`check`] runs its
//! checks.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use toml::Spanned;
use toml::de::{DeTable, DeValue};

use crate::mirror::RepoOrigin;

mod check;
mod plan;
#[cfg(test)]
mod tests;

pub use check::{Check, CheckFailure, CheckFailures, check};
pub use plan::{Known, Met, Plan, ProvidedVariable, SecretKind, SecretVariable, UnitPlan};

/// Why a table of `[needs.*]`, `[provides.*]`, `[wants]` or `[offers]` was
/// refused. Each has a stable code, which [`RefusalCode::as_str`] spells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum RefusalCode {
    /// A need holds `as` or `optional`, which moved to the starmap entry
    /// whose bind names the need: `mxv::needs::moved`.
    Moved,
    /// The file declares `[wants]` or `[offers]`, the spellings that
    /// `[needs.<name>]` and `[provides.<name>]` replaced:
    /// `mxv::needs::renamed`.
    Renamed,
    /// A need names no provider, as a table that describes a shared
    /// service does: `mxv::needs::service_keys`.
    ServiceKeys,
    /// A need is not a table, holds an unknown key, names both a
    /// repository and a contract, or holds a key whose value is refused:
    /// `mxv::needs::invalid`.
    Invalid,
    /// A provide is not a socket of a unit that the file declares, at a
    /// path inside the checkout: `mxv::provides::invalid`.
    ProvideInvalid,
}

impl RefusalCode {
    /// Returns the code as mxv spells it, such as `mxv::needs::moved`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Moved => "mxv::needs::moved",
            Self::Renamed => "mxv::needs::renamed",
            Self::ServiceKeys => "mxv::needs::service_keys",
            Self::Invalid => "mxv::needs::invalid",
            Self::ProvideInvalid => "mxv::provides::invalid",
        }
    }
}

/// One table that [`parse`] refused, with the line of its name. The
/// message never quotes a value of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Refusal {
    /// Why it was refused.
    pub code: RefusalCode,
    /// The line, counted from 1, of the table's name.
    pub line: Option<usize>,
    /// What is wrong and how to fix it, in a sentence.
    pub message: String,
}

/// Who provides a need.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Provider {
    /// The repository whose origin this is, such as
    /// `github.com/acme/dispatcher`.
    Repo(RepoOrigin),
    /// A contract, which a catalogue resolves to a repository.
    Contract(String),
}

/// One `[needs.<name>]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Need {
    /// The need's name, the table's key.
    pub name: String,
    /// Who provides it.
    pub provider: Provider,
    /// The port, route, unit or socket provide of the provider that it
    /// uses.
    pub from: String,
    /// The line, counted from 1, of the table's name.
    pub line: usize,
}

/// One `[provides.<name>]` table: a socket that a unit of the app serves.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Provide {
    /// The provide's name, the table's key.
    pub name: String,
    /// The process or container of the file that serves the socket.
    pub unit: String,
    /// The socket's path, relative to the top of the checkout.
    pub path: PathBuf,
    /// The line, counted from 1, of the table's name.
    pub line: usize,
}

/// The needs and provides that one file declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Declared {
    /// The needs, in name order.
    pub needs: Vec<Need>,
    /// The provides, in name order.
    pub provides: Vec<Provide>,
}

impl Declared {
    /// Reports whether the file declares no need and no provide.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.needs.is_empty() && self.provides.is_empty()
    }
}

/// The file that declares a set of needs and provides, in a workspace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Holder {
    /// The workspace manifest, or the one file that a manifest read on
    /// its own holds.
    Workspace,
    /// The `mxv.toml` of the checkout in this folder of the workspace.
    Checkout(String),
}

/// What [`parse`] read from one file: the tables it accepted and those it
/// refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Parsed {
    /// The needs and provides that it accepted.
    pub declared: Declared,
    /// The tables that it refused, in the order of their lines.
    pub refusals: Vec<Refusal>,
}

/// The keys of a need.
const NEED_KEYS: &[&str] = &["repo", "contract", "from"];

/// The keys that a need held before the starmap entry took them.
const MOVED_KEYS: &[&str] = &["as", "optional"];

/// The keys of a provide.
const PROVIDE_KEYS: &[&str] = &["kind", "unit", "path"];

/// The most bytes of a need's, provide's or contract's name.
const MOST_NAME_BYTES: usize = 64;

/// Returns the needs and provides of the manifest `text`, and each table
/// that it refuses. `declares` reports whether the file declares the
/// process or container of a name, which a provide's `unit` must name. A
/// text that is not valid TOML declares nothing, because the manifest
/// refuses it first.
#[must_use]
pub fn parse(text: &str, declares: &dyn Fn(&str) -> bool) -> Parsed {
    let mut parsed = Parsed::default();
    let Ok(table) = DeTable::parse(text) else {
        return parsed;
    };
    let reader = Reader { text, declares };
    for (key, value) in table.get_ref() {
        let line = Some(reader.line(&key.span()));
        match key.get_ref().as_ref() {
            "needs" => reader.tables(value, &mut parsed, &|table| {
                reader.need(table).map(Entry::Need)
            }),
            "provides" => reader.tables(value, &mut parsed, &|table| {
                reader.provide(table).map(Entry::Provide)
            }),
            "wants" => parsed.refusals.push(Refusal {
                code: RefusalCode::Renamed,
                line,
                message: "mxv.toml declares [wants], which is now spelled [needs.<name>], with the key from in place of provides".to_owned(),
            }),
            "offers" => parsed.refusals.push(Refusal {
                code: RefusalCode::Renamed,
                line,
                message: "mxv.toml declares [offers], which is now spelled [provides.<name>], and each provide is a socket with the keys kind, unit and path".to_owned(),
            }),
            _ => {}
        }
    }
    parsed.refusals.sort_by_key(|refusal| refusal.line);
    parsed
}

/// One table that [`Reader::tables`] accepted.
enum Entry {
    /// A need.
    Need(Need),
    /// A provide.
    Provide(Provide),
}

/// The text of one file and the units it declares, while [`parse`] reads
/// it.
struct Reader<'a> {
    /// The file's text.
    text: &'a str,
    /// Whether the file declares a unit of a name.
    declares: &'a dyn Fn(&str) -> bool,
}

/// One table as the reader sees it: its name, its keys and its line.
struct Table<'t, 'i> {
    /// The table's name, its key in `[needs]` or `[provides]`.
    name: &'t str,
    /// Its keys and values.
    keys: &'t DeTable<'i>,
    /// The line of its name.
    line: usize,
}

impl Table<'_, '_> {
    /// Returns the value of `key`, if the table holds it.
    fn get(&self, key: &str) -> Option<&DeValue<'_>> {
        self.keys
            .iter()
            .find(|(name, _)| name.get_ref().as_ref() == key)
            .map(|(_, value)| value.get_ref())
    }

    /// Reports whether the table holds `key`.
    fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Returns the string value of `key`, or the clause that says it is
    /// missing or not a string, in the words of `what`, such as `need`.
    fn string(&self, key: &str, what: &str) -> Result<&str, String> {
        match self.get(key) {
            None => Err(format!("the {what} `{}` has no key {key}", self.name)),
            Some(DeValue::String(text)) => Ok(text.as_ref()),
            Some(_) => Err(format!(
                "the key {key} of the {what} `{}` is a string",
                self.name
            )),
        }
    }
}

impl Reader<'_> {
    /// Returns the line, counted from 1, at the start of `span`.
    fn line(&self, span: &Range<usize>) -> usize {
        self.text
            .get(..span.start.min(self.text.len()))
            .map_or(0, |before| before.matches('\n').count())
            + 1
    }

    /// Reads each table of the top-level `value`, `[needs]` or
    /// `[provides]`, with `read`, and adds what it accepts and refuses to
    /// `parsed`.
    fn tables(
        &self,
        value: &Spanned<DeValue<'_>>,
        parsed: &mut Parsed,
        read: &dyn Fn(&Table<'_, '_>) -> Result<Entry, Refusal>,
    ) {
        let DeValue::Table(tables) = value.get_ref() else {
            parsed.refusals.push(Refusal {
                code: RefusalCode::Invalid,
                line: Some(self.line(&value.span())),
                message:
                    "the top-level key holds tables, one for each name, such as [needs.<name>]"
                        .to_owned(),
            });
            return;
        };
        for (name, entry) in tables {
            let line = self.line(&name.span());
            let DeValue::Table(keys) = entry.get_ref() else {
                parsed.refusals.push(Refusal {
                    code: RefusalCode::Invalid,
                    line: Some(line),
                    message: format!("`{}` is a table of keys", name.get_ref()),
                });
                continue;
            };
            let table = Table {
                name: name.get_ref().as_ref(),
                keys,
                line,
            };
            match read(&table) {
                Ok(Entry::Need(need)) => parsed.declared.needs.push(need),
                Ok(Entry::Provide(provide)) => parsed.declared.provides.push(provide),
                Err(refusal) => parsed.refusals.push(refusal),
            }
        }
    }

    /// Reads one need, or says why it is refused.
    fn need(&self, table: &Table<'_, '_>) -> Result<Need, Refusal> {
        let refuse = |code, message: String| Refusal {
            code,
            line: Some(table.line),
            message,
        };
        let invalid = |message: String| refuse(RefusalCode::Invalid, message);
        let name = table.name;
        if MOVED_KEYS.iter().any(|key| table.has(key)) {
            return Err(refuse(
                RefusalCode::Moved,
                format!(
                    "the need `{name}` holds as or optional, which moved to the starmap entry whose bind names the need: that entry names the variable, and its unset says whether the process can start without it; remove both keys from the need"
                ),
            ));
        }
        let provider = match (table.has("repo"), table.has("contract")) {
            (false, false) => {
                return Err(refuse(
                    RefusalCode::ServiceKeys,
                    format!(
                        "the need `{name}` holds neither repo nor contract, and a need names a provider; a process lists a shared service that it uses in its `needs` key, with the service declared under [service.<name>]"
                    ),
                ));
            }
            (true, true) => {
                return Err(invalid(format!(
                    "the need `{name}` holds both repo and contract, and names its provider with exactly one"
                )));
            }
            (true, false) => {
                let text = table.string("repo", "need").map_err(invalid)?;
                Provider::Repo(RepoOrigin::parse(text).ok_or_else(|| {
                    invalid(format!(
                        "the key repo of the need `{name}` is an origin, such as github.com/acme/dispatcher or a git URL"
                    ))
                })?)
            }
            (false, true) => {
                let text = table.string("contract", "need").map_err(invalid)?;
                if !is_contract(text) {
                    return Err(invalid(format!(
                        "the key contract of the need `{name}` is 1 to {MOST_NAME_BYTES} letters, digits, '.', '-' and '_'"
                    )));
                }
                Provider::Contract(text.to_owned())
            }
        };
        if let Some((key, _)) = table
            .keys
            .iter()
            .find(|(key, _)| !NEED_KEYS.contains(&key.get_ref().as_ref()))
        {
            return Err(invalid(format!(
                "the need `{name}` holds the key {}, and a need holds only repo or contract, and from",
                key.get_ref()
            )));
        }
        let from = table.string("from", "need").map_err(invalid)?;
        if !is_name(from) {
            return Err(invalid(format!(
                "the key from of the need `{name}` names a port, route, unit or socket of the provider, in 1 to {MOST_NAME_BYTES} lowercase letters, digits, '-' and '_'"
            )));
        }
        if !is_name(name) {
            return Err(invalid(format!(
                "the name of a need is 1 to {MOST_NAME_BYTES} lowercase letters, digits, '-' and '_'"
            )));
        }
        Ok(Need {
            name: name.to_owned(),
            provider,
            from: from.to_owned(),
            line: table.line,
        })
    }

    /// Reads one provide, or says why it is refused.
    fn provide(&self, table: &Table<'_, '_>) -> Result<Provide, Refusal> {
        let invalid = |message: String| Refusal {
            code: RefusalCode::ProvideInvalid,
            line: Some(table.line),
            message,
        };
        let name = table.name;
        if let Some((key, _)) = table
            .keys
            .iter()
            .find(|(key, _)| !PROVIDE_KEYS.contains(&key.get_ref().as_ref()))
        {
            return Err(invalid(format!(
                "the provide `{name}` holds the key {}, and a provide holds only kind, unit and path",
                key.get_ref()
            )));
        }
        if table.string("kind", "provide").map_err(invalid)? != "socket" {
            return Err(invalid(format!(
                "the key kind of the provide `{name}` is socket, the only kind so far"
            )));
        }
        let unit = table.string("unit", "provide").map_err(invalid)?;
        if !(self.declares)(unit) {
            return Err(invalid(format!(
                "the key unit of the provide `{name}` names a process or container that the file does not declare"
            )));
        }
        let path = Path::new(table.string("path", "provide").map_err(invalid)?);
        if !inside(path) {
            return Err(invalid(format!(
                "the key path of the provide `{name}` is a path inside the checkout, with no '..' name and no leading '/'"
            )));
        }
        if !is_name(name) {
            return Err(invalid(format!(
                "the name of a provide is 1 to {MOST_NAME_BYTES} lowercase letters, digits, '-' and '_'"
            )));
        }
        Ok(Provide {
            name: name.to_owned(),
            unit: unit.to_owned(),
            path: path.to_path_buf(),
            line: table.line,
        })
    }
}

/// Reports whether `text` is the name of a need, a provide or the provide
/// that a need uses: 1 to [`MOST_NAME_BYTES`] lowercase letters, digits,
/// '-' and '_'.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MOST_NAME_BYTES
        && text.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

/// Reports whether `text` names a contract: 1 to [`MOST_NAME_BYTES`]
/// letters, digits, '.', '-' and '_'.
fn is_contract(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MOST_NAME_BYTES
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Reports whether `path` is a relative path with at least one name and
/// no `..`.
fn inside(path: &Path) -> bool {
    let mut names = 0_usize;
    for component in path.components() {
        match component {
            Component::Normal(_) => names = names.saturating_add(1),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    names > 0
}
