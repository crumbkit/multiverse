//! The `[service.<name>]` tables of a manifest and the `needs` of its
//! processes, as [`crate::service`] describes them.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use toml::Spanned;

use super::{Errors, ManifestError, ManifestProblem, check_variable};
use crate::name::is_unit_name;
use crate::phase_plan::PHASE_SETS;
use crate::service::{
    AWS_PREFIX, Index, KeyAttribute, MOST_VISIBILITY_SECONDS, Queue, Service, ServiceKind,
    ServiceValue, Table, check_bindings, check_resources, is_bucket_name, is_key_name,
    is_queue_name, is_table_name,
};

#[cfg(test)]
mod tests;

/// One service as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawService {
    /// The kind.
    kind: Spanned<String>,
    /// The tables of a dynamodb service.
    tables: Option<Spanned<BTreeMap<Spanned<String>, RawTable>>>,
    /// The buckets of an s3 service.
    buckets: Option<Spanned<Vec<Spanned<String>>>>,
    /// The queues of an sqs service.
    queues: Option<Spanned<BTreeMap<Spanned<String>, RawQueue>>>,
    /// The keys of a kms service.
    keys: Option<Spanned<Vec<Spanned<String>>>>,
    /// The variables that each process that needs the service receives.
    #[serde(default)]
    env: BTreeMap<Spanned<String>, Spanned<String>>,
}

/// One table as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    /// The partition key, `<name>:<type>`.
    hash: Spanned<String>,
    /// The sort key, `<name>:<type>`.
    range: Option<Spanned<String>>,
    /// The expiry attribute.
    ttl: Option<Spanned<String>>,
    /// The global secondary indexes.
    #[serde(default)]
    indexes: BTreeMap<Spanned<String>, RawIndex>,
}

/// One index as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    /// The partition key, `<name>:<type>`.
    hash: Spanned<String>,
    /// The sort key, `<name>:<type>`.
    range: Option<Spanned<String>>,
}

/// One queue as TOML holds it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQueue {
    /// The visibility timeout in seconds.
    visibility_timeout_seconds: Option<Spanned<u64>>,
}

/// Checks every service of `raw`, by name, and that no two declare one
/// resource differently, and returns them.
pub(super) fn check_services(
    errors: &Errors<'_>,
    raw: &BTreeMap<Spanned<String>, RawService>,
) -> Result<BTreeMap<String, Service>, ManifestError> {
    let mut services = BTreeMap::new();
    for (name, raw_service) in raw {
        services.insert(
            name.get_ref().clone(),
            check_service(errors, name, raw_service)?,
        );
    }
    check_resources(&services).map_err(|clash| {
        let span = raw
            .keys()
            .find(|name| name.get_ref() == &clash.second)
            .map(Spanned::span);
        errors.at(
            span.as_ref(),
            ManifestProblem::ServiceClash {
                name: clash.name.clone(),
                message: format!(
                    "the services `{}` and `{}` declare the {what} `{}` differently, and a workspace's backend holds one {what} of each name",
                    clash.first,
                    clash.second,
                    clash.name,
                    what = clash.what
                ),
            },
        )
    })?;
    Ok(services)
}

/// Checks one raw service and returns it.
fn check_service(
    errors: &Errors<'_>,
    name: &Spanned<String>,
    raw: &RawService,
) -> Result<Service, ManifestError> {
    let service = name.get_ref();
    if !is_unit_name(service) {
        return Err(errors.invalid(
            &name.span(),
            format!(
                "`{service}` is not a service name, which holds 1 to 64 letters, digits, '-' and '_'"
            ),
        ));
    }
    let kind = ServiceKind::parse(raw.kind.get_ref()).ok_or_else(|| {
        errors.invalid(
            &raw.kind.span(),
            format!(
                "the kind `{}` of the service `{service}` is not one of dynamodb, s3, sqs and kms",
                raw.kind.get_ref()
            ),
        )
    })?;
    let present = [
        ("tables", raw.tables.as_ref().map(Spanned::span)),
        ("buckets", raw.buckets.as_ref().map(Spanned::span)),
        ("queues", raw.queues.as_ref().map(Spanned::span)),
        ("keys", raw.keys.as_ref().map(Spanned::span)),
    ];
    for (key, span) in present {
        if let Some(span) = span
            && key != kind.resources_key()
        {
            return Err(errors.invalid(
                &span,
                format!(
                    "the service `{service}` of kind {kind} declares `{key}`, and a {kind} service declares only `{}`",
                    kind.resources_key()
                ),
            ));
        }
    }
    let mut checked = Service::new(kind);
    if let Some(tables) = &raw.tables {
        for (table, raw_table) in tables.get_ref() {
            checked.tables.insert(
                table.get_ref().clone(),
                check_table(errors, service, table, raw_table)?,
            );
        }
    }
    checked.buckets = names(
        errors,
        service,
        "bucket",
        raw.buckets.as_ref(),
        is_bucket_name,
        "3 to 63 lowercase letters, digits, '-' and '.', starting and ending with a letter or a digit",
    )?;
    checked.keys = names(
        errors,
        service,
        "key",
        raw.keys.as_ref(),
        is_key_name,
        "1 to 250 letters, digits, '/', '_' and '-', not starting with 'aws/'",
    )?;
    if let Some(queues) = &raw.queues {
        for (queue, raw_queue) in queues.get_ref() {
            checked.queues.insert(
                queue.get_ref().clone(),
                check_queue(errors, service, queue, raw_queue)?,
            );
        }
    }
    checked.env = check_service_env(errors, service, &checked, &raw.env)?;
    Ok(checked)
}

/// Checks the names of a service's buckets or keys, which `valid` accepts
/// and `rule` describes, and returns them.
fn names(
    errors: &Errors<'_>,
    service: &str,
    what: &str,
    raw: Option<&Spanned<Vec<Spanned<String>>>>,
    valid: fn(&str) -> bool,
    rule: &str,
) -> Result<BTreeSet<String>, ManifestError> {
    let mut names = BTreeSet::new();
    for name in raw.map(Spanned::get_ref).into_iter().flatten() {
        if !valid(name.get_ref()) {
            return Err(errors.invalid(
                &name.span(),
                format!(
                    "the {what} `{}` of the service `{service}` is not a {what} name, which holds {rule}",
                    name.get_ref()
                ),
            ));
        }
        if !names.insert(name.get_ref().clone()) {
            return Err(errors.invalid(
                &name.span(),
                format!(
                    "the service `{service}` declares the {what} `{}` twice",
                    name.get_ref()
                ),
            ));
        }
    }
    Ok(names)
}

/// Returns the key attribute that `raw` writes, of the table or index
/// `owner` of the service `service`.
fn attribute(
    errors: &Errors<'_>,
    service: &str,
    owner: &str,
    raw: &Spanned<String>,
) -> Result<KeyAttribute, ManifestError> {
    KeyAttribute::parse(raw.get_ref()).ok_or_else(|| {
        errors.invalid(
            &raw.span(),
            format!(
                "the key `{}` of `{owner}` in the service `{service}` is not `<attribute>:<type>`, whose type is S, N or B, such as `pk:S`",
                raw.get_ref()
            ),
        )
    })
}

/// Checks one raw table of the service `service` and returns it.
fn check_table(
    errors: &Errors<'_>,
    service: &str,
    name: &Spanned<String>,
    raw: &RawTable,
) -> Result<Table, ManifestError> {
    let table = name.get_ref();
    if !is_table_name(table) {
        return Err(errors.invalid(
            &name.span(),
            format!(
                "the table `{table}` of the service `{service}` is not a table name, which holds 3 to 255 letters, digits, '_', '-' and '.'"
            ),
        ));
    }
    let mut types: BTreeMap<String, crate::service::AttributeType> = BTreeMap::new();
    let mut typed = |key: KeyAttribute, span: &Spanned<String>| match types
        .insert(key.name.clone(), key.kind)
    {
        Some(other) if other != key.kind => Err(errors.invalid(
            &span.span(),
            format!(
                "the table `{table}` of the service `{service}` gives the attribute `{}` two types",
                key.name
            ),
        )),
        _ => Ok(key),
    };
    let hash = typed(attribute(errors, service, table, &raw.hash)?, &raw.hash)?;
    let range = match &raw.range {
        Some(range) => Some(typed(attribute(errors, service, table, range)?, range)?),
        None => None,
    };
    let mut indexes = BTreeMap::new();
    for (index, raw_index) in &raw.indexes {
        if !is_table_name(index.get_ref()) {
            return Err(errors.invalid(
                &index.span(),
                format!(
                    "the index `{}` of the table `{table}` is not an index name, which holds 3 to 255 letters, digits, '_', '-' and '.'",
                    index.get_ref()
                ),
            ));
        }
        let hash = typed(
            attribute(errors, service, index.get_ref(), &raw_index.hash)?,
            &raw_index.hash,
        )?;
        let range = match &raw_index.range {
            Some(range) => Some(typed(
                attribute(errors, service, index.get_ref(), range)?,
                range,
            )?),
            None => None,
        };
        indexes.insert(index.get_ref().clone(), Index { hash, range });
    }
    let ttl = match &raw.ttl {
        Some(ttl) if ttl.get_ref().is_empty() || ttl.get_ref().len() > 255 => {
            return Err(errors.invalid(
                &ttl.span(),
                format!("the ttl attribute of the table `{table}` holds 1 to 255 bytes"),
            ));
        }
        Some(ttl) => Some(ttl.get_ref().clone()),
        None => None,
    };
    Ok(Table {
        hash,
        range,
        ttl,
        indexes,
    })
}

/// Checks one raw queue of the service `service` and returns it.
fn check_queue(
    errors: &Errors<'_>,
    service: &str,
    name: &Spanned<String>,
    raw: &RawQueue,
) -> Result<Queue, ManifestError> {
    if !is_queue_name(name.get_ref()) {
        return Err(errors.invalid(
            &name.span(),
            format!(
                "the queue `{}` of the service `{service}` is not a queue name, which holds 1 to 80 letters, digits, '-' and '_'",
                name.get_ref()
            ),
        ));
    }
    let visibility_timeout_seconds = match &raw.visibility_timeout_seconds {
        Some(seconds) => Some(
            u32::try_from(*seconds.get_ref())
                .ok()
                .filter(|seconds| *seconds <= MOST_VISIBILITY_SECONDS)
                .ok_or_else(|| {
                    errors.invalid(
                        &seconds.span(),
                        format!(
                            "visibility_timeout_seconds of the queue `{}` is not between 0 and {MOST_VISIBILITY_SECONDS}",
                            name.get_ref()
                        ),
                    )
                })?,
        ),
        None => None,
    };
    Ok(Queue {
        visibility_timeout_seconds,
    })
}

/// Checks the `env` of the service `service`, whose resources `checked`
/// holds: each variable's name, which may not be one that mxv or phase
/// sets, and each value's braces and the resources its placeholders name.
fn check_service_env(
    errors: &Errors<'_>,
    service: &str,
    checked: &Service,
    raw: &BTreeMap<Spanned<String>, Spanned<String>>,
) -> Result<BTreeMap<String, ServiceValue>, ManifestError> {
    let mut env = BTreeMap::new();
    for (variable, value) in raw {
        check_variable(errors, service, variable)?;
        let name = variable.get_ref();
        if name.starts_with(AWS_PREFIX) || PHASE_SETS.contains(&name.as_str()) {
            return Err(errors.invalid(
                &variable.span(),
                format!(
                    "the service `{service}` sets {name}, which mxv sets itself for every process that needs a service"
                ),
            ));
        }
        let parsed = ServiceValue::parse(value.get_ref()).map_err(|bad| {
            errors.invalid(
                &value.span(),
                format!(
                    "the value of `{name}` in the service `{service}` has a brace at `{bad}` that is not a placeholder such as `{{endpoint}}`, `{{port}}`, `{{host_port}}` or `{{table.<name>}}`; write `{{{{` or `}}}}` for a literal brace"
                ),
            )
        })?;
        if let Some(placeholder) = parsed
            .placeholders()
            .find(|placeholder| !checked.declares(placeholder))
        {
            return Err(errors.invalid(
                &value.span(),
                format!(
                    "the value of `{name}` in the service `{service}` names {}, which the service does not declare",
                    placeholder.text()
                ),
            ));
        }
        env.insert(name.clone(), parsed);
    }
    Ok(env)
}

/// Checks the `needs` of the process `process`: each names a service of
/// `services` or of `joined`, once, and no variable that the process sets
/// itself in `own` starts with `AWS_` or is one that a needed service
/// binds. Two needed services may not bind one variable to different
/// values. It returns the service names.
pub(super) fn check_needs(
    errors: &Errors<'_>,
    process: &str,
    raw: &[Spanned<String>],
    own: &[&Spanned<String>],
    services: &BTreeMap<String, Service>,
    joined: &[String],
) -> Result<Vec<String>, ManifestError> {
    let mut needs: Vec<String> = Vec::new();
    for service in raw {
        let name = service.get_ref();
        if !services.contains_key(name) && !joined.contains(name) {
            return Err(errors.invalid(
                &service.span(),
                format!(
                    "`{process}` needs the service `{name}`, which the manifest does not declare"
                ),
            ));
        }
        if needs.contains(name) {
            return Err(errors.invalid(
                &service.span(),
                format!("`{process}` needs the service `{name}` twice"),
            ));
        }
        needs.push(name.clone());
    }
    let Some(first) = raw.first() else {
        return Ok(needs);
    };
    let needed: Vec<(&String, &Service)> = needs
        .iter()
        .filter_map(|name| services.get_key_value(name))
        .collect();
    for variable in own {
        let name = variable.get_ref();
        if name.starts_with(AWS_PREFIX) {
            return Err(errors.invalid(
                &variable.span(),
                format!(
                    "the process `{process}` sets {name}, and a process that needs a service receives only the AWS_ variables that mxv sets"
                ),
            ));
        }
        if let Some((service, _)) = needed
            .iter()
            .find(|(_, service)| service.env.contains_key(name))
        {
            return Err(errors.invalid(
                &variable.span(),
                format!(
                    "the process `{process}` sets {name}, which the service `{service}` that it needs also sets"
                ),
            ));
        }
    }
    check_bindings(needed).map_err(|clash| {
        errors.at(
            Some(&first.span()),
            ManifestProblem::ServiceClash {
                name: clash.name.clone(),
                message: format!(
                    "`{process}` needs the services `{}` and `{}`, which set the variable {} to different values",
                    clash.first, clash.second, clash.name
                ),
            },
        )
    })?;
    Ok(needs)
}
