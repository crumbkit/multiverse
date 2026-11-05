//! The provisioning of a workspace's services: the tables, buckets,
//! queues and keys that its manifest declares, created in its backends.
//!
//! `mxv up` runs the mxv program as the oneshot app `mxv-provision` with
//! the [`Spec`] as one JSON argument, once the backends are ready. It
//! sends plain HTTP/1.0 requests to `127.0.0.1`: the JSON protocols of
//! DynamoDB, SQS and KMS, and `PUT /<bucket>` for S3. The backends do not
//! check signatures, so no request is signed. DynamoDB Local refuses a
//! request without an `Authorization` header, so its requests carry one
//! that names the placeholder access key `test` and no real signature.
//!
//! A resource that exists already counts as created, so a second run
//! changes nothing. A queue that exists with another visibility timeout
//! gets the declared one. Nothing is ever deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::clock;
use crate::manifest::Manifest;
use crate::service::{Backend, KeyAttribute, Queue, REGION, Table};

#[cfg(test)]
mod tests;

/// The most bytes of a backend's response that the provisioning reads.
/// A longer response fails as unreadable.
const MOST_RESPONSE_BYTES: u64 = 1024 * 1024;

/// How long one attempt of a request may wait to connect, and then for
/// each send or read. An attempt never waits past the provisioning's
/// deadline, so it may wait less.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Returns how long an attempt may wait now: [`REQUEST_TIMEOUT`] or the
/// time left before `deadline`, whichever is shorter.
///
/// # Errors
///
/// Returns a timed-out error once `deadline` has passed.
fn wait_until(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the deadline passed before the backend answered",
        ));
    }
    Ok(left.min(REQUEST_TIMEOUT))
}

/// The pause between two attempts of a request that found its backend
/// still starting.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// The `Authorization` header of a DynamoDB Local request. DynamoDB Local
/// reads only the access key and does not check the signature.
const DYNAMODB_AUTHORIZATION: &str = "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/dynamodb/aws4_request, SignedHeaders=host, Signature=0";

/// The resources of a workspace's services and the host ports of its
/// backends, which `mxv-provision` receives as one JSON argument.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Spec {
    /// The host port of DynamoDB Local, or `None` when no service needs
    /// it.
    pub dynamodb: Option<u16>,
    /// The host port of LocalStack, or `None` when no service needs it.
    pub aws: Option<u16>,
    /// The tables, by name.
    #[serde(default)]
    pub tables: BTreeMap<String, Table>,
    /// The buckets.
    #[serde(default)]
    pub buckets: BTreeSet<String>,
    /// The queues, by name.
    #[serde(default)]
    pub queues: BTreeMap<String, Queue>,
    /// The keys, each created with the alias `alias/<name>`.
    #[serde(default)]
    pub keys: BTreeSet<String>,
}

impl Spec {
    /// Returns the resources of every service of `manifest`, with the
    /// backends' host ports from `ports`, by port name.
    pub(crate) fn of(manifest: &Manifest, ports: &BTreeMap<String, u16>) -> Self {
        let mut spec = Self::default();
        for backend in manifest.backends() {
            let port = ports.get(backend.app()).copied();
            match backend {
                Backend::Dynamodb => spec.dynamodb = port,
                Backend::Aws => spec.aws = port,
            }
        }
        for service in manifest.services.values() {
            spec.tables.extend(service.tables.clone());
            spec.buckets.extend(service.buckets.iter().cloned());
            spec.queues.extend(service.queues.clone());
            spec.keys.extend(service.keys.iter().cloned());
        }
        spec
    }

    /// Returns the spec as the JSON argument of `mxv-provision`.
    pub(crate) fn to_argument(&self) -> String {
        // The spec is plain data, so it always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Returns the spec that `text`, the JSON argument, holds.
    ///
    /// # Errors
    ///
    /// Returns [`ProvisionError::BadSpec`] when `text` is not a spec.
    pub fn parse(text: &str) -> Result<Self, ProvisionError> {
        serde_json::from_str(text).map_err(|error| ProvisionError::BadSpec {
            message: error.to_string(),
        })
    }
}

/// Why the provisioning failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ProvisionError {
    /// The argument is not a [`Spec`].
    BadSpec {
        /// Why, from the JSON reader.
        message: String,
    },
    /// The spec names a resource whose backend has no port.
    NoBackend {
        /// The resource, such as `table inbox`.
        resource: String,
    },
    /// A backend did not answer, or answered with a server error, until
    /// the deadline.
    Unreachable {
        /// The resource, such as `table inbox`.
        resource: String,
        /// The last error.
        message: String,
    },
    /// A backend refused to create a resource.
    Refused {
        /// The resource, such as `table inbox`.
        resource: String,
        /// The HTTP status.
        status: u16,
        /// The backend's error code, such as `ValidationException`, and
        /// its message.
        error: String,
    },
    /// A table exists with a shape other than the manifest declares,
    /// which DynamoDB Local cannot change in place.
    Changed {
        /// The resource, such as `table inbox`.
        resource: String,
        /// What differs, such as `expiry on `gone``.
        what: String,
        /// Whether `what` names one thing, which the message calls `it`,
        /// or several, which it calls `them`.
        one: bool,
    },
}

impl fmt::Display for ProvisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadSpec { message } => {
                write!(
                    formatter,
                    "the argument is not a provisioning spec: {message}"
                )
            }
            Self::NoBackend { resource } => {
                write!(
                    formatter,
                    "the spec names the {resource}, and no port for its backend"
                )
            }
            Self::Unreachable { resource, message } => write!(
                formatter,
                "the backend of the {resource} did not answer before the deadline: {message}"
            ),
            Self::Refused {
                resource,
                status,
                error,
            } => write!(
                formatter,
                "the backend refused to create the {resource} with HTTP status {status}: {error}"
            ),
            Self::Changed {
                resource,
                what,
                one,
            } => write!(
                formatter,
                "the {resource} exists with {what} other than the manifest declares, and DynamoDB Local cannot change {}; run `mxv down`, which drops the backends' in-memory data, and then `mxv up`",
                if *one { "it" } else { "them" }
            ),
        }
    }
}

impl std::error::Error for ProvisionError {}

/// A backend's response.
#[derive(Debug)]
struct Response {
    /// The HTTP status.
    status: u16,
    /// The body.
    body: String,
}

impl Response {
    /// Reports whether the status is 2xx.
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Returns the error code of the body: the part after `#` of a JSON
    /// body's `__type`, or the `<Code>` of an XML body.
    fn code(&self) -> String {
        if let Ok(value) = serde_json::from_str::<Value>(&self.body)
            && let Some(kind) = value.get("__type").and_then(Value::as_str)
        {
            return kind.rsplit('#').next().unwrap_or(kind).to_owned();
        }
        self.body
            .split_once("<Code>")
            .and_then(|(_, rest)| rest.split_once("</Code>"))
            .map(|(code, _)| code.to_owned())
            .unwrap_or_default()
    }

    /// Returns the field `key` of a JSON body, or `Value::Null`.
    fn field(&self, key: &str) -> Value {
        serde_json::from_str::<Value>(&self.body)
            .ok()
            .and_then(|value| value.get(key).cloned())
            .unwrap_or(Value::Null)
    }

    /// Returns the refusal of creating `resource`.
    fn refusal(&self, resource: &str) -> ProvisionError {
        let mut error = self.code();
        let message = [self.field("message"), self.field("Message")]
            .into_iter()
            .find_map(|value| value.as_str().map(str::to_owned))
            .or_else(|| {
                self.body
                    .split_once("<Message>")
                    .and_then(|(_, rest)| rest.split_once("</Message>"))
                    .map(|(message, _)| message.to_owned())
            })
            .unwrap_or_default();
        if !message.is_empty() {
            error = format!("{error}: {message}");
        }
        ProvisionError::Refused {
            resource: resource.to_owned(),
            status: self.status,
            error,
        }
    }
}

/// One request to a backend on `127.0.0.1`.
struct Request<'a> {
    /// The backend's host port.
    port: u16,
    /// The method, such as `POST`.
    method: &'a str,
    /// The path, such as `/`.
    path: &'a str,
    /// The headers other than `Host` and `Content-Length`.
    headers: Vec<(&'a str, &'a str)>,
    /// The body.
    body: String,
}

impl Request<'_> {
    /// Sends the request once and reads the whole response. The request
    /// is HTTP/1.0, so the response ends when the backend closes the
    /// connection, and its body is never chunked.
    fn send_once(&self, deadline: Instant) -> io::Result<Response> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&address, wait_until(deadline)?)?;
        stream.set_write_timeout(Some(wait_until(deadline)?))?;
        let mut head = format!(
            "{} {} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n",
            self.method,
            self.path,
            self.port,
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(self.body.as_bytes())?;
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            // Each read waits at most until the deadline, so a backend that
            // answers slowly cannot hold the attempt past it.
            stream.set_read_timeout(Some(wait_until(deadline)?))?;
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(buffer.get(..read).unwrap_or_default());
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MOST_RESPONSE_BYTES {
                break;
            }
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MOST_RESPONSE_BYTES {
            return Err(io::Error::other(format!(
                "the response is longer than {MOST_RESPONSE_BYTES} bytes"
            )));
        }
        let text = String::from_utf8_lossy(&bytes);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse::<u16>().ok())
            .ok_or_else(|| io::Error::other("the response has no HTTP status line"))?;
        Ok(Response {
            status,
            body: body.to_owned(),
        })
    }

    /// Sends the request until the backend answers with a status below
    /// 500, pausing between attempts, and returns the answer.
    fn send(&self, resource: &str, deadline: Instant) -> Result<Response, ProvisionError> {
        loop {
            let last = match self.send_once(deadline) {
                Ok(response) if response.status < 500 => return Ok(response),
                Ok(response) => format!("HTTP status {}: {}", response.status, response.code()),
                Err(error) => error.to_string(),
            };
            if Instant::now() + RETRY_INTERVAL >= deadline {
                return Err(ProvisionError::Unreachable {
                    resource: resource.to_owned(),
                    message: last,
                });
            }
            clock::pause(RETRY_INTERVAL);
        }
    }
}

/// Returns a request of the JSON protocol to the backend on `port`, whose
/// service reads `target`, such as `DynamoDB_20120810.CreateTable`.
fn json_request<'a>(port: u16, target: &'a str, version: &'a str, body: &Value) -> Request<'a> {
    let mut headers = vec![("X-Amz-Target", target), ("Content-Type", version)];
    if target.starts_with("DynamoDB_") {
        headers.push(("Authorization", DYNAMODB_AUTHORIZATION));
    }
    Request {
        port,
        method: "POST",
        path: "/",
        headers,
        body: body.to_string(),
    }
}

/// The content type of DynamoDB's and SQS's JSON protocol.
const JSON_1_0: &str = "application/x-amz-json-1.0";

/// The content type of KMS's JSON protocol.
const JSON_1_1: &str = "application/x-amz-json-1.1";

/// Creates every resource of `spec`, each that exists already counting as
/// created, and retries each request until `deadline` while its backend
/// still starts. It calls `log` with one line for each resource.
///
/// # Errors
///
/// Returns the [`ProvisionError`] of the first resource that cannot be
/// created.
pub fn provision(
    spec: &Spec,
    deadline: Instant,
    log: &mut dyn FnMut(&str),
) -> Result<(), ProvisionError> {
    let backend = |port: Option<u16>, resource: &str| {
        port.ok_or_else(|| ProvisionError::NoBackend {
            resource: resource.to_owned(),
        })
    };
    for (name, table) in &spec.tables {
        let resource = format!("table {name}");
        let port = backend(spec.dynamodb, &resource)?;
        log(&create_table(port, name, table, &resource, deadline)?);
    }
    for name in &spec.buckets {
        let resource = format!("bucket {name}");
        let port = backend(spec.aws, &resource)?;
        log(&create_bucket(port, name, &resource, deadline)?);
    }
    for (name, queue) in &spec.queues {
        let resource = format!("queue {name}");
        let port = backend(spec.aws, &resource)?;
        log(&create_queue(port, name, queue, &resource, deadline)?);
    }
    for name in &spec.keys {
        let resource = format!("key alias/{name}");
        let port = backend(spec.aws, &resource)?;
        log(&create_key(port, name, &resource, deadline)?);
    }
    Ok(())
}

/// Returns the `AttributeDefinitions` and the `KeySchema` of a key.
fn key_schema(
    hash: &KeyAttribute,
    range: Option<&KeyAttribute>,
    attributes: &mut BTreeMap<String, &'static str>,
) -> Vec<Value> {
    let mut schema = Vec::new();
    for (attribute, role) in
        std::iter::once((hash, "HASH")).chain(range.map(|range| (range, "RANGE")))
    {
        let kind = match attribute.kind {
            crate::service::AttributeType::S => "S",
            crate::service::AttributeType::N => "N",
            crate::service::AttributeType::B => "B",
        };
        attributes.insert(attribute.name.clone(), kind);
        schema.push(json!({ "AttributeName": attribute.name, "KeyType": role }));
    }
    schema
}

/// Returns the body of DynamoDB's `CreateTable` for the table `name`.
pub(crate) fn create_table_body(name: &str, table: &Table) -> Value {
    let mut attributes = BTreeMap::new();
    let schema = key_schema(&table.hash, table.range.as_ref(), &mut attributes);
    let indexes: Vec<Value> = table
        .indexes
        .iter()
        .map(|(index, declared)| {
            json!({
                "IndexName": index,
                "KeySchema": key_schema(&declared.hash, declared.range.as_ref(), &mut attributes),
                "Projection": { "ProjectionType": "ALL" },
            })
        })
        .collect();
    let definitions: Vec<Value> = attributes
        .iter()
        .map(|(attribute, kind)| json!({ "AttributeName": attribute, "AttributeType": kind }))
        .collect();
    let mut body = json!({
        "TableName": name,
        "AttributeDefinitions": definitions,
        "KeySchema": schema,
        "BillingMode": "PAY_PER_REQUEST",
    });
    if !indexes.is_empty()
        && let Some(object) = body.as_object_mut()
    {
        object.insert("GlobalSecondaryIndexes".to_owned(), Value::Array(indexes));
    }
    body
}

/// Returns the parts of a table that the manifest declares, from either
/// a `CreateTable` body or DynamoDB's `Table` description: its key schema,
/// its attribute definitions by name, and the key schema of each global
/// secondary index by name. Other fields, such as its status, are left
/// out, and the order of the definitions and indexes does not matter.
fn table_shape(table: &Value) -> Value {
    let by_name = |list: &str, key: &str, value: &str| -> BTreeMap<String, Value> {
        table
            .get(list)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let name = item.get(key)?.as_str()?.to_owned();
                Some((name, item.get(value).cloned().unwrap_or(Value::Null)))
            })
            .collect()
    };
    json!({
        "KeySchema": table.get("KeySchema").cloned().unwrap_or(Value::Null),
        "AttributeDefinitions": by_name("AttributeDefinitions", "AttributeName", "AttributeType"),
        "Indexes": by_name("GlobalSecondaryIndexes", "IndexName", "KeySchema"),
    })
}

/// Creates the table `name`, and turns on its expiry when it declares a
/// `ttl` attribute, and returns the line that says what it did.
fn create_table(
    port: u16,
    name: &str,
    table: &Table,
    resource: &str,
    deadline: Instant,
) -> Result<String, ProvisionError> {
    let created = json_request(
        port,
        "DynamoDB_20120810.CreateTable",
        JSON_1_0,
        &create_table_body(name, table),
    )
    .send(resource, deadline)?;
    let mut line = if created.ok() {
        format!("created the {resource}")
    } else if created.code() == "ResourceInUseException" {
        let described = json_request(
            port,
            "DynamoDB_20120810.DescribeTable",
            JSON_1_0,
            &json!({ "TableName": name }),
        )
        .send(resource, deadline)?;
        if !described.ok() {
            return Err(described.refusal(resource));
        }
        if table_shape(&described.field("Table")) != table_shape(&create_table_body(name, table)) {
            return Err(ProvisionError::Changed {
                resource: resource.to_owned(),
                what: "keys, attribute types or indexes".to_owned(),
                one: false,
            });
        }
        format!("the {resource} exists already")
    } else {
        return Err(created.refusal(resource));
    };
    if let Some(attribute) = &table.ttl {
        let described = json_request(
            port,
            "DynamoDB_20120810.DescribeTimeToLive",
            JSON_1_0,
            &json!({ "TableName": name }),
        )
        .send(resource, deadline)?;
        if !described.ok() {
            return Err(described.refusal(resource));
        }
        let status = described.field("TimeToLiveDescription");
        let state = status.get("TimeToLiveStatus").and_then(Value::as_str);
        let current = status.get("AttributeName").and_then(Value::as_str);
        if matches!(state, Some("ENABLED" | "ENABLING"))
            && let Some(current) = current
            && current != attribute
        {
            return Err(ProvisionError::Changed {
                resource: resource.to_owned(),
                what: format!("expiry on `{current}`"),
                one: true,
            });
        }
        let enabled = state == Some("ENABLED") && current == Some(attribute);
        if !enabled {
            let updated = json_request(
                port,
                "DynamoDB_20120810.UpdateTimeToLive",
                JSON_1_0,
                &json!({
                    "TableName": name,
                    "TimeToLiveSpecification": { "Enabled": true, "AttributeName": attribute },
                }),
            )
            .send(resource, deadline)?;
            if !updated.ok() {
                return Err(updated.refusal(resource));
            }
            line.push_str(&format!(", with expiry on `{attribute}`"));
        }
    }
    Ok(line)
}

/// Creates the bucket `name`, unless it exists, and returns the line that
/// says what it did. The bucket is looked up first because LocalStack
/// answers a repeated create in `us-east-1` with success, which would hide
/// that the bucket existed.
fn create_bucket(
    port: u16,
    name: &str,
    resource: &str,
    deadline: Instant,
) -> Result<String, ProvisionError> {
    let path = format!("/{name}");
    let request = |method| Request {
        port,
        method,
        path: &path,
        headers: Vec::new(),
        body: String::new(),
    };
    let found = request("HEAD").send(resource, deadline)?;
    if found.ok() {
        return Ok(format!("the {resource} exists already"));
    }
    if found.status != 404 {
        return Err(found.refusal(resource));
    }
    let created = request("PUT").send(resource, deadline)?;
    if created.ok() {
        Ok(format!("created the {resource}"))
    } else if matches!(
        created.code().as_str(),
        "BucketAlreadyOwnedByYou" | "BucketAlreadyExists"
    ) {
        Ok(format!("the {resource} exists already"))
    } else {
        Err(created.refusal(resource))
    }
}

/// Creates the queue `name`, or gives an existing one the declared
/// visibility timeout, and returns the line that says what it did. The
/// queue is looked up first because SQS answers a repeated create with the
/// same attributes with success, which would hide that the queue existed.
fn create_queue(
    port: u16,
    name: &str,
    queue: &Queue,
    resource: &str,
    deadline: Instant,
) -> Result<String, ProvisionError> {
    let attributes: BTreeMap<&str, String> = queue
        .visibility_timeout_seconds
        .map(|seconds| ("VisibilityTimeout", seconds.to_string()))
        .into_iter()
        .collect();
    let found = json_request(
        port,
        "AmazonSQS.GetQueueUrl",
        JSON_1_0,
        &json!({ "QueueName": name }),
    )
    .send(resource, deadline)?;
    if !found.ok() {
        if !matches!(
            found.code().as_str(),
            "QueueDoesNotExist" | "AWS.SimpleQueueService.NonExistentQueue"
        ) {
            return Err(found.refusal(resource));
        }
        let created = json_request(
            port,
            "AmazonSQS.CreateQueue",
            JSON_1_0,
            &json!({ "QueueName": name, "Attributes": attributes }),
        )
        .send(resource, deadline)?;
        return if created.ok() {
            Ok(format!("created the {resource}"))
        } else {
            Err(created.refusal(resource))
        };
    }
    let set = json_request(
        port,
        "AmazonSQS.SetQueueAttributes",
        JSON_1_0,
        &json!({ "QueueUrl": found.field("QueueUrl"), "Attributes": attributes }),
    )
    .send(resource, deadline)?;
    if set.ok() {
        Ok(format!(
            "the {resource} exists already, and has the declared attributes"
        ))
    } else {
        Err(set.refusal(resource))
    }
}

/// Creates a symmetric key with the alias `alias/<name>`, unless the alias
/// exists, and returns the line that says what it did.
fn create_key(
    port: u16,
    name: &str,
    resource: &str,
    deadline: Instant,
) -> Result<String, ProvisionError> {
    let alias = format!("alias/{name}");
    let described = json_request(
        port,
        "TrentService.DescribeKey",
        JSON_1_1,
        &json!({ "KeyId": alias }),
    )
    .send(resource, deadline)?;
    if described.ok() {
        return Ok(format!("the {resource} exists already"));
    }
    if described.code() != "NotFoundException" {
        return Err(described.refusal(resource));
    }
    let created = json_request(
        port,
        "TrentService.CreateKey",
        JSON_1_1,
        &json!({ "Description": format!("mxv: {alias} in {REGION}") }),
    )
    .send(resource, deadline)?;
    if !created.ok() {
        return Err(created.refusal(resource));
    }
    let key = created.field("KeyMetadata");
    let aliased = json_request(
        port,
        "TrentService.CreateAlias",
        JSON_1_1,
        &json!({ "AliasName": alias, "TargetKeyId": key.get("KeyId") }),
    )
    .send(resource, deadline)?;
    if aliased.ok() {
        Ok(format!("created the {resource}"))
    } else if aliased.code() == "AlreadyExistsException" {
        Ok(format!("the {resource} exists already"))
    } else {
        Err(aliased.refusal(resource))
    }
}
