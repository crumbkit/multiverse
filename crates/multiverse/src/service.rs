//! The local AWS services of a manifest, which `[service.<name>]` tables
//! declare, and the backends that mxv runs for them.
//!
//! ```toml
//! [service.store]
//! kind = "dynamodb"
//! tables.inbox = { hash = "pk:S", range = "sk:S", ttl = "expires" }
//! env = { INBOX_TABLE = "{table.inbox}" }
//!
//! [service.mail]
//! kind = "s3"
//! buckets = ["inbox-mail"]
//!
//! [service.mail-queue]
//! kind = "sqs"
//! queues.inbox-mail = { visibility_timeout_seconds = 120 }
//!
//! [service.keys]
//! kind = "kms"
//! keys = ["inbox-mail"]
//!
//! [processes.worker]
//! argv = ["cargo", "run", "--bin", "worker"]
//! needs = ["store", "mail", "mail-queue", "keys"]
//! ```
//!
//! Every dynamodb service of a workspace shares one DynamoDB Local
//! container, the app [`Backend::Dynamodb`]. Every s3, sqs and kms service
//! shares one LocalStack container, the app [`Backend::Aws`]. Each backend
//! publishes one port of the workspace's port ledger, named after its app.
//! The host app [`PROVISION_APP`] creates the declared resources once the
//! backends are ready.
//!
//! A process that lists a service in `needs` starts after the service's
//! backend and the provisioning, and receives the variables that
//! [`variables`] returns: the AWS SDKs' endpoint, region and credential
//! variables, which point it at the backends, and the service's own `env`.
//! No value is a secret: the access keys are the literal `test`, which the
//! backends accept and real AWS refuses.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// The prefix of the names of mxv's own apps and ports. A manifest's
/// process, container or port may not start with it.
pub const RESERVED_PREFIX: &str = "mxv-";

/// The name of the host app that creates the declared resources.
pub const PROVISION_APP: &str = "mxv-provision";

/// The DynamoDB Local image, 3.3.1, pinned by its digest.
pub const DYNAMODB_IMAGE: &str = "amazon/dynamodb-local:3.3.1@sha256:ff89bd48ff32cd8d9be5fee8873b65b8854dc408f1afe881be6eb00247bc0dab";

/// The LocalStack community image, 4.14.0, pinned by its digest. Later
/// LocalStack releases need an account token, so an upgrade checks first
/// that the new tag still runs without one.
pub const LOCALSTACK_IMAGE: &str = "localstack/localstack:4.14.0@sha256:3ebc37595918b8accb852f8048fef2aff047d465167edd655528065b07bc364a";

/// The region that every process that needs a service receives.
pub const REGION: &str = "us-east-1";

/// The account id that LocalStack gives every resource.
pub const ACCOUNT: &str = "000000000000";

/// The address that every endpoint uses. The AWS SDKs choose S3's path
/// style only for an IP address, so no endpoint names `localhost`.
pub const HOST: &str = "127.0.0.1";

/// The variables that every process that needs a service receives, on top
/// of the endpoint of each kind it needs and the services' own `env`. The
/// access keys are placeholders that the backends accept. The last three
/// variables stop the SDKs from reading a real profile or the instance
/// metadata service.
pub const COMMON_VARIABLES: &[(&str, &str)] = &[
    ("AWS_REGION", REGION),
    ("AWS_ACCESS_KEY_ID", "test"),
    ("AWS_SECRET_ACCESS_KEY", "test"),
    ("AWS_EC2_METADATA_DISABLED", "true"),
    ("AWS_CONFIG_FILE", "/dev/null"),
    ("AWS_SHARED_CREDENTIALS_FILE", "/dev/null"),
];

/// The prefix of the AWS SDKs' variables. A process that needs a service
/// receives only the ones mxv sets, and its manifest may set none.
pub const AWS_PREFIX: &str = "AWS_";

/// What a service is: the AWS service that its backend serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    /// DynamoDB, which declares `tables`.
    Dynamodb,
    /// S3, which declares `buckets`.
    S3,
    /// SQS, which declares `queues`.
    Sqs,
    /// KMS, which declares `keys`.
    Kms,
}

impl ServiceKind {
    /// Every kind, in the order the manifest documents them.
    pub const ALL: [Self; 4] = [Self::Dynamodb, Self::S3, Self::Sqs, Self::Kms];

    /// Returns the kind as a manifest writes it, such as `dynamodb`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dynamodb => "dynamodb",
            Self::S3 => "s3",
            Self::Sqs => "sqs",
            Self::Kms => "kms",
        }
    }

    /// Returns the kind that a manifest writes as `text`, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// Returns the backend that serves the kind.
    #[must_use]
    pub const fn backend(self) -> Backend {
        match self {
            Self::Dynamodb => Backend::Dynamodb,
            Self::S3 | Self::Sqs | Self::Kms => Backend::Aws,
        }
    }

    /// Returns the AWS SDKs' endpoint variable of the kind, such as
    /// `AWS_ENDPOINT_URL_DYNAMODB`.
    #[must_use]
    pub const fn endpoint_variable(self) -> &'static str {
        match self {
            Self::Dynamodb => "AWS_ENDPOINT_URL_DYNAMODB",
            Self::S3 => "AWS_ENDPOINT_URL_S3",
            Self::Sqs => "AWS_ENDPOINT_URL_SQS",
            Self::Kms => "AWS_ENDPOINT_URL_KMS",
        }
    }

    /// Returns the key of the resources that a service of this kind
    /// declares, such as `tables`.
    #[must_use]
    pub const fn resources_key(self) -> &'static str {
        match self {
            Self::Dynamodb => "tables",
            Self::S3 => "buckets",
            Self::Sqs => "queues",
            Self::Kms => "keys",
        }
    }
}

impl fmt::Display for ServiceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A container that serves the services of a workspace. Its app and its
/// port in the port ledger share one name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Backend {
    /// DynamoDB Local, run as `-sharedDb -inMemory`, which serves every
    /// dynamodb service.
    Dynamodb,
    /// LocalStack, which serves every s3, sqs and kms service.
    Aws,
}

impl Backend {
    /// Returns the backend's app name, which is also its port's name.
    #[must_use]
    pub const fn app(self) -> &'static str {
        match self {
            Self::Dynamodb => "mxv-dynamodb",
            Self::Aws => "mxv-aws",
        }
    }

    /// Returns the image the backend runs.
    #[must_use]
    pub const fn image(self) -> &'static str {
        match self {
            Self::Dynamodb => DYNAMODB_IMAGE,
            Self::Aws => LOCALSTACK_IMAGE,
        }
    }

    /// Returns the port that the backend listens on inside its container.
    #[must_use]
    pub const fn container_port(self) -> u16 {
        match self {
            Self::Dynamodb => 8000,
            Self::Aws => 4566,
        }
    }

    /// Returns the backend's endpoint, `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn endpoint(port: u16) -> String {
        format!("http://{HOST}:{port}")
    }
}

/// The type of a key attribute of a table: a string, a number or binary
/// data, which DynamoDB writes `S`, `N` and `B`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttributeType {
    /// A string.
    S,
    /// A number.
    N,
    /// Binary data.
    B,
}

/// A key attribute of a table or an index, which a manifest writes as
/// `<name>:<type>`, such as `pk:S`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyAttribute {
    /// The attribute's name.
    pub name: String,
    /// The attribute's type.
    #[serde(rename = "type")]
    pub kind: AttributeType,
}

impl KeyAttribute {
    /// Returns the attribute that `text` writes as `<name>:<type>`, or
    /// `None` when it is not one. The name holds 1 to 255 bytes.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (name, kind) = text.rsplit_once(':')?;
        let kind = match kind {
            "S" => AttributeType::S,
            "N" => AttributeType::N,
            "B" => AttributeType::B,
            _ => return None,
        };
        (!name.is_empty() && name.len() <= 255).then(|| Self {
            name: name.to_owned(),
            kind,
        })
    }
}

/// A global secondary index of a table, which projects every attribute.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    /// The index's partition key, AWS's `HASH` key.
    pub hash: KeyAttribute,
    /// The index's sort key, AWS's `RANGE` key, or `None`.
    pub range: Option<KeyAttribute>,
}

/// A DynamoDB table, billed per request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// The partition key, AWS's `HASH` key.
    pub hash: KeyAttribute,
    /// The sort key, AWS's `RANGE` key, or `None`.
    pub range: Option<KeyAttribute>,
    /// The attribute that holds each item's expiry time, or `None` when
    /// items never expire.
    pub ttl: Option<String>,
    /// The global secondary indexes, by name.
    pub indexes: BTreeMap<String, Index>,
}

/// An SQS standard queue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    /// How long a received message stays hidden from other receivers, in
    /// seconds, or `None` for SQS's default of 30.
    pub visibility_timeout_seconds: Option<u32>,
}

/// The longest visibility timeout of a queue, twelve hours, in seconds.
pub const MOST_VISIBILITY_SECONDS: u32 = 43_200;

/// One `[service.<name>]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Service {
    /// The service's kind.
    pub kind: ServiceKind,
    /// The tables of a dynamodb service, by name.
    pub tables: BTreeMap<String, Table>,
    /// The buckets of an s3 service.
    pub buckets: BTreeSet<String>,
    /// The queues of an sqs service, by name.
    pub queues: BTreeMap<String, Queue>,
    /// The keys of a kms service, each a symmetric key with the alias
    /// `alias/<name>`.
    pub keys: BTreeSet<String>,
    /// The variables that each process that needs the service receives,
    /// whose values may name the service's endpoint and resources.
    pub env: BTreeMap<String, ServiceValue>,
}

impl Service {
    /// Returns an empty service of `kind`, with no resources and no `env`.
    #[must_use]
    pub const fn new(kind: ServiceKind) -> Self {
        Self {
            kind,
            tables: BTreeMap::new(),
            buckets: BTreeSet::new(),
            queues: BTreeMap::new(),
            keys: BTreeSet::new(),
            env: BTreeMap::new(),
        }
    }

    /// Reports whether the service declares the resource that
    /// `placeholder` names. A placeholder that names no resource, such as
    /// `{endpoint}`, is declared by every service.
    #[must_use]
    pub fn declares(&self, placeholder: &Placeholder) -> bool {
        match placeholder {
            Placeholder::Endpoint | Placeholder::Port | Placeholder::HostPort => true,
            Placeholder::Table(name) => self.tables.contains_key(name),
            Placeholder::Bucket(name) => self.buckets.contains(name),
            Placeholder::Queue(name) | Placeholder::QueueUrl(name) => {
                self.queues.contains_key(name)
            }
            Placeholder::Key(name) => self.keys.contains(name),
        }
    }

    /// Returns the variables of the service's `env`, each rendered with
    /// the host port `port` of its backend, or with `<port>` in its place
    /// when the workspace holds no port for the backend yet.
    #[must_use]
    pub fn rendered_env(&self, port: Option<u16>) -> BTreeMap<String, String> {
        self.env
            .iter()
            .map(|(variable, value)| (variable.clone(), value.render(port)))
            .collect()
    }
}

/// A placeholder of a service's `env` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placeholder {
    /// `{endpoint}`, the backend's `http://127.0.0.1:<port>`.
    Endpoint,
    /// `{port}`, the backend's host port.
    Port,
    /// `{host_port}`, `127.0.0.1:<port>` without a scheme, for a client
    /// such as DuckDB's httpfs that takes a host and a port.
    HostPort,
    /// `{table.<name>}`, the table's name.
    Table(String),
    /// `{bucket.<name>}`, the bucket's name.
    Bucket(String),
    /// `{queue.<name>}`, the queue's name.
    Queue(String),
    /// `{queue_url.<name>}`, the queue's URL.
    QueueUrl(String),
    /// `{key.<name>}`, the key's alias, `alias/<name>`.
    Key(String),
}

impl Placeholder {
    /// Returns the placeholder that `inside`, the text between the braces,
    /// names, or `None`.
    fn parse(inside: &str) -> Option<Self> {
        match inside {
            "endpoint" => return Some(Self::Endpoint),
            "port" => return Some(Self::Port),
            "host_port" => return Some(Self::HostPort),
            _ => {}
        }
        let (kind, name) = inside.split_once('.')?;
        if name.is_empty() {
            return None;
        }
        let name = name.to_owned();
        Some(match kind {
            "table" => Self::Table(name),
            "bucket" => Self::Bucket(name),
            "queue" => Self::Queue(name),
            "queue_url" => Self::QueueUrl(name),
            "key" => Self::Key(name),
            _ => return None,
        })
    }

    /// Returns the placeholder as a manifest writes it, such as
    /// `{table.inbox}`.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Endpoint => "{endpoint}".to_owned(),
            Self::Port => "{port}".to_owned(),
            Self::HostPort => "{host_port}".to_owned(),
            Self::Table(name) => format!("{{table.{name}}}"),
            Self::Bucket(name) => format!("{{bucket.{name}}}"),
            Self::Queue(name) => format!("{{queue.{name}}}"),
            Self::QueueUrl(name) => format!("{{queue_url.{name}}}"),
            Self::Key(name) => format!("{{key.{name}}}"),
        }
    }

    /// Returns the value of the placeholder with the backend's host port
    /// `port`, or with `<port>` in its place when it is `None`.
    fn render(&self, port: Option<u16>) -> String {
        let port = port.map_or_else(|| "<port>".to_owned(), |port| port.to_string());
        match self {
            Self::Endpoint => format!("http://{HOST}:{port}"),
            Self::Port => port,
            Self::HostPort => format!("{HOST}:{port}"),
            Self::Table(name) | Self::Bucket(name) | Self::Queue(name) => name.clone(),
            Self::QueueUrl(name) => format!("http://{HOST}:{port}/queue/{REGION}/{ACCOUNT}/{name}"),
            Self::Key(name) => format!("alias/{name}"),
        }
    }
}

/// The value of a service's `env` variable: literal text and
/// placeholders. A literal brace is written `{{` or `}}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceValue {
    /// The value's literal text and placeholders, in order.
    parts: Vec<ValuePart>,
}

/// One part of a [`ServiceValue`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum ValuePart {
    /// Literal text, with each doubled brace read as one brace.
    Text(String),
    /// A placeholder.
    Placeholder(Placeholder),
}

impl ServiceValue {
    /// Reads `text`, or returns the text of the first brace that is
    /// neither doubled nor part of a placeholder.
    ///
    /// # Errors
    ///
    /// Returns the text from the bad brace to the end of `text`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(['{', '}']) {
            literal.push_str(rest.get(..at).unwrap_or_default());
            let tail = rest.get(at..).unwrap_or_default();
            if let Some(after) = tail.strip_prefix("{{") {
                literal.push('{');
                rest = after;
            } else if let Some(after) = tail.strip_prefix("}}") {
                literal.push('}');
                rest = after;
            } else {
                let placeholder = tail
                    .strip_prefix('{')
                    .and_then(|inside| {
                        let end = inside.find('}')?;
                        Some((
                            Placeholder::parse(inside.get(..end)?)?,
                            inside.get(end + 1..)?,
                        ))
                    })
                    .ok_or_else(|| tail.to_owned())?;
                if !literal.is_empty() {
                    parts.push(ValuePart::Text(std::mem::take(&mut literal)));
                }
                parts.push(ValuePart::Placeholder(placeholder.0));
                rest = placeholder.1;
            }
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            parts.push(ValuePart::Text(literal));
        }
        Ok(Self { parts })
    }

    /// Returns the placeholders of the value, in order.
    pub fn placeholders(&self) -> impl Iterator<Item = &Placeholder> {
        self.parts.iter().filter_map(|part| match part {
            ValuePart::Placeholder(placeholder) => Some(placeholder),
            ValuePart::Text(_) => None,
        })
    }

    /// Returns the value with each placeholder replaced, for the backend's
    /// host port `port`, or with `<port>` in its place when it is `None`.
    #[must_use]
    pub fn render(&self, port: Option<u16>) -> String {
        self.parts
            .iter()
            .map(|part| match part {
                ValuePart::Text(text) => text.clone(),
                ValuePart::Placeholder(placeholder) => placeholder.render(port),
            })
            .collect()
    }
}

/// Returns the variables of a process that needs `services`, each with
/// its service, with the backends' host ports in `ports` by port name: the
/// [`COMMON_VARIABLES`], the endpoint variable of each kind, and each
/// service's own `env`. A backend whose port `ports` lacks renders as
/// `<port>`.
#[must_use]
pub(crate) fn variables<'a>(
    services: impl IntoIterator<Item = &'a Service>,
    ports: &BTreeMap<String, u16>,
) -> BTreeMap<String, String> {
    let mut variables: BTreeMap<String, String> = COMMON_VARIABLES
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    for service in services {
        let port = ports.get(service.kind.backend().app()).copied();
        variables.insert(
            service.kind.endpoint_variable().to_owned(),
            Placeholder::Endpoint.render(port),
        );
        variables.extend(service.rendered_env(port));
    }
    variables
}

/// Returns the backends that `services` need, in order.
pub(crate) fn backends<'a>(services: impl IntoIterator<Item = &'a Service>) -> BTreeSet<Backend> {
    services
        .into_iter()
        .map(|service| service.kind.backend())
        .collect()
}

/// Returns LocalStack's `SERVICES`, the kinds of `services` that it
/// serves, joined with commas in the order of [`ServiceKind::ALL`].
pub(crate) fn localstack_services<'a>(services: impl IntoIterator<Item = &'a Service>) -> String {
    let kinds: BTreeSet<ServiceKind> = services
        .into_iter()
        .map(|service| service.kind)
        .filter(|kind| kind.backend() == Backend::Aws)
        .collect();
    kinds
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Two services that declare one resource differently, or bind one
/// variable to different values for one process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Clash {
    /// What clashes: `table`, `queue` or `variable`.
    pub(crate) what: &'static str,
    /// The resource's or the variable's name, such as `inbox`.
    pub(crate) name: String,
    /// The first service.
    pub(crate) first: String,
    /// The second service.
    pub(crate) second: String,
}

/// Checks that no two of `services`, by name, declare one resource
/// differently. Every service of a workspace shares the backend of its
/// kind, so a table, bucket, queue or key that two services declare is one
/// resource, and two equal declarations are one.
///
/// # Errors
///
/// Returns the first [`Clash`].
pub(crate) fn check_resources<'a>(
    services: impl IntoIterator<Item = (&'a String, &'a Service)>,
) -> Result<(), Clash> {
    let mut tables: BTreeMap<&str, (&str, &Table)> = BTreeMap::new();
    let mut queues: BTreeMap<&str, (&str, &Queue)> = BTreeMap::new();
    for (name, service) in services {
        for (table, declared) in &service.tables {
            if let Some((first, other)) = tables.insert(table, (name, declared))
                && other != declared
            {
                return Err(Clash {
                    what: "table",
                    name: (*table).to_owned(),
                    first: first.to_owned(),
                    second: name.clone(),
                });
            }
        }
        for (queue, declared) in &service.queues {
            if let Some((first, other)) = queues.insert(queue, (name, declared))
                && other != declared
            {
                return Err(Clash {
                    what: "queue",
                    name: (*queue).to_owned(),
                    first: first.to_owned(),
                    second: name.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Checks that no two of `services`, which one process needs, bind one
/// variable to different values once rendered.
///
/// # Errors
///
/// Returns the first [`Clash`], whose name is the variable.
pub(crate) fn check_bindings<'a>(
    services: impl IntoIterator<Item = (&'a String, &'a Service)>,
) -> Result<(), Clash> {
    let mut bound: BTreeMap<&str, (&str, String)> = BTreeMap::new();
    for (name, service) in services {
        // Two services of one backend render a placeholder alike, so the
        // value with the backend's name in place of its port compares them.
        let backend = service.kind.backend().app();
        for (variable, value) in &service.env {
            let rendered = format!("{backend} {}", value.render(None));
            if let Some((first, other)) = bound.insert(variable, (name, rendered.clone()))
                && other != rendered
            {
                return Err(Clash {
                    what: "variable",
                    name: (*variable).to_owned(),
                    first: first.to_owned(),
                    second: name.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Reports whether `name` is a valid DynamoDB table or index name: 3 to
/// 255 letters, digits, `_`, `-` and `.`.
#[must_use]
pub(crate) fn is_table_name(name: &str) -> bool {
    (3..=255).contains(&name.len())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.".contains(character))
}

/// Reports whether `name` is a valid S3 bucket name: 3 to 63 lowercase
/// letters, digits, `-` and `.`, which starts and ends with a letter or a
/// digit and holds no `..`.
#[must_use]
pub(crate) fn is_bucket_name(name: &str) -> bool {
    let edge = |character: Option<char>| {
        character
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
    };
    (3..=63).contains(&name.len())
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || "-.".contains(character)
        })
        && edge(name.chars().next())
        && edge(name.chars().last())
        && !name.contains("..")
}

/// Reports whether `name` is a valid SQS standard queue name: 1 to 80
/// letters, digits, `-` and `_`.
#[must_use]
pub(crate) fn is_queue_name(name: &str) -> bool {
    (1..=80).contains(&name.len())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
}

/// Reports whether `name` is a valid KMS alias name after `alias/`: 1 to
/// 250 letters, digits, `/`, `_` and `-`, which does not start with
/// `aws/`, the prefix AWS keeps for its own keys.
#[must_use]
pub(crate) fn is_key_name(name: &str) -> bool {
    (1..=250).contains(&name.len())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/_-".contains(character))
        && !name.starts_with("aws/")
}
