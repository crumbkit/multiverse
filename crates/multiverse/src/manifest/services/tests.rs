use std::collections::BTreeMap;
use std::path::Path;

use crate::manifest::{Joined, Manifest, ManifestError, ManifestProblem};
use crate::service::{Backend, Placeholder, ServiceKind};

fn parse(text: &str) -> Result<Manifest, ManifestError> {
    Manifest::parse(text, Path::new("/repo/mxv.toml"))
}

/// Returns the line and the message of an invalid manifest.
fn invalid(text: &str) -> (Option<usize>, String) {
    let error = parse(text).unwrap_err();
    match error.problem {
        ManifestProblem::Invalid { message } => (error.line, message),
        other => panic!("expected an invalid manifest, got {other:?}"),
    }
}

const SERVICES: &str = r#"
ports = ["api"]

[service.store]
kind = "dynamodb"
tables.inbox = { hash = "pk:S", range = "sk:S", ttl = "expires" }
env = { INBOX_TABLE = "{table.inbox}", INBOX_DYNAMODB_PORT = "{port}" }

[service.mail]
kind = "s3"
buckets = ["inbox-mail"]
env = { MAIL_BUCKET = "{bucket.inbox-mail}", MAIL_S3 = "{host_port}" }

[service.jobs]
kind = "sqs"
queues.jobs = { visibility_timeout_seconds = 120 }
env = { JOBS_URL = "{queue_url.jobs}" }

[service.keys]
kind = "kms"
keys = ["inbox-mail"]
env = { MAIL_KEY = "{key.inbox-mail}" }

[processes.worker]
argv = ["worker"]
needs = ["store", "mail", "jobs", "keys"]

[processes.api]
argv = ["api"]
ports = { PORT = "api" }
listen = "api"
"#;

#[test]
fn a_manifest_declares_services_of_each_kind_with_their_resources_and_variables() {
    let manifest = parse(SERVICES).unwrap();
    let store = &manifest.services["store"];
    assert_eq!(store.kind, ServiceKind::Dynamodb);
    let inbox = &store.tables["inbox"];
    assert_eq!(inbox.hash.name, "pk");
    assert_eq!(inbox.range.as_ref().unwrap().name, "sk");
    assert_eq!(inbox.ttl.as_deref(), Some("expires"));
    assert!(manifest.services["mail"].buckets.contains("inbox-mail"));
    assert_eq!(
        manifest.services["jobs"].queues["jobs"].visibility_timeout_seconds,
        Some(120)
    );
    assert!(manifest.services["keys"].keys.contains("inbox-mail"));
    assert_eq!(
        manifest.processes["worker"].needs,
        ["store", "mail", "jobs", "keys"]
    );
    assert!(manifest.processes["api"].needs.is_empty());
}

#[test]
fn a_manifest_with_services_adds_each_backend_to_the_port_ledger_and_its_apps() {
    let manifest = parse(SERVICES).unwrap();
    assert_eq!(
        manifest.backends().into_iter().collect::<Vec<_>>(),
        [Backend::Dynamodb, Backend::Aws]
    );
    assert_eq!(manifest.ledger_ports(), ["api", "mxv-dynamodb", "mxv-aws"]);
    assert_eq!(
        manifest.service_apps(),
        ["mxv-dynamodb", "mxv-aws", "mxv-provision"]
    );
}

#[test]
fn a_manifest_of_one_s3_service_runs_only_localstack() {
    let manifest = parse("[service.mail]\nkind = \"s3\"\nbuckets = [\"inbox-mail\"]\n").unwrap();
    assert_eq!(manifest.ledger_ports(), ["mxv-aws"]);
    assert_eq!(manifest.service_apps(), ["mxv-aws", "mxv-provision"]);
}

#[test]
fn a_manifest_of_services_alone_is_valid() {
    let manifest =
        parse("[service.store]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\" }\n")
            .unwrap();
    assert!(manifest.processes.is_empty() && manifest.containers.is_empty());
    assert_eq!(manifest.services.len(), 1);
}

#[test]
fn a_process_that_needs_services_receives_their_variables_and_one_that_needs_none_receives_none() {
    let manifest = parse(SERVICES).unwrap();
    let ports = BTreeMap::from([
        ("mxv-dynamodb".to_owned(), 41_000),
        ("mxv-aws".to_owned(), 41_001),
    ]);
    let worker = manifest.service_variables(&manifest.processes["worker"], &ports);
    let expected = [
        ("AWS_ACCESS_KEY_ID", "test"),
        ("AWS_CONFIG_FILE", "/dev/null"),
        ("AWS_EC2_METADATA_DISABLED", "true"),
        ("AWS_ENDPOINT_URL_DYNAMODB", "http://127.0.0.1:41000"),
        ("AWS_ENDPOINT_URL_KMS", "http://127.0.0.1:41001"),
        ("AWS_ENDPOINT_URL_S3", "http://127.0.0.1:41001"),
        ("AWS_ENDPOINT_URL_SQS", "http://127.0.0.1:41001"),
        ("AWS_REGION", "us-east-1"),
        ("AWS_SECRET_ACCESS_KEY", "test"),
        ("AWS_SHARED_CREDENTIALS_FILE", "/dev/null"),
        ("INBOX_DYNAMODB_PORT", "41000"),
        ("INBOX_TABLE", "inbox"),
        (
            "JOBS_URL",
            "http://127.0.0.1:41001/queue/us-east-1/000000000000/jobs",
        ),
        ("MAIL_BUCKET", "inbox-mail"),
        ("MAIL_KEY", "alias/inbox-mail"),
        ("MAIL_S3", "127.0.0.1:41001"),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()));
    assert_eq!(worker, BTreeMap::from(expected));
    assert!(
        manifest
            .service_variables(&manifest.processes["api"], &ports)
            .is_empty()
    );
}

#[test]
fn an_unknown_kind_fails_with_its_line() {
    let (line, message) = invalid("\n[service.x]\nkind = \"redis\"\n");
    assert_eq!(line, Some(3));
    assert!(message.contains("`redis`"), "{message}");
}

#[test]
fn a_service_without_a_kind_fails_as_invalid() {
    let error = parse("[service.x]\nbuckets = [\"abc\"]\n").unwrap_err();
    assert!(error.to_string().contains("kind"), "{error}");
}

#[test]
fn an_unknown_key_of_a_service_or_a_table_fails_and_names_the_key() {
    for (text, key) in [
        ("[service.x]\nkind = \"s3\"\nimage = \"y\"\n", "image"),
        (
            "[service.x]\nkind = \"dynamodb\"\ntables.t1x = { hash = \"pk:S\", sort = \"sk:S\" }\n",
            "sort",
        ),
        (
            "[service.x]\nkind = \"sqs\"\nqueues.q = { fifo = true }\n",
            "fifo",
        ),
    ] {
        let error = parse(text).unwrap_err();
        assert_eq!(
            error.problem,
            ManifestProblem::UnknownKey {
                key: key.to_owned()
            },
            "{text}"
        );
    }
}

#[test]
fn a_resource_of_another_kind_fails_with_its_line() {
    let (line, message) =
        invalid("[service.mail]\nkind = \"s3\"\n\ntables.inbox = { hash = \"pk:S\" }\n");
    assert_eq!(line, Some(4));
    assert!(message.contains("declares only `buckets`"), "{message}");
}

#[test]
fn a_bad_key_attribute_and_an_attribute_with_two_types_fail_with_their_lines() {
    let (line, message) =
        invalid("[service.s]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk\" }\n");
    assert_eq!(line, Some(3));
    assert!(message.contains("`<attribute>:<type>`"), "{message}");
    let (_, message) = invalid(
        "[service.s]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\", indexes.by-pk = { hash = \"pk:N\" } }\n",
    );
    assert!(message.contains("two types"), "{message}");
}

#[test]
fn resource_names_that_aws_refuses_fail_with_their_lines() {
    for text in [
        "[service.s]\nkind = \"dynamodb\"\ntables.ab = { hash = \"pk:S\" }\n",
        "[service.s]\nkind = \"s3\"\nbuckets = [\"Upper\"]\n",
        "[service.s]\nkind = \"sqs\"\nqueues.\"a.fifo\" = {}\n",
        "[service.s]\nkind = \"kms\"\nkeys = [\"aws/s3\"]\n",
    ] {
        let (line, _) = invalid(text);
        assert_eq!(line, Some(3), "{text}");
    }
}

#[test]
fn a_bucket_declared_twice_fails_as_invalid() {
    let (_, message) = invalid("[service.s]\nkind = \"s3\"\nbuckets = [\"abc\", \"abc\"]\n");
    assert!(message.contains("twice"), "{message}");
}

#[test]
fn a_visibility_timeout_over_twelve_hours_fails_with_its_line() {
    let (line, _) =
        invalid("[service.s]\nkind = \"sqs\"\nqueues.q = { visibility_timeout_seconds = 43201 }\n");
    assert_eq!(line, Some(3));
}

#[test]
fn a_placeholder_of_an_undeclared_resource_fails_with_its_line() {
    let (line, message) = invalid(
        "[service.s]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\" }\nenv = { T = \"{table.other}\" }\n",
    );
    assert_eq!(line, Some(4));
    assert!(message.contains("{table.other}"), "{message}");
    let (_, message) =
        invalid("[service.s]\nkind = \"s3\"\nbuckets = [\"abc\"]\nenv = { T = \"{table.abc}\" }\n");
    assert!(message.contains("{table.abc}"), "{message}");
}

#[test]
fn a_brace_that_opens_no_placeholder_fails_with_its_line() {
    let (line, message) = invalid("[service.s]\nkind = \"s3\"\n\nenv = { T = \"{endpont}\" }\n");
    assert_eq!(line, Some(4));
    assert!(message.contains("{endpont}"), "{message}");
}

#[test]
fn a_service_that_sets_an_aws_variable_or_path_fails_with_its_line() {
    for variable in ["AWS_REGION", "AWS_PROFILE", "PATH"] {
        let (line, message) = invalid(&format!(
            "[service.s]\nkind = \"s3\"\n\nenv = {{ {variable} = \"x\" }}\n"
        ));
        assert_eq!(line, Some(4), "{variable}");
        assert!(message.contains(variable), "{message}");
    }
}

#[test]
fn a_need_of_an_undeclared_service_or_one_named_twice_fails_with_its_line() {
    let (line, message) = invalid("[processes.w]\nargv = [\"w\"]\nneeds = [\"store\"]\n");
    assert_eq!(line, Some(3));
    assert!(message.contains("`store`"), "{message}");
    let (_, message) = invalid(
        "[service.store]\nkind = \"s3\"\n\n[processes.w]\nargv = [\"w\"]\nneeds = [\"store\", \"store\"]\n",
    );
    assert!(message.contains("twice"), "{message}");
}

#[test]
fn a_process_that_needs_a_service_and_sets_an_aws_variable_fails_with_its_line() {
    let (line, message) = invalid(
        "[service.store]\nkind = \"s3\"\n\n[processes.w]\nargv = [\"w\"]\nneeds = [\"store\"]\nenv = { AWS_PROFILE = \"prod\" }\n",
    );
    assert_eq!(line, Some(7));
    assert!(message.contains("AWS_PROFILE"), "{message}");
}

#[test]
fn a_process_without_needs_may_set_its_own_aws_variables() {
    parse("[processes.w]\nargv = [\"w\"]\nenv = { AWS_REGION = \"eu-west-1\" }\n").unwrap();
}

#[test]
fn a_process_that_sets_a_variable_that_a_needed_service_binds_fails_with_its_line() {
    let (line, message) = invalid(
        "[service.store]\nkind = \"s3\"\nenv = { BUCKET = \"x\" }\n\n[processes.w]\nargv = [\"w\"]\nneeds = [\"store\"]\nenv = { BUCKET = \"y\" }\n",
    );
    assert_eq!(line, Some(8));
    assert!(message.contains("`store`"), "{message}");
}

#[test]
fn two_needed_services_that_bind_one_variable_differently_fail_with_a_service_clash() {
    let error = parse(
        "[service.a]\nkind = \"s3\"\nenv = { NAME = \"a\" }\n\n[service.b]\nkind = \"sqs\"\nenv = { NAME = \"b\" }\n\n[processes.w]\nargv = [\"w\"]\nneeds = [\"a\", \"b\"]\n",
    )
    .unwrap_err();
    assert_eq!(error.line, Some(11));
    assert!(
        matches!(&error.problem, ManifestProblem::ServiceClash { name, .. } if name == "NAME"),
        "{error}"
    );
}

#[test]
fn two_services_that_declare_one_table_differently_fail_with_a_service_clash() {
    let error = parse(
        "[service.a]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\" }\n\n[service.b]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"id:N\" }\n",
    )
    .unwrap_err();
    assert_eq!(error.line, Some(5));
    assert!(
        matches!(&error.problem, ManifestProblem::ServiceClash { name, .. } if name == "inbox"),
        "{error}"
    );
}

#[test]
fn two_services_that_declare_one_table_alike_share_it() {
    let manifest = parse(
        "[service.a]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\" }\n\n[service.b]\nkind = \"dynamodb\"\ntables.inbox = { hash = \"pk:S\" }\n",
    )
    .unwrap();
    assert_eq!(manifest.services.len(), 2);
}

#[test]
fn a_unit_or_port_named_with_the_mxv_prefix_fails_with_its_line() {
    for text in [
        "ports = [\"mxv-aws\"]\n",
        "\n[processes.mxv-provision]\nargv = [\"x\"]\n",
        "\n[containers.mxv-db]\nimage = \"busybox:1.37.0\"\n",
    ] {
        let (_, message) = invalid(text);
        assert!(message.contains("`mxv-`"), "{message}");
    }
}

#[test]
fn a_workspace_manifest_may_need_a_service_of_a_joined_manifest() {
    let joined = Joined {
        ports: Vec::new(),
        units: Vec::new(),
        services: vec!["store".to_owned()],
        ..Joined::default()
    };
    let manifest = Manifest::parse_joining(
        "[processes.w]\nargv = [\"w\"]\nneeds = [\"store\"]\n",
        Path::new("/ws/mxv.toml"),
        &joined,
    )
    .unwrap();
    assert_eq!(manifest.processes["w"].needs, ["store"]);
}

#[test]
fn a_renamed_manifest_keeps_its_service_names_and_needs() {
    let manifest = parse(SERVICES).unwrap();
    let renamed = manifest.renamed(&|name| format!("app-{name}"));
    assert_eq!(
        renamed.services.keys().collect::<Vec<_>>(),
        ["jobs", "keys", "mail", "store"]
    );
    assert_eq!(
        renamed.processes["app-worker"].needs,
        ["store", "mail", "jobs", "keys"]
    );
}

#[test]
fn a_misspelt_service_table_parses_with_a_warning_that_names_it() {
    let manifest = parse("[services.x]\nkind = \"s3\"\n").unwrap();
    assert!(
        manifest.warnings[0].contains("`service`"),
        "{:?}",
        manifest.warnings
    );
}

#[test]
fn every_placeholder_of_a_declared_resource_is_accepted() {
    let manifest = parse(SERVICES).unwrap();
    let placeholders: Vec<Placeholder> = manifest
        .services
        .values()
        .flat_map(|service| service.env.values())
        .flat_map(|value| value.placeholders().cloned().collect::<Vec<_>>())
        .collect();
    assert_eq!(placeholders.len(), 6);
}
