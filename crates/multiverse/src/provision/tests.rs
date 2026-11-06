use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::manifest::Manifest;
use crate::provision::{ProvisionError, Spec, create_table_body, provision};

/// One request that the fake backend received: the method and path, the
/// `X-Amz-Target` header, whether it carried an `Authorization` header,
/// and the body.
#[derive(Clone, Debug)]
struct Received {
    line: String,
    target: String,
    authorized: bool,
    body: String,
}

/// A backend on a free port of 127.0.0.1 that answers each request with
/// `answer` and records it, until the test ends.
struct FakeBackend {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeBackend {
    fn start(answer: fn(&Received) -> (u16, String)) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut length = 0;
                let mut target = String::new();
                let mut authorized = false;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(": ").unwrap();
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.parse().unwrap(),
                        "x-amz-target" => target = value.to_owned(),
                        "authorization" => authorized = true,
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = Received {
                    line: line.trim_end().to_owned(),
                    target,
                    authorized,
                    body: String::from_utf8(body).unwrap(),
                };
                let (status, text) = answer(&request);
                log.lock().unwrap().push(request);
                let _ = write!(
                    stream,
                    "HTTP/1.0 {status} X\r\nContent-Type: application/json\r\n\r\n{text}"
                );
            }
        });
        Self { port, received }
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

/// Returns DynamoDB's description of the table `inbox` as the manifest
/// declares it, with the sort key `sk` of the type `range` and fields that
/// the comparison ignores, in an order other than the declaration's.
fn described_inbox(range: &str) -> String {
    json!({"Table": {
        "TableName": "inbox",
        "TableStatus": "ACTIVE",
        "AttributeDefinitions": [
            {"AttributeName": "sk", "AttributeType": range},
            {"AttributeName": "pk", "AttributeType": "S"},
            {"AttributeName": "owner", "AttributeType": "S"},
            {"AttributeName": "at", "AttributeType": "N"},
        ],
        "KeySchema": [
            {"AttributeName": "pk", "KeyType": "HASH"},
            {"AttributeName": "sk", "KeyType": "RANGE"},
        ],
        "GlobalSecondaryIndexes": [{
            "IndexName": "by-owner",
            "IndexStatus": "ACTIVE",
            "KeySchema": [
                {"AttributeName": "owner", "KeyType": "HASH"},
                {"AttributeName": "at", "KeyType": "RANGE"},
            ],
            "Projection": {"ProjectionType": "ALL"},
        }],
        "ItemCount": 0,
    }})
    .to_string()
}

/// Answers as a backend in which every resource exists already.
fn everything_exists(request: &Received) -> (u16, String) {
    match request.target.as_str() {
        "DynamoDB_20120810.CreateTable" => (
            400,
            json!({"__type": "com.amazonaws.dynamodb.v20120810#ResourceInUseException", "Message": "Cannot create preexisting table"}).to_string(),
        ),
        "DynamoDB_20120810.DescribeTable" => (200, described_inbox("S")),
        "DynamoDB_20120810.DescribeTimeToLive" => (
            200,
            json!({"TimeToLiveDescription": {"TimeToLiveStatus": "ENABLED", "AttributeName": "expires"}}).to_string(),
        ),
        "AmazonSQS.CreateQueue" => (
            400,
            json!({"__type": "QueueNameExists", "message": "A queue already exists"}).to_string(),
        ),
        "AmazonSQS.GetQueueUrl" => (
            200,
            json!({"QueueUrl": "http://127.0.0.1:1/queue/us-east-1/000000000000/jobs"}).to_string(),
        ),
        "AmazonSQS.SetQueueAttributes" | "TrentService.DescribeKey" => (200, "{}".to_owned()),
        _ if request.line.starts_with("HEAD /") => (200, String::new()),
        _ => (500, "unexpected".to_owned()),
    }
}

/// Answers as an empty backend, which creates each resource.
fn creates_everything(request: &Received) -> (u16, String) {
    match request.target.as_str() {
        "DynamoDB_20120810.DescribeTimeToLive" => (
            200,
            json!({"TimeToLiveDescription": {"TimeToLiveStatus": "DISABLED"}}).to_string(),
        ),
        "TrentService.DescribeKey" => (
            400,
            json!({"__type": "NotFoundException", "message": "Unable to find KMS alias"})
                .to_string(),
        ),
        "TrentService.CreateKey" => (200, json!({"KeyMetadata": {"KeyId": "k-1"}}).to_string()),
        "AmazonSQS.GetQueueUrl" => (
            400,
            json!({"__type": "com.amazonaws.sqs#QueueDoesNotExist", "message": "The specified queue does not exist."}).to_string(),
        ),
        _ if request.line.starts_with("HEAD /") => (404, String::new()),
        _ => (200, "{}".to_owned()),
    }
}

const MANIFEST: &str = r#"
[service.store]
kind = "dynamodb"
tables.inbox = { hash = "pk:S", range = "sk:S", ttl = "expires", indexes.by-owner = { hash = "owner:S", range = "at:N" } }

[service.mail]
kind = "s3"
buckets = ["inbox-mail"]

[service.jobs]
kind = "sqs"
queues.jobs = { visibility_timeout_seconds = 120 }

[service.keys]
kind = "kms"
keys = ["inbox-mail"]
"#;

fn spec(dynamodb: u16, aws: u16) -> Spec {
    let manifest = Manifest::parse(MANIFEST, Path::new("/repo/mxv.toml")).unwrap();
    let ports = BTreeMap::from([
        ("mxv-dynamodb".to_owned(), dynamodb),
        ("mxv-aws".to_owned(), aws),
    ]);
    Spec::of(&manifest, &ports)
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

#[test]
fn the_spec_holds_every_services_resources_and_the_backends_ports_and_reads_back_from_its_argument()
{
    let spec = spec(41_000, 41_001);
    assert_eq!((spec.dynamodb, spec.aws), (Some(41_000), Some(41_001)));
    assert_eq!(spec.tables.keys().collect::<Vec<_>>(), ["inbox"]);
    assert_eq!(spec.buckets.iter().collect::<Vec<_>>(), ["inbox-mail"]);
    assert_eq!(spec.queues["jobs"].visibility_timeout_seconds, Some(120));
    assert_eq!(spec.keys.iter().collect::<Vec<_>>(), ["inbox-mail"]);
    assert_eq!(Spec::parse(&spec.to_argument()).unwrap(), spec);
}

#[test]
fn an_argument_that_is_not_a_spec_fails_as_a_bad_spec() {
    assert!(matches!(
        Spec::parse(r#"{"tables": 1}"#),
        Err(ProvisionError::BadSpec { .. })
    ));
}

#[test]
fn a_table_is_created_on_demand_with_its_keys_and_indexes() {
    let spec = spec(1, 2);
    let body = create_table_body("inbox", &spec.tables["inbox"]);
    assert_eq!(
        body,
        json!({
            "TableName": "inbox",
            "AttributeDefinitions": [
                {"AttributeName": "at", "AttributeType": "N"},
                {"AttributeName": "owner", "AttributeType": "S"},
                {"AttributeName": "pk", "AttributeType": "S"},
                {"AttributeName": "sk", "AttributeType": "S"},
            ],
            "KeySchema": [
                {"AttributeName": "pk", "KeyType": "HASH"},
                {"AttributeName": "sk", "KeyType": "RANGE"},
            ],
            "GlobalSecondaryIndexes": [{
                "IndexName": "by-owner",
                "KeySchema": [
                    {"AttributeName": "owner", "KeyType": "HASH"},
                    {"AttributeName": "at", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "ALL"},
            }],
            "BillingMode": "PAY_PER_REQUEST",
        })
    );
}

#[test]
fn provisioning_an_empty_backend_creates_each_resource_and_turns_on_the_tables_expiry() {
    let backend = FakeBackend::start(creates_everything);
    let mut lines = Vec::new();
    provision(&spec(backend.port, backend.port), deadline(), &mut |line| {
        lines.push(line.to_owned());
    })
    .unwrap();
    assert_eq!(
        lines,
        [
            "created the table inbox, with expiry on `expires`",
            "created the bucket inbox-mail",
            "created the queue jobs",
            "created the key alias/inbox-mail",
        ]
    );
    let received = backend.received();
    let targets: Vec<&str> = received
        .iter()
        .map(|request| {
            if request.target.is_empty() {
                request.line.as_str()
            } else {
                request.target.as_str()
            }
        })
        .collect();
    assert_eq!(
        targets,
        [
            "DynamoDB_20120810.CreateTable",
            "DynamoDB_20120810.DescribeTimeToLive",
            "DynamoDB_20120810.UpdateTimeToLive",
            "HEAD /inbox-mail HTTP/1.0",
            "PUT /inbox-mail HTTP/1.0",
            "AmazonSQS.GetQueueUrl",
            "AmazonSQS.CreateQueue",
            "TrentService.DescribeKey",
            "TrentService.CreateKey",
            "TrentService.CreateAlias",
        ]
    );
    let alias: serde_json::Value = serde_json::from_str(&received[9].body).unwrap();
    assert_eq!(
        alias,
        json!({"AliasName": "alias/inbox-mail", "TargetKeyId": "k-1"})
    );
    let queue: serde_json::Value = serde_json::from_str(&received[6].body).unwrap();
    assert_eq!(
        queue,
        json!({"QueueName": "jobs", "Attributes": {"VisibilityTimeout": "120"}})
    );
}

#[test]
fn only_dynamodb_requests_carry_an_authorization_header_and_none_is_signed() {
    let backend = FakeBackend::start(creates_everything);
    provision(&spec(backend.port, backend.port), deadline(), &mut |_| {}).unwrap();
    for request in backend.received() {
        assert_eq!(
            request.authorized,
            request.target.starts_with("DynamoDB_"),
            "{request:?}"
        );
    }
}

#[test]
fn provisioning_a_backend_that_holds_every_resource_succeeds_and_creates_nothing() {
    let backend = FakeBackend::start(everything_exists);
    let mut lines = Vec::new();
    provision(&spec(backend.port, backend.port), deadline(), &mut |line| {
        lines.push(line.to_owned());
    })
    .unwrap();
    assert_eq!(
        lines,
        [
            "the table inbox exists already",
            "the bucket inbox-mail exists already",
            "the queue jobs exists already, and has the declared attributes",
            "the key alias/inbox-mail exists already",
        ]
    );
}

#[test]
fn a_table_that_exists_with_another_key_schema_fails_and_says_to_run_down_then_up() {
    let backend = FakeBackend::start(|request| match request.target.as_str() {
        "DynamoDB_20120810.DescribeTable" => (200, described_inbox("N")),
        _ => everything_exists(request),
    });
    let error = provision(&spec(backend.port, backend.port), deadline(), &mut |_| {}).unwrap_err();
    assert!(
        matches!(&error, ProvisionError::Changed { resource, .. } if resource == "table inbox"),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the table inbox exists with keys, attribute types or indexes other than the manifest declares, and DynamoDB Local cannot change them; run `mxv down`, which drops the backends' in-memory data, and then `mxv up`"
    );
}

#[test]
fn a_table_whose_expiry_is_on_another_attribute_fails_and_says_to_run_down_then_up() {
    let backend = FakeBackend::start(|request| {
        match request.target.as_str() {
        "DynamoDB_20120810.DescribeTimeToLive" => (
            200,
            json!({"TimeToLiveDescription": {"TimeToLiveStatus": "ENABLED", "AttributeName": "gone"}}).to_string(),
        ),
        _ => everything_exists(request),
    }
    });
    let error = provision(&spec(backend.port, backend.port), deadline(), &mut |_| {}).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the table inbox exists with expiry on `gone` other than the manifest declares, and DynamoDB Local cannot change it; run `mxv down`, which drops the backends' in-memory data, and then `mxv up`"
    );
}

#[test]
fn a_backend_that_refuses_a_resource_fails_with_the_resource_and_the_backends_error() {
    let backend = FakeBackend::start(|_| {
        (
            400,
            json!({"__type": "com.amazon.coral.validate#ValidationException", "Message": "bad key"})
                .to_string(),
        )
    });
    let error = provision(&spec(backend.port, backend.port), deadline(), &mut |_| {}).unwrap_err();
    assert_eq!(
        error.to_string(),
        "the backend refused to create the table inbox with HTTP status 400: ValidationException: bad key"
    );
}

#[test]
fn a_backend_that_never_answers_fails_as_unreachable_at_the_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut spec = spec(port, port);
    spec.buckets.clear();
    spec.queues.clear();
    spec.keys.clear();
    let error = provision(
        &spec,
        Instant::now() + Duration::from_millis(600),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        matches!(&error, ProvisionError::Unreachable { resource, .. } if resource == "table inbox"),
        "{error}"
    );
}

#[test]
fn a_backend_that_accepts_and_never_answers_fails_soon_after_the_deadline() {
    // The listener accepts connections into its backlog and never reads
    // or writes, so each request waits for an answer that never comes.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut spec = spec(port, port);
    spec.buckets.clear();
    spec.queues.clear();
    spec.keys.clear();
    let started = Instant::now();
    let error = provision(&spec, started + Duration::from_secs(1), &mut |_| {}).unwrap_err();
    assert!(
        matches!(&error, ProvisionError::Unreachable { resource, .. } if resource == "table inbox"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    drop(listener);
}

#[test]
fn a_resource_whose_backend_has_no_port_fails_without_a_request() {
    let mut spec = spec(1, 2);
    spec.dynamodb = None;
    let error = provision(&spec, deadline(), &mut |_| {}).unwrap_err();
    assert!(matches!(error, ProvisionError::NoBackend { .. }));
}
