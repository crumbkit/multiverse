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

    fn digest(&self, key: &[u8; 32]) -> [u8; 32] {
        // The test double mixes the key and the pairs' lengths. A real
        // implementation uses a keyed cryptographic hash.
        let mut digest = *key;
        for (index, (name, value)) in self.0.iter().enumerate() {
            if let Some(byte) = digest.get_mut(index % 32) {
                *byte ^= u8::try_from(name.len() + value.len()).unwrap_or(u8::MAX);
            }
        }
        digest
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

/// A source that gives each process of an app its own `PORT`, as mxv does
/// when two processes listen on different named ports.
struct PerProcess;

impl super::EnvironmentSource for PerProcess {
    fn resolve(
        &self,
        _app: &crate::name::AppName,
        process: &crate::name::ProcessName,
        _deadline: std::time::Instant,
    ) -> Result<Box<dyn AppliedEnvironment>, EnvironmentError> {
        let port = if process.as_str() == "server" {
            "41000"
        } else {
            "41001"
        };
        Ok(Box::new(Fixed(BTreeMap::from([(
            "PORT".to_owned(),
            port.to_owned(),
        )]))))
    }
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test builds Commands that it never starts"
)]
fn two_processes_of_one_app_receive_different_values_for_one_name() {
    use super::EnvironmentSource;
    let deadline = std::time::Instant::now();
    let app = "api".parse().unwrap();
    let mut server = Command::new("server");
    let mut admin = Command::new("admin");

    PerProcess
        .resolve(&app, &"server".parse().unwrap(), deadline)
        .unwrap()
        .apply(&mut server);
    PerProcess
        .resolve(&app, &"admin".parse().unwrap(), deadline)
        .unwrap()
        .apply(&mut admin);

    assert_eq!(
        envs(&server),
        vec![(OsStr::new("PORT"), Some(OsStr::new("41000")))]
    );
    assert_eq!(
        envs(&admin),
        vec![(OsStr::new("PORT"), Some(OsStr::new("41001")))]
    );
}
