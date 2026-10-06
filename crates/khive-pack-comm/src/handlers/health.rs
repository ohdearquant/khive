use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::{Note, NoteFilter};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};

/// A channel is schedule-stale after three complete nominal poll intervals.
/// The grace avoids flagging a live poller during ordinary tick and I/O jitter.
const STALLED_AFTER_INTERVALS: u64 = 3;

pub(super) fn channel_stalled(props: &Value, as_of: &DateTime<Utc>) -> Option<bool> {
    // A known failure enters intentional exponential backoff, so the nominal
    // cadence cannot distinguish an overdue poll from a scheduled retry.
    let consecutive_failures = props.get("consecutive_failures").and_then(Value::as_u64)?;
    if consecutive_failures > 0 {
        return None;
    }
    let poll_interval_secs = props.get("poll_interval_secs")?.as_u64()?;
    if poll_interval_secs == 0 {
        return None;
    }
    let stall_after_millis = poll_interval_secs
        .checked_mul(STALLED_AFTER_INTERVALS)?
        .checked_mul(1_000)?;
    let last_poll_attempt =
        DateTime::parse_from_rfc3339(props.get("last_poll_attempt_at")?.as_str()?)
            .ok()?
            .with_timezone(&Utc);
    let elapsed_millis = u64::try_from(
        as_of
            .signed_duration_since(last_poll_attempt)
            .num_milliseconds(),
    )
    .ok()?;
    Some(elapsed_millis > stall_after_millis)
}

/// Project a persisted `channel_health` note into the `comm.health()` channel
/// entry shape. Missing or malformed cadence/timestamp fields (including rows
/// written before #1472) produce `stalled: null` rather than pretending the
/// channel is current.
fn channel_health_to_json(note: &Note, as_of: &DateTime<Utc>, quarantined_count: u64) -> Value {
    let props = note.properties.clone().unwrap_or_else(|| json!({}));
    let poll_interval_secs = props
        .get("poll_interval_secs")
        .and_then(Value::as_u64)
        .filter(|interval| *interval > 0);
    let stalled = channel_stalled(&props, as_of);
    json!({
        "channel_kind": props.get("channel_kind").cloned().unwrap_or(Value::Null),
        "channel_slug": props.get("channel_slug").cloned().unwrap_or(Value::Null),
        "poll_interval_secs": poll_interval_secs,
        "stalled": stalled,
        "last_success_at": props.get("last_success_at").cloned().unwrap_or(Value::Null),
        "last_poll_attempt_at": props.get("last_poll_attempt_at").cloned().unwrap_or(Value::Null),
        "last_failure_at": props.get("last_failure_at").cloned().unwrap_or(Value::Null),
        "last_error": props.get("last_error").cloned().unwrap_or(Value::Null),
        "consecutive_failures": props.get("consecutive_failures").cloned().unwrap_or(json!(0)),
        "quarantined_count": quarantined_count,
    })
}

#[derive(Default)]
struct QuarantineCounts {
    total: u64,
    unattributed: u64,
    by_channel: HashMap<(String, String), u64>,
}

/// Count live quarantine dispositions in one authorized namespace. The
/// generic marker accepts both the string spelling emitted by today's channel
/// envelope metadata and a JSON boolean so future adapters do not need an
/// email-specific encoding. Rows lacking a complete transport identity remain
/// visible in `unattributed` and are never guessed onto a heartbeat.
async fn load_quarantine_counts(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
) -> Result<QuarantineCounts, RuntimeError> {
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("quarantine_counts_select").into(),
            params: vec![SqlValue::Text(token.namespace().as_str().to_string())],
            label: Some("comm_health_quarantined_counts".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;

    let mut counts = QuarantineCounts::default();
    for row in rows {
        let count = match row.get("quarantined_count") {
            Some(SqlValue::Integer(count)) if *count >= 0 => *count as u64,
            other => {
                return Err(RuntimeError::Internal(format!(
                    "comm.health: storage returned an invalid quarantine count: {other:?}"
                )))
            }
        };
        counts.total = counts.total.checked_add(count).ok_or_else(|| {
            RuntimeError::Internal("comm.health: quarantine count overflow".into())
        })?;

        let kind = row.get("channel_kind").and_then(|value| match value {
            SqlValue::Text(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            _ => None,
        });
        let slug = row.get("channel_slug").and_then(|value| match value {
            SqlValue::Text(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            _ => None,
        });
        if let (Some(kind), Some(slug)) = (kind, slug) {
            let channel_count = counts.by_channel.entry((kind, slug)).or_default();
            *channel_count = channel_count.checked_add(count).ok_or_else(|| {
                RuntimeError::Internal("comm.health: per-channel quarantine count overflow".into())
            })?;
        } else {
            counts.unattributed = counts.unattributed.checked_add(count).ok_or_else(|| {
                RuntimeError::Internal("comm.health: unattributed quarantine count overflow".into())
            })?;
        }
    }

    Ok(counts)
}

fn quarantine_only_channel_to_json(
    channel_kind: String,
    channel_slug: String,
    quarantined_count: u64,
) -> Value {
    json!({
        "channel_kind": channel_kind,
        "channel_slug": channel_slug,
        "poll_interval_secs": Value::Null,
        "stalled": Value::Null,
        "last_success_at": Value::Null,
        "last_poll_attempt_at": Value::Null,
        "last_failure_at": Value::Null,
        "last_error": Value::Null,
        "consecutive_failures": Value::Null,
        "quarantined_count": quarantined_count,
    })
}

/// `health` — read-only per-channel health snapshot (khive #606). Reads
/// `channel_health` rows from `token.namespace()` (khive #877 namespace
/// scoping), then unions exact channel identities found on live quarantine
/// notes in that same namespace (khive #1383). The additive `stalled`
/// schedule fact is deliberately narrower than a computed `healthy: bool`;
/// quarantine counts are an independent backlog observation, and overall
/// health judgment still belongs to the caller. See
/// crates/khive-pack-comm/docs/api/channel-health.md#handlersrshandle_health
/// for the `role`/`namespace`/`resource` field semantics (ADR-103 Stage 1).
///
/// `resource` is a process-level self-report of this process's own CPU/RSS
/// (via `getrusage`) plus in-flight background phase names. `cpu_us`/
/// `rss_bytes` are `null` only if `getrusage` is unavailable; `active_phases`
/// is always present and empty when nothing is in flight — raw observations
/// only, same "no computed healthy bool" rule as the rest of this verb.
pub(crate) async fn handle_health(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let has_args = match params.as_object() {
        Some(obj) => !obj.is_empty(),
        None => !params.is_null(),
    };
    if has_args {
        return Err(RuntimeError::InvalidInput(
            "health: takes no arguments".into(),
        ));
    }

    let store = runtime.notes(token)?;
    const MAX_CHANNELS: usize = 200;
    let filter = NoteFilter {
        kind: Some("channel_health".to_string()),
        ..Default::default()
    };
    let page = store
        .query_notes_filtered_count_free(
            token.namespace().as_str(),
            &filter,
            PageRequest {
                limit: MAX_CHANNELS as u32,
                offset: 0,
            },
        )
        .await?;

    if page.items.len() == MAX_CHANNELS {
        tracing::debug!(
            max_channels = MAX_CHANNELS,
            "comm.health: channel_health row count hit the page limit; \
             heartbeat rows take the full response budget and later rows are omitted"
        );
    }

    let now = Utc::now();
    let mut quarantine_counts = load_quarantine_counts(runtime, token).await?;
    let has_heartbeat_state = !page.items.is_empty();
    let mut channels: Vec<Value> = page
        .items
        .iter()
        .map(|note| {
            let key = note.properties.as_ref().and_then(|props| {
                Some((
                    props.get("channel_kind")?.as_str()?.trim().to_string(),
                    props.get("channel_slug")?.as_str()?.trim().to_string(),
                ))
            });
            let quarantined_count = key
                .and_then(|key| quarantine_counts.by_channel.remove(&key))
                .unwrap_or(0);
            channel_health_to_json(note, &now, quarantined_count)
        })
        .collect();

    // A message namespace can intentionally differ from the operational
    // heartbeat namespace. Preserve those exact channel identities as
    // quarantine-only entries instead of hiding them merely because this
    // scoped read has no heartbeat row. Heartbeats consume the bounded response
    // budget first: if their page is full, an identity whose heartbeat exists
    // beyond that page is omitted rather than misclassified as quarantine-only.
    // When heartbeat rows leave capacity, exact quarantine-only identities fill
    // it in deterministic lexical order without changing heartbeat order.
    let mut quarantine_only: Vec<_> = quarantine_counts.by_channel.into_iter().collect();
    quarantine_only.sort_by(|(left, _), (right, _)| left.cmp(right));
    let quarantine_capacity = MAX_CHANNELS.saturating_sub(channels.len());
    let omitted_quarantine_channels = quarantine_only.len().saturating_sub(quarantine_capacity);
    channels.extend(quarantine_only.into_iter().take(quarantine_capacity).map(
        |((channel_kind, channel_slug), count)| {
            quarantine_only_channel_to_json(channel_kind, channel_slug, count)
        },
    ));
    if omitted_quarantine_channels > 0 {
        tracing::debug!(
            max_channels = MAX_CHANNELS,
            omitted_quarantine_channels,
            "comm.health: quarantine-only channel identities exceeded the capacity left after \
             heartbeat rows; later lexical identities are omitted"
        );
    }
    let as_of = now.to_rfc3339();

    let (role, source) = if !has_heartbeat_state {
        ("client", None::<&str>)
    } else {
        ("daemon", Some("daemon-heartbeat"))
    };

    let usage = khive_runtime::process_resource_usage();
    let resource = json!({
        "cpu_us": usage.map(|u| u.cpu_us),
        "rss_bytes": usage.map(|u| u.rss_bytes),
        "active_phases": khive_runtime::active_phase_names(),
    });

    Ok(json!({
        "role": role,
        "source": source,
        "as_of": as_of,
        "namespace": token.namespace().as_str(),
        "quarantined_count": quarantine_counts.total,
        "unattributed_quarantined_count": quarantine_counts.unattributed,
        "channels": channels,
        "resource": resource,
    }))
}
