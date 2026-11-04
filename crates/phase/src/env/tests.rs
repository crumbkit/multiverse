use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::process::Command;
use std::time::SystemTime;

use super::{AppliedEnvironment, EnvironmentError};

/// An environment that holds its values in a map, as a test double of
/// starmap's.
struct Fixed(BTreeMap<String, String>);

impl AppliedEnvironment for Fixed {
    fn names(&self) -> BTreeSet<String> {
        self.0.keys().cloned().collect()
    }

    fn expires_at(&self) -> Option<SystemTime> {
        None
    }

    fn apply_as(&self, command: &mut Command, reference: &dyn Fn(&str) -> Option<String>) {
        for (name, value) in &self.0 {
            if let Some(reference) = reference(name) {
                command.env(reference, value);
            }
        }
    }
}

fn fixed() -> Box<dyn AppliedEnvironment> {
    Box::new(Fixed(BTreeMap::from([
        ("PATH".to_owned(), "/opt/app/bin".to_owned()),
        ("STRIPE_KEY".to_owned(), "a value".to_owned()),
    ])))
}

fn envs(command: &Command) -> Vec<(&OsStr, Option<&OsStr>)> {
    command.get_envs().collect()
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test builds a Command that it never starts"
)]
fn an_applied_environment_sets_each_value_under_its_own_name() {
    let mut command = Command::new("true");

    fixed().apply(&mut command);

    assert_eq!(
        envs(&command),
        vec![
            (OsStr::new("PATH"), Some(OsStr::new("/opt/app/bin"))),
            (OsStr::new("STRIPE_KEY"), Some(OsStr::new("a value"))),
        ]
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test builds a Command that it never starts"
)]
fn an_applied_environment_sets_each_value_under_the_reference_it_is_given() {
    let mut command = Command::new("docker");

    fixed().apply_as(&mut command, &|name| {
        (name == "PATH").then(|| "MXV_ENV_api_1".to_owned())
    });

    assert_eq!(
        envs(&command),
        vec![(
            OsStr::new("MXV_ENV_api_1"),
            Some(OsStr::new("/opt/app/bin"))
        )]
    );
}

#[test]
fn an_applied_environment_lists_names_without_values() {
    let names = fixed().names();

    assert_eq!(
        names,
        BTreeSet::from(["PATH".to_owned(), "STRIPE_KEY".to_owned()])
    );
}

#[test]
fn an_environment_error_names_the_variable_and_the_resolver() {
    let error = EnvironmentError::new(
        "STRIPE_KEY".into(),
        "1password".into(),
        "the vault is locked".into(),
    );

    assert_eq!(
        error.to_string(),
        "1password could not supply the variable STRIPE_KEY"
    );
    assert_eq!(error.variable(), "STRIPE_KEY");
    assert_eq!(error.resolver(), "1password");
}
