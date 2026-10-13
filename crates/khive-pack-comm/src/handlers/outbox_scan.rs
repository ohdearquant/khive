use chrono::{DateTime, Utc};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde_json::{json, Value};

use crate::params::HeartbeatParams;

/// Result of the bounded pending-row query, before any delivery is attempted.
pub enum OutboxScanOutcome {
    Success {
        pending_rows_seen: u64,
        pending_rows_limit: u32,
        oldest_pending_created_at: Option<DateTime<Utc>>,
    },
    Failure {
        error_message: String,
    },
}

/// One completed query attempt; skipped rate-limit pauses are not scans.
pub struct OutboxScan {
    pub completed_at: DateTime<Utc>,
    pub interval_secs: u64,
    pub outcome: OutboxScanOutcome,
}

/// Trusted Rust-only writer using the outbox runtime's authorized namespace.
/// Shares the guarded heartbeat write, without changing inbound poll facts.
/// This function is not registered as an MCP verb.
pub async fn record_outbox_scan(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    channel_kind: &str,
    channel_slug: &str,
    scan: OutboxScan,
) -> Result<Value, RuntimeError> {
    let (outcome, error_class, error_message) = match &scan.outcome {
        OutboxScanOutcome::Success { .. } => ("success", None, None),
        OutboxScanOutcome::Failure { error_message } => (
            "failure",
            Some("storage".into()),
            Some(error_message.clone()),
        ),
    };
    let params = HeartbeatParams {
        channel_kind: channel_kind.into(),
        channel_slug: channel_slug.into(),
        outcome: outcome.into(),
        poll_interval_secs: Some(scan.interval_secs),
        error_class,
        error_message,
        at: Some(scan.completed_at.to_rfc3339()),
    };
    super::quarantine_heartbeat::persist_heartbeat(runtime, token, params, Some(scan)).await
}

pub(super) fn update_properties(props: &mut Value, scan: OutboxScan) -> Result<(), RuntimeError> {
    let mut outbox = props
        .get("outbox")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let at = scan.completed_at.to_rfc3339();
    outbox["last_scan_completed_at"] = json!(at);
    outbox["scan_interval_secs"] = json!(scan.interval_secs);
    match scan.outcome {
        OutboxScanOutcome::Success {
            pending_rows_seen,
            pending_rows_limit,
            oldest_pending_created_at,
        } => {
            if pending_rows_limit == 0 || pending_rows_seen > u64::from(pending_rows_limit) {
                return Err(RuntimeError::InvalidInput(
                    "outbox scan: pending row count must fit its positive page limit".into(),
                ));
            }
            if pending_rows_seen == 0 && oldest_pending_created_at.is_some() {
                return Err(RuntimeError::InvalidInput(
                    "outbox scan: an empty page cannot have an oldest pending row".into(),
                ));
            }
            outbox["last_scan_succeeded"] = json!(true);
            outbox["last_success_at"] = json!(at);
            outbox["consecutive_failures"] = json!(0);
            outbox["pending_rows_seen"] = json!(pending_rows_seen);
            outbox["pending_rows_limit"] = json!(pending_rows_limit);
            outbox["oldest_pending_created_at"] =
                json!(oldest_pending_created_at.map(|time| time.to_rfc3339()));
        }
        OutboxScanOutcome::Failure { error_message } => {
            outbox["last_scan_succeeded"] = json!(false);
            outbox["last_failure_at"] = json!(at);
            let failures = outbox
                .get("consecutive_failures")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            outbox["consecutive_failures"] = json!(failures.saturating_add(1));
            outbox["last_error"] = json!({"class": "storage", "message": error_message, "at": at});
            // Last successful page observations remain explicitly dated by last_success_at.
        }
    }
    props["outbox"] = outbox;
    Ok(())
}
