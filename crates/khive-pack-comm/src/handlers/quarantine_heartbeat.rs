//! Channel heartbeat writes and expired-quarantine maintenance.

use std::future::Future;

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::note::Note;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{AttachmentSubstrate, ContentRef};

use crate::params::{
    deser, CleanupExpiredQuarantineParams, HeartbeatParams, QuarantineCleanupMode,
};

#[cfg(test)]
use super::race_seam;
use super::validation::parse_supplied_timestamp;

/// Detach only the main-backend original owned by this already hard-deleted
/// legacy note. The conditional DELETE rejects a competing replacement of the
/// role after the owner read; it never scans for unrelated ownerless rows.
pub(super) async fn detach_deleted_legacy_original(
    runtime: &KhiveRuntime,
    id: Uuid,
    expected_ref: Option<&str>,
) -> Result<bool, RuntimeError> {
    let core = runtime.core();
    let attachments = core.attachments()?;
    let Some(owner) = attachments
        .get_attachment(id, "quarantine-original")
        .await?
    else {
        return Ok(false);
    };
    let expected = expected_ref.ok_or_else(|| {
        RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} has an original owner but no original ref"
        ))
    })?;
    let expected = ContentRef::from_hex(expected.to_string()).map_err(|error| {
        RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} has an invalid original ref: {error}"
        ))
    })?;
    if owner.substrate != AttachmentSubstrate::Note || owner.content_ref != expected {
        return Err(RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} has a mismatched original owner"
        )));
    }
    let detached = attachments
        .delete_attachment_if(
            id,
            "quarantine-original",
            AttachmentSubstrate::Note,
            &expected,
        )
        .await?;
    if !detached {
        return Err(RuntimeError::Internal(format!(
            "cleanup_expired_quarantine: deleted note {id} original owner changed during cleanup"
        )));
    }
    Ok(true)
}

fn delete_note_error_committed(error: &RuntimeError, id: Uuid) -> bool {
    let RuntimeError::Khive(domain) = error.refusal_source() else {
        return false;
    };
    let Some(details) = domain.details() else {
        return false;
    };
    details.get("reason") == Some("post_commit_degraded")
        && details.get("operation") == Some("delete_note")
        && details
            .get("record_id")
            .is_some_and(|stored| Uuid::parse_str(stored).ok() == Some(id))
        && details.get("committed") == Some("true")
}

pub(super) async fn note_deleted_after_attempt<F>(
    delete_result: &Result<bool, RuntimeError>,
    id: Uuid,
    probe_absent: F,
) -> Result<bool, RuntimeError>
where
    F: Future<Output = Result<bool, RuntimeError>>,
{
    match delete_result {
        Ok(deleted) => Ok(*deleted),
        Err(error) if delete_note_error_committed(error, id) => Ok(true),
        Err(_) => probe_absent.await,
    }
}

/// Internal channel-poller maintenance. One bounded page per tick ensures an
/// empty poll still makes progress without monopolizing the writer. The token
/// carries the ingest namespace explicitly; heartbeat rows use a different
/// namespace and must not be used as the maintenance scope.
pub(crate) async fn handle_cleanup_expired_quarantine(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
    quarantine_retention: std::time::Duration,
) -> Result<Value, RuntimeError> {
    let p: CleanupExpiredQuarantineParams = deser(params)?;
    if p.channel_kind.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "cleanup_expired_quarantine: channel_kind must be nonblank".into(),
        ));
    }
    match p.mode {
        QuarantineCleanupMode::Channel if p.channel_slug.trim().is_empty() => {
            return Err(RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: channel_kind and channel_slug must be nonblank".into(),
            ));
        }
        QuarantineCleanupMode::LegacySlugless if !p.channel_slug.is_empty() => {
            return Err(RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: legacy_slugless requires an empty channel_slug".into(),
            ));
        }
        _ => {}
    }
    let as_of = p
        .as_of_micros
        .unwrap_or_else(|| Utc::now().timestamp_micros());
    let legacy_cutoff = if p.mode == QuarantineCleanupMode::LegacySlugless {
        let retention_us = i64::try_from(quarantine_retention.as_micros()).map_err(|_| {
            RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: retention exceeds i64 microseconds".into(),
            )
        })?;
        Some(as_of.checked_sub(retention_us).ok_or_else(|| {
            RuntimeError::InvalidInput(
                "cleanup_expired_quarantine: retention cutoff underflow".into(),
            )
        })?)
    } else {
        None
    };
    let namespace = token.namespace().as_str();
    let (sql, sql_params) = if let Some(cutoff) = legacy_cutoff {
        // Match the #3497 boot-repair selector: missing, JSON null, or a
        // SQLite-space-only text slug. Existing tombstones stay eligible,
        // including an operator-soft-deleted historical quarantine.
        (
            khive_runtime::sql!("quarantine_legacy_expired_select"),
            vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(as_of),
                SqlValue::Integer(cutoff),
                SqlValue::Text(p.channel_kind.clone()),
            ],
        )
    } else {
        (
            khive_runtime::sql!("quarantine_expired_select"),
            vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(as_of),
                SqlValue::Text(p.channel_kind.clone()),
                SqlValue::Text(p.channel_slug.clone()),
            ],
        )
    };
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let rows = reader
        .query_all(SqlStatement {
            sql: sql.into(),
            params: sql_params,
            label: Some("comm_cleanup_expired_quarantine".into()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    drop(reader);

    // Hard deletion by ID is not namespace-scoped. Re-read each UUID through
    // the authorized store and enforce the query's full predicate.
    let store = runtime.notes(token)?;
    let mut deleted = 0usize;
    let mut routed_owner_detached = 0usize;
    for row in rows {
        let id = match row.get("id") {
            Some(SqlValue::Text(id)) => Uuid::parse_str(id).map_err(|error| {
                RuntimeError::Internal(format!(
                    "cleanup_expired_quarantine: invalid stored note id: {error}"
                ))
            })?,
            _ => {
                return Err(RuntimeError::Internal(
                    "cleanup_expired_quarantine: query returned no text id".into(),
                ));
            }
        };
        let note = if legacy_cutoff.is_some() {
            store.get_note_including_deleted(id).await?
        } else {
            store.get_note(id).await?
        };
        let Some(note) = note else {
            continue;
        };
        let properties = note.properties.as_ref();
        let slug = properties.and_then(|props| props.get("channel_slug"));
        let slug_matches = match p.mode {
            QuarantineCleanupMode::Channel => {
                slug.and_then(Value::as_str) == Some(p.channel_slug.as_str())
            }
            QuarantineCleanupMode::LegacySlugless => match slug {
                None | Some(Value::Null) => true,
                Some(Value::String(value)) => value.trim().is_empty(),
                _ => false,
            },
        };
        let still_due = note.namespace == namespace
            && note.kind == "message"
            && match legacy_cutoff {
                Some(cutoff) => note
                    .expires_at
                    .map_or(note.created_at <= cutoff, |expires| expires <= as_of),
                None => {
                    note.deleted_at.is_none()
                        && note.expires_at.is_some_and(|expires| expires <= as_of)
                }
            }
            && properties
                .and_then(|props| props.get("channel_kind"))
                .and_then(Value::as_str)
                == Some(p.channel_kind.as_str())
            && slug_matches
            && (properties.and_then(|props| props.get("quarantined")) == Some(&Value::Bool(true))
                || properties
                    .and_then(|props| props.get("quarantined"))
                    .and_then(Value::as_str)
                    == Some("true"));
        if !still_due {
            continue;
        }
        let routed_legacy = p.mode == QuarantineCleanupMode::LegacySlugless
            && runtime.backend_id() != runtime.core().backend_id();
        let expected_ref = properties
            .and_then(|props| props.get("quarantine_content_ref"))
            .and_then(Value::as_str);
        // The runtime hard-delete removes incident graph edges alongside the
        // note and its local attachments in one transaction. A routed note's
        // repaired original lives on canonical main and is detached by exact
        // ID/ref only after that transaction commits.
        let delete_result = runtime.delete_note(token, id, true).await;
        // A typed post-commit error already proves the row/edge transaction
        // committed. A second read can fail and must not block detaching the
        // routed original that the now-absent note can never select again.
        let note_deleted = note_deleted_after_attempt(&delete_result, id, async {
            Ok(store.get_note_including_deleted(id).await?.is_none())
        })
        .await?;
        if note_deleted {
            deleted += 1;
            if routed_legacy {
                for attempt in 1..=3 {
                    match detach_deleted_legacy_original(runtime, id, expected_ref).await {
                        Ok(detached) => {
                            routed_owner_detached += usize::from(detached);
                            break;
                        }
                        Err(error) if attempt < 3 => {
                            tracing::warn!(
                                note_id = %id,
                                attempt,
                                error = %error,
                                "targeted legacy quarantine owner detach will retry"
                            );
                            tokio::task::yield_now().await;
                        }
                        Err(error) => {
                            // There is no note left to select on a later tick.
                            // The possible residue belongs to the operator's
                            // blob ownerless-row inspection path (#3178).
                            return Err(RuntimeError::Internal(format!(
                                "cleanup_expired_quarantine: deleted_notes={deleted}, \
                                 routed_owners_detached={routed_owner_detached}, \
                                 possible_owner_residue=1 for note {id} after 3 attempts: {error}"
                            )));
                        }
                    }
                }
            }
        }
        delete_result?;
    }
    Ok(json!({
        "ok": true,
        "deleted": deleted,
        "routed_owners_detached": routed_owner_detached,
    }))
}

/// Deterministic UUID identifying the `channel_health` row for one
/// `(namespace, channel_kind, channel_slug)` triple (khive #606). Hashes the
/// triple as a JSON array (not a `:`-joined string, which is not injective
/// when a component itself contains `:`). See
/// crates/khive-pack-comm/docs/api/channel-health.md#handlersrsheartbeat_note_id
pub(super) fn heartbeat_note_id(namespace: &str, channel_kind: &str, channel_slug: &str) -> Uuid {
    let key = serde_json::to_vec(&(
        "khive:channel_health",
        namespace,
        channel_kind,
        channel_slug,
    ))
    .expect("a 4-tuple of &str always serializes to JSON");
    Uuid::new_v5(&Uuid::NAMESPACE_URL, &key)
}

/// `heartbeat` — persist one poll attempt's outcome into the channel's
/// heartbeat row (khive #606). Internal subhandler with no MCP wire path: its
/// production local caller is the daemon's channel poll loop, and khive #917
/// also lets authorized per-tenant writers reach it via `dispatch_as`.
/// Read-modify-write: `created_at` is preserved across updates, `last_error`
/// is RETAINED across a subsequent success (design review amendment 3), and
/// `consecutive_failures` resets on success / increments on failure, read from
/// the prior row (correct across restarts).
pub(crate) async fn handle_heartbeat(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    // HeartbeatParams omits deny_unknown_fields — mirrors IngestParams (dispatch
    // consumes `namespace` before the handler runs).
    let p: HeartbeatParams = serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("heartbeat: bad params: {e}")))?;

    if p.channel_kind.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `channel_kind` must not be empty".into(),
        ));
    }
    if p.channel_slug.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `channel_slug` must not be empty".into(),
        ));
    }
    if p.poll_interval_secs == Some(0) {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `poll_interval_secs` must be greater than zero".into(),
        ));
    }
    let outcome = match p.outcome.as_str() {
        s @ ("success" | "failure") => s,
        other => {
            return Err(RuntimeError::InvalidInput(format!(
                "heartbeat: invalid `outcome` {other:?}; expected \"success\" or \"failure\""
            )));
        }
    };
    if outcome == "failure"
        && p.error_class
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
    {
        return Err(RuntimeError::InvalidInput(
            "heartbeat: `error_class` is required when outcome is \"failure\"".into(),
        ));
    }

    // Issue #917: heartbeat rows persist under `token.namespace()` — the
    // dispatch-authorized namespace every other comm verb already uses —
    // rather than the fixed `crate::CHANNEL_HEALTH_NAMESPACE` constant #606
    // pinned this to. `comm.heartbeat` is `Visibility::Subhandler` (never
    // reachable from the MCP wire); the only callers able to dispatch it are
    // trusted internal Rust code holding a `&VerbRegistry` handle, so the
    // gate check `VerbRegistry::dispatch_with_identity` already runs for
    // every dispatch (subhandlers included) is the sole authorization
    // boundary here (ADR-018) — this handler must not layer a second,
    // handler-local namespace check on top of it.
    //
    // The local single-tenant poll loop (`khive-mcp`'s
    // `record_channel_heartbeat`) is unaffected: it always passes
    // `"namespace": crate::CHANNEL_HEALTH_NAMESPACE` explicitly in its own
    // dispatch params, so it keeps writing under `"local"` exactly as
    // before. An authorized per-tenant writer (#917) instead dispatches via
    // `VerbRegistry::dispatch_as` with a `VerifiedActor` (an out-of-band
    // authenticated tenant principal, never derived from a wire-supplied
    // field — this verb has no wire path at all) and passes that tenant's
    // own namespace as this same explicit `namespace` dispatch param. Those
    // heartbeat rows land under that tenant's namespace, so a tenant-scoped
    // `comm.health` (#877) now observes real writer state
    // instead of an empty set by construction.
    let ns = token.namespace().as_str();
    let store = runtime.notes(token)?;
    let id = heartbeat_note_id(ns, &p.channel_kind, &p.channel_slug);

    let existing = store
        .get_note(id)
        .await
        .map_err(|e| RuntimeError::Internal(format!("heartbeat: get_note: {e}")))?;
    #[cfg(test)]
    race_seam::pause_after_read().await;

    let now = Utc::now();
    // A supplied `at` must resolve to an instant before it is stored: the
    // staleness reader parses `last_poll_attempt_at` inside an Option chain,
    // so an unparseable stored value makes staleness silently unknown and the
    // channel can never read as stale — failing toward looking healthy. Same
    // rule as ingest's `sent_at` (`canonicalize_ingest_sent_at`).
    let at = match p.at.as_deref() {
        Some(raw) => {
            parse_supplied_timestamp("heartbeat", "at", raw)?;
            raw.trim().to_string()
        }
        None => now.to_rfc3339(),
    };

    // Preserve the current row's other properties, then validate the final
    // object: a legacy row may already carry a runtime-owned key.
    let mut props = existing
        .as_ref()
        .and_then(|n| n.properties.clone())
        .unwrap_or_else(|| json!({}));

    props["channel_kind"] = json!(p.channel_kind);
    props["channel_slug"] = json!(p.channel_slug);
    props["last_poll_attempt_at"] = json!(at);
    if let Some(poll_interval_secs) = p.poll_interval_secs {
        props["poll_interval_secs"] = json!(poll_interval_secs);
    }

    match outcome {
        "success" => {
            props["last_success_at"] = json!(at);
            props["consecutive_failures"] = json!(0);
            // last_error is intentionally left untouched — design review amendment 3.
        }
        "failure" => {
            props["last_failure_at"] = json!(at);
            let prev_failures = props
                .get("consecutive_failures")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            props["consecutive_failures"] = json!(prev_failures + 1);
            props["last_error"] = json!({
                "class": p.error_class.clone().unwrap_or_default(),
                "message": p.error_message.clone().unwrap_or_default(),
                "at": at,
            });
        }
        _ => unreachable!("outcome already validated above"),
    }

    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&props))?;
    khive_runtime::secret_gate::check_json_at(&props, "channel", "properties")?;

    let content = format!("channel heartbeat: {}:{}", p.channel_kind, p.channel_slug);
    khive_runtime::secret_gate::check_at(&content, "channel", "content")?;

    let created_at = existing
        .as_ref()
        .map(|n| n.created_at)
        .unwrap_or_else(|| now.timestamp_micros());

    // `updated_at` is also the optimistic-concurrency revision `replace_note_if_unchanged`
    // checks with `?10 > updated_at`. Two heartbeats landing in the same stored microsecond,
    // or a backward wall-clock step, must not report a conflict when no concurrent writer
    // touched the row — make the replacement revision strictly advance past the existing
    // snapshot instead of trusting `Utc::now()` alone (mirrors
    // `update_note_from_snapshot_with_embedding_report`). A brand-new channel has no prior
    // revision to advance past.
    let updated_at_micros = match existing.as_ref() {
        Some(snapshot) => {
            let minimum = snapshot.updated_at.checked_add(1).ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "heartbeat: channel {}:{} updated_at is already at i64::MAX and cannot advance",
                    p.channel_kind, p.channel_slug
                ))
            })?;
            now.timestamp_micros().max(minimum)
        }
        None => now.timestamp_micros(),
    };

    let note = Note {
        version: 1,
        key: None,
        id,
        namespace: ns.to_string(),
        kind: "channel_health".to_string(),
        status: "active".to_string(),
        name: Some(format!("{}:{}", p.channel_kind, p.channel_slug)),
        content,
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(props),
        created_at,
        updated_at: updated_at_micros,
        deleted_at: None,
    };

    // Guard the read-modify-write: `props` above was carried forward from
    // `existing`, so a concurrent heartbeat that committed after that read
    // (e.g. flipping `consecutive_failures`/`last_error`) must not be
    // silently discarded by this write — the same shape
    // `replace_note_if_unchanged` uses for every other guarded note update
    // (khive-runtime's `update_note_from_snapshot_with_embedding_report`).
    //
    // The `None` branch needs its own guard, and being a new row is not what
    // makes it safe. The hazard there is not losing prior state, it is two
    // concurrent FIRST writes: the note id is deterministic per channel, so two
    // heartbeats racing on a channel's first report can both read `None` and
    // both take that branch. `upsert_note` rewrites every mutable column on an
    // id conflict and reports no conflict outcome, so the later write would
    // silently replace the earlier report. `insert_note_if_absent` leaves an
    // existing row untouched and reports whether this call inserted it, so the
    // loser is told it lost — the same conflict the `Some` arm returns, reached
    // from the other direction.
    match existing {
        Some(snapshot) => {
            let persisted = store
                .replace_note_if_unchanged(note, snapshot.updated_at, snapshot.deleted_at)
                .await
                .map_err(|e| {
                    RuntimeError::Internal(format!("heartbeat: replace_note_if_unchanged: {e}"))
                })?;
            if !persisted {
                return Err(RuntimeError::Khive(khive_types::KhiveError::conflict(
                    format!(
                        "heartbeat: channel {}:{} changed concurrently after it was read; retry",
                        p.channel_kind, p.channel_slug
                    ),
                )));
            }
        }
        None => {
            let inserted = store.insert_note_if_absent(note).await.map_err(|e| {
                RuntimeError::Internal(format!("heartbeat: insert_note_if_absent: {e}"))
            })?;
            if !inserted {
                return Err(RuntimeError::Khive(khive_types::KhiveError::conflict(
                    format!(
                        "heartbeat: channel {}:{} was first reported concurrently; retry",
                        p.channel_kind, p.channel_slug
                    ),
                )));
            }
        }
    }

    Ok(json!({
        "ok": true,
        "channel_kind": p.channel_kind,
        "channel_slug": p.channel_slug,
        "outcome": outcome,
    }))
}
