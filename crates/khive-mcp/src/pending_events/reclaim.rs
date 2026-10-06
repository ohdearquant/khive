use super::*;

pub(super) async fn requeue_legacy_claim(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    firing_at: i64,
    selected_properties: &str,
) -> Result<bool> {
    let updated_at = Utc::now().timestamp_micros();
    check_fixed_path_whole_object_snapshot(selected_properties)?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for legacy reclaim")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_remove( \
                        json_set(properties, '$.status', 'pending'), \
                        '$.firing_at', '$.lease_expires_at' \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?2 \
                    AND namespace = ?3 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND (json_extract(properties, '$.firing_at') IS NULL \
                         OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?4) \
                    AND json_extract(properties, '$.dispatch_receipt') IS NULL \
                    AND properties = ?5"
                .to_string(),
            params: vec![
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(firing_at),
                SqlValue::Text(selected_properties.to_string()),
            ],
            label: Some("pending_events_requeue_legacy_claim".into()),
        })
        .await
        .context("pending-events: requeue legacy firing claim")?;
    Ok(rows == 1)
}

pub(super) async fn finalize_corrupt_receipt(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    firing_at: i64,
    properties: &Value,
    expired_at: i64,
    selected_properties: &str,
) -> Result<bool> {
    let mut properties = properties.clone();
    if let Some(object) = properties.as_object_mut() {
        object.remove("firing_at");
        object.remove("lease_expires_at");
    }
    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&properties))?;
    let serialized = serde_json::to_string(&properties)
        .context("pending-events: serialize corrupt receipt failure state")?;
    let updated_at = Utc::now().timestamp_micros();
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for corrupt receipt")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND (json_extract(properties, '$.firing_at') IS NULL \
                         OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5) \
                    AND ( \
                      (json_extract(properties, '$.lease_expires_at') IS NOT NULL \
                       AND CAST(json_extract(properties, '$.lease_expires_at') AS INTEGER) <= ?6) \
                      OR \
                      (json_extract(properties, '$.lease_expires_at') IS NULL \
                       AND (json_extract(properties, '$.firing_at') IS NULL \
                            OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?7)) \
                    ) \
                    AND properties = ?8"
                .to_string(),
            params: vec![
                SqlValue::Text(serialized),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(firing_at),
                SqlValue::Integer(expired_at),
                SqlValue::Integer(expired_at.saturating_sub(LEGACY_STALE_FIRING_TIMEOUT_MICROS)),
                SqlValue::Text(selected_properties.to_string()),
            ],
            label: Some("pending_events_finalize_corrupt_receipt".into()),
        })
        .await
        .context("pending-events: finalize corrupt dispatch receipt")?;
    Ok(rows == 1)
}

/// Reconcile expired firing leases without blindly replaying an invocation.
/// A receipt with a durable outcome resumes finalization; `claimed` is safe to
/// retry because invocation never began; `invoking` is terminally
/// indeterminate because generic verb dispatch cannot prove whether its side
/// effect committed before the claimant disappeared.
pub(super) async fn reclaim_stale_firing_events(
    rt: &KhiveRuntime,
    now_micros: i64,
) -> Result<ReclaimSummary> {
    let legacy_stale_before = now_micros.saturating_sub(LEGACY_STALE_FIRING_TIMEOUT_MICROS);
    let rows = {
        let mut reader = rt
            .sql()
            .reader()
            .await
            .context("pending-events: open SQL reader for expired leases")?;
        reader
            .query_all(SqlStatement {
                sql: "SELECT id, namespace, properties FROM notes \
                      WHERE kind = 'scheduled_event' \
                        AND deleted_at IS NULL \
                        AND json_extract(properties, '$.status') = 'firing' \
                        AND ( \
                          (json_extract(properties, '$.lease_expires_at') IS NOT NULL \
                           AND CAST(json_extract(properties, '$.lease_expires_at') AS INTEGER) <= ?1) \
                          OR \
                          (json_extract(properties, '$.lease_expires_at') IS NULL \
                           AND (json_extract(properties, '$.firing_at') IS NULL \
                                OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?2)) \
                        ) \
                      ORDER BY created_at ASC, id ASC"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now_micros),
                    SqlValue::Integer(legacy_stale_before),
                ],
                label: Some("pending_events_expired_dispatch_leases".into()),
            })
            .await
            .context("pending-events: query expired dispatch leases")?
    };

    let mut summary = ReclaimSummary::default();
    for row in rows {
        let id = match row.get("id") {
            Some(SqlValue::Text(value)) => uuid::Uuid::parse_str(value)
                .with_context(|| format!("pending-events: invalid stale event id {value:?}"))?,
            other => {
                return Err(anyhow::anyhow!(
                    "pending-events: expired lease has invalid id column {other:?}"
                ));
            }
        };
        let namespace = match row.get("namespace") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => {
                return Err(anyhow::anyhow!(
                    "pending-events: expired lease {id} has invalid namespace {other:?}"
                ));
            }
        };
        let selected_properties = match row.get("properties") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => {
                return Err(anyhow::anyhow!(
                    "pending-events: expired lease {id} has invalid properties {other:?}"
                ));
            }
        };
        let mut properties: Value = serde_json::from_str(&selected_properties)
            .with_context(|| format!("pending-events: parse expired receipt for {id}"))?;
        let firing_at = properties
            .get("firing_at")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(receipt) = properties.get("dispatch_receipt").cloned() else {
            match requeue_legacy_claim(rt, &namespace, id, firing_at, &selected_properties).await {
                Ok(true) => {
                    summary.rows += 1;
                    summary.retry_pending += 1;
                    summary.finalized += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        scheduled_event_id = %id,
                        namespace,
                        error = %error,
                        "pending-events: legacy expired-claim recovery failed; continuing"
                    );
                    summary.failed += 1;
                }
            }
            continue;
        };

        let validated = match validate_dispatch_receipt(id, firing_at, &properties, receipt) {
            Ok(validated) => validated,
            Err(validation_error) => {
                let error = format!("{validation_error}; refusing automatic replay");
                let invalid_receipt = properties
                    .get("dispatch_receipt")
                    .cloned()
                    .unwrap_or(Value::Null);
                mark_dispatch_receipt_indeterminate(
                    &mut properties,
                    invalid_receipt,
                    &error,
                    now_micros,
                );
                match finalize_corrupt_receipt(
                    rt,
                    &namespace,
                    id,
                    firing_at,
                    &properties,
                    now_micros,
                    &selected_properties,
                )
                .await
                {
                    Ok(true) => {
                        summary.rows += 1;
                        summary.indeterminate += 1;
                        summary.outcomes_persisted += 1;
                        summary.finalized += 1;
                        summary.failed += 1;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            namespace,
                            error = %error,
                            "pending-events: corrupt-receipt quarantine failed; continuing"
                        );
                        summary.failed += 1;
                    }
                }
                continue;
            }
        };
        let ValidatedDispatchReceipt {
            value: mut receipt,
            occurrence_id,
            invocation_id,
            actor,
            state,
        } = validated;
        let claim = DispatchClaim {
            firing_at,
            occurrence_id,
            invocation_id,
            actor,
        };
        if state == DispatchReceiptState::Claimed {
            let error =
                "dispatch claimant expired before invocation began; occurrence is retryable";
            receipt["state"] = json!(DispatchReceiptState::NotInvoked.as_str());
            receipt["completed_at"] = json!(now_micros);
            receipt["error"] = json!(error);
            receipt["error_payload"] = Value::Null;
            properties["dispatch_receipt"] = receipt;
            properties["status"] = json!("pending");
            match finalize_expired_firing_event(
                rt,
                &namespace,
                id,
                &properties,
                Utc::now().timestamp_micros(),
                &claim,
                RecoverySnapshot {
                    expired_at: now_micros,
                    properties: &selected_properties,
                },
            )
            .await
            {
                Ok(true) => {
                    summary.rows += 1;
                    summary.retry_pending += 1;
                    summary.finalized += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        scheduled_event_id = %id,
                        namespace,
                        error = %error,
                        "pending-events: pre-invocation expired-claim recovery failed; continuing"
                    );
                    summary.failed += 1;
                }
            }
            continue;
        }

        if matches!(
            state,
            DispatchReceiptState::NotInvoked | DispatchReceiptState::Missed
        ) {
            let error = format!(
                "completed dispatch receipt state {} cannot remain attached to a firing row; \
                 refusing automatic replay",
                state.as_str()
            );
            mark_dispatch_receipt_indeterminate(&mut properties, receipt, &error, now_micros);
            match finalize_corrupt_receipt(
                rt,
                &namespace,
                id,
                firing_at,
                &properties,
                now_micros,
                &selected_properties,
            )
            .await
            {
                Ok(true) => {
                    summary.rows += 1;
                    summary.indeterminate += 1;
                    summary.outcomes_persisted += 1;
                    summary.finalized += 1;
                    summary.failed += 1;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        scheduled_event_id = %id,
                        namespace,
                        error = %error,
                        "pending-events: completed pre-invocation receipt quarantine failed; continuing"
                    );
                    summary.failed += 1;
                }
            }
            continue;
        }

        let recovery_persisted_outcome = match state {
            DispatchReceiptState::Invoking => {
                let completion = completion_from_receipt(&receipt);
                receipt["state"] = json!(DispatchReceiptState::Indeterminate.as_str());
                receipt["completed_at"] = json!(now_micros);
                receipt["error"] = json!(match &completion {
                    DispatchCompletion::Indeterminate(error) => error.as_str(),
                    DispatchCompletion::Succeeded => "",
                    DispatchCompletion::Failed(error) => error.as_str(),
                });
                receipt["error_payload"] = Value::Null;
                true
            }
            DispatchReceiptState::Succeeded
            | DispatchReceiptState::Failed
            | DispatchReceiptState::Indeterminate => false,
            DispatchReceiptState::Claimed
            | DispatchReceiptState::NotInvoked
            | DispatchReceiptState::Missed => unreachable!("states handled above"),
        };
        let completion = completion_from_receipt(&receipt);
        let trigger_at_fixed = properties
            .get("trigger_at")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<DateTime<FixedOffset>>().ok());
        let repeat = properties
            .get("repeat")
            .and_then(Value::as_str)
            .map(str::to_string);
        let (final_properties, disposition) = match trigger_at_fixed {
            Some(trigger_at_fixed) => final_properties_after_dispatch(
                properties,
                receipt,
                &completion,
                trigger_at_fixed.with_timezone(&Utc),
                *trigger_at_fixed.offset(),
                &repeat,
            ),
            None => {
                properties["dispatch_receipt"] = receipt;
                properties["status"] = json!("failed");
                let (error_key, error_at_key) = dispatch_error_property_keys(&properties);
                properties[error_key] =
                    json!("cannot recover dispatch outcome: trigger_at is invalid");
                properties[error_at_key] = json!(Utc::now().to_rfc3339());
                (properties, FinalDisposition::Indeterminate)
            }
        };
        if disposition == FinalDisposition::RecurrenceFailed {
            tracing::error!(
                scheduled_event_id = %id,
                namespace,
                error = %final_properties["recurrence_error"].as_str().unwrap_or(UNADVANCEABLE_REPEAT),
                "pending-events: recurrence advancement failed during expired-claim recovery"
            );
        }
        match finalize_expired_firing_event(
            rt,
            &namespace,
            id,
            &final_properties,
            Utc::now().timestamp_micros(),
            &claim,
            RecoverySnapshot {
                expired_at: now_micros,
                properties: &selected_properties,
            },
        )
        .await
        {
            Ok(true) => {
                summary.rows += 1;
                if recovery_persisted_outcome {
                    summary.outcomes_persisted += 1;
                }
                summary.finalized += 1;
                match disposition {
                    FinalDisposition::Fired => summary.fired += 1,
                    FinalDisposition::Advanced => summary.advanced += 1,
                    FinalDisposition::RetryPending => summary.retry_pending += 1,
                    FinalDisposition::Indeterminate => summary.indeterminate += 1,
                    FinalDisposition::RecurrenceFailed => {}
                }
                if disposition == FinalDisposition::RecurrenceFailed
                    || !matches!(completion, DispatchCompletion::Succeeded)
                {
                    summary.failed += 1;
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::error!(
                    scheduled_event_id = %id,
                    namespace,
                    error = %error,
                    "pending-events: expired dispatch outcome finalization failed; continuing"
                );
                summary.failed += 1;
            }
        }
    }
    Ok(summary)
}

/// Read a `scheduled_event` note's CURRENT `properties` column, verbatim as
/// stored (no round-trip through `serde_json` re-serialization), so a caller
/// can use the exact byte string as an exact-equality CAS guard on a later
/// write. Returns `Ok(None)` when the row is absent, soft-deleted, or no
/// longer a `scheduled_event` note.
pub(super) async fn current_note_properties_text(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
) -> Result<Option<String>> {
    let mut reader = rt
        .sql()
        .reader()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL reader: {e}"))?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM notes \
                  WHERE id = ?1 AND namespace = ?2 AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL"
                .to_string(),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("pending_events_current_properties".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: read current properties: {e}"))?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => match row.get("properties") {
            Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
            Some(SqlValue::Null) | None => Ok(None),
            other => Err(anyhow::anyhow!(
                "pending-events: unexpected properties column shape: {other:?}"
            )),
        },
        _ => Err(anyhow::anyhow!(
            "pending-events: multiple rows for scheduled_event {id}"
        )),
    }
}

/// These direct SQL updates change only fixed, non-reserved JSON paths. The
/// final object's top-level reserved-key membership is therefore identical to
/// this snapshot's. Each caller either already has an exact-properties CAS or
/// adds one to its UPDATE, so a concurrent writer cannot change the object
/// between this check and the write.
pub(super) fn check_fixed_path_whole_object_snapshot(properties: &str) -> Result<()> {
    let value: Value = serde_json::from_str(properties)
        .context("pending-events: parse properties for reservation check")?;
    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&value))?;
    Ok(())
}

/// Parses a finalizer's freshly read current-properties CAS snapshot into the
/// `Value` base a terminal write's field mutations are applied to. Callers
/// must build their write on this value, not on the page-query snapshot taken
/// before the claim — a property written between that snapshot and this read
/// still passes the CAS fence (it is part of what "current" means by the time
/// this is called) but would otherwise be silently discarded by a write whose
/// base predates it. Returns `None` (and logs) if the stored text is not
/// valid JSON; the caller must treat that as a failed finalization.
pub(super) fn expected_properties_value(
    expected_properties: &str,
    id: uuid::Uuid,
) -> Option<Value> {
    match serde_json::from_str(expected_properties) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::error!(
                scheduled_event_id = %id,
                error = %error,
                "pending-events: could not parse current properties for finalization"
            );
            None
        }
    }
}

/// Read the row's raw current properties at the same read boundary as a
/// pending-action finalization decision, for use as `finalize_fired_event`'s
/// mandatory exact-properties CAS fence. Returns `None` (and logs) on a read
/// error or a vanished row; the caller must treat that as a failed
/// finalization rather than retry with a stale or synthetic snapshot.
pub(super) async fn current_properties_for_finalize(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    context: &'static str,
) -> Option<String> {
    match current_note_properties_text(rt, namespace, id).await {
        Ok(Some(text)) => Some(text),
        Ok(None) => {
            tracing::error!(
                scheduled_event_id = %id,
                "pending-events: row vanished before {context} finalization"
            );
            None
        }
        Err(error) => {
            tracing::error!(
                scheduled_event_id = %id,
                error = %error,
                "pending-events: could not read current properties before {context} finalization"
            );
            None
        }
    }
}

/// CAS-persist the post-drain state of a claimed event: `firing -> {fired |
/// pending | missed | failed}` (`pending` is an advanced repeat; `failed` is
/// the unattributed-generic-action policy state). `claimed_firing_at` is
/// the claim token from `claim_pending_event`; the CAS requires the row's
/// CURRENT `firing_at` to still equal it, not merely `status='firing'`.
/// Clears `firing_at` on the terminal write. Returns
/// `Ok(true)` iff exactly one row was updated. See
/// `crates/khive-mcp/docs/api/pending-events.md`.
/// Bundles `finalize_firing_event`'s two independent CAS guard inputs — the
/// recovery-only legacy-stale timing predicate and the exact-properties
/// equality predicate any caller may supply — into one parameter so the
/// function stays under clippy's argument-count lint.
#[derive(Clone, Copy, Default)]
struct FinalizeGuard<'a> {
    expired_at: Option<i64>,
    expected_properties: Option<&'a str>,
}

/// Finalize a row this process's own claim is dispatching, guarded on the
/// row's exact current properties as well as claim identity. `expected_properties`
/// is mandatory — not `Option` — so a future branch cannot silently drop the
/// content fence by passing `None`: the claim token alone is an ownership
/// fence, not a substitute for detecting a concurrent property writer that
/// landed between claim and finalization (ADR-106). Callers must read the
/// row's raw current properties at the same read boundary as their
/// finalization decision — see `current_note_properties_text` — and pass
/// that snapshot here.
pub(super) async fn finalize_fired_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claim: &DispatchClaim,
    expected_properties: &str,
) -> Result<bool> {
    finalize_firing_event(
        rt,
        namespace,
        id,
        properties,
        updated_at,
        claim,
        FinalizeGuard {
            expired_at: None,
            expected_properties: Some(expected_properties),
        },
    )
    .await
}

/// Finalize a row selected by the expired-lease recovery pass, but only while
/// its CURRENT properties still exactly match the expired snapshot selected
/// by that pass. A renewal or outcome write between the recovery SELECT and
/// this CAS wins and makes recovery a no-op.
pub(super) async fn finalize_expired_firing_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claim: &DispatchClaim,
    snapshot: RecoverySnapshot<'_>,
) -> Result<bool> {
    finalize_firing_event(
        rt,
        namespace,
        id,
        properties,
        updated_at,
        claim,
        FinalizeGuard {
            expired_at: Some(snapshot.expired_at),
            expected_properties: Some(snapshot.properties),
        },
    )
    .await
}

async fn finalize_firing_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claim: &DispatchClaim,
    guard: FinalizeGuard<'_>,
) -> Result<bool> {
    let FinalizeGuard {
        expired_at,
        expected_properties,
    } = guard;
    let legacy_stale_before =
        expired_at.map(|value| value.saturating_sub(LEGACY_STALE_FIRING_TIMEOUT_MICROS));
    let mut properties = properties.clone();
    if let Some(obj) = properties.as_object_mut() {
        obj.remove("firing_at");
        obj.remove("lease_expires_at");
    }
    // Same direct-SQL seam as `persist_dispatch_outcome` above, mask-not-
    // block credential content for the same reason: this is the terminal write shared by
    // fresh-dispatch finalization and expired-lease recovery, no caller can
    // act on a content-scan refusal here, and refusing would leave the row `firing`
    // forever instead of recording the outcome that already happened. It
    // re-embeds the same handler-supplied failure content
    // (`dispatch_receipt.error`/`error_payload`, plus the legacy flat
    // `dispatch_error`/`delivery_error` mirror) into a fresh properties blob.
    // Mask exactly those four fields, not the whole blob: the rest of
    // `properties` already passed this scan when it was originally written.
    //
    // `dispatch_receipt.error`/`error_payload` may already have been masked
    // by `persist_dispatch_outcome` on the fresh-dispatch path (the receipt
    // it returns is what `final_properties_after_dispatch` copies into
    // `properties["dispatch_receipt"]` before this function runs); the
    // expired-lease recovery path can also reach here with an
    // already-masked receipt carried over from an earlier pass. A second
    // `mask_json_content` pass over already-masked text is a no-op:
    // `bounded_masked_log_text` (secret_gate.rs:770-791) masks through
    // `mask_secrets` (secret_gate.rs:569-586), whose span collector matches
    // only known credential shapes and high-entropy runs beside a trigger
    // word (`TRIGGER_WORDS`, secret_gate.rs:1313-1326); `REDACTION_MARKER`
    // ("***MASKED***", secret_gate.rs:251) is plain uppercase ASCII with no
    // digit/hex/base64 shape and contains none of those trigger substrings,
    // so `collect_mask_spans` finds no span in it and `mask_secrets` returns
    // its input unchanged (`Cow::Borrowed`, secret_gate.rs:571-572).
    // `neutralize_log_unsafe_chars` (secret_gate.rs:877-879) is idempotent
    // the same way: its `\u{XXXX}` escape output contains no Cc/Cf/Zl/Zp
    // codepoint, so nothing it has already escaped is escaped again.
    //
    // `dispatch_error`/`delivery_error` are the legacy flat mirror and
    // reach this function raw from `final_properties_after_dispatch`
    // (which derives them from the original, unmasked
    // `DispatchCompletion`), so this is their first and only mask.
    let receipt_error = properties
        .pointer("/dispatch_receipt/error")
        .cloned()
        .map(|v| mask_json_content(&v));
    let receipt_error_payload = properties
        .pointer("/dispatch_receipt/error_payload")
        .cloned()
        .map(|v| mask_json_content(&v));
    let dispatch_error = properties
        .get("dispatch_error")
        .cloned()
        .map(|v| mask_json_content(&v));
    let delivery_error = properties
        .get("delivery_error")
        .cloned()
        .map(|v| mask_json_content(&v));
    if let Some(masked) = &receipt_error {
        if let Some(slot) = properties.pointer_mut("/dispatch_receipt/error") {
            *slot = masked.clone();
        }
    }
    if let Some(masked) = &receipt_error_payload {
        if let Some(slot) = properties.pointer_mut("/dispatch_receipt/error_payload") {
            *slot = masked.clone();
        }
    }
    if let Some(masked) = &dispatch_error {
        properties["dispatch_error"] = masked.clone();
    }
    if let Some(masked) = &delivery_error {
        properties["delivery_error"] = masked.clone();
    }
    if let Err(gate_error) = khive_runtime::secret_gate::check_json_at(
        &json!({
            "dispatch_receipt.error": &receipt_error,
            "dispatch_receipt.error_payload": &receipt_error_payload,
            "dispatch_error": &dispatch_error,
            "delivery_error": &delivery_error,
        }),
        "scheduled_event",
        "dispatch_receipt",
    ) {
        // Same masking-invariant posture as `persist_dispatch_outcome`
        // above: the masker was supposed to make this input pass and did
        // not. `gate_error`'s `Display` never echoes scanned content (see
        // the comment there), so it is safe to log in full. Persist the
        // masked record anyway -- it is the record about to be written
        // either way, and a durable terminal outcome is worth more than a
        // belt-and-braces assert.
        tracing::error!(
            scheduled_event_id = %id,
            error = %gate_error,
            "pending-events: masking invariant failed on finalized dispatch receipt; \
             persisting the masked record anyway"
        );
    }
    khive_runtime::secret_gate::reject_reserved_secret_gate_property(Some(&properties))?;
    let props_json = serde_json::to_string(&properties)
        .map_err(|e| anyhow::anyhow!("pending-events: serialize properties: {e}"))?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL writer: {e}"))?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = ?1, updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND ( \
                      ?7 IS NULL OR ( \
                        (json_extract(properties, '$.lease_expires_at') IS NOT NULL \
                         AND CAST(json_extract(properties, '$.lease_expires_at') AS INTEGER) <= ?7) \
                        OR \
                        (json_extract(properties, '$.lease_expires_at') IS NULL \
                         AND (json_extract(properties, '$.firing_at') IS NULL \
                              OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?8)) \
                      ) \
                    ) \
                    AND (?9 IS NULL OR properties = ?9)"
                .to_string(),
            params: vec![
                SqlValue::Text(props_json),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
                expired_at.map_or(SqlValue::Null, SqlValue::Integer),
                legacy_stale_before.map_or(SqlValue::Null, SqlValue::Integer),
                expected_properties.map_or(SqlValue::Null, |value| SqlValue::Text(value.to_string())),
            ],
            label: Some("pending_events_finalize_fired".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: finalize conditional update: {e}"))?;
    Ok(rows == 1)
}
