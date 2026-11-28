use std::collections::{BTreeMap, BTreeSet};

use crate::needs::{
    Check, Known, Met, Plan, ProvidedVariable, SecretKind, SecretVariable, UnitPlan, check,
};

/// A value that a failure must never quote.
const VALUE: &str = "hunter2-value";

/// Returns a plan of the workspace with the checkouts `harness` and
/// `dispatcher`, whose units `harness-runner` and `dispatcher-api` listen
/// on `harness-http` and `dispatcher-http`, and which passes every check.
fn sound() -> Plan {
    let known = Known {
        checkouts: BTreeSet::from(["harness".to_owned(), "dispatcher".to_owned()]),
        ports: BTreeMap::from([
            (
                "harness-runner".to_owned(),
                BTreeSet::from(["harness-http".to_owned()]),
            ),
            (
                "dispatcher-api".to_owned(),
                BTreeSet::from(["dispatcher-http".to_owned()]),
            ),
        ]),
    };
    let runner = UnitPlan {
        provided: vec![
            ProvidedVariable {
                variable: "DISPATCHER_URL".to_owned(),
                required: true,
                met: Met::Port {
                    checkout: Some("dispatcher".to_owned()),
                    unit: "dispatcher-api".to_owned(),
                    port: "dispatcher-http".to_owned(),
                },
            },
            ProvidedVariable {
                variable: "BILLING_URL".to_owned(),
                required: false,
                met: Met::Unset,
            },
        ],
        secrets: vec![SecretVariable {
            variable: "API_TOKEN".to_owned(),
            store: "1password".to_owned(),
            reference: Some("op://acme/harness/token".to_owned()),
        }],
        declared_secrets: BTreeMap::from([("API_TOKEN".to_owned(), SecretKind::Secret)]),
        literals: BTreeSet::from(["RUST_LOG".to_owned()]),
        after: vec!["dispatcher-api".to_owned()],
    };
    let api = UnitPlan::default();
    Plan {
        units: BTreeMap::from([
            ("harness-runner".to_owned(), runner),
            ("dispatcher-api".to_owned(), api),
        ]),
        known,
    }
}

/// Returns the check, unit and variable of each failure of `plan`.
fn failed(plan: &Plan) -> Vec<(Check, String, Option<String>)> {
    match check(plan) {
        Ok(()) => Vec::new(),
        Err(failures) => failures
            .iter()
            .map(|failure| {
                assert!(
                    !failure.sentence().contains(VALUE),
                    "{}",
                    failure.sentence()
                );
                (
                    failure.check,
                    failure.unit.clone(),
                    failure.variable.clone(),
                )
            })
            .collect(),
    }
}

fn runner(plan: &mut Plan) -> &mut UnitPlan {
    plan.units.get_mut("harness-runner").unwrap()
}

#[test]
fn a_sound_plan_passes_every_check() {
    assert_eq!(failed(&sound()), []);
}

#[test]
fn a_manifest_literal_for_a_variable_that_the_starmap_file_declares_secret_fails_the_secret_value_check()
 {
    let mut plan = sound();
    runner(&mut plan).literals.insert("API_TOKEN".to_owned());
    assert_eq!(
        failed(&plan),
        [(
            Check::SecretValue,
            "harness-runner".to_owned(),
            Some("API_TOKEN".to_owned())
        )]
    );
}

#[test]
fn a_manifest_literal_for_a_generated_variable_passes_the_secret_value_check() {
    let mut plan = sound();
    let unit = runner(&mut plan);
    unit.declared_secrets
        .insert("SESSION_KEY".to_owned(), SecretKind::Generated);
    unit.literals.insert("SESSION_KEY".to_owned());
    assert_eq!(failed(&plan), []);
}

#[test]
fn a_required_variable_left_unset_fails_the_required_check() {
    let mut plan = sound();
    runner(&mut plan).provided[0].met = Met::Unset;
    assert_eq!(
        failed(&plan),
        [(
            Check::RequiredMet,
            "harness-runner".to_owned(),
            Some("DISPATCHER_URL".to_owned())
        )]
    );
}

#[test]
fn a_variable_met_by_a_checkout_unit_or_port_the_workspace_lacks_fails_the_known_check() {
    for (checkout, unit, port) in [
        (Some("billing"), "dispatcher-api", "dispatcher-http"),
        (Some("dispatcher"), "dispatcher-worker", "dispatcher-http"),
        (Some("dispatcher"), "dispatcher-api", "dispatcher-grpc"),
    ] {
        let mut plan = sound();
        runner(&mut plan).provided[0].met = Met::Port {
            checkout: checkout.map(str::to_owned),
            unit: unit.to_owned(),
            port: port.to_owned(),
        };
        assert_eq!(
            failed(&plan),
            [(
                Check::KnownProviders,
                "harness-runner".to_owned(),
                Some("DISPATCHER_URL".to_owned())
            )],
            "{unit} {port}"
        );
    }
}

#[test]
fn a_secret_that_reaches_a_unit_whose_starmap_file_does_not_declare_it_fails_the_reach_check() {
    let mut plan = sound();
    plan.units
        .get_mut("dispatcher-api")
        .unwrap()
        .secrets
        .push(SecretVariable {
            variable: "API_TOKEN".to_owned(),
            store: "1password".to_owned(),
            reference: Some("op://acme/harness/token".to_owned()),
        });
    assert_eq!(
        failed(&plan),
        [(
            Check::SecretReach,
            "dispatcher-api".to_owned(),
            Some("API_TOKEN".to_owned())
        )]
    );
}

#[test]
fn a_unit_that_starts_after_a_unit_not_in_the_plan_fails_the_start_order_check() {
    let mut plan = sound();
    runner(&mut plan).after.push("auth-server".to_owned());
    assert_eq!(
        failed(&plan),
        [(Check::StartOrder, "harness-runner".to_owned(), None)]
    );
}

#[test]
fn a_cycle_of_the_start_order_fails_the_start_order_check_for_each_unit_in_it() {
    let mut plan = sound();
    plan.units
        .get_mut("dispatcher-api")
        .unwrap()
        .after
        .push("harness-runner".to_owned());
    assert_eq!(
        failed(&plan),
        [
            (Check::StartOrder, "dispatcher-api".to_owned(), None),
            (Check::StartOrder, "harness-runner".to_owned(), None)
        ]
    );
}

#[test]
fn every_failure_of_a_plan_is_listed_in_the_order_of_the_checks() {
    let mut plan = sound();
    let unit = runner(&mut plan);
    unit.literals.insert("API_TOKEN".to_owned());
    unit.provided[0].met = Met::Unset;
    let checks: Vec<Check> = failed(&plan)
        .into_iter()
        .map(|(check, _, _)| check)
        .collect();
    assert_eq!(checks, [Check::SecretValue, Check::RequiredMet]);
}

#[test]
fn a_failure_names_its_check_unit_and_variable_in_its_sentence() {
    let mut plan = sound();
    runner(&mut plan).literals.insert("API_TOKEN".to_owned());
    let failures = check(&plan).unwrap_err();
    let sentence = failures.first().sentence();
    assert!(sentence.contains("no-secret-value"), "{sentence}");
    assert!(sentence.contains("harness-runner"), "{sentence}");
    assert!(sentence.contains("API_TOKEN"), "{sentence}");
}

#[test]
fn the_checks_have_five_names_in_their_order() {
    let names: Vec<&str> = Check::ALL.iter().map(|check| check.name()).collect();
    assert_eq!(
        names,
        [
            "no-secret-value",
            "required-met",
            "known-providers",
            "secret-reach",
            "start-order"
        ]
    );
}
