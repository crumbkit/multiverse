#![expect(clippy::disallowed_methods, reason = "the tests read the real clock")]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use phase::{AppName, EnvironmentSource as _, ProcessName, SecretRead, WorkspaceId};
use starmap::Secret;
use starmap_resolve::{Lookup, Resolver, SecretError};
use tempfile::TempDir;

use super::{
    ProcessNote, Resolved, SecretProblem, SecretSource, SecretValues, UnitSecrets, WithSecrets,
    keychain_service, of_unit,
};
use crate::manifest::Manifest;
use crate::name::WorkspaceName;
use crate::phase_plan::{Built, CallerEnvironment, Inputs, MAIN, ProxyUrls, build, permissions};
use crate::testing::write;

/// The manifest of one checkout: `dispatcher` reads its `starmap.toml`,
/// runs in the sandbox and depends on nothing.
const MANIFEST: &str = r#"
[processes.dispatcher]
argv = ["dispatcher"]
permissions = { write = ["$STATE"] }

[processes.web]
argv = ["web"]
env = { NOTE = "set by the manifest" }

[secrets.client_secret]
check = ["op", "read", "op://acme-staging/auth-client-dispatcher/credential"]
sign_in = "op signin --account acme.1password.com"
"#;

/// The reference of the dispatcher's client secret.
const REFERENCE: &str = "op://acme-staging/auth-client-dispatcher/credential";

/// Returns a variable of a starmap file whose source is `source`, required
/// unless `unset` says otherwise.
fn variable(name: &str, source: &str, unset: &str) -> String {
    format!(
        "[variables.{name}]\npurpose = \"A value the app keeps secret.\"\ntype = \"string\"\nunset = {unset}\nshow = \"withheld\"\n{source}\n\n"
    )
}

/// The dispatcher's starmap file: a client secret from 1Password, a
/// signing key that the host generates, and a partner token from 1Password
/// that may stay unset on a developer's machine.
fn dispatcher_spec() -> String {
    let mut spec = variable(
        "CLIENT_SECRET",
        &format!("source = \"secret\"\nbind = {{ one-password = \"{REFERENCE}\" }}"),
        "\"required\"",
    );
    spec.push_str(&variable(
        "SIGNING_KEY",
        "source = \"generated\"",
        "\"required\"",
    ));
    spec.push_str(&variable(
        "PARTNER_TOKEN",
        "source = \"secret\"\nbind = { one-password = \"op://acme-staging/partner/token\" }",
        "{ local = \"optional\", production = \"required\" }",
    ));
    spec
}

/// A checkout whose `starmap.toml` holds `spec`.
fn checkout_with(spec: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path().join("starmap.toml"), spec);
    dir
}

fn manifest() -> Manifest {
    Manifest::parse(MANIFEST, Path::new("/repo/mxv.toml")).unwrap()
}

fn id() -> WorkspaceId {
    WorkspaceId::try_from("0a1b2c3d4e5f60718293a4b5c6d7e8f9".to_owned()).unwrap()
}

/// Returns the inputs of the plan of `manifest` in `checkout`.
fn inputs<'a>(
    manifest: &'a Manifest,
    checkout: &'a Path,
    workspace: &'a WorkspaceName,
    id: &'a WorkspaceId,
    ports: &'a BTreeMap<String, u16>,
    caller: &'a CallerEnvironment,
) -> Inputs<'a> {
    Inputs {
        manifest,
        workspace,
        id,
        checkout,
        roots: vec![checkout.to_path_buf()],
        ports,
        workspace_dir: None,
        grace: Duration::from_secs(5),
        caller,
        own_command: None,
        urls: ProxyUrls::default(),
        read_starmaps: true,
    }
}

/// Runs `check` with the inputs of [`MANIFEST`] in a checkout whose
/// `starmap.toml` holds `spec`.
fn with_inputs<T>(spec: &str, check: impl FnOnce(&Inputs<'_>) -> T) -> T {
    let dir = checkout_with(spec);
    let manifest = manifest();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = id();
    let ports = BTreeMap::new();
    let caller = CallerEnvironment::default();
    check(&inputs(
        &manifest,
        dir.path(),
        &workspace,
        &id,
        &ports,
        &caller,
    ))
}

/// Returns the secrets of the dispatcher in a checkout whose
/// `starmap.toml` holds `spec`.
fn dispatcher(spec: &str) -> (UnitSecrets, TempDir) {
    let dir = checkout_with(spec);
    let manifest = manifest();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = id();
    let ports = BTreeMap::new();
    let caller = CallerEnvironment::default();
    let inputs = inputs(&manifest, dir.path(), &workspace, &id, &ports, &caller);
    let process = manifest.processes.get("dispatcher").unwrap();
    (
        of_unit(&inputs, "dispatcher", process).unwrap().unwrap(),
        dir,
    )
}

/// A store that records each lookup it is asked for, without a value, and
/// answers a reference from `values`, a generated value with a value
/// named after its workspace, and a saved value with nothing.
#[derive(Default)]
struct Fake {
    /// The value of each reference it holds.
    values: BTreeMap<String, String>,
    /// The references, and the workspaces of the generated values, that
    /// it was asked for, in order.
    asked: Mutex<Vec<String>>,
    /// The failure it gives every reference, if any.
    failing: bool,
}

impl Fake {
    fn holding(values: &[(&str, &str)]) -> Self {
        Self {
            values: values
                .iter()
                .map(|(reference, value)| ((*reference).to_owned(), (*value).to_owned()))
                .collect(),
            ..Self::default()
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Resolver for Fake {
    fn prepare(&self, lookups: &[Lookup<'_>], _deadline: Instant) {
        for lookup in lookups {
            if let Lookup::Reference { reference, .. } = lookup {
                self.asked
                    .lock()
                    .unwrap()
                    .push(format!("prepare {}", super::reference_text(reference)));
            }
        }
    }

    fn resolve(&self, lookup: Lookup<'_>, _deadline: Instant) -> Result<Secret, SecretError> {
        match lookup {
            Lookup::Reference { reference, .. } => {
                let text = super::reference_text(reference);
                self.asked.lock().unwrap().push(format!("read {text}"));
                if self.failing {
                    return Err(SecretError::Locked);
                }
                self.values
                    .get(&text)
                    .map(|value| Secret::new(value.clone()))
                    .ok_or(SecretError::NotFound)
            }
            Lookup::Generated { slot, .. } => {
                let workspace = slot.workspace.as_str().to_owned();
                self.asked
                    .lock()
                    .unwrap()
                    .push(format!("generate {workspace}/{}", slot.app.as_str()));
                Ok(Secret::new(format!("generated for {workspace}")))
            }
            Lookup::Saved { .. } | _ => Err(SecretError::NotFound),
        }
    }
}

fn granted() -> Vec<SecretRead> {
    vec![
        SecretRead::new("CLIENT_SECRET", "1password", REFERENCE),
        SecretRead::new(
            "PARTNER_TOKEN",
            "1password",
            "op://acme-staging/partner/token",
        ),
    ]
}

fn resolve_with(unit: &UnitSecrets, fake: &Fake, granted: &[SecretRead]) -> Resolved {
    unit.resolve(
        &id(),
        &[fake],
        granted,
        Instant::now() + Duration::from_secs(5),
        &manifest(),
    )
}

/// Returns the variables that `environment` sets on a command for `app`.
fn applied(environment: &WithSecrets<'_>, app: &str) -> BTreeMap<String, String> {
    let applied = environment
        .resolve(
            &AppName::try_from(app.to_owned()).unwrap(),
            &ProcessName::try_from(MAIN.to_owned()).unwrap(),
            Instant::now(),
        )
        .unwrap();
    #[expect(
        clippy::disallowed_methods,
        reason = "the test builds a command only to read the variables that the environment applies to it, and never runs it"
    )]
    let mut command = Command::new("true");
    command.env_clear();
    applied.apply(&mut command);
    command
        .get_envs()
        .filter_map(|(name, value)| Some((name.to_str()?.to_owned(), value?.to_str()?.to_owned())))
        .collect()
}

fn plan(spec: &str) -> Built {
    with_inputs(spec, |inputs| build(inputs).unwrap())
}

#[test]
fn the_plan_names_each_secret_and_generated_variable_as_optional_and_gives_it_no_value() {
    let built = plan(&dispatcher_spec());

    let app = built
        .plan
        .apps()
        .iter()
        .find(|app| app.name.as_str() == "dispatcher")
        .unwrap();
    let main = app.processes.first().unwrap();
    for name in ["CLIENT_SECRET", "SIGNING_KEY", "PARTNER_TOKEN"] {
        assert!(main.env.contains(name), "{name} is named");
        assert!(main.optional_env.contains(name), "{name} is optional");
    }
    let manifest_values = applied(
        &WithSecrets {
            manifest: &built.environment,
            secrets: &SecretValues::default(),
        },
        "dispatcher",
    );
    assert!(!manifest_values.contains_key("CLIENT_SECRET"));
    assert!(!manifest_values.contains_key("SIGNING_KEY"));
}

#[test]
fn a_sandboxed_process_names_each_secret_variable_in_its_permissions() {
    let built = plan(&dispatcher_spec());

    let app = built
        .plan
        .apps()
        .iter()
        .find(|app| app.name.as_str() == "dispatcher")
        .unwrap();
    let secrets = &app.permissions.as_ref().unwrap().secrets;
    assert!(secrets.contains("CLIENT_SECRET"));
    assert!(secrets.contains("SIGNING_KEY"));
    let requested = with_inputs(&dispatcher_spec(), |inputs| {
        permissions(inputs, &["dispatcher".to_owned()]).unwrap()
    });
    let grant = requested
        .get(&AppName::try_from("dispatcher".to_owned()).unwrap())
        .unwrap();
    let phase::Grant::Sandboxed(granted) = grant else {
        panic!("the dispatcher runs in the sandbox");
    };
    assert!(granted.secrets.contains("CLIENT_SECRET"));
}

#[test]
fn a_variable_that_the_manifest_sets_is_no_secret_of_the_unit() {
    let spec = variable(
        "NOTE",
        "source = \"secret\"\nbind = { one-password = \"op://acme/note/credential\" }",
        "\"required\"",
    );
    let dir = checkout_with(&spec);
    let manifest = manifest();
    let workspace = WorkspaceName::try_from("triage".to_owned()).unwrap();
    let id = id();
    let ports = BTreeMap::new();
    let caller = CallerEnvironment::default();
    let inputs = inputs(&manifest, dir.path(), &workspace, &id, &ports, &caller);

    let secrets = of_unit(&inputs, "web", manifest.processes.get("web").unwrap()).unwrap();

    assert!(secrets.is_none());
}

#[test]
fn a_granted_reference_is_read_and_reaches_the_process_through_the_source() {
    let (unit, _dir) = dispatcher(&dispatcher_spec());
    let fake = Fake::holding(&[
        (REFERENCE, "fake-client-secret"),
        ("op://acme-staging/partner/token", "fake-partner-token"),
    ]);

    let Resolved::Ready { values, notes } = resolve_with(&unit, &fake, &granted()) else {
        panic!("every secret resolves");
    };

    assert!(notes.is_empty());
    let built = plan(&dispatcher_spec());
    let mut secrets = SecretValues::default();
    secrets.insert(AppName::try_from("dispatcher".to_owned()).unwrap(), values);
    let environment = WithSecrets {
        manifest: &built.environment,
        secrets: &secrets,
    };
    let applied = applied(&environment, "dispatcher");
    assert_eq!(
        applied.get("CLIENT_SECRET").map(String::as_str),
        Some("fake-client-secret")
    );
    assert_eq!(
        applied.get("PARTNER_TOKEN").map(String::as_str),
        Some("fake-partner-token")
    );
}

#[test]
fn an_ungranted_reference_never_reaches_a_store_and_fails_the_unit_as_not_granted() {
    let (unit, _dir) = dispatcher(&dispatcher_spec());
    let fake = Fake::holding(&[(REFERENCE, "fake-client-secret")]);

    let Resolved::Failed(failure) = resolve_with(&unit, &fake, &[]) else {
        panic!("the client secret has no grant");
    };

    assert_eq!(failure.unit, "dispatcher");
    assert_eq!(failure.variable, "CLIENT_SECRET");
    assert_eq!(failure.problem, SecretProblem::NotGranted);
    assert_eq!(
        failure.source,
        SecretSource::Reference {
            store: "1password".to_owned(),
            reference: REFERENCE.to_owned(),
        }
    );
    assert!(
        fake.asked().iter().all(|asked| !asked.contains("op://")),
        "no reference reached the store: {:?}",
        fake.asked()
    );
}

#[test]
fn an_optional_secret_without_a_grant_stays_unset_with_a_warning_that_names_it() {
    let (unit, _dir) = dispatcher(&dispatcher_spec());
    let fake = Fake::holding(&[(REFERENCE, "fake-client-secret")]);
    let client_only = vec![SecretRead::new("CLIENT_SECRET", "1password", REFERENCE)];

    let Resolved::Ready { values, notes } = resolve_with(&unit, &fake, &client_only) else {
        panic!("the partner token may stay unset");
    };

    assert!(values.iter().all(|(name, _)| name != "PARTNER_TOKEN"));
    let [ProcessNote::SecretDefaulted { variable, message }] = notes.as_slice() else {
        panic!("one warning: {notes:?}");
    };
    assert_eq!(variable, "PARTNER_TOKEN");
    assert_eq!(
        message,
        "could not read the secret PARTNER_TOKEN from op://acme-staging/partner/token, because no person granted it, so mxv left it unset"
    );
    assert!(
        fake.asked()
            .iter()
            .all(|asked| !asked.contains("op://acme-staging/partner/token")),
        "{:?}",
        fake.asked()
    );
}

#[test]
fn an_unreadable_secret_fails_as_unavailable_with_the_sign_in_of_the_matching_table() {
    let (unit, _dir) = dispatcher(&dispatcher_spec());
    let fake = Fake {
        failing: true,
        ..Fake::default()
    };

    let Resolved::Failed(failure) = resolve_with(&unit, &fake, &granted()) else {
        panic!("the store is locked");
    };

    assert_eq!(failure.variable, "CLIENT_SECRET");
    assert_eq!(
        failure.problem,
        SecretProblem::Unavailable {
            reason: "the secret store is locked".to_owned()
        }
    );
    assert_eq!(
        failure.sign_in.as_deref(),
        Some("op signin --account acme.1password.com")
    );
}

#[test]
fn a_generated_value_is_kept_under_the_workspace_stable_id() {
    let (unit, _dir) = dispatcher(&dispatcher_spec());
    let fake = Fake::holding(&[
        (REFERENCE, "fake-client-secret"),
        ("op://acme-staging/partner/token", "fake-partner-token"),
    ]);

    let Resolved::Ready { values, .. } = resolve_with(&unit, &fake, &granted()) else {
        panic!("every secret resolves");
    };

    let workspace = format!("mxv-{}", id().as_str());
    assert!(
        fake.asked()
            .contains(&format!("generate {workspace}/dispatcher")),
        "{:?}",
        fake.asked()
    );
    let key = values
        .iter()
        .find(|(name, _)| name == "SIGNING_KEY")
        .map(|(_, secret)| secret.expose().to_owned());
    assert_eq!(key, Some(format!("generated for {workspace}")));
    assert_eq!(keychain_service(&id()), format!("starmap.{workspace}"));
}

#[test]
fn two_workspaces_of_one_repository_keep_separate_generated_values() {
    let other = WorkspaceId::try_from("ffeeddccbbaa99887766554433221100".to_owned()).unwrap();

    assert_ne!(keychain_service(&id()), keychain_service(&other));
}

#[test]
fn the_digest_of_a_process_changes_with_a_secret_value() {
    let built = plan(&dispatcher_spec());
    let app = AppName::try_from("dispatcher".to_owned()).unwrap();
    let process = ProcessName::try_from(MAIN.to_owned()).unwrap();
    let digest = |value: &str| {
        let mut secrets = SecretValues::default();
        secrets.insert(
            app.clone(),
            vec![("CLIENT_SECRET".to_owned(), Secret::new(value.to_owned()))],
        );
        WithSecrets {
            manifest: &built.environment,
            secrets: &secrets,
        }
        .resolve(&app, &process, Instant::now())
        .unwrap()
        .digest(&[7; 32])
    };

    assert_ne!(digest("fake-one"), digest("fake-two"));
    assert_eq!(digest("fake-one"), digest("fake-one"));
}
