use std::collections::BTreeMap;

use crate::service::{
    AttributeType, Backend, KeyAttribute, Placeholder, Queue, Service, ServiceKind, ServiceValue,
    Table, check_bindings, check_resources, is_bucket_name, is_key_name, is_queue_name,
    is_table_name, localstack_services, variables,
};

fn table(hash: &str) -> Table {
    Table {
        hash: KeyAttribute::parse(hash).unwrap(),
        range: None,
        ttl: None,
        indexes: BTreeMap::new(),
    }
}

fn ports() -> BTreeMap<String, u16> {
    BTreeMap::from([
        ("mxv-dynamodb".to_owned(), 41_000),
        ("mxv-aws".to_owned(), 41_001),
    ])
}

#[test]
fn each_placeholder_renders_with_the_backends_port_on_the_ip_literal() {
    let value = ServiceValue::parse(
        "{endpoint} {port} {host_port} {table.inbox} {bucket.mail} {queue.jobs} {queue_url.jobs} {key.mail}",
    )
    .unwrap();
    assert_eq!(
        value.render(Some(41_001)),
        "http://127.0.0.1:41001 41001 127.0.0.1:41001 inbox mail jobs http://127.0.0.1:41001/queue/us-east-1/000000000000/jobs alias/mail"
    );
}

#[test]
fn a_value_renders_port_in_angle_brackets_before_the_workspace_holds_a_port() {
    let value = ServiceValue::parse("{endpoint}").unwrap();
    assert_eq!(value.render(None), "http://127.0.0.1:<port>");
}

#[test]
fn doubled_braces_are_literal_braces_and_any_other_brace_is_refused() {
    let value = ServiceValue::parse("{{x}} {port}").unwrap();
    assert_eq!(value.render(Some(1)), "{x} 1");
    assert_eq!(ServiceValue::parse("a {nope} b").unwrap_err(), "{nope} b");
    assert_eq!(ServiceValue::parse("{table.}").unwrap_err(), "{table.}");
    assert_eq!(ServiceValue::parse("open {").unwrap_err(), "{");
    assert_eq!(ServiceValue::parse("close }").unwrap_err(), "}");
}

#[test]
fn a_value_lists_the_resources_its_placeholders_name() {
    let value = ServiceValue::parse("{table.a}/{key.b}").unwrap();
    let placeholders: Vec<&Placeholder> = value.placeholders().collect();
    assert_eq!(
        placeholders,
        [
            &Placeholder::Table("a".to_owned()),
            &Placeholder::Key("b".to_owned())
        ]
    );
}

#[test]
fn a_key_attribute_is_a_name_and_one_of_s_n_and_b() {
    assert_eq!(
        KeyAttribute::parse("pk:S"),
        Some(KeyAttribute {
            name: "pk".to_owned(),
            kind: AttributeType::S
        })
    );
    assert_eq!(
        KeyAttribute::parse("a:b:N").map(|key| key.name),
        Some("a:b".to_owned())
    );
    assert_eq!(KeyAttribute::parse("pk"), None);
    assert_eq!(KeyAttribute::parse("pk:X"), None);
    assert_eq!(KeyAttribute::parse(":S"), None);
}

#[test]
fn resource_names_follow_aws_rules() {
    assert!(is_table_name("inbox.v1_x-y"));
    assert!(!is_table_name("ab"));
    assert!(is_bucket_name("inbox-mail.v1"));
    assert!(!is_bucket_name("Inbox"));
    assert!(!is_bucket_name("-mail"));
    assert!(!is_bucket_name("a..b"));
    assert!(is_queue_name("jobs_1-a"));
    assert!(!is_queue_name("jobs.fifo"));
    assert!(is_key_name("inbox/mail"));
    assert!(!is_key_name("aws/s3"));
}

#[test]
fn a_process_receives_the_common_variables_each_endpoint_it_needs_and_each_services_env() {
    let mut store = Service::new(ServiceKind::Dynamodb);
    store.tables.insert("inbox".to_owned(), table("pk:S"));
    store.env.insert(
        "INBOX_TABLE".to_owned(),
        ServiceValue::parse("{table.inbox}").unwrap(),
    );
    let mut mail = Service::new(ServiceKind::S3);
    mail.env.insert(
        "MAIL_ENDPOINT".to_owned(),
        ServiceValue::parse("{host_port}").unwrap(),
    );
    let variables = variables([&store, &mail], &ports());
    assert_eq!(
        variables,
        BTreeMap::from(
            [
                ("AWS_ACCESS_KEY_ID", "test"),
                ("AWS_CONFIG_FILE", "/dev/null"),
                ("AWS_EC2_METADATA_DISABLED", "true"),
                ("AWS_ENDPOINT_URL_DYNAMODB", "http://127.0.0.1:41000"),
                ("AWS_ENDPOINT_URL_S3", "http://127.0.0.1:41001"),
                ("AWS_REGION", "us-east-1"),
                ("AWS_SECRET_ACCESS_KEY", "test"),
                ("AWS_SHARED_CREDENTIALS_FILE", "/dev/null"),
                ("INBOX_TABLE", "inbox"),
                ("MAIL_ENDPOINT", "127.0.0.1:41001"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
        )
    );
}

#[test]
fn dynamodb_runs_on_dynamodb_local_and_the_other_kinds_share_localstack() {
    assert_eq!(ServiceKind::Dynamodb.backend(), Backend::Dynamodb);
    for kind in [ServiceKind::S3, ServiceKind::Sqs, ServiceKind::Kms] {
        assert_eq!(kind.backend(), Backend::Aws);
    }
    let services = [
        Service::new(ServiceKind::Kms),
        Service::new(ServiceKind::Dynamodb),
        Service::new(ServiceKind::S3),
        Service::new(ServiceKind::Kms),
    ];
    assert_eq!(localstack_services(&services), "s3,kms");
}

#[test]
fn two_services_that_declare_one_table_alike_share_it_and_differently_clash() {
    let mut first = Service::new(ServiceKind::Dynamodb);
    first.tables.insert("inbox".to_owned(), table("pk:S"));
    let same = first.clone();
    let mut other = Service::new(ServiceKind::Dynamodb);
    other.tables.insert("inbox".to_owned(), table("id:N"));
    let names = ["a".to_owned(), "b".to_owned()];
    assert_eq!(
        check_resources([(&names[0], &first), (&names[1], &same)]),
        Ok(())
    );
    let clash = check_resources([(&names[0], &first), (&names[1], &other)]).unwrap_err();
    assert_eq!(
        (
            clash.what,
            clash.name.as_str(),
            clash.first.as_str(),
            clash.second.as_str()
        ),
        ("table", "inbox", "a", "b")
    );
}

#[test]
fn two_services_that_declare_one_queue_with_different_timeouts_clash() {
    let mut first = Service::new(ServiceKind::Sqs);
    first.queues.insert("jobs".to_owned(), Queue::default());
    let mut other = Service::new(ServiceKind::Sqs);
    other.queues.insert(
        "jobs".to_owned(),
        Queue {
            visibility_timeout_seconds: Some(60),
        },
    );
    let names = ["a".to_owned(), "b".to_owned()];
    assert_eq!(
        check_resources([(&names[0], &first), (&names[1], &other)])
            .unwrap_err()
            .what,
        "queue"
    );
}

#[test]
fn two_services_that_bind_one_variable_clash_unless_it_renders_alike() {
    let bound = |kind, text: &str| {
        let mut service = Service::new(kind);
        service
            .env
            .insert("ENDPOINT".to_owned(), ServiceValue::parse(text).unwrap());
        service
    };
    let names = ["a".to_owned(), "b".to_owned()];
    let s3 = bound(ServiceKind::S3, "{endpoint}");
    let sqs = bound(ServiceKind::Sqs, "{endpoint}");
    let dynamodb = bound(ServiceKind::Dynamodb, "{endpoint}");
    assert_eq!(
        check_bindings([(&names[0], &s3), (&names[1], &sqs)]),
        Ok(())
    );
    let clash = check_bindings([(&names[0], &s3), (&names[1], &dynamodb)]).unwrap_err();
    assert_eq!((clash.what, clash.name.as_str()), ("variable", "ENDPOINT"));
}
