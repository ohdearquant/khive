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
    let outbox = outbox_health_to_json(&props, as_of);
    let inbound_failures = if outbox.is_some() && props.get("last_poll_attempt_at").is_none() {
        Value::Null
    } else {
        props
            .get("consecutive_failures")
            .cloned()
            .unwrap_or(json!(0))
    };
    json!({
        "channel_kind": props.get("channel_kind").cloned().unwrap_or(Value::Null),
        "channel_slug": props.get("channel_slug").cloned().unwrap_or(Value::Null),
        "poll_interval_secs": poll_interval_secs,
        "stalled": stalled,
        "last_success_at": props.get("last_success_at").cloned().unwrap_or(Value::Null),
        "last_poll_attempt_at": props.get("last_poll_attempt_at").cloned().unwrap_or(Value::Null),
        "last_failure_at": props.get("last_failure_at").cloned().unwrap_or(Value::Null),
        "last_error": props.get("last_error").cloned().unwrap_or(Value::Null),
        "consecutive_failures": inbound_failures,
        "outbox": outbox,
        // Null on rows from a writer that does not report holds.
        "consecutive_held_polls": props.get("consecutive_held_polls").cloned().unwrap_or(Value::Null),
        "held_messages": props.get("held_messages").cloned().unwrap_or(Value::Null),
        "last_held_at": props.get("last_held_at").cloned().unwrap_or(Value::Null),
        "last_quarantined_after_hold_at": props
            .get("last_quarantined_after_hold_at")
            .cloned()
            .unwrap_or(Value::Null),
        // Null on rows from a writer that does not report notice delivery.
        "originals_not_retained": props.get("originals_not_retained").cloned().unwrap_or(Value::Null),
        "last_original_not_retained_at": props
            .get("last_original_not_retained_at")
            .cloned()
            .unwrap_or(Value::Null),
        "last_original_not_retained_reason": props
            .get("last_original_not_retained_reason")
            .cloned()
            .unwrap_or(Value::Null),
        "original_retention_state": props
            .get("original_retention_state")
            .cloned()
            .unwrap_or(Value::Null),
        "notices_failed": props.get("notices_failed").cloned().unwrap_or(Value::Null),
        "last_notice_failed_at": props
            .get("last_notice_failed_at")
            .cloned()
            .unwrap_or(Value::Null),
        "notice_config_error": props.get("notice_config_error").cloned().unwrap_or(Value::Null),
        "quarantined_count": quarantined_count,
    })
}

fn outbox_health_to_json(props: &Value, as_of: &DateTime<Utc>) -> Option<Value> {
    let outbox = props.get("outbox")?.as_object()?;
    let interval = outbox
        .get("scan_interval_secs")
        .and_then(Value::as_u64)
        .filter(|interval| *interval > 0);
    let completed_at = outbox
        .get("last_scan_completed_at")
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .filter(|at| at <= as_of);
    let stalled = interval.and_then(|interval| {
        let threshold = interval
            .checked_mul(STALLED_AFTER_INTERVALS)?
            .checked_mul(1_000)?;
        let elapsed = u64::try_from(
            as_of
                .signed_duration_since(completed_at?)
                .num_milliseconds(),
        )
        .ok()?;
        Some(elapsed > threshold)
    });
    let oldest_pending_age_secs = outbox
        .get("oldest_pending_created_at")
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .filter(|at| at <= as_of)
        .and_then(|at| u64::try_from(as_of.signed_duration_since(at).num_seconds()).ok());
    let mut projected = serde_json::Map::new();
    for field in [
        "last_scan_completed_at",
        "last_scan_succeeded",
        "last_success_at",
        "last_failure_at",
        "consecutive_failures",
        "last_error",
        "pending_rows_seen",
        "pending_rows_limit",
        "oldest_pending_created_at",
    ] {
        projected.insert(
            field.into(),
            outbox.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    projected.insert("scan_interval_secs".into(), json!(interval));
    projected.insert("stalled".into(), json!(stalled));
    projected.insert(
        "oldest_pending_age_secs".into(),
        json!(oldest_pending_age_secs),
    );
    Some(Value::Object(projected))
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
        "outbox": Value::Null,
        "consecutive_held_polls": Value::Null,
        "held_messages": Value::Null,
        "last_held_at": Value::Null,
        "last_quarantined_after_hold_at": Value::Null,
        "originals_not_retained": Value::Null,
        "last_original_not_retained_at": Value::Null,
        "last_original_not_retained_reason": Value::Null,
        "original_retention_state": Value::Null,
        "notices_failed": Value::Null,
        "last_notice_failed_at": Value::Null,
        "notice_config_error": Value::Null,
        "quarantined_count": quarantined_count,
    })
}

#[cfg(test)]
#[path = "outbox_health_tests.rs"]
mod outbox_health_tests;

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
/// plus in-flight background phase names. `rss_bytes` is current resident
/// memory, and `max_rss_bytes` is the lifetime peak. A failed current RSS
/// sample leaves CPU and peak intact; a failed getrusage leaves CPU and peak
/// null while the independent current reading may still be available.
/// `active_phases` is always present and empty when nothing is in flight — raw observations
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

    let current = khive_runtime::resource::current_rss_bytes();
    let usage = khive_runtime::process_resource_usage();
    let resource = resource_value(usage, current, json!(khive_runtime::active_phase_names()));

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

fn resource_value(
    usage: Option<khive_runtime::resource::ProcessResourceUsage>,
    current: Option<i64>,
    phases: Value,
) -> Value {
    json!({
        "cpu_us": usage.map(|u| u.cpu_us),
        "rss_bytes": current,
        "max_rss_bytes": usage.map(|u| u.max_rss_bytes),
        "active_phases": phases,
    })
}

#[cfg(test)]
mod resource_tests {
    use super::resource_value;
    use serde_json::json;

    #[test]
    fn health_resource_keeps_both_keys_and_independent_samples() {
        let usage = khive_runtime::resource::ProcessResourceUsage {
            cpu_us: 12,
            max_rss_bytes: 20,
        };
        let no_current = resource_value(Some(usage), None, json!([]));
        assert!(no_current.get("rss_bytes").expect("current key").is_null());
        assert_eq!(no_current["cpu_us"], 12);
        assert_eq!(no_current["max_rss_bytes"], 20);
        let no_usage = resource_value(None, Some(10), json!([]));
        assert_eq!(no_usage["rss_bytes"], 10);
        assert!(no_usage.get("max_rss_bytes").expect("peak key").is_null());
        assert!(no_usage["cpu_us"].is_null());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn health_live_resource_has_null_or_positive_current_and_positive_peak() {
        let current = khive_runtime::resource::current_rss_bytes();
        let usage = khive_runtime::process_resource_usage().expect("getrusage fixture");
        let result = resource_value(Some(usage), current, json!([]));
        assert!(result.get("rss_bytes").is_some());
        assert!(!matches!(current, Some(value) if value <= 0));
        assert!(result.get("max_rss_bytes").is_some());
        assert!(usage.max_rss_bytes > 0);
    }
}
