use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use khive_runtime::{
    KhiveRuntime, NamespaceToken, RuntimeError, TelemetryCarrier, TelemetryFailurePosture,
    TelemetryPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

const PAGE_SIZE: i64 = 1_000;
const MAX_SCANNED: usize = 50_000;
const MAX_GROUPS: usize = 1_000;
const MAX_GROUP_KEY_BYTES: usize = 4_096;

fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidInput(message.into())
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params).map_err(|error| invalid(error.to_string()))
}

fn label(field: &str, value: &str) -> Result<(), RuntimeError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{field} must be nonempty, at most 256 UTF-8 bytes, and contain no control characters"
        )));
    }
    Ok(())
}

fn event_kind(field: &str, value: &str) -> Result<(), RuntimeError> {
    label(field, value)?;
    if value.contains('*') || value.chars().any(char::is_whitespace) {
        return Err(invalid(format!(
            "{field} must be an exact event kind without whitespace or '*'"
        )));
    }
    Ok(())
}

fn kind_filter(kinds: Option<Vec<String>>) -> Result<Option<HashSet<String>>, RuntimeError> {
    kinds
        .map(|kinds| {
            if kinds.is_empty() || kinds.len() > 100 {
                return Err(invalid("kinds must contain 1..100 exact event kinds"));
            }
            let mut filter = HashSet::with_capacity(kinds.len());
            for kind in kinds {
                event_kind("kinds entry", &kind)?;
                if !filter.insert(kind.clone()) {
                    return Err(invalid(format!("kinds contains duplicate entry {kind:?}")));
                }
            }
            Ok(filter)
        })
        .transpose()
}

fn matches_kind(record: &Value, kinds: &Option<HashSet<String>>) -> bool {
    kinds.as_ref().is_none_or(|kinds| {
        record
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kinds.contains(kind))
    })
}

fn cursor_kind(carrier: TelemetryCarrier) -> &'static str {
    match carrier {
        TelemetryCarrier::Durable => "log",
        TelemetryCarrier::Ephemeral => "none",
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelsParams {
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

pub(crate) fn channels(runtime: &KhiveRuntime, params: Value) -> Result<Value, RuntimeError> {
    let _: ChannelsParams = parse(params)?;
    let config = &runtime.config().telemetry;
    let channels: Vec<_> = config
        .channels
        .iter()
        .map(|channel| {
            json!({
                "kinds": channel.kinds,
                "carrier": channel.carrier,
                "failure_posture": channel.failure_posture,
                "cursor_kind": cursor_kind(channel.carrier),
            })
        })
        .collect();
    Ok(json!({
        "stream": config.stream,
        "default_carrier": config.default_carrier,
        "default_cursor_kind": cursor_kind(config.default_carrier),
        "channels": channels,
        "ephemeral_retention": "none",
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmitParams {
    kind: String,
    payload: Value,
    run_id: Option<String>,
    actor: Option<String>,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

pub(crate) async fn emit(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    if params.get("payload").is_none() {
        return Err(invalid("missing field `payload`"));
    }
    let params: EmitParams = parse(params)?;
    event_kind("kind", &params.kind)?;
    if let Some(run_id) = &params.run_id {
        label("run_id", run_id)?;
    }
    let caller = token.actor();
    let actor = if caller.kind == "actor" {
        caller.id.clone()
    } else {
        format!("{}:{}", caller.kind, caller.id)
    };
    if params.actor.as_ref().is_some_and(|value| value != &actor) {
        return Err(invalid("actor must match the authenticated caller actor"));
    }
    let config = &runtime.config().telemetry;
    let policy = config.policy_for_kind(&params.kind);
    if policy.carrier == TelemetryCarrier::Ephemeral {
        return Ok(json!({
            "accepted": true,
            "carrier": policy.carrier,
            "failure_posture": policy.failure_posture,
            "cursor_kind": "none",
            "dropped": true,
            "receipt_id": Uuid::new_v4(),
            "receipt_persisted": false,
            "ephemeral_retention": "none",
        }));
    }

    let mut record = json!({"kind": params.kind, "payload": params.payload, "actor": actor});
    if let Some(run_id) = params.run_id {
        record["run_id"] = run_id.into();
    }
    let appended = runtime
        .stream_append(
            token,
            &config.stream,
            &record,
            None,
            "observation",
            None,
            None,
            Some(false),
            None,
        )
        .await;
    append_response(&config.stream, policy, appended)
}

fn append_response(
    stream: &str,
    policy: TelemetryPolicy,
    appended: Result<Value, RuntimeError>,
) -> Result<Value, RuntimeError> {
    let appended = match appended {
        Ok(appended) => appended,
        Err(error)
            if policy.failure_posture == TelemetryFailurePosture::Gap
                && error.admission_failure_context().is_some() =>
        {
            return Ok(json!({
                "accepted": false,
                "carrier": policy.carrier,
                "failure_posture": policy.failure_posture,
                "cursor_kind": "log",
                "stream": stream,
                "dropped": true,
                "gap": true,
                "domain_disposition": "not_committed",
                "receipt_id": Uuid::new_v4(),
                "receipt_persisted": false,
                "reason": "write_admission_refused",
            }));
        }
        // Other failures can have unknown effects; never turn them into a claimed drop.
        Err(error) => return Err(error),
    };
    Ok(json!({
        "accepted": true,
        "carrier": policy.carrier,
        "failure_posture": policy.failure_posture,
        "cursor_kind": "log",
        "stream": stream,
        "dropped": false,
        "receipt_id": appended["id"],
        "receipt_persisted": true,
        "seq": appended["seq"],
        "created_at": appended["created_at"],
    }))
}

#[derive(Deserialize, Serialize)]
struct StreamEntry {
    seq: i64,
    id: String,
    record: Value,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct StreamPage {
    entries: Vec<StreamEntry>,
    head_seq: i64,
    next_after: Option<i64>,
}

async fn stream_page(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    stream: &str,
    after: i64,
    limit: i64,
) -> Result<StreamPage, RuntimeError> {
    serde_json::from_value(runtime.stream_read(token, stream, after, limit).await?)
        .map_err(|error| RuntimeError::Internal(format!("invalid stream page: {error}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    stream: String,
    since: Option<i64>,
    limit: Option<i64>,
    kinds: Option<Vec<String>>,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

pub(crate) async fn read(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: ReadParams = parse(params)?;
    let since = params.since.unwrap_or(0);
    let limit = params.limit.unwrap_or(100);
    if since < 0 || !(1..=PAGE_SIZE).contains(&limit) {
        return Err(invalid(
            "telemetry.read requires since >= 0 and limit in 1..1000",
        ));
    }
    let kinds = kind_filter(params.kinds)?;
    let page = stream_page(runtime, token, &params.stream, since, limit).await?;
    let next_cursor = page
        .entries
        .last()
        .map_or(since.max(page.head_seq), |entry| entry.seq);
    let events: Vec<_> = page
        .entries
        .into_iter()
        .filter(|entry| matches_kind(&entry.record, &kinds))
        .collect();
    Ok(json!({
        "stream": params.stream,
        "carrier": "durable",
        "cursor_kind": "log",
        "events": events,
        "next_cursor": next_cursor,
        "head_seq": page.head_seq,
        "has_more": page.next_after.is_some(),
        "ephemeral_retention": "none",
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowParams {
    since: String,
    until: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CountsParams {
    stream: String,
    window: WindowParams,
    group_by: Option<Vec<String>>,
    kinds: Option<Vec<String>>,
    #[serde(rename = "namespace")]
    _namespace: Option<String>,
}

fn timestamp(field: &str, value: &str) -> Result<DateTime<Utc>, RuntimeError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| {
            invalid(format!(
                "{field} must be an RFC3339 timestamp, e.g. 2026-01-01T00:00:00Z"
            ))
        })
}

fn dimensions(group_by: Option<Vec<String>>) -> Result<Vec<String>, RuntimeError> {
    let fields = group_by.unwrap_or_else(|| vec!["kind".into()]);
    if fields.is_empty() || fields.len() > 8 {
        return Err(invalid("group_by must contain 1..8 dotted record fields"));
    }
    let mut unique = HashSet::new();
    for field in &fields {
        if field.len() > 128
            || field.chars().any(char::is_control)
            || field
                .split('.')
                .any(|component| component.trim().is_empty())
        {
            return Err(invalid(format!(
                "invalid group_by field {field:?}: use a nonempty dotted path of at most 128 bytes"
            )));
        }
        if !unique.insert(field) {
            return Err(invalid(format!("duplicate group_by field {field:?}")));
        }
    }
    Ok(fields)
}

struct Rollup {
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    group_by: Vec<String>,
    kinds: Option<HashSet<String>>,
    head: Option<i64>,
    after: i64,
    scanned: usize,
    max_scanned: usize,
    total: u64,
    groups: BTreeMap<String, (Map<String, Value>, u64)>,
}

impl Rollup {
    fn consume(&mut self, page: StreamPage) -> Result<bool, RuntimeError> {
        let head = *self.head.get_or_insert(page.head_seq);
        if page.entries.is_empty() {
            return Ok(true);
        }
        for entry in page.entries {
            if entry.seq > head {
                return Ok(true);
            }
            if entry.seq <= self.after {
                return Err(RuntimeError::Internal(
                    "stream cursor did not advance".into(),
                ));
            }
            self.after = entry.seq;
            self.scanned += 1;
            if self.scanned > self.max_scanned {
                return Err(invalid(format!(
                    "telemetry.counts scan exceeds {} stream rows; no partial counts returned",
                    self.max_scanned
                )));
            }
            if entry.created_at < self.since
                || entry.created_at >= self.until
                || !matches_kind(&entry.record, &self.kinds)
            {
                continue;
            }
            let mut key = Map::new();
            for field in &self.group_by {
                let value = field
                    .split('.')
                    .try_fold(&entry.record, |record, part| record.get(part))
                    .unwrap_or(&Value::Null);
                if value.is_array() || value.is_object() {
                    return Err(invalid(format!(
                        "group_by field {field:?} must resolve to a scalar or null (stream sequence {})",
                        entry.seq
                    )));
                }
                key.insert(field.clone(), value.clone());
            }
            let encoded = serde_json::to_string(&key)
                .map_err(|error| RuntimeError::Internal(error.to_string()))?;
            if encoded.len() > MAX_GROUP_KEY_BYTES {
                return Err(invalid(format!(
                    "telemetry.counts group key exceeds {MAX_GROUP_KEY_BYTES} bytes"
                )));
            }
            if !self.groups.contains_key(&encoded) && self.groups.len() == MAX_GROUPS {
                return Err(invalid(format!(
                    "telemetry.counts exceeds {MAX_GROUPS} groups; no partial counts returned"
                )));
            }
            self.groups.entry(encoded).or_insert((key, 0)).1 += 1;
            self.total += 1;
        }
        Ok(self.after >= head || page.next_after.is_none())
    }
}

pub(crate) async fn counts(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let params: CountsParams = parse(params)?;
    let since = timestamp("window.since", &params.window.since)?;
    let until = params
        .window
        .until
        .as_deref()
        .map(|value| timestamp("window.until", value))
        .transpose()?
        .unwrap_or_else(Utc::now);
    if until <= since {
        return Err(invalid("window.until must be later than window.since"));
    }
    let mut rollup = Rollup {
        since,
        until,
        group_by: dimensions(params.group_by)?,
        kinds: kind_filter(params.kinds)?,
        head: None,
        after: 0,
        scanned: 0,
        max_scanned: MAX_SCANNED,
        total: 0,
        groups: BTreeMap::new(),
    };
    loop {
        let limit = (MAX_SCANNED - rollup.scanned + 1).min(PAGE_SIZE as usize) as i64;
        let page = stream_page(runtime, token, &params.stream, rollup.after, limit).await?;
        if rollup.consume(page)? {
            break;
        }
    }
    let rows: Vec<_> = rollup
        .groups
        .into_values()
        .map(|(key, count)| json!({"key": key, "count": count}))
        .collect();
    Ok(json!({
        "stream": params.stream,
        "rows": rows,
        "window": {"since": since, "until": until},
        "group_by": rollup.group_by,
        "total": rollup.total,
        "scanned": rollup.scanned,
        "head_seq": rollup.head,
        "complete": true,
    }))
}

#[cfg(test)]
mod tests;
