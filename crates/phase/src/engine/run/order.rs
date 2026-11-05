//! Which apps and processes an operation acts on, and in what order.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::PhaseError;
use crate::name::{AppName, ProcessName};
use crate::plan::{AppPlan, Plan, ProcessPlan};
use crate::record::run::RunRecord;
use crate::select::Selection;

/// The processes of one app that an operation acts on, in dependency
/// order.
#[derive(Debug)]
pub(super) struct Chosen<'p> {
    /// The app.
    pub(super) app: &'p AppPlan,
    /// Its processes that the operation acts on.
    pub(super) processes: Vec<&'p ProcessPlan>,
}

/// Returns the apps and processes of `plan` that `select` names, in
/// dependency order. With `depends`, it adds every process of each app
/// that a chosen app depends on, and each process of a chosen app that a
/// chosen process depends on.
///
/// # Errors
///
/// Returns [`PhaseError::UnknownTarget`] when `select` names an app or a
/// process that `plan` lacks.
pub(super) fn choose<'p>(
    plan: &'p Plan,
    select: &Selection,
    depends: bool,
) -> Result<Vec<Chosen<'p>>, PhaseError> {
    // `None` stands for every process of the app.
    let mut wanted: BTreeMap<&AppName, Option<BTreeSet<&ProcessName>>> = BTreeMap::new();
    match select {
        Selection::All => {
            for app in plan.apps() {
                wanted.insert(&app.name, None);
            }
        }
        Selection::Only(targets) => {
            for target in targets {
                let unknown = || PhaseError::UnknownTarget {
                    target: target.to_string(),
                };
                let app = plan.app(&target.app).ok_or_else(unknown)?;
                let slot = wanted
                    .entry(&app.name)
                    .or_insert_with(|| Some(BTreeSet::new()));
                match &target.process {
                    None => *slot = None,
                    Some(name) => {
                        let process = app
                            .processes
                            .iter()
                            .find(|process| &process.name == name)
                            .ok_or_else(unknown)?;
                        if let Some(set) = slot {
                            set.insert(&process.name);
                        }
                    }
                }
            }
        }
    }
    if depends {
        add_depends(plan, &mut wanted);
    }
    Ok(sorted(plan.apps(), |app| &app.name, |app| &app.depends)
        .into_iter()
        .filter_map(|app| {
            let only = wanted.get(&app.name)?;
            let processes = sorted(
                &app.processes,
                |process| &process.name,
                |process| &process.depends,
            )
            .into_iter()
            .filter(|process| {
                only.as_ref()
                    .is_none_or(|only| only.contains(&process.name))
            })
            .collect();
            Some(Chosen { app, processes })
        })
        .collect())
}

/// Adds to `wanted` every process of each app that a wanted app depends
/// on, and each process that a wanted process depends on.
fn add_depends<'p>(
    plan: &'p Plan,
    wanted: &mut BTreeMap<&'p AppName, Option<BTreeSet<&'p ProcessName>>>,
) {
    let mut queue: Vec<&AppName> = wanted.keys().copied().collect();
    while let Some(name) = queue.pop() {
        let Some(app) = plan.app(name) else {
            continue;
        };
        for depend in &app.depends {
            if wanted.insert(depend, None) != Some(None) {
                queue.push(depend);
            }
        }
    }
    for (name, only) in wanted.iter_mut() {
        let (Some(app), Some(set)) = (plan.app(name), only.as_mut()) else {
            continue;
        };
        let mut queue: Vec<&ProcessName> = set.iter().copied().collect();
        while let Some(name) = queue.pop() {
            let Some(process) = app.processes.iter().find(|process| &process.name == name) else {
                continue;
            };
            for depend in &process.depends {
                if set.insert(depend) {
                    queue.push(depend);
                }
            }
        }
    }
}

/// Returns `items` in dependency order: each item after the items that it
/// depends on, and otherwise in the order of `items`. A dependency that
/// `items` lacks is ignored, and a cycle is broken where it closes.
pub(super) fn sorted<T, N: PartialEq>(
    items: &[T],
    name: impl Fn(&T) -> &N,
    depends: impl Fn(&T) -> &[N],
) -> Vec<&T> {
    let mut done = vec![false; items.len()];
    let mut order = Vec::new();
    for at in 0..items.len() {
        visit(at, items, &name, &depends, &mut done, &mut order);
    }
    order
}

/// Adds the item at `at` to `order` after the items it depends on.
fn visit<'a, T, N: PartialEq>(
    at: usize,
    items: &'a [T],
    name: &impl Fn(&T) -> &N,
    depends: &impl Fn(&T) -> &[N],
    done: &mut [bool],
    order: &mut Vec<&'a T>,
) {
    let (Some(item), Some(seen)) = (items.get(at), done.get_mut(at)) else {
        return;
    };
    if *seen {
        return;
    }
    *seen = true;
    for depend in depends(item) {
        if let Some(found) = items.iter().position(|other| name(other) == depend) {
            visit(found, items, name, depends, done, order);
        }
    }
    order.push(item);
}

/// Returns the apps that `record` names, in the reverse of their
/// dependency order, so each app comes before the apps it depends on. An
/// app with processes or containers that the record's list of apps lacks,
/// as in a record of an earlier phase, comes first.
pub(super) fn stopping_order(record: &RunRecord) -> Vec<AppName> {
    let mut order: Vec<AppName> = sorted(&record.apps, |app| &app.name, |app| &app.depends)
        .into_iter()
        .map(|app| app.name.clone())
        .collect();
    let named = record
        .processes
        .iter()
        .map(|entry| &entry.app)
        .chain(record.containers.iter().map(|entry| &entry.app));
    for app in named {
        if !order.contains(app) {
            order.push(app.clone());
        }
    }
    order.reverse();
    order
}
