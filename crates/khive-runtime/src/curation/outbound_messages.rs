use super::{
    stale_note_snapshot_error, EdgeListFilter, EdgeRelation, FilterOp, KhiveRuntime,
    NamespaceToken, NoteFilter, OutboxSlugFilter, PageRequest, PropertyFilter, RuntimeError,
    RuntimeResult, SqlValue, Uuid, Value,
};

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

    pub(super) async fn mark_outbound_message_claim_failed_from_snapshot(
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
}
