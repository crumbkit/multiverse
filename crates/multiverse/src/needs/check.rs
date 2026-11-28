//! The five checks that the dependency plan of a start must pass before mxv
//! builds or starts anything.
//!
//! 1. [`Check::SecretValue`]: the manifest's `env` writes no value for a
//!    variable that the unit's starmap file declares `source = "secret"`.
//! 2. [`Check::RequiredMet`]: every provided variable that the starmap file
//!    requires is met.
//! 3. [`Check::KnownProviders`]: every met variable names a checkout, unit
//!    and port that the workspace has.
//! 4. [`Check::SecretReach`]: a secret reaches only the units whose starmap
//!    file declares it.
//! 5. [`Check::StartOrder`]: every unit that a unit starts after is in the
//!    plan, and the start order has no cycle.
//!
//! A failure names its check, its unit and its variable, and never quotes
//! a value, because the plan holds none.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::plan::{Met, Plan, SecretKind};

#[cfg(test)]
mod tests;

/// One of the five checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Check {
    /// The manifest writes no value for a secret variable.
    SecretValue,
    /// Every required provided variable is met.
    RequiredMet,
    /// Every met variable names a checkout, unit and port of the
    /// workspace.
    KnownProviders,
    /// A secret reaches only the units that declare it.
    SecretReach,
    /// Every unit a unit starts after exists, and the start order has no
    /// cycle.
    StartOrder,
}

impl Check {
    /// Every check, in the order [`check`] runs them.
    pub const ALL: &'static [Self] = &[
        Self::SecretValue,
        Self::RequiredMet,
        Self::KnownProviders,
        Self::SecretReach,
        Self::StartOrder,
    ];

    /// Returns the check's name, such as `no-secret-value`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SecretValue => "no-secret-value",
            Self::RequiredMet => "required-met",
            Self::KnownProviders => "known-providers",
            Self::SecretReach => "secret-reach",
            Self::StartOrder => "start-order",
        }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// One unit, and the variable when there is one, that failed a check.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckFailure {
    /// The check that failed.
    pub check: Check,
    /// The unit, by its merged name.
    pub unit: String,
    /// The variable, or `None` for a failure of the unit's start order.
    pub variable: Option<String>,
    /// What is wrong, in a clause that holds no value.
    pub reason: String,
}

impl CheckFailure {
    /// Returns the sentence that names the check, the unit, the variable
    /// and what is wrong.
    #[must_use]
    pub fn sentence(&self) -> String {
        match &self.variable {
            Some(variable) => format!(
                "{}: the variable {variable} of the unit `{}` {}",
                self.check, self.unit, self.reason
            ),
            None => format!("{}: the unit `{}` {}", self.check, self.unit, self.reason),
        }
    }
}

/// The failures of a plan's checks, at least one, in the order of the
/// checks and then of their unit and variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckFailures {
    /// The first failure.
    first: CheckFailure,
    /// The failures after the first.
    rest: Vec<CheckFailure>,
}

impl CheckFailures {
    /// Returns the failures of `failures`, or `None` when it is empty.
    fn of(failures: Vec<CheckFailure>) -> Option<Self> {
        let mut failures = failures.into_iter();
        let first = failures.next()?;
        Some(Self {
            first,
            rest: failures.collect(),
        })
    }

    /// Returns the first failure.
    #[must_use]
    pub fn first(&self) -> &CheckFailure {
        &self.first
    }

    /// Returns every failure, the first one first.
    pub fn iter(&self) -> impl Iterator<Item = &CheckFailure> {
        std::iter::once(&self.first).chain(&self.rest)
    }

    /// Returns how many failures there are, at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        1 + self.rest.len()
    }

    /// Reports whether there are no failures, which is never so.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Display for CheckFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sentences: Vec<String> = self.iter().map(CheckFailure::sentence).collect();
        write!(
            formatter,
            "the dependency plan fails {} of its checks: {}",
            self.len(),
            sentences.join("; ")
        )
    }
}

/// Runs every check on `plan`.
///
/// # Errors
///
/// Returns every failure of every check, in the order of [`Check::ALL`].
pub fn check(plan: &Plan) -> Result<(), CheckFailures> {
    let mut failures = Vec::new();
    secret_values(plan, &mut failures);
    required_met(plan, &mut failures);
    known_providers(plan, &mut failures);
    secret_reach(plan, &mut failures);
    start_order(plan, &mut failures);
    CheckFailures::of(failures).map_or(Ok(()), Err)
}

/// Returns the failure of `check` for `unit`'s `variable`.
fn failure(check: Check, unit: &str, variable: Option<&str>, reason: &str) -> CheckFailure {
    CheckFailure {
        check,
        unit: unit.to_owned(),
        variable: variable.map(str::to_owned),
        reason: reason.to_owned(),
    }
}

/// Check 1: no `env` value of the manifest for a secret variable.
fn secret_values(plan: &Plan, failures: &mut Vec<CheckFailure>) {
    for (name, unit) in &plan.units {
        for variable in &unit.literals {
            if unit.declared_secrets.get(variable) == Some(&SecretKind::Secret) {
                failures.push(failure(
                    Check::SecretValue,
                    name,
                    Some(variable),
                    "is a secret in its starmap file, and the manifest's env writes a value for it; remove it from env so mxv reads it from its store",
                ));
            }
        }
    }
}

/// Check 2: every required provided variable is met.
fn required_met(plan: &Plan, failures: &mut Vec<CheckFailure>) {
    for (name, unit) in &plan.units {
        for provided in &unit.provided {
            if provided.required && provided.met == Met::Unset {
                failures.push(failure(
                    Check::RequiredMet,
                    name,
                    Some(&provided.variable),
                    "is required by its starmap file, and the plan leaves it unset",
                ));
            }
        }
    }
}

/// Check 3: every met variable names a checkout, unit and port of the
/// workspace.
fn known_providers(plan: &Plan, failures: &mut Vec<CheckFailure>) {
    for (name, unit) in &plan.units {
        for provided in &unit.provided {
            let Met::Port {
                checkout,
                unit: provider,
                port,
            } = &provided.met
            else {
                continue;
            };
            let reason = if checkout
                .as_ref()
                .is_some_and(|checkout| !plan.known.checkouts.contains(checkout))
            {
                Some("names a checkout that the workspace does not have")
            } else {
                match plan.known.ports.get(provider) {
                    None => Some("names a unit that the workspace does not have"),
                    Some(ports) if !ports.contains(port) => {
                        Some("names a port that its unit does not have")
                    }
                    Some(_) => None,
                }
            };
            if let Some(reason) = reason {
                failures.push(failure(
                    Check::KnownProviders,
                    name,
                    Some(&provided.variable),
                    reason,
                ));
            }
        }
    }
}

/// Check 4: a secret reaches only the units whose starmap file declares
/// it.
fn secret_reach(plan: &Plan, failures: &mut Vec<CheckFailure>) {
    for (name, unit) in &plan.units {
        for secret in &unit.secrets {
            if !unit.declared_secrets.contains_key(&secret.variable) {
                failures.push(failure(
                    Check::SecretReach,
                    name,
                    Some(&secret.variable),
                    "receives a secret that its starmap file does not declare",
                ));
            }
        }
    }
}

/// Check 5: every unit that a unit starts after is in the plan, and the
/// start order has no cycle.
fn start_order(plan: &Plan, failures: &mut Vec<CheckFailure>) {
    for (name, unit) in &plan.units {
        if unit
            .after
            .iter()
            .any(|after| !plan.units.contains_key(after))
        {
            failures.push(failure(
                Check::StartOrder,
                name,
                None,
                "starts after a unit that is not in the plan",
            ));
        }
    }
    let after: BTreeMap<&str, Vec<&str>> = plan
        .units
        .iter()
        .map(|(name, unit)| {
            (
                name.as_str(),
                unit.after
                    .iter()
                    .map(String::as_str)
                    .filter(|after| plan.units.contains_key(*after))
                    .collect(),
            )
        })
        .collect();
    for name in in_cycles(&after) {
        failures.push(failure(
            Check::StartOrder,
            name,
            None,
            "is in a cycle of the start order",
        ));
    }
}

/// Returns each unit of `after` that a cycle of the start order holds, in
/// name order. A unit is in a cycle when it can reach itself.
fn in_cycles<'a>(after: &BTreeMap<&'a str, Vec<&'a str>>) -> Vec<&'a str> {
    after
        .keys()
        .copied()
        .filter(|start| {
            let mut seen = BTreeSet::new();
            let mut waiting: Vec<&str> = after.get(start).cloned().unwrap_or_default();
            while let Some(next) = waiting.pop() {
                if next == *start {
                    return true;
                }
                if seen.insert(next) {
                    waiting.extend(after.get(next).into_iter().flatten().copied());
                }
            }
            false
        })
        .collect()
}
