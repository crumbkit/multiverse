//! The dependency plan of one start: for each unit, the provided variables
//! it receives and how each was met, the secret variables it receives by
//! name and reference, and the units it starts after. The plan holds no
//! value of any variable, so a check can name everything in it.

use std::collections::{BTreeMap, BTreeSet};

/// The dependency plan of one start of a workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// Each unit of the start, by its name in the merged manifest.
    pub units: BTreeMap<String, UnitPlan>,
    /// What the workspace has, which every met variable must name.
    pub known: Known,
}

/// The checkouts, units and ports that a workspace has.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Known {
    /// The folders of the workspace's checkouts.
    pub checkouts: BTreeSet<String>,
    /// Each unit of the merged manifest, with the ports it listens on or
    /// publishes and those its variables name, by their merged names.
    pub ports: BTreeMap<String, BTreeSet<String>>,
}

/// What one unit receives and what it starts after.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnitPlan {
    /// Each provided variable of the unit's starmap file, in name order.
    pub provided: Vec<ProvidedVariable>,
    /// Each secret or generated variable that the unit receives, in name
    /// order.
    pub secrets: Vec<SecretVariable>,
    /// Each variable that the unit's starmap file declares as a secret or
    /// a generated value, with which of the two it is, including those
    /// that the manifest sets in their place.
    pub declared_secrets: BTreeMap<String, SecretKind>,
    /// The variables whose value the manifest's `env` table writes.
    pub literals: BTreeSet<String>,
    /// The units that the unit starts after, by their merged names.
    pub after: Vec<String>,
}

/// A provided variable of a unit, and how the plan met it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProvidedVariable {
    /// The variable.
    pub variable: String,
    /// Whether the unit's starmap file requires it in local mode.
    pub required: bool,
    /// How the plan met it.
    pub met: Met,
}

/// How the plan met a provided variable.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Met {
    /// A port of a unit of the workspace gives its value.
    Port {
        /// The checkout folder that the bind names as `<checkout>/<unit>`,
        /// or `None` when it names a unit by its bare name.
        checkout: Option<String>,
        /// The unit, by its merged name.
        unit: String,
        /// The port, by its merged name.
        port: String,
    },
    /// The plan leaves it unset, which only an optional variable allows.
    Unset,
}

/// A secret or generated variable that a unit receives.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SecretVariable {
    /// The variable.
    pub variable: String,
    /// The store that holds its value, such as `1password` or `keychain`.
    pub store: String,
    /// The reference that the store reads, such as a 1Password reference,
    /// or `None` for a value that the Keychain keeps under the workspace.
    pub reference: Option<String>,
}

/// What a starmap file declares a variable's source as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SecretKind {
    /// `source = "secret"`.
    Secret,
    /// `source = "generated"`.
    Generated,
}
