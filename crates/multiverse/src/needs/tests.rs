use std::path::PathBuf;

use crate::needs::{Need, Parsed, Provide, Provider, RefusalCode, parse};

/// The units that the files of these tests declare.
fn units(unit: &str) -> bool {
    matches!(unit, "runner" | "api" | "db" | "worker")
}

fn parsed(text: &str) -> Parsed {
    parse(text, &units)
}

/// Returns the code and line of each refusal of `text`.
fn refused(text: &str) -> Vec<(RefusalCode, Option<usize>)> {
    parsed(text)
        .refusals
        .iter()
        .map(|refusal| (refusal.code, refusal.line))
        .collect()
}

/// Returns the name, origin and `from` of each need of `text`.
fn needs_of(text: &str) -> Vec<(String, String, String)> {
    parsed(text)
        .declared
        .needs
        .iter()
        .map(|need: &Need| {
            let provider = match &need.provider {
                Provider::Repo(origin) => origin.as_str().to_owned(),
                Provider::Contract(contract) => format!("contract {contract}"),
            };
            (need.name.clone(), provider, need.from.clone())
        })
        .collect()
}

/// The harness's `mxv.toml` of case A, with its needs naming only their
/// providers.
const HARNESS: &str = r#"version = 1
ports = ["http"]
routes = ["http"]

[processes.runner]
argv = ["./target/debug/harness", "serve"]
build = ["cargo", "build", "--bin", "harness"]
ports = { HARNESS_PORT = "http" }
listen = "http"

[needs.dispatcher-http]
repo = "github.com/acme/dispatcher"
from = "http"

[needs.agent-http]
repo = "github.com/acme/agent"
from = "http"
"#;

/// The dispatcher's `mxv.toml` of case A, with its needs naming only their
/// providers.
const DISPATCHER: &str = r#"version = 1
ports = ["http", "db"]

[containers.db]
image = "postgres:17.2"
ports = { 5432 = "db" }

[processes.api]
argv = ["./target/debug/dispatcher"]
ports = { DISPATCHER_PORT = "http" }
listen = "http"
depends_on = ["db"]

[needs.agent-http]
repo = "github.com/acme/agent"
from = "http"

[needs.auth-api]
repo = "github.com/acme/auth"
from = "api"

[needs.harness-events]
repo = "github.com/acme/harness"
from = "http"
"#;

#[test]
fn the_harness_file_of_case_a_parses_into_its_two_needs_with_their_origins_and_from() {
    assert_eq!(refused(HARNESS), []);
    let mut needs = needs_of(HARNESS);
    needs.sort();
    assert_eq!(
        needs,
        [
            (
                "agent-http".to_owned(),
                "github.com/acme/agent".to_owned(),
                "http".to_owned()
            ),
            (
                "dispatcher-http".to_owned(),
                "github.com/acme/dispatcher".to_owned(),
                "http".to_owned()
            ),
        ]
    );
}

#[test]
fn the_dispatcher_file_of_case_a_parses_into_its_three_needs_with_their_origins_and_from() {
    assert_eq!(refused(DISPATCHER), []);
    let mut needs = needs_of(DISPATCHER);
    needs.sort();
    assert_eq!(
        needs,
        [
            (
                "agent-http".to_owned(),
                "github.com/acme/agent".to_owned(),
                "http".to_owned()
            ),
            (
                "auth-api".to_owned(),
                "github.com/acme/auth".to_owned(),
                "api".to_owned()
            ),
            (
                "harness-events".to_owned(),
                "github.com/acme/harness".to_owned(),
                "http".to_owned()
            ),
        ]
    );
}

#[test]
fn a_need_records_the_line_of_its_table() {
    let parsed = parsed(HARNESS);
    let need = parsed
        .declared
        .needs
        .iter()
        .find(|need| need.name == "dispatcher-http")
        .unwrap();
    assert_eq!(need.line, 11);
}

#[test]
fn a_need_names_a_contract_in_place_of_a_repository() {
    let text = "[needs.auth-api]\ncontract = \"acme.auth.v1\"\nfrom = \"api\"\n";
    assert_eq!(refused(text), []);
    assert_eq!(
        needs_of(text),
        [(
            "auth-api".to_owned(),
            "contract acme.auth.v1".to_owned(),
            "api".to_owned()
        )]
    );
}

#[test]
fn a_need_with_as_is_refused_as_moved_with_its_line() {
    let text = "ports = []\n\n[needs.agent-http]\nrepo = \"github.com/acme/agent\"\nfrom = \"http\"\nas = \"AGENT_URL\"\n";
    assert_eq!(refused(text), [(RefusalCode::Moved, Some(3))]);
    assert!(needs_of(text).is_empty());
}

#[test]
fn a_need_with_optional_is_refused_as_moved_with_its_line() {
    let text = "[needs.billing-api]\nrepo = \"github.com/acme/billing\"\nfrom = \"api\"\noptional = true\n";
    assert_eq!(refused(text), [(RefusalCode::Moved, Some(1))]);
}

#[test]
fn the_moved_refusal_says_that_the_starmap_entry_names_the_variable_and_its_unset() {
    let text = "[needs.agent-http]\nrepo = \"github.com/acme/agent\"\nfrom = \"http\"\nas = \"AGENT_URL\"\noptional = true\n";
    let parsed = parsed(text);
    let [refusal] = parsed.refusals.as_slice() else {
        panic!("{:?}", parsed.refusals);
    };
    assert_eq!(refusal.code.as_str(), "mxv::needs::moved");
    assert!(refusal.message.contains("starmap"), "{}", refusal.message);
    assert!(refusal.message.contains("unset"), "{}", refusal.message);
    assert!(
        !refusal.message.contains("AGENT_URL"),
        "{}",
        refusal.message
    );
}

#[test]
fn a_need_with_both_a_repository_and_a_contract_is_refused_with_its_line() {
    let text = "ports = []\n[needs.auth-api]\nrepo = \"github.com/acme/auth\"\ncontract = \"acme.auth.v1\"\nfrom = \"api\"\n";
    assert_eq!(refused(text), [(RefusalCode::Invalid, Some(2))]);
}

#[test]
fn a_need_with_neither_a_repository_nor_a_contract_is_refused_with_its_line() {
    let text = "\n\n[needs.auth-api]\nfrom = \"api\"\n";
    assert_eq!(refused(text), [(RefusalCode::ServiceKeys, Some(3))]);
}

#[test]
fn a_need_for_a_shared_service_warns_that_a_process_lists_the_service_in_its_needs_key() {
    let parsed = parsed("[needs.db]\nkind = \"postgres\"\n");
    let [refusal] = parsed.refusals.as_slice() else {
        panic!("{:?}", parsed.refusals);
    };
    assert_eq!(refusal.code.as_str(), "mxv::needs::service_keys");
    assert_eq!(refusal.line, Some(1));
    assert!(
        refusal.message.contains("names a provider"),
        "{}",
        refusal.message
    );
    assert!(
        refusal.message.contains("`needs` key"),
        "{}",
        refusal.message
    );
}

#[test]
fn a_need_whose_repository_is_not_an_origin_is_refused_without_quoting_it() {
    let parsed = parsed("[needs.auth-api]\nrepo = \"not an origin\"\nfrom = \"api\"\n");
    let [refusal] = parsed.refusals.as_slice() else {
        panic!("{:?}", parsed.refusals);
    };
    assert_eq!(refusal.code, RefusalCode::Invalid);
    assert!(
        !refusal.message.contains("not an origin"),
        "{}",
        refusal.message
    );
}

#[test]
fn a_need_without_from_or_with_an_unknown_key_is_refused() {
    assert_eq!(
        refused("[needs.auth-api]\nrepo = \"github.com/acme/auth\"\n"),
        [(RefusalCode::Invalid, Some(1))]
    );
    assert_eq!(
        refused("[needs.auth-api]\nrepo = \"github.com/acme/auth\"\nfrom = \"api\"\nport = 1\n"),
        [(RefusalCode::Invalid, Some(1))]
    );
}

#[test]
fn a_refused_need_leaves_the_other_needs_of_the_file() {
    let text = "[needs.a]\nrepo = \"github.com/acme/auth\"\nfrom = \"api\"\nas = \"X\"\n\n[needs.b]\nrepo = \"github.com/acme/agent\"\nfrom = \"http\"\n";
    assert_eq!(refused(text), [(RefusalCode::Moved, Some(1))]);
    assert_eq!(
        needs_of(text),
        [(
            "b".to_owned(),
            "github.com/acme/agent".to_owned(),
            "http".to_owned()
        )]
    );
}

#[test]
fn the_old_spellings_wants_and_offers_are_refused_as_renamed() {
    let text = "ports = []\n[wants.db]\nfrom = \"x\"\n\n[offers.socket]\nunit = \"api\"\n";
    assert_eq!(
        refused(text),
        [
            (RefusalCode::Renamed, Some(2)),
            (RefusalCode::Renamed, Some(5))
        ]
    );
    let messages: Vec<String> = parsed(text)
        .refusals
        .into_iter()
        .map(|refusal| refusal.message)
        .collect();
    assert!(messages[0].contains("[needs.<name>]"), "{messages:?}");
    assert!(messages[1].contains("[provides.<name>]"), "{messages:?}");
}

#[test]
fn a_socket_provide_parses_with_its_unit_and_path() {
    let text = "[provides.deadband-socket]\nkind = \"socket\"\nunit = \"worker\"\npath = \"run/deadband.sock\"\n";
    let parsed = parsed(text);
    assert_eq!(parsed.refusals, []);
    assert_eq!(
        parsed.declared.provides,
        [Provide {
            name: "deadband-socket".to_owned(),
            unit: "worker".to_owned(),
            path: PathBuf::from("run/deadband.sock"),
            line: 1,
        }]
    );
}

#[test]
fn a_socket_provide_that_names_an_undeclared_unit_is_refused_with_its_line() {
    let text = "ports = []\n\n[provides.deadband-socket]\nkind = \"socket\"\nunit = \"daemon\"\npath = \"run/deadband.sock\"\n";
    let parsed = parsed(text);
    assert_eq!(parsed.declared.provides, []);
    let [refusal] = parsed.refusals.as_slice() else {
        panic!("{:?}", parsed.refusals);
    };
    assert_eq!(refusal.code, RefusalCode::ProvideInvalid);
    assert_eq!(refusal.line, Some(3));
    assert!(
        refusal.message.contains("does not declare"),
        "{}",
        refusal.message
    );
}

#[test]
fn a_provide_of_another_kind_or_with_a_path_outside_the_checkout_is_refused() {
    for text in [
        "[provides.s]\nkind = \"port\"\nunit = \"api\"\npath = \"a.sock\"\n",
        "[provides.s]\nkind = \"socket\"\nunit = \"api\"\npath = \"../a.sock\"\n",
        "[provides.s]\nkind = \"socket\"\nunit = \"api\"\npath = \"/tmp/a.sock\"\n",
        "[provides.s]\nkind = \"socket\"\nunit = \"api\"\n",
        "[provides.s]\nkind = \"socket\"\nunit = \"api\"\npath = \"a.sock\"\nmode = 1\n",
    ] {
        assert_eq!(
            refused(text),
            [(RefusalCode::ProvideInvalid, Some(1))],
            "{text}"
        );
    }
}

#[test]
fn a_file_without_needs_or_provides_declares_none() {
    let parsed = parsed("ports = [\"http\"]\n[processes.api]\nargv = [\"true\"]\n");
    assert_eq!(parsed.refusals, []);
    assert!(parsed.declared.needs.is_empty());
    assert!(parsed.declared.provides.is_empty());
}
