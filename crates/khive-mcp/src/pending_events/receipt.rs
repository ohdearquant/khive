use super::*;

pub(super) fn dispatch_occurrence_id(
    event_id: uuid::Uuid,
    trigger_at: DateTime<Utc>,
) -> uuid::Uuid {
    uuid::Uuid::new_v5(&event_id, trigger_at.to_rfc3339().as_bytes())
}

fn receipt_timestamp(receipt: &Value, field: &str) -> std::result::Result<i64, String> {
    let value = receipt
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("dispatch receipt {field} is missing or not an integer"))?;
    DateTime::<Utc>::from_timestamp_micros(value).ok_or_else(|| {
        format!("dispatch receipt {field} is outside the supported timestamp range")
    })?;
    Ok(value)
}

pub(super) fn validate_dispatch_receipt(
    event_id: uuid::Uuid,
    firing_at: i64,
    properties: &Value,
    receipt: Value,
) -> std::result::Result<ValidatedDispatchReceipt, String> {
    if !receipt.is_object() {
        return Err("dispatch receipt is not an object".to_string());
    }
    if properties.get("firing_at").and_then(Value::as_i64) != Some(firing_at) {
        return Err("dispatch firing claim timestamp is missing or malformed".to_string());
    }
    if receipt.get("version").and_then(Value::as_u64) != Some(DISPATCH_RECEIPT_VERSION) {
        return Err("dispatch receipt version is missing or unsupported".to_string());
    }

    let occurrence_id = receipt
        .get("occurrence_id")
        .and_then(Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| "dispatch receipt occurrence_id is missing or malformed".to_string())?;
    let invocation_id = receipt
        .get("invocation_id")
        .and_then(Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .ok_or_else(|| "dispatch receipt invocation_id is missing or malformed".to_string())?;
    let actor = receipt
        .get("actor")
        .and_then(Value::as_str)
        .filter(|actor| {
            *actor == "anonymous:local"
                || actor
                    .strip_prefix("actor:")
                    .is_some_and(|identity| !identity.trim().is_empty())
        })
        .ok_or_else(|| "dispatch receipt actor is missing or malformed".to_string())?
        .to_string();
    let claimed_at = receipt_timestamp(&receipt, "claimed_at")?;
    if claimed_at != firing_at {
        return Err("dispatch receipt claimed_at does not match the firing claim".to_string());
    }

    let trigger_at = properties
        .get("trigger_at")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "scheduled event trigger_at is missing during receipt validation".to_string()
        })?
        .parse::<DateTime<FixedOffset>>()
        .map_err(|_| {
            "scheduled event trigger_at is malformed during receipt validation".to_string()
        })?
        .with_timezone(&Utc);
    if occurrence_id != dispatch_occurrence_id(event_id, trigger_at) {
        return Err(
            "dispatch receipt occurrence_id does not match the event and scheduled instant"
                .to_string(),
        );
    }

    let state = receipt
        .get("state")
        .and_then(Value::as_str)
        .and_then(DispatchReceiptState::parse)
        .ok_or_else(|| "dispatch receipt state is missing or unsupported".to_string())?;
    match state {
        DispatchReceiptState::Claimed => {
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(
                    "dispatch receipt state claimed cannot carry an error payload".to_string(),
                );
            }
        }
        DispatchReceiptState::Invoking => {
            receipt_timestamp(&receipt, "invocation_started_at")?;
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(
                    "dispatch receipt state invoking cannot carry an error payload".to_string(),
                );
            }
        }
        DispatchReceiptState::Succeeded | DispatchReceiptState::Missed => {
            receipt_timestamp(&receipt, "completed_at")?;
            if receipt.get("error") != Some(&Value::Null) {
                return Err(format!(
                    "dispatch receipt state {} requires error=null",
                    state.as_str()
                ));
            }
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(format!(
                    "dispatch receipt state {} cannot carry an error payload",
                    state.as_str()
                ));
            }
        }
        DispatchReceiptState::Failed | DispatchReceiptState::Indeterminate => {
            receipt_timestamp(&receipt, "completed_at")?;
            if receipt
                .get("error")
                .and_then(Value::as_str)
                .is_none_or(|error| error.trim().is_empty())
            {
                return Err(format!(
                    "dispatch receipt state {} requires a non-empty error",
                    state.as_str()
                ));
            }
        }
        DispatchReceiptState::NotInvoked => {
            receipt_timestamp(&receipt, "completed_at")?;
            if receipt
                .get("error")
                .and_then(Value::as_str)
                .is_none_or(|error| error.trim().is_empty())
            {
                return Err(
                    "dispatch receipt state not_invoked requires a non-empty error".to_string(),
                );
            }
            if receipt
                .get("error_payload")
                .is_some_and(|payload| !payload.is_null())
            {
                return Err(
                    "dispatch receipt state not_invoked cannot carry an action error payload"
                        .to_string(),
                );
            }
        }
    }

    Ok(ValidatedDispatchReceipt {
        value: receipt,
        occurrence_id,
        invocation_id,
        actor,
        state,
    })
}

/// CAS-claim a pending scheduled event and atomically persist the occurrence
/// and invocation identity before any action future can be polled.
///
/// `expected_trigger_at` is the raw `trigger_at` string the caller's page
/// snapshot saw, and the claim refuses unless the row still carries those exact
/// bytes. That is what keeps the persisted receipt's `occurrence_id` — derived
/// from the snapshot's instant — describing the same occurrence the row is
/// scheduled for. Without it a writer landing between the page query and this
/// claim reschedules the event while the claim stamps the old occurrence onto
/// it, and the resulting terminal row fails receipt validation and is
/// quarantined as indeterminate rather than read as the dispatch it was.
/// A refusal costs nothing: the row stays `pending` and the next drain picks it
/// up from the value the writer actually left. The comparison is on bytes, not
/// on the parsed instant, so a rewrite to a different spelling of the same
/// instant also refuses; that is stricter than the invariant strictly needs and
/// the extra refusals cost one drain interval each.
pub(super) async fn claim_pending_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    occurrence_id: uuid::Uuid,
    expected_trigger_at: &str,
    actor: &str,
    lease: DispatchLeaseConfig,
) -> Result<Option<DispatchClaim>> {
    let updated_at = Utc::now().timestamp_micros();
    let claim = DispatchClaim {
        firing_at: updated_at,
        occurrence_id,
        invocation_id: uuid::Uuid::new_v4(),
        actor: actor.to_string(),
    };
    let lease_expires_at = lease.expires_at(updated_at);
    let receipt = claim.claimed_receipt();
    let receipt_json = serde_json::to_string(&receipt)
        .context("pending-events: serialize dispatch claim receipt")?;
    let Some(snapshot) = current_note_properties_text(rt, namespace, id).await? else {
        return Ok(None);
    };
    check_fixed_path_whole_object_snapshot(&snapshot)?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL writer: {e}"))?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set( \
                        COALESCE(properties, '{}'), \
                        '$.status', 'firing', \
                        '$.firing_at', ?1, \
                        '$.lease_expires_at', ?2, \
                        '$.dispatch_receipt', json(?3) \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?4 \
                    AND namespace = ?5 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending' \
                    AND json_extract(properties, '$.trigger_at') = ?6 \
                    AND properties = ?7"
                .to_string(),
            params: vec![
                SqlValue::Integer(updated_at),
                SqlValue::Integer(lease_expires_at),
                SqlValue::Text(receipt_json),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(expected_trigger_at.to_string()),
                SqlValue::Text(snapshot),
            ],
            label: Some("pending_events_claim_firing".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: claim conditional update: {e}"))?;
    Ok((rows == 1).then_some(claim))
}

pub(super) async fn mark_dispatch_invoking(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    claim: &DispatchClaim,
    lease: DispatchLeaseConfig,
) -> Result<bool> {
    let now = Utc::now().timestamp_micros();
    let Some(snapshot) = current_note_properties_text(rt, namespace, id).await? else {
        return Ok(false);
    };
    check_fixed_path_whole_object_snapshot(&snapshot)?;
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for invocation receipt")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set( \
                        properties, \
                        '$.dispatch_receipt.state', 'invoking', \
                        '$.dispatch_receipt.invocation_started_at', ?1, \
                        '$.lease_expires_at', ?2 \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND json_extract(properties, '$.dispatch_receipt.state') = 'claimed' \
                    AND properties = ?7"
                .to_string(),
            params: vec![
                SqlValue::Integer(now),
                SqlValue::Integer(lease.expires_at(now)),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
                SqlValue::Text(snapshot),
            ],
            label: Some("pending_events_mark_invoking".into()),
        })
        .await
        .context("pending-events: persist invocation-start receipt")?;
    Ok(rows == 1)
}

pub(super) async fn renew_dispatch_lease(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    claim: &DispatchClaim,
    lease: DispatchLeaseConfig,
) -> Result<bool> {
    let now = Utc::now().timestamp_micros();
    let mut writer = rt
        .sql()
        .writer()
        .await
        .context("pending-events: open SQL writer for lease renewal")?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set(properties, '$.lease_expires_at', ?1), \
                      updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND json_extract(properties, '$.dispatch_receipt.state') = 'invoking'"
                .to_string(),
            params: vec![
                SqlValue::Integer(lease.expires_at(now)),
                SqlValue::Integer(now),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claim.firing_at),
                SqlValue::Text(claim.invocation_id.to_string()),
            ],
            label: Some("pending_events_renew_dispatch_lease".into()),
        })
        .await
        .context("pending-events: renew dispatch lease")?;
    Ok(rows == 1)
}

/// Recursively mask handler-supplied JSON content before it is embedded in a
/// durable record.
///
/// - `Value::String` leaves are masked with
///   [`khive_runtime::secret_gate::bounded_masked_log_text`].
/// - `Value::Array` is masked element-wise.
/// - `Value::Object` masks BOTH keys and values: `secret_gate::check_json`'s
///   own scanner checks object keys too (`scan_json_value` calls `check(k)`
///   on every key), so a masker that skipped keys would leave standing
///   exactly the one thing the post-mask assert at each call site is
///   guaranteed to catch. Two distinct keys can mask to the same string;
///   rather than let the later one silently overwrite the earlier, later
///   collisions are disambiguated with a `#2`, `#3`, ... suffix. Silent key
///   loss on a durable failure receipt is the exact class of defect this
///   function exists to remove.
/// - Numbers, bools, and null pass through unchanged.
pub(super) fn mask_json_content(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(khive_runtime::secret_gate::bounded_masked_log_text(s)),
        Value::Array(items) => Value::Array(items.iter().map(mask_json_content).collect()),
        Value::Object(map) => {
            let mut masked = serde_json::Map::with_capacity(map.len());
            for (key, val) in map {
                let masked_key_base = khive_runtime::secret_gate::bounded_masked_log_text(key);
                let mut masked_key = masked_key_base.clone();
                let mut suffix = 2u32;
                while masked.contains_key(&masked_key) {
                    masked_key = format!("{masked_key_base}#{suffix}");
                    suffix += 1;
                }
                masked.insert(masked_key, mask_json_content(val));
            }
            Value::Object(masked)
        }
        other => other.clone(),
    }
}

pub(super) async fn persist_dispatch_outcome(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    claim: &DispatchClaim,
    completion: &DispatchCompletion,
) -> Result<Option<Value>> {
    let completed_at = Utc::now().timestamp_micros();
    let (state, error, error_payload) = match completion {
        DispatchCompletion::Succeeded => ("succeeded", Value::Null, Value::Null),
        DispatchCompletion::Failed(error) => (
            "failed",
            json!(error.as_str()),
            error.payload.clone().unwrap_or(Value::Null),
        ),
        DispatchCompletion::Indeterminate(error) => (
            "indeterminate",
            json!(error.as_str()),
            error.payload.clone().unwrap_or(Value::Null),
        ),
    };
    // Direct SQL write, bypassing the ordinary note-write path (and with it
    // curation.rs's write-time content scan) for durability reasons unique to
    // this seam. The dispatch already happened and the lease is already held
    // by the time this runs, so a hard credential-content refusal here would not stop anything
    // from occurring -- it would only lose the record of what did, leaving
    // the row `firing` forever and the occurrence permanently re-drainable.
    // No caller can act on a content-scan refusal at this seam. Mask the handler-supplied
    // failure content instead of blocking it, then run the same scan the
    // outbound-message path applies to `last_error` as a POST-MASK ASSERT on
    // exactly what is about to be embedded -- confirming the masker did its
    // job, never gating whether the outcome gets recorded.
    let error = mask_json_content(&error);
    let error_payload = mask_json_content(&error_payload);
    if let Err(gate_error) = khive_runtime::secret_gate::check_json_at(
        &json!({
            "error": &error,
            "error_payload": &error_payload,
        }),
        "scheduled_event",
        "dispatch_receipt",
    ) {
        // The masker was supposed to make this input pass and did not. This
        // is a masking-invariant failure, not a caller-actionable refusal:
        // `SecretMatch`'s `Display` (via `RuntimeError::SecretDetected`'s
        // `"write blocked: {0}"`) prints only the detector name, an optional
        // trigger word, the location, and static guidance text -- never the
        // scanned content, not even the masked excerpt -- so it is safe to
        // log in full. Persist the masked record anyway: a durable outcome
        // is worth more than a belt-and-braces assert, and the record about
        // to be written is the masked one either way.
        tracing::error!(
            scheduled_event_id = %id,
            error = %gate_error,
            "pending-events: masking invariant failed on dispatch outcome receipt; \
             persisting the masked record anyway"
        );
    }
    let receipt = json!({
        "version": DISPATCH_RECEIPT_VERSION,
        "occurrence_id": claim.occurrence_id,
        "invocation_id": claim.invocation_id,
        "actor": claim.actor.as_str(),
        "state": state,
        "claimed_at": claim.firing_at,
        "completed_at": completed_at,
        "error": error,
        "error_payload": error_payload,
    });
    let receipt_json = serde_json::to_string(&receipt)
        .context("pending-events: serialize dispatch outcome receipt")?;
    // A lease renewal may win this exact-properties CAS; retry against its
    // current object while the invocation identity remains guarded below.
    for _ in 0..8 {
        let Some(snapshot) = current_note_properties_text(rt, namespace, id).await? else {
            return Ok(None);
        };
        check_fixed_path_whole_object_snapshot(&snapshot)?;
        let mut writer = rt
            .sql()
            .writer()
            .await
            .context("pending-events: open SQL writer for dispatch outcome")?;
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes \
                  SET properties = json_set( \
                        properties, \
                        '$.dispatch_receipt', json(?1), \
                        '$.lease_expires_at', ?2 \
                      ), \
                      updated_at = ?2 \
                  WHERE id = ?3 \
                    AND namespace = ?4 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5 \
                    AND json_extract(properties, '$.dispatch_receipt.invocation_id') = ?6 \
                    AND json_extract(properties, '$.dispatch_receipt.state') = 'invoking' \
                    AND properties = ?7"
                    .to_string(),
                params: vec![
                    SqlValue::Text(receipt_json.clone()),
                    SqlValue::Integer(completed_at),
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Integer(claim.firing_at),
                    SqlValue::Text(claim.invocation_id.to_string()),
                    SqlValue::Text(snapshot.clone()),
                ],
                label: Some("pending_events_persist_dispatch_outcome".into()),
            })
            .await
            .context("pending-events: persist dispatch outcome")?;
        if rows == 1 {
            return Ok(Some(receipt));
        }
        drop(writer);
        match current_note_properties_text(rt, namespace, id).await? {
            Some(current) if current != snapshot => continue,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

pub(super) fn completion_from_receipt(receipt: &Value) -> DispatchCompletion {
    let error = || {
        let message = receipt
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("scheduled dispatch failed without an error message")
            .to_string();
        let payload = receipt
            .get("error_payload")
            .filter(|payload| !payload.is_null())
            .cloned();
        DispatchFailure { message, payload }
    };
    match receipt.get("state").and_then(Value::as_str) {
        Some("succeeded") => DispatchCompletion::Succeeded,
        Some("failed") => DispatchCompletion::Failed(error()),
        Some("indeterminate") => DispatchCompletion::Indeterminate(error()),
        Some("claimed") => DispatchCompletion::Failed(DispatchFailure::plain(
            "dispatch claimant expired before invocation began; occurrence is retryable",
        )),
        Some("invoking") => DispatchCompletion::Indeterminate(DispatchFailure::plain(
            "dispatch lease expired without a durable outcome; refusing automatic replay because the side effect may already have occurred",
        )),
        other => DispatchCompletion::Indeterminate(DispatchFailure::plain(format!(
            "dispatch receipt has unsupported state {other:?}; refusing automatic replay"
        ))),
    }
}

pub(super) fn dispatch_error_property_keys(properties: &Value) -> (&'static str, &'static str) {
    if properties
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("remind")
        == "remind"
    {
        ("delivery_error", "delivery_failed_at")
    } else {
        ("dispatch_error", "dispatch_failed_at")
    }
}

pub(super) fn mark_recurrence_failure(properties: &mut Value, error: &str, failed_at: &str) {
    properties["status"] = json!("failed");
    properties["recurrence_error"] = json!(error);
    properties["recurrence_failed_at"] = json!(failed_at);
}

pub(super) fn mark_dispatch_receipt_indeterminate(
    properties: &mut Value,
    invalid_receipt: Value,
    error: &str,
    completed_at: i64,
) {
    properties["dispatch_receipt"] = json!({
        "version": DISPATCH_RECEIPT_VERSION,
        "state": DispatchReceiptState::Indeterminate.as_str(),
        "completed_at": completed_at,
        "error": error,
        "error_payload": null,
        "invalid_receipt": invalid_receipt,
    });
    properties["status"] = json!("failed");
    let (error_key, error_at_key) = dispatch_error_property_keys(properties);
    properties[error_key] = json!(error);
    properties[error_at_key] = json!(Utc::now().to_rfc3339());
}

pub(super) fn final_properties_after_dispatch(
    mut properties: Value,
    receipt: Value,
    completion: &DispatchCompletion,
    trigger_at: DateTime<Utc>,
    trigger_offset: FixedOffset,
    repeat: &Option<String>,
) -> (Value, FinalDisposition) {
    let completed_at = receipt
        .get("completed_at")
        .and_then(Value::as_i64)
        .and_then(DateTime::<Utc>::from_timestamp_micros)
        .unwrap_or_else(Utc::now);
    let completed_at_rfc = completed_at.to_rfc3339();
    properties["dispatch_receipt"] = receipt;
    properties["last_attempted_at"] = json!(completed_at_rfc);

    let (error_key, error_at_key) = dispatch_error_property_keys(&properties);

    match completion {
        DispatchCompletion::Succeeded => {
            if let Some(object) = properties.as_object_mut() {
                object.remove(error_key);
                object.remove(error_at_key);
            }
            properties["fired_at"] = json!(completed_at_rfc);
            match next_trigger_at_for_event(&mut properties, repeat, trigger_at) {
                Ok(Some(next_at)) => {
                    properties["trigger_at"] =
                        json!(next_at.with_timezone(&trigger_offset).to_rfc3339());
                    properties["status"] = json!("pending");
                    (properties, FinalDisposition::Advanced)
                }
                Ok(None) => {
                    properties["status"] = json!("fired");
                    (properties, FinalDisposition::Fired)
                }
                Err(error) => {
                    mark_recurrence_failure(&mut properties, error, &completed_at_rfc);
                    (properties, FinalDisposition::RecurrenceFailed)
                }
            }
        }
        DispatchCompletion::Failed(error) => {
            properties[error_key] = json!(error.as_str());
            properties[error_at_key] = json!(completed_at_rfc);
            match next_trigger_at_for_event(&mut properties, repeat, trigger_at) {
                Ok(Some(next_at)) => {
                    properties["trigger_at"] =
                        json!(next_at.with_timezone(&trigger_offset).to_rfc3339());
                    properties["status"] = json!("pending");
                    (properties, FinalDisposition::Advanced)
                }
                Ok(None) => {
                    if properties
                        .pointer("/dispatch_receipt/error_payload")
                        .is_some_and(action_error_disposition_may_have_committed)
                    {
                        // A repeat with a next occurrence can advance despite
                        // this error. With no next occurrence, Failed would
                        // replay this same action; the durable receipt must
                        // instead say that its domain outcome is uncertain.
                        properties["dispatch_receipt"]["state"] =
                            json!(DispatchReceiptState::Indeterminate.as_str());
                        properties["status"] = json!("failed");
                        (properties, FinalDisposition::Indeterminate)
                    } else {
                        properties["status"] = json!("pending");
                        (properties, FinalDisposition::RetryPending)
                    }
                }
                Err(error) => {
                    if properties
                        .pointer("/dispatch_receipt/error_payload")
                        .is_some_and(action_error_disposition_may_have_committed)
                    {
                        properties["dispatch_receipt"]["state"] =
                            json!(DispatchReceiptState::Indeterminate.as_str());
                    }
                    mark_recurrence_failure(&mut properties, error, &completed_at_rfc);
                    (properties, FinalDisposition::RecurrenceFailed)
                }
            }
        }
        DispatchCompletion::Indeterminate(error) => {
            properties[error_key] = json!(error.as_str());
            properties[error_at_key] = json!(completed_at_rfc);
            properties["status"] = json!("failed");
            (properties, FinalDisposition::Indeterminate)
        }
    }
}

pub(super) fn apply_final_disposition(summary: &mut DrainSummary, disposition: FinalDisposition) {
    summary.finalized += 1;
    match disposition {
        FinalDisposition::Fired => summary.fired += 1,
        FinalDisposition::Advanced => summary.advanced += 1,
        FinalDisposition::RetryPending => summary.retry_pending += 1,
        FinalDisposition::Indeterminate => summary.indeterminate += 1,
        FinalDisposition::RecurrenceFailed => summary.failed += 1,
    }
}
