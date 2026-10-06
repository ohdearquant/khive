// Licensed under the Apache License, Version 2.0.

// FILE SIZE JUSTIFICATION: curation.rs holds entity/note/edge patch types alongside
// their update and merge implementations. The implementations share private helpers
// (merge_properties, namespace checks, dedup policy) that need pub(crate) access to
// runtime internals. Inline tests cover merge semantics that require direct access to
// those helpers. Split plan: extract patch types into `curation/patch.rs` and merge
// logic into `curation/merge.rs` once the dedup policy API stabilises.
//! Curation operations: entity update/merge and edge-list filter type.

use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use khive_db::{pool::RuntimeWriteOperation, SqliteError};
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
use khive_storage::types::{EdgeFilter, PageRequest, SqlValue, TextDocument};
use khive_storage::{AtomicUnitOp, EdgeRelation, Entity, SqlStatement, SubstrateKind};
use khive_types::{Details, EdgeEndpointRule, EventKind, KhiveError};
use rusqlite::OptionalExtension;

use crate::error::{RuntimeError, RuntimeResult};
use crate::event_store_guard::EventAttribution;
use crate::operations::{base_entity_rule_allows, canonical_edge_endpoints, endpoint_matches};
use crate::runtime::{KhiveRuntime, NamespaceToken};

mod note_curation;
mod note_merge;
mod note_merge_guard;
pub(crate) mod note_reindex;

pub use note_merge_guard::{GuardedNoteMerge, MergeAssertion, NoteMergeGuard};

/// Restrict an outbox scan before its SQL page bound is applied. A held row
/// for another or unconfigured channel must not consume a channel's page.
enum OutboxSlugFilter<'a> {
    Any,
    Exact(&'a str),
    Missing,
}

/// Test-only pause point at the read/write boundary of a guarded
/// read-modify-write, so a race between two concurrent callers of the same
/// PRODUCTION entry point (not the underlying store primitive) can be
/// reproduced deterministically instead of relying on scheduler luck or
/// sleeps. A no-op unless the calling task runs inside
/// `AFTER_READ_BARRIER.scope(...)`; production code never establishes that
/// scope, so `pause_after_read` costs nothing outside these regression
/// tests, and it does not exist at all in non-test builds.
#[cfg(test)]
#[path = "curation/race_seam_tests.rs"]
pub(crate) mod race_seam;

mod types_and_guards;

use types_and_guards::{
    append_merge_event_in_transaction, edge_row_budget_bytes, map_merge_entity_storage_error,
    map_merge_note_storage_error, EmbeddingModelPlan, EntityMergeRefusal, EntityMergeValidation,
    MergeEventContext, MergeSqlError, MergeTxBudget,
};
pub use types_and_guards::{
    entity_merge_guard_compared_values, entity_merge_guard_error,
    entity_merge_guard_refusal_message, validate_entity_merge_floor, ContentMergeStrategy,
    EdgeListFilter, EdgePatch, EntityDedupMergePolicy, EntityMergeGuard, EntityPatch,
    MergeEdgeConflictPreimage, MergeEdgePreimage, MergeSummary, MergeTxBudgetReport, MergeTxLimits,
    NotePatch, NoteUpdatePolicy,
};
pub(crate) use types_and_guards::{
    normalize_note_update_tags, stale_edge_snapshot_error, stale_entity_snapshot_error,
    stale_note_snapshot_error,
};
#[cfg(test)]
use types_and_guards::{MERGE_TX_MAX_BYTES, MERGE_TX_MAX_ROWS};

mod entity_curation;
mod merge_edges;

use merge_edges::{
    collect_merge_drop_incident_edge_preimages, delete_merge_drop_edges, edge_row_preimage,
    merge_rewire_endpoint_contract_allows, resolve_merge_edge_endpoint_budgeted, EdgeRow,
};

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl KhiveRuntime {
    /// Claim `external_id` on an outbound `message` note through the
    /// ADR-124-sanctioned store-level owner path, bypassing the
    /// caller-facing owner-established-property refusal in
    /// [`Self::update_note_with_embedding_report`] (and its crate-internal prepare path). This is deliberately
    /// NOT exposed through any registered verb (ADR-124's stated bound): it is
    /// reachable only from pack/runtime code that owns outbox bookkeeping for
    /// the `message` note kind.
    ///
    /// Refuses (returns `Err`, never writes) unless the live row is a
    /// `message` note, `properties.direction == "outbound"`, and
    /// `properties.external_id` is currently absent or empty. The claim is
    /// committed against that exact snapshot and advances its timestamp so
    /// competing claims and delivery-outcome CAS writes cannot overwrite it.
    pub async fn claim_outbound_message_external_id(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        external_id: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let store = self.notes(token)?;
        let note = store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        if note.kind != "message" {
            return Err(RuntimeError::InvalidInput(format!(
                "external_id can only be claimed on a `message` note; note {id} is a `{}`",
                note.kind
            )));
        }
        let props = note.properties.as_ref().and_then(|v| v.as_object());
        let direction = props
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str());
        if direction != Some("outbound") {
            return Err(RuntimeError::InvalidInput(format!(
                "external_id can only be claimed on an outbound message note; note {id} has \
                 direction {:?}",
                direction
            )));
        }
        if note.deleted_at.is_some()
            || Self::outbound_delivery_is_terminal(props)
            || props
                .and_then(|p| p.get("delivered_at"))
                .is_some_and(|value| !value.is_null())
        {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} is not pending outbound delivery"
            )));
        }
        let existing = props
            .and_then(|p| p.get("external_id"))
            .and_then(|v| v.as_str());
        if existing.is_some_and(|v| !v.is_empty()) {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} already has an external_id claimed"
            )));
        }
        let mut properties = props
            .cloned()
            .expect("outbound direction requires an object");
        properties.insert("external_id".to_string(), Value::String(external_id));
        self.replace_outbound_message_properties_as_owner(token, note, properties)
            .await
    }

    /// Non-wire outbox scan for the channel delivery loops.
    ///
    /// Fetches live `message` notes matching the SQL-side pending predicate
    /// newest-first (`created_at DESC, id ASC`), bounded by an internal page
    /// cap. Direction, `delivered_at`, terminal `delivery` state, the optional
    /// `to_actor` channel prefix, and `next_attempt_at` are filtered by SQLite
    /// before the page bound. Pending means `delivered_at`
    /// is absent or null, `properties.delivery` carries no terminal state
    /// (`"delivered"` / `"failed"`), and a valid `next_attempt_at` is absent
    /// or due (ADR-122 §1). Malformed legacy deadlines fail open so a bad
    /// property cannot strand mail forever.
    ///
    /// The channel prefix has to be in the statement, not applied to the
    /// fetched page: every actor-to-actor outbound row matches the pending
    /// predicate forever (nothing marks those delivered). A full `name:`
    /// channel prefix also supplies an indexed bucket equality, followed by
    /// an indexed deadline bound; arbitrary partial prefixes retain the
    /// recipient range. The final newest-first sort preserves delivery order.
    /// This lives on the runtime rather than going through the wire registry
    /// for the same reason as
    /// [`Self::claim_outbound_message_external_id`]: the delivery loop must
    /// scan the backend that actually holds comm's notes, and under a
    /// `[packs.comm]` backend assignment that is not the backend serving the
    /// generic kg verbs.
    pub async fn list_undelivered_outbound_messages(
        &self,
        token: &NamespaceToken,
        to_prefix: Option<&str>,
        limit: u32,
    ) -> RuntimeResult<Vec<khive_storage::note::Note>> {
        self.list_undelivered_outbound_messages_scoped(
            token,
            to_prefix,
            limit,
            OutboxSlugFilter::Any,
        )
        .await
    }

    /// Channel-specific scan for the delivery pass. An explicit slug belongs
    /// only to its named adapter; a missing slug is eligible only when that
    /// kind has exactly one configured adapter. Querying these disjoint
    /// partitions before paging prevents held unknown or ambiguous rows from
    /// crowding an eligible row out of the finite outbox scan window.
    pub async fn list_undelivered_outbound_messages_for_channel(
        &self,
        token: &NamespaceToken,
        to_prefix: &str,
        channel_slug: &str,
        include_legacy: bool,
        limit: u32,
    ) -> RuntimeResult<Vec<khive_storage::note::Note>> {
        let mut notes = self
            .list_undelivered_outbound_messages_scoped(
                token,
                Some(to_prefix),
                limit,
                OutboxSlugFilter::Exact(channel_slug),
            )
            .await?;
        if include_legacy {
            notes.extend(
                self.list_undelivered_outbound_messages_scoped(
                    token,
                    Some(to_prefix),
                    limit,
                    OutboxSlugFilter::Missing,
                )
                .await?,
            );
            notes.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
            notes.truncate(limit as usize);
        }
        Ok(notes)
    }

    async fn list_undelivered_outbound_messages_scoped(
        &self,
        token: &NamespaceToken,
        to_prefix: Option<&str>,
        limit: u32,
        slug_filter: OutboxSlugFilter<'_>,
    ) -> RuntimeResult<Vec<khive_storage::note::Note>> {
        const MAX_PAGE_TOTAL: u32 = 10_000;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now_micros = chrono::Utc::now().timestamp_micros();
        // The former Rust predicate compared `timestamp_micros()`, so a
        // deadline within the current microsecond counted as due. Preserve
        // that boundary when comparing the SQL function's nanosecond keys.
        let due_through = chrono::DateTime::<chrono::Utc>::from_timestamp_micros(now_micros)
            .expect("current UTC time fits a chrono timestamp")
            + chrono::Duration::nanoseconds(999);
        let mut property_filters = vec![
            PropertyFilter {
                json_path: "$.direction".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("outbound".to_string()),
            },
            PropertyFilter {
                json_path: "$.delivered_at".to_string(),
                op: FilterOp::JsonTypeMissingOrNullIndexed,
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.delivery".to_string(),
                op: FilterOp::NotInOrMissing(vec![
                    SqlValue::Text("delivered".to_string()),
                    SqlValue::Text("failed".to_string()),
                ]),
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.delivery_hold".to_string(),
                op: FilterOp::JsonTypeMissingOrNullIndexed,
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.next_attempt_at".to_string(),
                op: FilterOp::Rfc3339LteOrInvalid,
                value: SqlValue::Timestamp(due_through),
            },
        ];
        if let Some(prefix) = to_prefix {
            let op = if prefix
                .strip_suffix(':')
                .is_some_and(|head| !head.is_empty() && !head.contains(':'))
            {
                FilterOp::TextColonPrefixBucketIndexed
            } else {
                FilterOp::TextStartsWithIndexed
            };
            property_filters.push(PropertyFilter {
                json_path: "$.to_actor".to_string(),
                op,
                value: SqlValue::Text(prefix.to_string()),
            });
        }
        match slug_filter {
            OutboxSlugFilter::Any => {}
            OutboxSlugFilter::Exact(slug) => property_filters.push(PropertyFilter {
                json_path: "$.channel_slug".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text(slug.to_string()),
            }),
            OutboxSlugFilter::Missing => property_filters.push(PropertyFilter {
                json_path: "$.channel_slug".to_string(),
                op: FilterOp::JsonTypeMissing,
                value: SqlValue::Null,
            }),
        }
        let filter = NoteFilter {
            kind: Some("message".to_string()),
            property_filters,
            ..Default::default()
        };
        let page = self
            .notes(token)?
            .query_notes_filtered_count_free(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    limit: limit.min(MAX_PAGE_TOTAL),
                    offset: 0,
                },
            )
            .await?;
        Ok(page.items)
    }

    /// Load a live outbound `message` note, returning `InvalidInput`
    /// otherwise. Guard shared by the delivery-outcome markers: they take
    /// caller-supplied UUIDs, and the generic note patch path would happily
    /// stamp delivery properties onto any note kind.
    async fn outbound_message(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let note = self
            .notes(token)?
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))?;
        if note.kind != "message" || note.deleted_at.is_some() {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} is not a live message note (kind {})",
                note.kind
            )));
        }
        let outbound = note
            .properties
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("direction"))
            .and_then(|v| v.as_str())
            == Some("outbound");
        if !outbound {
            return Err(RuntimeError::InvalidInput(format!(
                "note {id} is not an outbound message"
            )));
        }
        Ok(note)
    }

    async fn replace_outbound_message_properties(
        &self,
        token: &NamespaceToken,
        mut snapshot: khive_storage::note::Note,
        properties: serde_json::Map<String, Value>,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let expected_updated_at = snapshot.updated_at;
        let expected_deleted_at = snapshot.deleted_at;
        let id = snapshot.id;
        snapshot.properties = Some(Value::Object(properties));
        crate::secret_gate::reject_reserved_secret_gate_property(snapshot.properties.as_ref())?;
        snapshot.updated_at = chrono::Utc::now().timestamp_micros().max(
            expected_updated_at.checked_add(1).ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "note {id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?,
        );

        // Delivery outcomes preserve transport-owned route fields from the loaded snapshot.
        let store = self.raw_notes(token)?;
        let persisted = store
            .replace_note_if_unchanged(snapshot, expected_updated_at, expected_deleted_at)
            .await?;
        if !persisted {
            return Err(stale_note_snapshot_error(id));
        }
        // Storage assigns the persisted revision; the pre-write snapshot
        // still carries the old version even when this CAS succeeds.
        store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))
    }

    async fn replace_outbound_message_properties_as_owner(
        &self,
        token: &NamespaceToken,
        mut snapshot: khive_storage::note::Note,
        properties: serde_json::Map<String, Value>,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let expected_updated_at = snapshot.updated_at;
        let expected_deleted_at = snapshot.deleted_at;
        let id = snapshot.id;
        snapshot.properties = Some(Value::Object(properties));
        crate::secret_gate::reject_reserved_secret_gate_property(snapshot.properties.as_ref())?;
        snapshot.updated_at = chrono::Utc::now().timestamp_micros().max(
            expected_updated_at.checked_add(1).ok_or_else(|| {
                RuntimeError::Internal(format!("note {id} updated_at cannot advance"))
            })?,
        );
        // Owner operations preserve any transport evidence on the snapshot.
        // Re-running the public full-row guard would reject that existing evidence.
        let store = self.raw_notes(token)?;
        if !store
            .replace_note_if_unchanged(snapshot, expected_updated_at, expected_deleted_at)
            .await?
        {
            return Err(stale_note_snapshot_error(id));
        }
        // Return the storage-assigned revision, just as the claim path does.
        store
            .get_note(id)
            .await?
            .ok_or_else(|| RuntimeError::NotFound(format!("note {id}")))
    }

    /// True once a `delivery` outcome has been terminally recorded
    /// (`"delivered"` or `"failed"`). Shared by every delivery-outcome
    /// marker: concurrent outbox workers (two daemon processes overlapping
    /// during a restart, per ADR-122 §4/Consequences) can both load the same
    /// pending note before either writes, so a marker must re-check the
    /// freshly loaded snapshot rather than trust the compare-and-swap alone
    /// -- the CAS only rejects a write against a snapshot that has since
    /// changed, not a write that starts from an up-to-date terminal snapshot
    /// and would otherwise happily overwrite it with a different outcome.
    fn outbound_delivery_is_terminal(props: Option<&serde_json::Map<String, Value>>) -> bool {
        props
            .and_then(|properties| properties.get("delivery"))
            .and_then(Value::as_str)
            .is_some_and(|state| state == "delivered" || state == "failed")
    }

    /// Record a transient transport failure while leaving the message
    /// pending. The retry deadline is derived from the incremented persisted
    /// attempt count, using `base_delay * 2^(attempt - 1)` capped at
    /// `max_delay`.
    pub async fn mark_outbound_message_transient_failure(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        attempted_at: chrono::DateTime<chrono::Utc>,
        last_error: String,
        base_delay: std::time::Duration,
        max_delay: std::time::Duration,
    ) -> RuntimeResult<khive_storage::note::Note> {
        if base_delay.is_zero() || max_delay < base_delay {
            return Err(RuntimeError::InvalidInput(
                "outbound retry delays require a non-zero base no greater than the ceiling"
                    .to_string(),
            ));
        }

        crate::secret_gate::check_json_at(
            &serde_json::json!({
                "last_error": &last_error,
            }),
            "message",
            "last_error",
        )?;

        let snapshot = self.outbound_message(token, id).await?;
        let props = snapshot.properties.as_ref().and_then(Value::as_object);
        if Self::outbound_delivery_is_terminal(props) {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} already has a terminal delivery outcome"
            )));
        }

        let properties = Self::outbound_retry_properties(
            props,
            attempted_at,
            last_error,
            base_delay,
            max_delay,
        )?;
        self.replace_outbound_message_properties(token, snapshot, properties)
            .await
    }

    fn outbound_retry_properties(
        props: Option<&serde_json::Map<String, Value>>,
        attempted_at: chrono::DateTime<chrono::Utc>,
        last_error: String,
        base_delay: std::time::Duration,
        max_delay: std::time::Duration,
    ) -> RuntimeResult<serde_json::Map<String, Value>> {
        let attempts = props
            .and_then(|properties| properties.get("delivery_attempts"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        let exponent = attempts.saturating_sub(1).min(127) as u32;
        let delay_nanos = base_delay
            .as_nanos()
            .saturating_mul(1u128 << exponent)
            .min(max_delay.as_nanos());
        let delay = std::time::Duration::new(
            (delay_nanos / 1_000_000_000) as u64,
            (delay_nanos % 1_000_000_000) as u32,
        );
        let chrono_delay = chrono::TimeDelta::from_std(delay).map_err(|_| {
            RuntimeError::InvalidInput("outbound retry ceiling exceeds RFC 3339 range".to_string())
        })?;
        let next_attempt_at = attempted_at
            .checked_add_signed(chrono_delay)
            .ok_or_else(|| {
                RuntimeError::InvalidInput(
                    "outbound retry deadline exceeds RFC 3339 range".to_string(),
                )
            })?;

        let mut properties = props.cloned().unwrap_or_default();
        properties.insert("delivery_attempts".to_string(), Value::from(attempts));
        properties.insert(
            "next_attempt_at".to_string(),
            Value::String(next_attempt_at.to_rfc3339()),
        );
        properties.insert("last_error".to_string(), Value::String(last_error));
        Ok(properties)
    }

    /// Schedule an external-id claim retry only for a still-unclaimed
    /// outbound snapshot. Existing claims and terminal outcomes are unchanged.
    pub async fn mark_outbound_message_claim_transient_failure(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        attempted_at: chrono::DateTime<chrono::Utc>,
        last_error: String,
        base_delay: std::time::Duration,
        max_delay: std::time::Duration,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let snapshot = self.outbound_message(token, id).await?;
        let props = snapshot.properties.as_ref().and_then(Value::as_object);
        let has_claim = props
            .and_then(|properties| properties.get("external_id"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        let has_delivery = props
            .and_then(|properties| properties.get("delivered_at"))
            .is_some_and(|value| !value.is_null());
        if has_claim || has_delivery || Self::outbound_delivery_is_terminal(props) {
            return Ok(snapshot);
        }
        if base_delay.is_zero() || max_delay < base_delay {
            return Err(RuntimeError::InvalidInput(
                "outbound retry delays require a non-zero base no greater than the ceiling"
                    .to_string(),
            ));
        }
        crate::secret_gate::check_json_at(
            &serde_json::json!({"last_error": &last_error}),
            "message",
            "last_error",
        )?;
        let properties = Self::outbound_retry_properties(
            props,
            attempted_at,
            last_error,
            base_delay,
            max_delay,
        )?;
        self.replace_outbound_message_properties_as_owner(token, snapshot, properties)
            .await
    }

    /// Mark an outbound `message` note delivered by merging the ADR-122 §1
    /// terminal-outcome properties (`delivery = "delivered"`, `delivered_at`,
    /// and `transport_message_id` when the transport minted one), and clearing
    /// `delivery_attempts` / `next_attempt_at`. The compare-and-swap protects
    /// unrelated properties from a concurrent full-row overwrite;
    /// `delivered_at` remains deliberately caller-patchable, pinned by
    /// `generic_update_can_still_patch_delivered_at_on_message_note`.
    /// Refuses (`InvalidInput`) unless `id` names a live outbound `message`
    /// note. Non-wire companion to
    /// [`Self::list_undelivered_outbound_messages`] so the delivery loop
    /// writes the backend that holds the note.
    pub async fn mark_outbound_message_delivered(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        delivered_at: String,
        transport_message_id: Option<String>,
    ) -> RuntimeResult<khive_storage::note::Note> {
        crate::secret_gate::check_json_at(
            &serde_json::json!({
                "delivered_at": &delivered_at,
                "transport_message_id": &transport_message_id,
            }),
            "message",
            "delivered",
        )?;
        let snapshot = self.outbound_message(token, id).await?;
        if Self::outbound_delivery_is_terminal(
            snapshot.properties.as_ref().and_then(Value::as_object),
        ) {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} already has a terminal delivery outcome"
            )));
        }
        let mut props = snapshot
            .properties
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        props.remove("delivery_attempts");
        props.remove("next_attempt_at");
        props.insert("delivery".into(), Value::String("delivered".into()));
        props.insert("delivered_at".into(), Value::String(delivered_at));
        if let Some(transport_message_id) = transport_message_id {
            props.insert(
                "transport_message_id".into(),
                Value::String(transport_message_id),
            );
        }
        self.replace_outbound_message_properties(token, snapshot, props)
            .await
    }

    /// Record a permanent delivery failure on an outbound `message` note:
    /// `delivery = "failed"`, `failed_at`, `last_error` (ADR-122 §2 — an
    /// allowlist rejection must be recorded, not skipped, or the row stays
    /// pending forever while the caller saw `ok: true`). Any retry counter and
    /// deadline are cleared because the outcome is terminal. Refuses
    /// (`InvalidInput`) unless `id` names a live outbound `message` note.
    pub async fn mark_outbound_message_failed(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        failed_at: String,
        last_error: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        crate::secret_gate::check_json_at(
            &serde_json::json!({
                "failed_at": &failed_at,
                "last_error": &last_error,
            }),
            "message",
            "failed",
        )?;
        let snapshot = self.outbound_message(token, id).await?;
        if Self::outbound_delivery_is_terminal(
            snapshot.properties.as_ref().and_then(Value::as_object),
        ) {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} already has a terminal delivery outcome"
            )));
        }
        let mut props = snapshot
            .properties
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        props.remove("delivery_attempts");
        props.remove("next_attempt_at");
        props.insert("delivery".into(), Value::String("failed".into()));
        props.insert("failed_at".into(), Value::String(failed_at));
        props.insert("last_error".into(), Value::String(last_error));
        self.replace_outbound_message_properties(token, snapshot, props)
            .await
    }

    /// Owner-only visible hold for an outbound email whose stored Message-ID
    /// cannot be bound to its own row and configured sending domain.
    pub async fn hold_outbound_message_external_id_unverifiable(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        reason: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        crate::secret_gate::check_at(&reason, "message", "delivery_hold_reason")?;
        let snapshot = self.outbound_message(token, id).await?;
        let props = snapshot.properties.as_ref().and_then(Value::as_object);
        if props
            .and_then(|p| p.get("delivery_hold"))
            .and_then(Value::as_str)
            == Some("external_id_unverifiable")
        {
            return Ok(snapshot);
        }
        if Self::outbound_delivery_is_terminal(props)
            || props
                .and_then(|p| p.get("delivered_at"))
                .is_some_and(|v| !v.is_null())
        {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} is no longer pending delivery"
            )));
        }
        if props
            .and_then(|p| p.get("external_id"))
            .and_then(Value::as_str)
            .is_none_or(|s| s.is_empty())
        {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} has no nonempty external_id to hold"
            )));
        }
        let mut properties = props.cloned().unwrap_or_default();
        properties.remove("delivery_attempts");
        properties.remove("next_attempt_at");
        properties.insert(
            "delivery_hold".into(),
            Value::String("external_id_unverifiable".into()),
        );
        properties.insert("delivery_hold_reason".into(), Value::String(reason));
        properties.insert(
            "delivery_hold_at".into(),
            Value::String(chrono::Utc::now().to_rfc3339()),
        );
        self.replace_outbound_message_properties_as_owner(token, snapshot, properties)
            .await
    }

    /// Bounded maintenance scan for holds whose keyed diagnostic still needs
    /// confirmation. These rows never enter the ordinary send selection.
    pub async fn list_outbound_external_id_holds_missing_diagnostic(
        &self,
        token: &NamespaceToken,
        limit: u32,
    ) -> RuntimeResult<Vec<khive_storage::note::Note>> {
        let filter = NoteFilter {
            kind: Some("message".to_string()),
            property_filters: vec![
                PropertyFilter {
                    json_path: "$.direction".into(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text("outbound".into()),
                },
                PropertyFilter {
                    json_path: "$.to_actor".into(),
                    op: FilterOp::TextStartsWithIndexed,
                    value: SqlValue::Text("email:".into()),
                },
                PropertyFilter {
                    json_path: "$.delivery_hold".into(),
                    op: FilterOp::Eq,
                    value: SqlValue::Text("external_id_unverifiable".into()),
                },
                PropertyFilter {
                    json_path: "$.external_id_diagnostic_note_id".into(),
                    op: FilterOp::JsonTypeMissingOrNullIndexed,
                    value: SqlValue::Null,
                },
            ],
            ..Default::default()
        };
        Ok(self
            .notes(token)?
            .query_notes_filtered_count_free(
                token.namespace().as_str(),
                &filter,
                PageRequest {
                    limit: limit.min(200),
                    offset: 0,
                },
            )
            .await?
            .items)
    }

    pub async fn mark_outbound_external_id_diagnostic_recorded(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        diagnostic_id: Uuid,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let snapshot = self.outbound_message(token, id).await?;
        let props = snapshot.properties.as_ref().and_then(Value::as_object);
        let diagnostic_id_text = diagnostic_id.to_string();
        if props
            .and_then(|p| p.get("external_id_diagnostic_note_id"))
            .and_then(Value::as_str)
            == Some(diagnostic_id_text.as_str())
        {
            return Ok(snapshot);
        }
        if props
            .and_then(|p| p.get("external_id_diagnostic_note_id"))
            .and_then(Value::as_str)
            .is_some()
        {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} already names a different diagnostic"
            )));
        }
        if props
            .and_then(|p| p.get("delivery_hold"))
            .and_then(Value::as_str)
            != Some("external_id_unverifiable")
        {
            return Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} has no external_id_unverifiable hold"
            )));
        }
        let mut properties = props.cloned().unwrap_or_default();
        properties.insert(
            "external_id_diagnostic_note_id".into(),
            Value::String(diagnostic_id_text),
        );
        self.replace_outbound_message_properties_as_owner(token, snapshot, properties)
            .await
    }

    /// One keyed operator-visible observation, atomically annotated to the
    /// offending message. A replay cannot create a second diagnostic.
    pub async fn record_outbound_external_id_diagnostic(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        reason: &str,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let key = format!("outbound-email-external-id-unverifiable:{id}");
        let content = format!("Outbound email message {id} is held: {reason}");
        let properties = serde_json::json!({
            "diagnostic_code": "external_id_unverifiable",
            "offending_message_id": id.to_string(),
        });
        let (note, _) = self
            .create_note_with_options(
                token,
                "observation",
                Some("Outbound email Message-ID unverifiable"),
                &content,
                None,
                None,
                None,
                Some(properties),
                vec![id],
                None,
                crate::note_write::NoteWriteOptions {
                    key: Some(key.clone()),
                    embed: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        // A keyed replay can return an existing note without running the
        // creation-only annotation work. Verify the link before marking the
        // message's diagnostic as recorded.
        let linked = self
            .list_edges(
                token,
                EdgeListFilter {
                    source_id: Some(note.id),
                    target_id: Some(id),
                    relations: vec![EdgeRelation::Annotates],
                    ..Default::default()
                },
                1,
                0,
            )
            .await?
            .len()
            == 1;
        if linked {
            Ok(note)
        } else {
            Err(RuntimeError::InvalidInput(format!(
                "outbound message {id} diagnostic has no annotation link"
            )))
        }
    }

    /// Park a deterministic external-id claim refusal only while the exact
    /// current outbound snapshot remains unclaimed. An existing claim or a
    /// terminal delivery outcome is returned unchanged. Non-wire owner API:
    /// a second worker must not turn another worker's successful claim into
    /// a permanent delivery failure.
    pub async fn mark_outbound_message_claim_failed(
        &self,
        token: &NamespaceToken,
        id: Uuid,
        failed_at: String,
        last_error: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let snapshot = self.outbound_message(token, id).await?;
        self.mark_outbound_message_claim_failed_from_snapshot(
            token, snapshot, failed_at, last_error,
        )
        .await
    }

    async fn mark_outbound_message_claim_failed_from_snapshot(
        &self,
        token: &NamespaceToken,
        snapshot: khive_storage::note::Note,
        failed_at: String,
        last_error: String,
    ) -> RuntimeResult<khive_storage::note::Note> {
        let props = snapshot.properties.as_ref().and_then(Value::as_object);
        let has_claim = props
            .and_then(|properties| properties.get("external_id"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        let has_delivery = props
            .and_then(|properties| properties.get("delivered_at"))
            .is_some_and(|value| !value.is_null());
        if has_claim || has_delivery || Self::outbound_delivery_is_terminal(props) {
            return Ok(snapshot);
        }
        crate::secret_gate::check_json_at(
            &serde_json::json!({ "failed_at": &failed_at, "last_error": &last_error }),
            "message",
            "failed",
        )?;
        let mut properties = props.cloned().unwrap_or_default();
        properties.remove("delivery_attempts");
        properties.remove("next_attempt_at");
        properties.insert("delivery".into(), Value::String("failed".into()));
        properties.insert("failed_at".into(), Value::String(failed_at));
        properties.insert("last_error".into(), Value::String(last_error));
        self.replace_outbound_message_properties_as_owner(token, snapshot, properties)
            .await
    }

    /// Merge `from_id` note into `into_id` note.
    ///
    /// Both notes must exist in the namespace and have the same `kind`. Content is merged
    /// per `content_strategy`. Properties are merged per `strategy`. `from_id` is
    /// tombstoned (status='deleted', deleted_at set). Returns a summary.
    ///
    /// If `dry_run` is true, computes and returns the planned summary without mutating
    /// any rows, edges, or indexes.
    /// The NoteMerged event, including destructive edge preimages, commits in
    /// the same SQL transaction as the note and edge changes.
    pub async fn merge_note(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_note_with_reason(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            None,
        )
        .await
    }

    /// Merge `from_id` note into `into_id` note and include an optional audit reason.
    // REASON: these arguments mirror the merge verb's policy, content strategy,
    // dry-run, and audit-reason fields; a builder would only move that surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn merge_note_with_reason(
        &self,
        token: &NamespaceToken,
        into_id: Uuid,
        from_id: Uuid,
        strategy: EntityDedupMergePolicy,
        content_strategy: ContentMergeStrategy,
        dry_run: bool,
        reason: Option<String>,
    ) -> RuntimeResult<MergeSummary> {
        self.merge_note_with_guard(
            token,
            into_id,
            from_id,
            strategy,
            content_strategy,
            dry_run,
            reason,
            None,
        )
        .await
        .map(|(summary, _)| summary)
    }
}

/// Keep executable schedule intent behind the schedule pack's state-machine verbs.
///
/// `scheduled_event` notes carry both replay payloads and lifecycle state. Allowing
/// generic note update/merge to rewrite either would turn the immutable creator event
/// into a bearer credential for attacker-selected work: replay would attribute the
/// changed row to its original creator. Schedule's own transitions use its private
/// note-store CAS helpers and therefore do not pass through this generic curation seam.
fn reject_pack_managed_schedule_mutation(
    note: &khive_storage::note::Note,
    operation: &str,
) -> RuntimeResult<()> {
    if note.kind == "scheduled_event" {
        return Err(RuntimeError::InvalidInput(format!(
            "cannot {operation} a schedule-managed `scheduled_event` note through generic KG \
             mutation; use schedule.cancel or create a replacement schedule"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FTS document construction
// ---------------------------------------------------------------------------

/// Build the canonical text embedded for an entity on create, update, merge,
/// and repair paths.
pub fn entity_embedding_text(entity: &Entity) -> String {
    match &entity.description {
        Some(description) if !description.is_empty() => {
            format!("{} {description}", entity.name)
        }
        _ => entity.name.clone(),
    }
}

/// Build the canonical text embedded for a note when no explicit bounded
/// embedding prefix was supplied at creation time.
pub fn note_embedding_text(note: &Note) -> String {
    note_embedding_text_ref(note).to_owned()
}

/// Borrow the canonical note embedding text for runtime paths that do not
/// require ownership.
pub(crate) fn note_embedding_text_ref(note: &Note) -> &str {
    &note.content
}

/// Build the `TextDocument` for an entity. This is the single source of truth for
/// entity FTS document shape; all write paths (create, update, merge, reindex, backfill)
/// must go through this function so search parity is guaranteed.
///
/// Body rule: when the entity has a non-empty description, prepend the name
/// (`"<name> <description>"`). Otherwise the body is just the name. This
/// matches the FTS index contract: `title` and `body` are the ranked columns;
/// `tags`, `metadata`, and `namespace` are UNINDEXED.
///
/// `updated_at` is taken from the entity's own timestamp so that backfill and
/// reindex runs record the entity's actual mutation time rather than the
/// reindex execution time.
pub fn entity_fts_document(entity: &Entity) -> TextDocument {
    let updated_at =
        chrono::DateTime::from_timestamp_micros(entity.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: entity.id,
        kind: SubstrateKind::Entity,
        record_kind: Some(entity.kind.clone()),
        title: Some(entity.name.clone()),
        body: entity_embedding_text(entity),
        tags: entity.tags.clone(),
        namespace: entity.namespace.clone(),
        metadata: entity.properties.clone(),
        updated_at,
    }
}

/// Build the `TextDocument` for a note. This is the single source of truth for
/// note FTS document shape; all write paths (create, update, reindex) must go
/// through this function so recall parity is guaranteed. Changes here apply to
/// every caller automatically.
///
/// Body rule: when the note has a `name`, prepend it to the content
/// (`"<name> <content>"`). This matches the FTS index contract: title and body
/// both contribute to ranking, and the name is the most salient signal.
///
/// `updated_at` is taken from the note's own timestamp (not `Utc::now()`) so
/// that backfill and reindex runs record the note's actual mutation time rather
/// than the reindex execution time.
pub fn note_fts_document(note: &Note) -> TextDocument {
    let body = match &note.name {
        Some(n) => format!("{n} {}", note.content),
        None => note.content.clone(),
    };
    let updated_at =
        chrono::DateTime::from_timestamp_micros(note.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: note.id,
        kind: SubstrateKind::Note,
        record_kind: Some(note.kind.clone()),
        title: note.name.clone(),
        body,
        tags: vec![],
        namespace: note.namespace.clone(),
        metadata: note.properties.clone(),
        updated_at,
    }
}

/// SQL-bind–ready scalars derived from [`note_fts_document`].
///
/// Used by `merge_note_sql` to guarantee that the raw SQL FTS INSERT stores
/// exactly what [`Fts5TextSearch::upsert_document`] would write, preventing
/// null/empty-string divergence on the `title` column for nameless notes.
pub(crate) struct NoteFtsScalars {
    /// Granular note kind used by the indexed corpus classifier.
    pub record_kind: String,
    /// Empty string when `note.name` is `None` — matches the `unwrap_or("")` in
    /// `Fts5TextSearch::upsert_document`.
    pub title: String,
    pub body: String,
    /// Always the JSON array `"[]"`.
    pub tags: String,
    /// Serialised `note.properties`, or `None` when properties are absent.
    pub metadata: Option<String>,
    /// `note.updated_at` converted to `DateTime<Utc>` timestamp_micros.
    pub updated_at_micros: i64,
}

/// Derive [`NoteFtsScalars`] from a [`Note`].
///
/// All values match the encoding that [`Fts5TextSearch::upsert_document`]
/// applies when given the output of [`note_fts_document`].
pub(crate) fn note_fts_scalars(note: &Note) -> NoteFtsScalars {
    let doc = note_fts_document(note);
    NoteFtsScalars {
        record_kind: doc.record_kind.unwrap_or_default(),
        title: doc.title.unwrap_or_default(),
        body: doc.body,
        tags: "[]".to_string(),
        metadata: doc
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
        updated_at_micros: doc.updated_at.timestamp_micros(),
    }
}

// ---------------------------------------------------------------------------
// Transactional merge SQL helpers
// ---------------------------------------------------------------------------

/// Cheap SQL-side byte-length probe for one merge entity, evaluated BEFORE
/// [`read_merge_entity`] copies its columns into Rust `String`s and parses
/// `properties`/`tags` as JSON. `LENGTH()` still requires SQLite to touch the
/// stored bytes, but skips the Rust-side allocation and JSON parse — the
/// expensive part for an oversized record. Charging this probe against the
/// budget before the full read means an over-budget record is rejected
/// without ever being materialized or parsed inside the writer transaction.
/// Each column is wrapped in `CAST(... AS BLOB)` — plain `LENGTH(text)`
/// returns SQLite's *character* count for TEXT values, not the UTF-8 byte
/// count the budget is denominated in, so a multibyte (CJK/emoji) record
/// could under-report and pass a probe its true byte size exceeds. Casting
/// to BLOB forces `LENGTH()` to report octets instead.
/// A missing row probes as zero; `read_merge_entity`'s own "not found" error
/// fires on the subsequent full read and is unaffected by this probe.
fn probe_merge_entity_bytes(conn: &rusqlite::Connection, id: Uuid) -> Result<usize, SqliteError> {
    let id_str = id.to_string();
    let len: Option<i64> = conn
        .query_row(
            "SELECT LENGTH(CAST(name AS BLOB)) \
                    + COALESCE(LENGTH(CAST(description AS BLOB)), 0) \
                    + COALESCE(LENGTH(CAST(properties AS BLOB)), 0) \
                    + LENGTH(CAST(tags AS BLOB)) \
             FROM entities WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id_str],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;
    Ok(128_usize.saturating_add(len.unwrap_or(0).max(0) as usize))
}

/// Read one entity row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_entity(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<Entity, SqliteError> {
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, entity_type, name, description, properties, tags, \
         created_at, updated_at, deleted_at, merged_into, merge_event_id, \
         (SELECT a.content_ref FROM attachments AS a \
          WHERE a.record_uuid = entities.id AND a.substrate = 'entity' \
            AND a.role = 'content') AS content_ref, entities.version \
         FROM entities WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("entity {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let entity_type: Option<String> = row.get(3)?;
    let name: String = row.get(4)?;
    let description: Option<String> = row.get(5)?;
    let properties_str: Option<String> = row.get(6)?;
    let tags_str: String = row.get(7)?;
    let created_at: i64 = row.get(8)?;
    let updated_at: i64 = row.get(9)?;
    let deleted_at: Option<i64> = row.get(10)?;
    let merged_into_str: Option<String> = row.get(11)?;
    let merge_event_id_str: Option<String> = row.get(12)?;
    let content_ref: Option<String> = row.get(13)?;
    let version: i64 = row.get(14)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "entity {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let entity_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<Value> = properties_str
        .map(|s| {
            serde_json::from_str::<Value>(&s).map_err(|e| SqliteError::InvalidData(e.to_string()))
        })
        .transpose()?;
    let tags: Vec<String> =
        serde_json::from_str(&tags_str).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merged_into = merged_into_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let merge_event_id = merge_event_id_str
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| SqliteError::InvalidData(e.to_string()))?;

    Ok(Entity {
        id: entity_id,
        namespace: ns,
        kind,
        entity_type,
        name,
        description,
        properties,
        tags,
        created_at,
        updated_at,
        version,
        deleted_at,
        merged_into,
        merge_event_id,
        content_ref,
    })
}

/// All merge SQL on one connection inside an already-open `BEGIN IMMEDIATE` transaction.
///
/// Reads both entities, rewires/drops incident edges, merges entity fields, updates FTS,
/// deletes the `from` vec entry (if `vec_table` is Some), and tombstones `from` with merge
/// provenance.  Returns the updated `into` entity so the caller can do the async vec re-insert.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: merge requires both entity IDs, the namespace, FTS and vec table names, merge
// policy, and dry-run flag — all are load-bearing; reducing to a struct would obscure
// the sync/async boundary split that keeps this function off the async runtime.
#[allow(clippy::too_many_arguments)]
fn merge_entity_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
    pack_rules: Vec<EdgeEndpointRule>,
    validation: EntityMergeValidation,
    limits: MergeTxLimits,
    merge_event_id: Uuid,
    event_context: Option<MergeEventContext>,
) -> Result<(MergeSummary, Entity), MergeSqlError> {
    let mut budget = MergeTxBudget::new(limits);
    // Config-scaled fanout (one FTS/vector delete per table, one contract rule
    // set per pack) is charged in bytes only: it is bounded by configuration,
    // not by graph shape, but belongs in the same account it amortizes over.
    budget.charge(
        0,
        vec_tables.iter().map(String::len).sum::<usize>()
            + pack_rules.len() * std::mem::size_of::<EdgeEndpointRule>(),
        "preparing pack and vector fanout",
    )?;

    budget.charge(
        1,
        probe_merge_entity_bytes(conn, into_id)?,
        "reading merge records",
    )?;
    let into_entity = read_merge_entity(conn, into_id, &namespace)?;
    budget.charge(
        1,
        probe_merge_entity_bytes(conn, from_id)?,
        "reading merge records",
    )?;
    let from_entity = read_merge_entity(conn, from_id, &namespace)?;

    // ADR-115 A1: no production stamp is admitted yet. Check both guarded
    // preimages, even when the chosen fold would discard or replace a key.
    for properties in [&into_entity.properties, &from_entity.properties] {
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())
            .map_err(MergeSqlError::Refusal)?;
    }

    match validation {
        EntityMergeValidation::LegacyKind if into_entity.kind != from_entity.kind => {
            return Err(MergeSqlError::Refusal(
                EntityMergeRefusal::LegacyKind {
                    into_id,
                    into_kind: into_entity.kind,
                    from_id,
                    from_kind: from_entity.kind,
                }
                .into_runtime_error(),
            ));
        }
        EntityMergeValidation::SafetyFloor => {
            validate_entity_merge_floor(&into_entity, &from_entity).map_err(|guard| {
                MergeSqlError::Refusal(EntityMergeRefusal::SafetyFloor(guard).into_runtime_error())
            })?;
        }
        EntityMergeValidation::LegacyKind | EntityMergeValidation::Forced => {}
    }

    // --- Collect edges incident to from_id ---
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    let from_str = from_id.to_string();

    // Namespace-agnostic (khive#1236): edge endpoints resolve by-ID regardless of
    // namespace (ADR-007 Rev 6), and `link` stamps an edge with its *creator's*
    // namespace, not either endpoint's — so an edge incident to `from_id` can live
    // in any namespace. Scoping this collection to the merge's own namespace missed
    // those edges entirely. Each row's own `namespace` column is carried through
    // (`EdgeRow::namespace`) and used for every subsequent SQL op against that row.
    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            outbound.push(edge);
        }
    }

    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, \
                    updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE target_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            inbound.push(edge);
        }
    }

    // Deduplicate by edge ID (a self-edge from_id→from_id appears in both lists).
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }
    let original_edges: HashMap<Uuid, EdgeRow> = all_edges
        .iter()
        .map(|edge| (edge.id, edge.clone()))
        .collect();

    // --- Merge entity fields ---
    let (merged_props, properties_merged) =
        merge_properties(&into_entity.properties, &from_entity.properties, strategy);
    crate::secret_gate::reject_reserved_secret_gate_property(merged_props.as_ref())
        .map_err(MergeSqlError::Refusal)?;
    let merged_name = merge_string_field(&into_entity.name, &from_entity.name, strategy);
    let (merged_description, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            let into_desc = into_entity.description.as_deref().unwrap_or("");
            let from_desc = from_entity.description.as_deref().unwrap_or("");
            if from_desc.is_empty() {
                (into_entity.description.clone(), false)
            } else if into_desc.is_empty() {
                (from_entity.description.clone(), true)
            } else {
                (Some(format!("{}\n\n---\n\n{}", into_desc, from_desc)), true)
            }
        }
        // Description selection follows `content_strategy` directly — it is a
        // deliberate, independently-settable choice, not derived from the
        // entity-field `strategy` (properties/name/tags merge policy).
        ContentMergeStrategy::PreferInto => (into_entity.description.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_entity.description.clone(), false),
    };
    let (merged_tags, tags_unioned) = union_tags(&into_entity.tags, &from_entity.tags);

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let tags_json = serde_json::to_string(&merged_tags).unwrap_or_else(|_| "[]".to_string());

    // Writes are gated on `!dry_run` below, but the loop itself always runs so a
    // dry-run response reports a predictive `edges_rewired` count instead of zero.
    let mut rewired_edge_ids = HashSet::new();
    let mut edges_contract_skipped = 0usize;
    let mut edge_conflict_preimages = Vec::new();
    let mut edges_self_loop_dropped = 0usize;
    let mut self_loop_edge_preimages = Vec::new();
    let mut self_loop_incident_edge_preimages = Vec::new();
    let mut contract_drop_edge_preimages = Vec::new();
    let mut contract_drop_incident_edge_preimages = Vec::new();
    let mut planned_deleted_edge_ids = HashSet::new();
    for edge in all_edges {
        if planned_deleted_edge_ids.contains(&edge.id) {
            continue;
        }
        let raw_src = if edge.source_id == from_id {
            into_id
        } else {
            edge.source_id
        };
        let raw_tgt = if edge.target_id == from_id {
            into_id
        } else {
            edge.target_id
        };
        let relation_typed = edge.relation.parse::<EdgeRelation>().ok();
        // Symmetric relations must be stored with source_uuid < target_uuid.
        // Apply canonicalization so the conflict check and UPDATE both use the canonical form.
        let (new_src, new_tgt) = match relation_typed {
            Some(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
            None => (raw_src, raw_tgt),
        };

        if new_src == new_tgt {
            let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &planned_deleted_edge_ids,
                &mut budget,
                "collecting self-loop cascade rows",
            )?;
            for incident in &incident_edge_preimages {
                planned_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            planned_deleted_edge_ids.insert(edge.id);
            self_loop_edge_preimages.push(edge_row_preimage(&edge)?);
            self_loop_incident_edge_preimages.extend(incident_edge_preimages.iter().cloned());
            edges_self_loop_dropped += 1;
            if !dry_run {
                delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
            }
            continue;
        }

        // Endpoint-contract check (khive#1216): the rewired triple must still pass
        // the same allowlist `link` enforces. `into_id` and `from_id` share `kind`
        // (enforced by the caller), but `entity_type` may differ between them, so a
        // pack rule scoped via `EntityOfType` can accept `from_id`'s edge yet reject
        // the post-rewrite pair against `into_id`. A violating edge is dropped and
        // counted, mirroring the existing dangling-endpoint skip behavior rather
        // than silently writing a contract-violating edge or aborting the merge.
        let contract_ok = match relation_typed {
            // `annotates` targets may be events or edges, which
            // `resolve_merge_edge_endpoint` cannot resolve — evaluate its
            // (unconditional) exemption before endpoint resolution so valid
            // annotates edges are not dropped as unresolvable.
            Some(EdgeRelation::Annotates) => true,
            Some(rel) => {
                let src_info = if new_src == into_id {
                    Some((
                        "entity",
                        into_entity.kind.clone(),
                        into_entity.entity_type.clone(),
                    ))
                } else {
                    resolve_merge_edge_endpoint_budgeted(conn, new_src, &mut budget)?
                };
                let tgt_info = if new_tgt == into_id {
                    Some((
                        "entity",
                        into_entity.kind.clone(),
                        into_entity.entity_type.clone(),
                    ))
                } else {
                    resolve_merge_edge_endpoint_budgeted(conn, new_tgt, &mut budget)?
                };
                match (src_info, tgt_info) {
                    (Some((src_sub, src_kind, src_type)), Some((tgt_sub, tgt_kind, tgt_type))) => {
                        merge_rewire_endpoint_contract_allows(
                            &pack_rules,
                            rel,
                            src_sub,
                            &src_kind,
                            src_type.as_deref(),
                            tgt_sub,
                            &tgt_kind,
                            tgt_type.as_deref(),
                        )
                    }
                    // An endpoint no longer resolves (e.g. concurrently hard-deleted)
                    // — cannot evaluate the contract, so drop rather than assume ok.
                    _ => false,
                }
            }
            // Relation string predates the closed EdgeRelation enum (pre-migration
            // data); leave existing behavior in place rather than guessing.
            None => true,
        };
        if !contract_ok {
            let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &planned_deleted_edge_ids,
                &mut budget,
                "collecting contract-drop cascade rows",
            )?;
            for incident in &incident_edge_preimages {
                planned_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            planned_deleted_edge_ids.insert(edge.id);
            contract_drop_edge_preimages.push(edge_row_preimage(&edge)?);
            contract_drop_incident_edge_preimages.extend(incident_edge_preimages.iter().cloned());
            if !dry_run {
                delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
            }
            tracing::warn!(
                edge_id = %edge.id,
                source = %new_src,
                target = %new_tgt,
                relation = %edge.relation,
                "merge_entity: dropping rewired edge — endpoint contract violation post-merge"
            );
            edges_contract_skipped += 1;
            continue;
        }

        let now_ts = chrono::Utc::now().timestamp_micros();
        // Preserve the original edge ID where possible so callers can still get()
        // it by the ID returned from link(): update in-place when there's no
        // conflict; when into_id already owns this (source,target,relation), the
        // incoming (from-side) duplicate is dropped and the existing into-edge is
        // left untouched (ADR-039 `ON CONFLICT ... DO NOTHING` semantics).
        // Check for a conflict: does into_id already have this natural key?
        let conflict_id: Option<String> = {
            let conflict_src = new_src.to_string();
            let conflict_tgt = new_tgt.to_string();
            conn.query_row(
                khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                rusqlite::params![
                    &edge.namespace,
                    &conflict_src,
                    &conflict_tgt,
                    &edge.relation,
                    edge.id.to_string(),
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(SqliteError::Rusqlite)?
        };

        if let Some(conflict_id) = conflict_id {
            // A live or soft-deleted row already owns this natural key: drop the
            // incoming duplicate. The surviving row's weight/metadata/deleted_at
            // are never mutated or resurrected. Capture the duplicate and the
            // complete hard-delete cascade before removing either, so the audit
            // event contains enough state to restore every destroyed row.
            let surviving_edge_id = Uuid::parse_str(&conflict_id)
                .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
            let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                conn,
                edge.id,
                &original_edges,
                &planned_deleted_edge_ids,
                &mut budget,
                "collecting conflict cascade rows",
            )?;
            for incident in &incident_edge_preimages {
                planned_deleted_edge_ids.insert(incident.id);
                rewired_edge_ids.remove(&incident.id);
            }
            planned_deleted_edge_ids.insert(edge.id);
            rewired_edge_ids.insert(edge.id);

            if !dry_run {
                delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
            }
            edge_conflict_preimages.push(MergeEdgeConflictPreimage {
                surviving_edge_id,
                dropped_edge: edge_row_preimage(&edge)?,
                incident_edge_preimages,
            });
        } else {
            if dry_run {
                rewired_edge_ids.insert(edge.id);
                continue;
            }
            let changed = conn.execute(
                "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                rusqlite::params![
                    new_src.to_string(),
                    new_tgt.to_string(),
                    now_ts,
                    &edge.namespace,
                    edge.id.to_string(),
                ],
            )?;
            if changed > 0 {
                rewired_edge_ids.insert(edge.id);
            }
        }
    }
    let edges_rewired = rewired_edge_ids.len();

    if !dry_run {
        // UPDATE only the merged fields — a full-row INSERT OR REPLACE silently
        // nulls any column missing from its list (entity_type and the former
        // entity-owned content_ref were lost this way; khive#1214). Attachments
        // now live in their own table and this targeted UPDATE leaves them alone.
        conn.execute(
            "UPDATE entities SET version = version + 1, \
                 name = ?1, description = ?2, properties = ?3, tags = ?4, \
                 updated_at = ?5, merged_into = NULL, merge_event_id = NULL \
             WHERE namespace = ?6 AND id = ?7",
            rusqlite::params![
                &merged_name,
                &merged_description,
                &props_str,
                &tags_json,
                now,
                &namespace,
                &into_str,
            ],
        )?;

        // Body formula mirrors entity_fts_document (the canonical constructor):
        // this path is sync/spawn_blocking so it can't call it directly, but
        // must stay field-identical.
        let fts_body = match &merged_description {
            Some(d) if !d.is_empty() => format!("{} {}", merged_name, d),
            _ => merged_name.clone(),
        };
        let kind_str = SubstrateKind::Entity.to_string();
        let fts_map = khive_db::stores::text::rowid_map_table(&fts_table);

        // `into`'s old FTS row (via the map, not a namespace/subject_id
        // scan), then the new merged row, then the map upsert to the new
        // rowid. No separate map-row delete first: `INSERT OR REPLACE`
        // overwrites it in place (see `delete_document_statement`'s doc
        // comment in khive-db).
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {} \
                (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                &kind_str,
                &merged_name,
                &fts_body,
                &tags_json,
                &namespace,
                &props_str,
                now,
                &into_entity.kind,
            ],
        )?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO {fts_map} (namespace, subject_id, rowid) \
                 VALUES (?1, ?2, last_insert_rowid())"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;

        // `from`'s FTS row is gone for good (merged away, not reinserted) —
        // its map row must be removed too, or it would keep pointing at a
        // rowid the DELETE above already reclaimed.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &from_str],
        )?;
        conn.execute(
            &format!("DELETE FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![&namespace, &from_str],
        )?;

        khive_db::stores::vectors::delete_subject_from_vector_tables(
            conn,
            &vec_tables,
            from_id,
            &namespace,
        )?;

        conn.execute(
            "UPDATE entities \
             SET deleted_at = ?1, merged_into = ?2, merge_event_id = ?3, updated_at = ?1, version = version + 1 \
             WHERE namespace = ?4 AND id = ?5 AND deleted_at IS NULL",
            rusqlite::params![
                now,
                into_str,
                merge_event_id.to_string(),
                &namespace,
                &from_str,
            ],
        )?;
    }

    let updated_entity = Entity {
        id: into_id,
        namespace,
        kind: into_entity.kind,
        entity_type: into_entity.entity_type,
        name: merged_name,
        description: merged_description,
        properties: merged_props,
        tags: merged_tags,
        created_at: into_entity.created_at,
        updated_at: now,
        deleted_at: into_entity.deleted_at,
        merged_into: None,
        merge_event_id: None,
        version: if dry_run {
            into_entity.version
        } else {
            into_entity
                .version
                .checked_add(1)
                .ok_or_else(|| SqliteError::InvalidData("entity version overflow".into()))?
        },
        content_ref: into_entity.content_ref,
    };

    let summary = MergeSummary {
        kept_id: into_id,
        removed_id: from_id,
        edges_rewired,
        edges_self_loop_dropped,
        self_loop_edge_preimages,
        self_loop_incident_edge_preimages,
        edges_contract_skipped,
        contract_drop_edge_preimages,
        contract_drop_incident_edge_preimages,
        edge_conflict_preimages,
        properties_merged,
        tags_unioned,
        content_appended,
        dry_run,
        tx_budget: budget.report(),
        embedding_truncation: Default::default(),
        post_commit_reindex_error: None,
    };
    // The event is the only durable copy of destructive edge preimages. An
    // insertion failure must abort this transaction along with the merge.
    if !dry_run {
        if let Some(context) = event_context {
            append_merge_event_in_transaction(conn, context, &summary, &updated_entity.namespace)?;
        }
    }
    Ok((summary, updated_entity))
}

// ---------------------------------------------------------------------------
// Note merge SQL helpers
// ---------------------------------------------------------------------------

/// Cheap SQL-side byte-length probe for one merge note — see
/// [`probe_merge_entity_bytes`] for why this runs before
/// [`read_merge_note`]'s full column copy and JSON parse, and why each
/// column is cast to BLOB before `LENGTH()`.
fn probe_merge_note_bytes(conn: &rusqlite::Connection, id: Uuid) -> Result<usize, SqliteError> {
    let id_str = id.to_string();
    let len: Option<i64> = conn
        .query_row(
            "SELECT COALESCE(LENGTH(CAST(name AS BLOB)), 0) \
                    + LENGTH(CAST(content AS BLOB)) \
                    + COALESCE(LENGTH(CAST(properties AS BLOB)), 0) \
             FROM notes WHERE id = ?1 AND deleted_at IS NULL",
            rusqlite::params![id_str],
            |row| row.get(0),
        )
        .optional()
        .map_err(SqliteError::Rusqlite)?;
    Ok(128_usize.saturating_add(len.unwrap_or(0).max(0) as usize))
}

/// Read one note row by ID within a namespace, returning `SqliteError` on missing/wrong-ns.
fn read_merge_note(
    conn: &rusqlite::Connection,
    id: Uuid,
    namespace: &str,
) -> Result<khive_storage::note::Note, SqliteError> {
    use khive_storage::note::Note;
    let id_str = id.to_string();
    let mut stmt = conn.prepare(
        "SELECT id, namespace, kind, status, name, content, salience, decay_factor, \
         expires_at, properties, created_at, updated_at, deleted_at, key, version \
         FROM notes WHERE id = ?1 AND deleted_at IS NULL",
    )?;
    let mut rows = stmt.query(rusqlite::params![id_str])?;
    let row = rows
        .next()?
        .ok_or_else(|| SqliteError::InvalidData(format!("note {id} not found")))?;

    let id_s: String = row.get(0)?;
    let ns: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    let name: Option<String> = row.get(4)?;
    let content: String = row.get(5)?;
    let salience: Option<f64> = row.get(6)?;
    let decay_factor: Option<f64> = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    let properties_str: Option<String> = row.get(9)?;
    let created_at: i64 = row.get(10)?;
    let updated_at: i64 = row.get(11)?;
    let deleted_at: Option<i64> = row.get(12)?;
    let key: Option<String> = row.get(13)?;
    let version: i64 = row.get(14)?;

    if ns != namespace {
        return Err(SqliteError::InvalidData(format!(
            "note {id} belongs to namespace '{ns}', not '{namespace}'"
        )));
    }

    let note_id = Uuid::parse_str(&id_s).map_err(|e| SqliteError::InvalidData(e.to_string()))?;
    let properties: Option<serde_json::Value> = properties_str
        .map(|s| serde_json::from_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string())))
        .transpose()?;

    Ok(Note {
        id: note_id,
        namespace: ns,
        kind,
        status,
        name,
        content,
        salience,
        decay_factor,
        expires_at,
        properties,
        created_at,
        updated_at,
        deleted_at,
        key,
        version,
    })
}

fn max_option_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

fn append_merge_history(props: Option<Value>, entry: Value) -> Result<Option<Value>, SqliteError> {
    use serde_json::{json, Map};
    let mut obj: Map<String, Value> = match props {
        Some(Value::Object(m)) => m,
        Some(other) => {
            let mut m = Map::new();
            m.insert("_value".into(), other);
            m
        }
        None => Map::new(),
    };
    let history = obj
        .entry("_merge_history".to_string())
        .or_insert_with(|| json!([]));
    if let Value::Array(arr) = history {
        arr.push(entry);
    }
    Ok(Some(Value::Object(obj)))
}

/// All note merge SQL on one connection inside a `BEGIN IMMEDIATE` transaction.
///
/// Reads both notes (must have same `kind`), rewires/drops incident edges, merges content
/// per `content_strategy`, tombstones `from`. Returns the updated `into` note for async
/// re-embedding.
///
/// When `dry_run` is true, all reads and computations are performed but no writes are issued.
// REASON: note merge additionally requires a content_strategy parameter versus entity merge;
// same sync/async boundary rationale as merge_entity_sql applies here.
#[allow(clippy::too_many_arguments)]
fn merge_note_sql(
    conn: &rusqlite::Connection,
    namespace: String,
    fts_table: String,
    vec_tables: Vec<String>,
    into_id: Uuid,
    from_id: Uuid,
    strategy: EntityDedupMergePolicy,
    content_strategy: ContentMergeStrategy,
    dry_run: bool,
    pack_rules: Vec<EdgeEndpointRule>,
    preserve_owner_established: bool,
    limits: MergeTxLimits,
    event_context: Option<MergeEventContext>,
    guard: Option<NoteMergeGuard>,
) -> Result<(MergeSummary, khive_storage::note::Note), MergeSqlError> {
    let mut budget = MergeTxBudget::new(limits);
    // Same accounting as `merge_entity_sql`: config-scaled fanout in bytes only.
    budget.charge(
        0,
        vec_tables.iter().map(String::len).sum::<usize>()
            + pack_rules.len() * std::mem::size_of::<EdgeEndpointRule>(),
        "preparing pack and vector fanout",
    )?;

    budget.charge(
        1,
        probe_merge_note_bytes(conn, into_id)?,
        "reading merge records",
    )?;
    let into_note = read_merge_note(conn, into_id, &namespace)?;
    budget.charge(
        1,
        probe_merge_note_bytes(conn, from_id)?,
        "reading merge records",
    )?;
    let from_note = read_merge_note(conn, from_id, &namespace)?;
    if let Some(guard) = guard.as_ref() {
        guard.enforce(conn, &namespace, &into_note, &from_note, &mut budget)?;
    }

    // Preimages are read in the same guarded unit as the eventual mutation.
    // Checking only the fold would allow a stamp to be discarded by a merge.
    for properties in [&into_note.properties, &from_note.properties] {
        crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())
            .map_err(MergeSqlError::Refusal)?;
    }

    if into_note.kind != from_note.kind {
        return Err(SqliteError::InvalidData(format!(
            "cannot merge notes of different kinds: {} vs {}",
            into_note.kind, from_note.kind
        ))
        .into());
    }

    // A quarantined message participates in no merges, in either role. Folding
    // its content into an ordinary message would retain the body while the
    // marker restoration below drops the `quarantined` disposition — laundering
    // quarantined transport content into an unmarked record. Release is the
    // channel-ingest path's decision, never a side effect of curation.
    if into_note.kind == "message"
        && (message_is_quarantined(&into_note) || message_is_quarantined(&from_note))
    {
        return Err(SqliteError::InvalidData(
            "cannot merge a quarantined message: quarantine disposition is              transport-owned and must be released by the channel-ingest path              before the content can be folded into another record"
                .to_string(),
        ).into());
    }

    let now = chrono::Utc::now().timestamp_micros();
    let into_str = into_id.to_string();
    let from_str = from_id.to_string();

    // Collect edges incident to from_id.
    let parse_id =
        |s: String| Uuid::parse_str(&s).map_err(|e| SqliteError::InvalidData(e.to_string()));

    // Namespace-agnostic (khive#1236): see the equivalent comment in
    // `merge_entity_sql` — edge endpoints resolve by-ID regardless of namespace.
    let mut outbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE source_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            outbound.push(edge);
        }
    }
    let mut inbound: Vec<EdgeRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, namespace, source_id, target_id, relation, weight, created_at, updated_at, deleted_at, target_backend, metadata \
             FROM graph_edges WHERE target_id = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![&from_str])?;
        while let Some(row) = rows.next()? {
            let edge = EdgeRow {
                id: parse_id(row.get(0)?)?,
                namespace: row.get(1)?,
                source_id: parse_id(row.get(2)?)?,
                target_id: parse_id(row.get(3)?)?,
                relation: row.get(4)?,
                weight: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                deleted_at: row.get(8)?,
                target_backend: row.get(9)?,
                metadata: row.get(10)?,
            };
            budget.charge(1, edge_row_budget_bytes(&edge), "collecting incident edges")?;
            inbound.push(edge);
        }
    }
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut all_edges: Vec<EdgeRow> = Vec::new();
    for edge in outbound.into_iter().chain(inbound) {
        if seen.insert(edge.id) {
            all_edges.push(edge);
        }
    }
    let original_edges: HashMap<Uuid, EdgeRow> = all_edges
        .iter()
        .map(|edge| (edge.id, edge.clone()))
        .collect();

    // Merge note fields.
    let (merged_content, content_appended) = match content_strategy {
        ContentMergeStrategy::Append => {
            if from_note.content.is_empty() {
                (into_note.content.clone(), false)
            } else {
                (
                    format!("{}\n\n---\n\n{}", into_note.content, from_note.content),
                    true,
                )
            }
        }
        ContentMergeStrategy::PreferInto => (into_note.content.clone(), false),
        ContentMergeStrategy::PreferFrom => (from_note.content.clone(), false),
    };

    let merged_name = match strategy {
        EntityDedupMergePolicy::PreferFrom => from_note.name.clone().or(into_note.name.clone()),
        _ => into_note.name.clone().or(from_note.name.clone()),
    };

    let (mut merged_props, _) =
        merge_properties(&into_note.properties, &from_note.properties, strategy);

    // A merge folds two records together; it does not transfer attribution.
    // On a pack-owned note kind the into-note's owned identity properties are
    // restored after the fold, under every strategy including `PreferFrom`, so
    // the surviving row still says who wrote it.
    if preserve_owner_established {
        preserve_owner_established_properties(&into_note.properties, &mut merged_props);
    }
    preserve_property_keys(
        kind_owned_properties(&into_note.kind),
        &into_note.properties,
        &mut merged_props,
    );

    // Recomputed from the final retained properties rather than carried
    // forward from the fold's own count. The fold's count and post-
    // restoration reality diverge whenever an owner-established key holds a
    // nested object: `union` recurses into it and counts the absorbed
    // note's leaf as merged, but restoration then reverts the whole key,
    // and the fold's flat "keys contributed" number cannot express a
    // partial reversal of a nested contribution. Diffing the final object
    // against the into-note's pre-merge properties sidesteps that fold/
    // restoration coupling entirely.
    let properties_merged = count_new_property_keys(
        into_note.properties.as_ref(),
        merged_props.as_ref(),
        strategy,
    );

    let mut merge_history_entry = serde_json::json!({
        "merged_from": from_id.to_string(),
        "merged_at": now,
        "strategy": format!("{:?}", strategy),
        "content_strategy": format!("{:?}", content_strategy),
    });
    if let Some(guard) = guard.as_ref() {
        guard.apply_to_survivor(
            &into_note.kind,
            preserve_owner_established,
            &mut merged_props,
            &mut merge_history_entry,
        )?;
    }
    let merged_props = append_merge_history(merged_props, merge_history_entry)?;
    crate::secret_gate::reject_reserved_secret_gate_property(merged_props.as_ref())
        .map_err(MergeSqlError::Refusal)?;

    let merged_salience = max_option_f64(into_note.salience, from_note.salience);
    let merged_expires_at = match (into_note.expires_at, from_note.expires_at) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let props_str = merged_props
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    let (due_key, due_source) = khive_db::stores::note::note_due_key_values(&merged_props);

    // The loop always runs so a dry-run reports a predictive `edges_rewired`
    // count instead of zero (mirrors the entity merge path).
    let mut rewired_edge_ids = HashSet::new();
    let mut edges_contract_skipped = 0usize;
    let mut edge_conflict_preimages = Vec::new();
    let mut edges_self_loop_dropped = 0usize;
    let mut self_loop_edge_preimages = Vec::new();
    let mut self_loop_incident_edge_preimages = Vec::new();
    let mut contract_drop_edge_preimages = Vec::new();
    let mut contract_drop_incident_edge_preimages = Vec::new();
    let mut planned_deleted_edge_ids = HashSet::new();
    {
        for edge in all_edges {
            if planned_deleted_edge_ids.contains(&edge.id) {
                continue;
            }
            let raw_src = if edge.source_id == from_id {
                into_id
            } else {
                edge.source_id
            };
            let raw_tgt = if edge.target_id == from_id {
                into_id
            } else {
                edge.target_id
            };
            let relation_typed = edge.relation.parse::<EdgeRelation>().ok();
            // Canonicalize symmetric relations before conflict check + UPDATE.
            let (new_src, new_tgt) = match relation_typed {
                Some(rel) => canonical_edge_endpoints(rel, raw_src, raw_tgt),
                None => (raw_src, raw_tgt),
            };
            if new_src == new_tgt {
                let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &planned_deleted_edge_ids,
                    &mut budget,
                    "collecting self-loop cascade rows",
                )?;
                for incident in &incident_edge_preimages {
                    planned_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                planned_deleted_edge_ids.insert(edge.id);
                self_loop_edge_preimages.push(edge_row_preimage(&edge)?);
                self_loop_incident_edge_preimages.extend(incident_edge_preimages.iter().cloned());
                edges_self_loop_dropped += 1;
                if !dry_run {
                    delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
                }
                continue;
            }

            // Endpoint-contract check (khive#1216/#1236): see the equivalent
            // block in `merge_entity_sql` for the full rationale. Here the
            // rewiring endpoint is a note (`into_id`'s kind, substrate "note"),
            // not an entity.
            let contract_ok = match relation_typed {
                // Same rationale as the entity-merge path: annotates targets may
                // be events or edges, unresolvable by substrate lookup — the
                // exemption must precede endpoint resolution.
                Some(EdgeRelation::Annotates) => true,
                Some(rel) => {
                    let src_info = if new_src == into_id {
                        Some(("note", into_note.kind.clone(), None))
                    } else {
                        resolve_merge_edge_endpoint_budgeted(conn, new_src, &mut budget)?
                    };
                    let tgt_info = if new_tgt == into_id {
                        Some(("note", into_note.kind.clone(), None))
                    } else {
                        resolve_merge_edge_endpoint_budgeted(conn, new_tgt, &mut budget)?
                    };
                    match (src_info, tgt_info) {
                        (
                            Some((src_sub, src_kind, src_type)),
                            Some((tgt_sub, tgt_kind, tgt_type)),
                        ) => merge_rewire_endpoint_contract_allows(
                            &pack_rules,
                            rel,
                            src_sub,
                            &src_kind,
                            src_type.as_deref(),
                            tgt_sub,
                            &tgt_kind,
                            tgt_type.as_deref(),
                        ),
                        _ => false,
                    }
                }
                None => true,
            };
            if !contract_ok {
                let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &planned_deleted_edge_ids,
                    &mut budget,
                    "collecting contract-drop cascade rows",
                )?;
                for incident in &incident_edge_preimages {
                    planned_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                planned_deleted_edge_ids.insert(edge.id);
                contract_drop_edge_preimages.push(edge_row_preimage(&edge)?);
                contract_drop_incident_edge_preimages
                    .extend(incident_edge_preimages.iter().cloned());
                if !dry_run {
                    delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
                }
                tracing::warn!(
                    edge_id = %edge.id,
                    source = %new_src,
                    target = %new_tgt,
                    relation = %edge.relation,
                    "merge_note: dropping rewired edge — endpoint contract violation post-merge"
                );
                edges_contract_skipped += 1;
                continue;
            }

            let now_ts = chrono::Utc::now().timestamp_micros();
            let conflict_id: Option<String> = {
                let conflict_src = new_src.to_string();
                let conflict_tgt = new_tgt.to_string();
                conn.query_row(
                    khive_db::stores::graph::EDGE_SYMMETRIC_CONFLICT_PROBE_SQL,
                    rusqlite::params![
                        &edge.namespace,
                        &conflict_src,
                        &conflict_tgt,
                        &edge.relation,
                        edge.id.to_string(),
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(SqliteError::Rusqlite)?
            };

            if let Some(conflict_id) = conflict_id {
                // A live or soft-deleted row already owns this natural key: drop
                // the incoming duplicate (ADR-039 `ON CONFLICT ... DO NOTHING`).
                // The surviving row's weight/metadata/deleted_at are never
                // mutated or resurrected. Match hard `delete_edge`: cascade
                // incident annotations, and preserve every removed row first.
                let surviving_edge_id = Uuid::parse_str(&conflict_id)
                    .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
                let incident_edge_preimages = collect_merge_drop_incident_edge_preimages(
                    conn,
                    edge.id,
                    &original_edges,
                    &planned_deleted_edge_ids,
                    &mut budget,
                    "collecting conflict cascade rows",
                )?;
                for incident in &incident_edge_preimages {
                    planned_deleted_edge_ids.insert(incident.id);
                    rewired_edge_ids.remove(&incident.id);
                }
                planned_deleted_edge_ids.insert(edge.id);
                rewired_edge_ids.insert(edge.id);

                if !dry_run {
                    delete_merge_drop_edges(conn, &edge, &incident_edge_preimages)?;
                }
                edge_conflict_preimages.push(MergeEdgeConflictPreimage {
                    surviving_edge_id,
                    dropped_edge: edge_row_preimage(&edge)?,
                    incident_edge_preimages,
                });
            } else {
                if dry_run {
                    rewired_edge_ids.insert(edge.id);
                    continue;
                }
                let changed = conn.execute(
                    "UPDATE graph_edges SET \
                     source_id = ?1, target_id = ?2, updated_at = ?3 \
                     WHERE namespace = ?4 AND id = ?5",
                    rusqlite::params![
                        new_src.to_string(),
                        new_tgt.to_string(),
                        now_ts,
                        &edge.namespace,
                        edge.id.to_string(),
                    ],
                )?;
                if changed > 0 {
                    rewired_edge_ids.insert(edge.id);
                }
            }
        }
    }
    let edges_rewired = rewired_edge_ids.len();

    if !dry_run {
        conn.prepare_cached(khive_db::stores::note::NOTE_UPSERT_SQL)?
            .execute(rusqlite::params![
                &into_str,
                &namespace,
                &into_note.kind,
                &into_note.status,
                &merged_name,
                &merged_content,
                merged_salience,
                into_note.decay_factor,
                merged_expires_at,
                &props_str,
                into_note.created_at,
                now,
                into_note.deleted_at,
                &into_note.key,
                &due_key,
                &due_source,
            ])?;

        let fts_map = khive_db::stores::text::rowid_map_table(&fts_table);

        // `into`'s old FTS row (via the map), then the new merged row, then
        // the map upsert to the new rowid — see `merge_entity_sql`'s matching
        // comment for why no separate map-row delete is needed here.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;
        // Derive FTS scalars through the shared constructor so this raw SQL path
        // is field-identical to TextSearch::upsert_document: critically, `title`
        // is an empty string (not SQL NULL) for nameless notes, so get_document
        // round-trips None <-> "" correctly.
        let fts_merged = {
            let mut merged_note = Note::new(&namespace, &*into_note.kind, &*merged_content);
            merged_note.id = into_id;
            merged_note.name = merged_name.clone();
            merged_note.properties = merged_props.clone();
            merged_note.updated_at = now;
            note_fts_scalars(&merged_note)
        };
        conn.execute(
            &format!(
                "INSERT INTO {} \
                (subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                fts_table
            ),
            rusqlite::params![
                &into_str,
                SubstrateKind::Note.to_string(),
                &fts_merged.title,
                &fts_merged.body,
                &fts_merged.tags,
                &namespace,
                &fts_merged.metadata,
                fts_merged.updated_at_micros,
                &fts_merged.record_kind,
            ],
        )?;
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO {fts_map} (namespace, subject_id, rowid) \
                 VALUES (?1, ?2, last_insert_rowid())"
            ),
            rusqlite::params![&namespace, &into_str],
        )?;

        // `from`'s FTS row is gone for good — remove its map row too.
        conn.execute(
            &format!(
                "DELETE FROM {fts_table} WHERE rowid IN \
                 (SELECT rowid FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2) \
                 AND namespace = ?1 AND subject_id = ?2"
            ),
            rusqlite::params![&namespace, &from_str],
        )?;
        conn.execute(
            &format!("DELETE FROM {fts_map} WHERE namespace = ?1 AND subject_id = ?2"),
            rusqlite::params![&namespace, &from_str],
        )?;

        khive_db::stores::vectors::delete_subject_from_vector_tables(
            conn,
            &vec_tables,
            from_id,
            &namespace,
        )?;

        conn.execute(
            "UPDATE notes SET status = 'deleted', deleted_at = ?1, updated_at = ?1 \
             WHERE namespace = ?2 AND id = ?3 AND deleted_at IS NULL",
            rusqlite::params![now, &namespace, &from_str],
        )?;
    }

    let updated_note = khive_storage::note::Note {
        id: into_id,
        namespace: namespace.clone(),
        kind: into_note.kind.clone(),
        status: into_note.status.clone(),
        name: merged_name,
        content: merged_content,
        salience: merged_salience,
        decay_factor: into_note.decay_factor,
        expires_at: merged_expires_at,
        properties: merged_props,
        created_at: into_note.created_at,
        updated_at: now,
        deleted_at: into_note.deleted_at,
        key: into_note.key.clone(),
        version: conn.query_row(
            "SELECT version FROM notes WHERE id = ?1",
            [&into_str],
            |row| row.get(0),
        )?,
    };

    let summary = MergeSummary {
        kept_id: into_id,
        removed_id: from_id,
        edges_rewired,
        edges_self_loop_dropped,
        self_loop_edge_preimages,
        self_loop_incident_edge_preimages,
        edges_contract_skipped,
        contract_drop_edge_preimages,
        contract_drop_incident_edge_preimages,
        edge_conflict_preimages,
        properties_merged,
        tags_unioned: 0,
        content_appended,
        dry_run,
        tx_budget: budget.report(),
        embedding_truncation: Default::default(),
        post_commit_reindex_error: None,
    };
    if !dry_run {
        if let Some(context) = event_context {
            append_merge_event_in_transaction(conn, context, &summary, &updated_note.namespace)?;
        }
    }
    Ok((summary, updated_note))
}

// ---------------------------------------------------------------------------
// Merge helpers (pure functions — easier to unit test)
// ---------------------------------------------------------------------------

/// `pub(crate)` so `crate::atomic_prepare::prepare_merge` can reuse this exact
/// field-fold semantics for atomic/non-atomic parity.
pub(crate) fn merge_string_field(
    into: &str,
    from: &str,
    strategy: EntityDedupMergePolicy,
) -> String {
    match strategy {
        EntityDedupMergePolicy::PreferInto | EntityDedupMergePolicy::Union => into.to_string(),
        EntityDedupMergePolicy::PreferFrom => from.to_string(),
    }
}

/// Property keys on a pack-owned note that the owning pack establishes and
/// then reads back to decide something structural about the record.
///
/// The test for membership is that both halves hold: the key is written
/// under the owner's authority rather than from caller input, AND its value
/// is read to decide identity, grouping, routing, lifecycle, visibility,
/// authorization, deduplication, or membership. `from_actor`, `direction` and
/// `sent_at` answer "who wrote this, in which direction, when"; `outbound_ref`
/// and `thread_id` answer "which record is this one's author-side original,
/// and which conversation does it belong to". `subject` is reproduced
/// verbatim when a record is re-emitted; `wire_message_id` and `external_id`
/// are the author-side citation and correlation key a reply is routed
/// against. The set is therefore not "keys that identify a party" — it is
/// "keys the owner established and later trusts".
///
/// Naming one of these in a caller-supplied `properties` patch is refused by
/// `update` on a pack-owned kind (see [`owner_established_property_named_in`]).
///
/// `to_actor` belongs here alongside `from_actor`: comm establishes it at
/// send time from the `to=` param, and `comm.read` trusts a present string
/// value to decide whether the caller is the addressee, failing open only
/// when the key is absent or non-string. A caller must not be able to
/// retarget a delivered message's addressee via a patch that names no other
/// currently-protected key.
///
/// Membership here governs writes to an EXISTING record only. Introducing one
/// of these keys at create time is a separate question and is not addressed
/// by this constant.
pub(crate) const OWNER_ESTABLISHED_PROPERTIES: &[&str] = &[
    "from_actor",
    "to_actor",
    "direction",
    "sent_at",
    "outbound_ref",
    "thread_id",
    "subject",
    "wire_message_id",
    "external_id",
];

/// Kind-specific identity that generic updates cannot patch and merges must
/// retain from the surviving record. Message transport evidence belongs to
/// `comm.ingest`; health coordinates determine the UUID used by `comm.heartbeat`.
/// Unlike OWNER_ESTABLISHED_PROPERTIES, these names remain ordinary metadata
/// on other kinds, including tasks and memories.
const KIND_OWNED_PROPERTIES: &[(&str, &[&str])] = &[
    (
        "message",
        &[
            "quarantined",
            "channel_kind",
            "channel_slug",
            "delivery_hold",
            "delivery_hold_reason",
            "delivery_hold_at",
            "external_id_diagnostic_note_id",
        ],
    ),
    ("channel_health", &["channel_kind", "channel_slug"]),
];

pub(crate) fn kind_owned_properties(kind: &str) -> &'static [&'static str] {
    KIND_OWNED_PROPERTIES
        .iter()
        .find_map(|(owned_kind, keys)| (*owned_kind == kind).then_some(*keys))
        .unwrap_or(&[])
}

/// Whether a stored message note carries a live quarantine disposition.
///
/// The marker is written by transports as JSON `true` and by some channel
/// adapters as the string `"true"`; both spellings are live in stored data
/// (`comm.health` counts both). Any present value other than an explicit
/// boolean `false` or string `"false"` reads as quarantined, so an unexpected
/// encoding fails closed.
fn message_is_quarantined(note: &khive_storage::note::Note) -> bool {
    let Some(Value::Object(map)) = note.properties.as_ref() else {
        return false;
    };
    match map.get("quarantined") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => value != "false",
        Some(_) => true,
    }
}

/// The first [`OWNER_ESTABLISHED_PROPERTIES`] key a caller-supplied
/// `properties` patch names, if any.
///
/// Naming a key is the whole test: `update_note` folds the patch with
/// `PreferFrom`, so a named key overwrites the stored value and an unnamed one
/// leaves it untouched. A non-object patch names nothing.
pub(crate) fn owner_established_property_named_in(patch: &Value) -> Option<&'static str> {
    let Value::Object(map) = patch else {
        return None;
    };
    OWNER_ESTABLISHED_PROPERTIES
        .iter()
        .copied()
        .find(|key| map.contains_key(*key))
}

/// Restore the into-note's [`OWNER_ESTABLISHED_PROPERTIES`] into `merged`
/// after a property fold.
///
/// A key absent on the into-note is removed from `merged` rather than left as
/// the from-note's value: a record that carried no owner-established value
/// must not acquire one by being merged into. That applies to grouping as much
/// as to attribution — a note with no `thread_id` must not join a conversation
/// because another note was folded into it.
///
/// A fold can also yield a value that is not an object at all: `merge_json`
/// applies a non-object `from` directly under `PreferFrom`, replacing the
/// into-note's whole object with a scalar. A scalar cannot carry the
/// owner-established keys, so there is nothing to restore them into and they
/// would be erased. The into-note's properties are kept instead — the scalar
/// contributes no key that could coexist with them, so nothing the fold
/// intended is lost.
///
/// This function only restores values; callers that need to report how many
/// properties genuinely survived a merge should diff the final result
/// against the into-note's pre-merge properties (see
/// [`count_new_property_keys`]) rather than try to track the restoration as
/// a correction to the fold's own count — a nested owner-established value
/// (an object) makes that correction ill-defined, since the fold's flat
/// "keys contributed" number cannot express a partial reversal of a nested
/// contribution.
pub(crate) fn preserve_owner_established_properties(
    into: &Option<Value>,
    merged: &mut Option<Value>,
) {
    preserve_property_keys(OWNER_ESTABLISHED_PROPERTIES, into, merged);
}

fn preserve_property_keys(keys: &[&str], into: &Option<Value>, merged: &mut Option<Value>) {
    if !matches!(merged, Some(Value::Object(_))) {
        let Some(Value::Object(into_map)) = into else {
            return;
        };
        let owned_on_into = keys.iter().any(|key| into_map.contains_key(*key));
        if owned_on_into {
            *merged = into.clone();
        }
        return;
    }
    let Some(Value::Object(merged_map)) = merged.as_mut() else {
        return;
    };
    let into_map = match into {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    for key in keys {
        match into_map.and_then(|m| m.get(*key)) {
            Some(value) => {
                // Already present on `into` — restore it verbatim.
                merged_map.insert((*key).to_string(), value.clone());
            }
            None => {
                // Absent from `into` — a value here came from `from` and
                // must not survive the merge.
                merged_map.remove(*key);
            }
        }
    }
}

/// Count properties present in `final_value` that are new relative to
/// `original` — the same "did this key actually get added" question
/// [`merge_json`]'s fold answers, but computed from what the record finally
/// holds rather than carried forward through the fold-then-restore pipeline.
///
/// A key present in both `original` and `final_value` is never counted, even
/// when its value changed — this matches `merge_json`'s own rule that an
/// overwrite of a key already present on `into` is not a merged addition.
/// Nested objects recurse only when the key exists on both sides (mirroring
/// `merge_json`'s `Union` recursion); a key that is wholly new at some level
/// counts once for that level, not once per leaf beneath it.
pub(crate) fn count_new_property_keys(
    original: Option<&Value>,
    final_value: Option<&Value>,
    strategy: EntityDedupMergePolicy,
) -> usize {
    match (original, final_value) {
        (_, None) => 0,
        (None, Some(Value::Object(map))) => map.len(),
        (None, Some(_)) => 1,
        (Some(Value::Object(orig_map)), Some(Value::Object(final_map))) => {
            count_new_keys_within_object(orig_map, final_map, strategy)
        }
        // The record ended up holding an object where it previously held
        // something else. `merge_json` scores that replacement as ONE
        // contribution however many keys the new object carries, and this arm
        // keeps that rule rather than counting the keys — the alternative
        // silently changes `properties_merged` for ordinary notes, which never
        // enter the restoration path and were being reported correctly by the
        // fold. The rule here is: an empty final object has no contribution
        // left to report, whatever emptied it — restoration removing every
        // owner-established key is one way that happens, but an ordinary
        // `PreferFrom` replacement with an empty object reaches this same arm.
        (Some(_), Some(Value::Object(final_map))) => usize::from(!final_map.is_empty()),
        // Whole-value replacement by a non-object. `merge_json` scores a
        // `PreferFrom` fold that replaces one properties value with a
        // differently-shaped one as a single contribution, and that is the right
        // answer: what the record now holds came from the from-note. A bare 0
        // here would under-report every such replacement, including on note
        // kinds that have no owner-established properties and never enter the
        // restoration path at all. Equal values mean nothing was contributed,
        // which is the `properties: Some(a)` merged with `properties: None`
        // case.
        (Some(orig), Some(final_val)) => usize::from(orig != final_val),
    }
}

/// Per-key counting inside a properties object.
///
/// Deliberately NOT the same rule as the top level: within an object, a key that
/// already exists and is merely overwritten counts 0, matching `merge_json`'s
/// rule that only keys absent from the into-note are counted as added.
///
/// Recursion is STRATEGY-AWARE, and it has to be, because `merge_json` only
/// descends into a same-named nested object under [`Union`]. Under `PreferFrom`
/// an existing top-level key is replaced wholesale, and under `PreferInto` it is
/// kept wholesale; in neither case is anything merged *beneath* that key, so
/// descending here would count a nested value that the fold never treated as a
/// separate contribution. Counting `{"meta":{"old":1}}` merged with
/// `{"meta":{"new":2}}` under `PreferFrom` as 1 is exactly that mistake — one
/// existing property was replaced, none was added.
///
/// [`Union`]: EntityDedupMergePolicy::Union
fn count_new_keys_within_object(
    orig_map: &serde_json::Map<String, Value>,
    final_map: &serde_json::Map<String, Value>,
    strategy: EntityDedupMergePolicy,
) -> usize {
    final_map
        .iter()
        .map(|(key, value)| match orig_map.get(key) {
            None => 1,
            Some(Value::Object(nested_orig))
                if matches!(strategy, EntityDedupMergePolicy::Union) =>
            {
                match value {
                    Value::Object(nested_final) => {
                        count_new_keys_within_object(nested_orig, nested_final, strategy)
                    }
                    _ => 0,
                }
            }
            Some(_) => 0,
        })
        .sum()
}

/// Merge two property objects. Returns (merged, count_of_fields_from_from_that_were_added).
/// `pub(crate)` so `crate::atomic_prepare` can reuse this exact properties-merge
/// semantics when building an `update` write plan's row statement, matching
/// `update_entity`/`update_note`'s own patch behavior byte-for-byte.
pub(crate) fn merge_properties(
    into: &Option<Value>,
    from: &Option<Value>,
    strategy: EntityDedupMergePolicy,
) -> (Option<Value>, usize) {
    match (into, from) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(a.clone()), 0),
        (None, Some(b)) => {
            let count = if let Value::Object(m) = b { m.len() } else { 1 };
            (Some(b.clone()), count)
        }
        (Some(into_val), Some(from_val)) => {
            let (merged, added) = merge_json(into_val, from_val, strategy);
            (Some(merged), added)
        }
    }
}

/// Compare note-update values using the semantics exposed by note readers.
/// `serde_json::Value` already compares objects without depending on insertion
/// order; the top-level `properties.tags` array is compared as an
/// order-independent multiset because readers treat it as a set while
/// preserving duplicate entries as a meaningful representation change.
fn note_update_values_equal(left: &Option<Value>, right: &Option<Value>) -> bool {
    fn equal(left: &Value, right: &Value, is_tags_field: bool, is_properties_object: bool) -> bool {
        match (left, right) {
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, value)| {
                        b.get(key).is_some_and(|other| {
                            equal(value, other, is_properties_object && key == "tags", false)
                        })
                    })
            }
            (Value::Array(a), Value::Array(b)) if is_tags_field => {
                if a.len() != b.len() {
                    return false;
                }
                let mut left = a.iter().map(Value::to_string).collect::<Vec<_>>();
                let mut right = b.iter().map(Value::to_string).collect::<Vec<_>>();
                left.sort_unstable();
                right.sort_unstable();
                left == right
            }
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(left, right)| equal(left, right, false, false))
            }
            _ => left == right,
        }
    }

    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => equal(left, right, false, true),
        _ => false,
    }
}

/// Deep-merge two JSON values per strategy. Returns (merged, keys_contributed_by_from).
fn merge_json(into: &Value, from: &Value, strategy: EntityDedupMergePolicy) -> (Value, usize) {
    match (into, from, strategy) {
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::Union) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v_from) in b {
                if let Some(v_into) = a.get(k) {
                    let (merged, sub_added) =
                        merge_json(v_into, v_from, EntityDedupMergePolicy::Union);
                    result.insert(k.clone(), merged);
                    added += sub_added;
                } else {
                    result.insert(k.clone(), v_from.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferInto) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                if !a.contains_key(k) {
                    result.insert(k.clone(), v.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferFrom) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                result.insert(k.clone(), v.clone());
                if !a.contains_key(k) {
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        // Non-object scalars: apply strategy directly.
        (_into_val, from_val, EntityDedupMergePolicy::PreferFrom) => (from_val.clone(), 1),
        _ => (into.clone(), 0),
    }
}

/// `pub(crate)` so `crate::atomic_prepare::prepare_merge` can reuse this for
/// atomic/non-atomic parity.
pub(crate) fn union_tags(into: &[String], from: &[String]) -> (Vec<String>, usize) {
    let mut seen: HashSet<&str> = into.iter().map(|s| s.as_str()).collect();
    let mut result: Vec<String> = into.to_vec();
    let mut added = 0usize;
    for tag in from {
        if seen.insert(tag.as_str()) {
            result.push(tag.clone());
            added += 1;
        }
    }
    (result, added)
}

// ---------------------------------------------------------------------------
// INLINE TEST JUSTIFICATION: tests here exercise patch/merge helpers and the
// update_note/update_entity paths that share private merge_properties logic.
// Moving them to tests/ would require pub-exporting merge_properties, which is
// an internal invariant not suitable for the public API surface. Broad
// behavioral curation tests live in tests/integration.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod merge_reservation_tests;

#[cfg(test)]
#[path = "curation_tests.rs"]
mod tests;
