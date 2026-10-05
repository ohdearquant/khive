use super::*;

/// One-shot drain: fire all pending, due scheduled events.
///
/// - Scans for `scheduled_event` notes with `status="pending"` and
///   `trigger_at <= now`.
/// - Dispatches the stored action DSL or reminder inbox delivery in the event's namespace.
/// - Persists a claim-bound dispatch receipt before lifecycle finalization.
/// - Marks successful one-shots `"fired"`; failed one-shots return to
///   `"pending"` for a later pass.
/// - For repeating events with named aliases (`"daily"` / `"weekly"` /
///   `"monthly"`), resets status to `"pending"` and advances `trigger_at`.
///   Unsupported recurrence is rejected at creation and fails closed for
///   legacy rows (see module-level documentation).
///
/// Per-event failures accumulate in the returned [`DrainSummary`] without
/// aborting the drain.
pub async fn run_pending_events(
    db: Option<&str>,
    namespace: &str,
    verbose: bool,
) -> Result<DrainSummary> {
    run_pending_events_with_config(db, None, namespace, verbose).await
}

/// One-shot drain with an explicit configuration-file selection.
///
/// This is the `kkernel exec --pending-events --config …` entrypoint. The
/// compatibility wrapper above retains the original discovery behavior for
/// callers that do not carry a config path.
pub async fn run_pending_events_with_config(
    db: Option<&str>,
    config: Option<&std::path::Path>,
    namespace: &str,
    verbose: bool,
) -> Result<DrainSummary> {
    // Resolves through the same multi-backend-aware construction the daemon
    // boot path uses, with the namespace marked explicit but NOT an actor
    // override — `namespace_explicit: true, actor_explicit: false` — so a
    // `"local"`-resolved default namespace still falls through to the
    // project-configured actor. See "Server construction: explicit namespace,
    // implicit actor" in `crates/khive-mcp/docs/pending-events.md`.
    let ns = Namespace::parse(namespace)
        .map_err(|e| anyhow::anyhow!("pending-events: invalid namespace {namespace:?}: {e}"))?;
    let args = crate::args::Args {
        db: db.map(str::to_string),
        actor: None,
        namespace: None,
        no_embed: false,
        pack: Vec::new(),
        config: config.map(std::path::Path::to_path_buf),
        daemon: false,
        lifetime: None,
        idle_timeout_secs: None,
        transport: None,
        bind: None,
        brain_profile: None,
        resumed_generation: None,
    };
    // A `DatabaseOverrideConflict` raised by the builder must pass through
    // unchanged so `kkernel exec`'s caller receives it as the top-level error
    // and `db_override_refusal_envelope`'s `downcast_ref` recognizes it,
    // emitting the documented JSON refusal envelope. Every other build
    // failure keeps the generic "pending-events: build server" provenance.
    let (server, schedule_rt) =
        match crate::serve::build_server_with_explicit_namespace(&args, ns, true, false).await {
            Ok(built) => built,
            Err(error) => {
                if error
                    .downcast_ref::<crate::serve::DatabaseOverrideConflict>()
                    .is_some()
                {
                    return Err(error);
                }
                return Err(error.context("pending-events: build server"));
            }
        };
    tracing::info!(target: "khive.boot", "{}", crate::serve::resolved_actor_disclosure(server.actor_id()));
    let rt = schedule_rt.ok_or_else(|| {
        anyhow::anyhow!(
            "pending-events: resolved pack set does not include \"schedule\"; nothing to drain"
        )
    })?;
    run_pending_events_on(&rt, &server, verbose).await
}

/// One-shot drain against an already-constructed [`KhiveRuntime`] +
/// [`KhiveMcpServer`] pair (ADR-106).
///
/// The caller supplies an already-resolved, already-validated pair — both by
/// reference — so the drain's storage target, actor identity, and pack set
/// are always identical to the server it is ticking for. `rt` and `server`
/// serve two different roles that must NOT be collapsed into one: `rt` is
/// the **schedule pack's own runtime** (the scan/claim/finalize SQL below
/// reads and CAS-writes `scheduled_event` notes directly through it) while
/// `server` is the **daemon's live, fully-wired `KhiveMcpServer`**, used
/// only for `dispatch_action` (replaying a stored action's DSL) — building a
/// second server from `rt` alone would misroute replayed actions in a
/// multi-backend deployment. See
/// `crates/khive-mcp/docs/api/pending-events.md` for the full rationale.
pub async fn run_pending_events_on(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
) -> Result<DrainSummary> {
    run_pending_events_on_with_lease(rt, server, verbose, DispatchLeaseConfig::from_env()).await
}

pub(super) async fn run_pending_events_on_with_lease(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
    lease: DispatchLeaseConfig,
) -> Result<DrainSummary> {
    let now = Utc::now();
    let grace = fire_grace_from_env();
    let mut summary = DrainSummary::default();

    // ── Step 0: reconcile claims whose renewable lease expired ───────────
    // A durable succeeded/failed outcome is finalized without invoking the
    // action again. An `invoking` receipt has an ambiguous crash boundary and
    // fails closed instead of risking a duplicate side effect. Only legacy
    // pre-receipt claims retain the historical pending retry behavior.
    let reclaimed = reclaim_stale_firing_events(rt, now.timestamp_micros()).await?;
    summary.reclaimed = reclaimed.rows;
    summary.outcomes_persisted += reclaimed.outcomes_persisted;
    summary.fired += reclaimed.fired;
    summary.advanced += reclaimed.advanced;
    summary.retry_pending += reclaimed.retry_pending;
    summary.indeterminate += reclaimed.indeterminate;
    summary.finalized += reclaimed.finalized;
    summary.failed += reclaimed.failed;
    if verbose && summary.reclaimed > 0 {
        eprintln!(
            "[pending-events] reconciled {} expired \"firing\" row(s)",
            summary.reclaimed
        );
    }

    // ── Step 1: discover all distinct namespaces with pending scheduled_event notes ──
    let namespaces = discover_pending_namespaces(rt, now).await?;

    if verbose {
        eprintln!(
            "[pending-events] scan: now={}, namespaces_with_pending={}",
            now.to_rfc3339(),
            namespaces.len()
        );
    }

    // ── Step 2: per-namespace drain ──────────────────────────────────────────
    for ns_str in &namespaces {
        if let Err(e) = Namespace::parse(ns_str) {
            if verbose {
                eprintln!("[pending-events] skip invalid namespace {ns_str:?}: {e}");
            }
            continue;
        }

        // Bounded, mutation-immune keyset pagination: the due-ness predicate
        // (`trigger_at <= now`) runs in SQL directly so future events are
        // never fetched, and pages advance on the immutable `(created_at,
        // id)` keyset rather than `LIMIT/OFFSET`, so a row mutated between
        // pages can never shift a later page's boundary. See "Keyset
        // pagination and due-ness comparison" in
        // `crates/khive-mcp/docs/pending-events.md`.
        const PAGE_SIZE: u32 = 200;
        let now_rfc = now.to_rfc3339();
        let mut cursor: Option<(i64, String)> = None;
        loop {
            let (sql, params): (String, Vec<SqlValue>) = match &cursor {
                // Due-ness compares via SQLite's `datetime()`, not a raw
                // string `<=`: stored `trigger_at` values are not normalized
                // to UTC, so a raw lexicographic compare mis-ranks non-UTC
                // offsets. `datetime()` returns NULL for an unparseable
                // value; the `OR ... IS NULL` clause keeps such a row in the
                // candidate set instead of silently dropping it.
                None => (
                    "SELECT id, content, properties, created_at FROM notes \
                     WHERE namespace = ?1 AND kind = 'scheduled_event' \
                       AND deleted_at IS NULL \
                       AND json_extract(properties, '$.status') = 'pending' \
                       AND ( \
                         datetime(json_extract(properties, '$.trigger_at')) <= datetime(?2) \
                         OR datetime(json_extract(properties, '$.trigger_at')) IS NULL \
                       ) \
                     ORDER BY created_at ASC, id ASC LIMIT ?3"
                        .to_string(),
                    vec![
                        SqlValue::Text(ns_str.clone()),
                        SqlValue::Text(now_rfc.clone()),
                        SqlValue::Integer(i64::from(PAGE_SIZE)),
                    ],
                ),
                Some((c_created_at, c_id)) => (
                    "SELECT id, content, properties, created_at FROM notes \
                     WHERE namespace = ?1 AND kind = 'scheduled_event' \
                       AND deleted_at IS NULL \
                       AND json_extract(properties, '$.status') = 'pending' \
                       AND ( \
                         datetime(json_extract(properties, '$.trigger_at')) <= datetime(?2) \
                         OR datetime(json_extract(properties, '$.trigger_at')) IS NULL \
                       ) \
                       AND (created_at > ?3 OR (created_at = ?3 AND id > ?4)) \
                     ORDER BY created_at ASC, id ASC LIMIT ?5"
                        .to_string(),
                    vec![
                        SqlValue::Text(ns_str.clone()),
                        SqlValue::Text(now_rfc.clone()),
                        SqlValue::Integer(*c_created_at),
                        SqlValue::Text(c_id.clone()),
                        SqlValue::Integer(i64::from(PAGE_SIZE)),
                    ],
                ),
            };

            let rows = {
                let mut reader = rt
                    .sql()
                    .reader()
                    .await
                    .context("pending-events: open SQL reader for candidate page")?;
                reader
                    .query_all(SqlStatement {
                        sql,
                        params,
                        label: Some("pending_events_candidate_page".into()),
                    })
                    .await
                    .with_context(|| {
                        format!("pending-events: candidate page query failed for ns={ns_str}")
                    })?
            };

            let page_len = rows.len();
            if page_len == 0 {
                break;
            }

            for row in &rows {
                let id_str = match row.get("id") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip row with unexpected id column {other:?}"
                            );
                        }
                        continue;
                    }
                };
                let row_created_at = match row.get("created_at") {
                    Some(SqlValue::Integer(v)) => *v,
                    other => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip row {id_str}: unexpected created_at \
                                 column {other:?}"
                            );
                        }
                        continue;
                    }
                };
                // Advance the cursor even when this row fails downstream
                // parsing/processing below: the cursor is a pure positional
                // marker over `(created_at, id)`, not a per-row success
                // marker, so a single malformed row can never wedge the pass
                // by being re-fetched on every subsequent page query.
                cursor = Some((row_created_at, id_str.clone()));

                let id = match uuid::Uuid::parse_str(&id_str) {
                    Ok(u) => u,
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] skip row: unparseable id {id_str:?}: {e}");
                        }
                        continue;
                    }
                };
                let properties: Option<Value> = match row.get("properties") {
                    Some(SqlValue::Text(s)) => match serde_json::from_str(s) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            if verbose {
                                eprintln!(
                                    "[pending-events] skip note {id}: unparseable properties: {e}"
                                );
                            }
                            continue;
                        }
                    },
                    Some(SqlValue::Null) | None => None,
                    other => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip note {id}: unexpected properties column \
                                 {other:?}"
                            );
                        }
                        continue;
                    }
                };
                let content = match row.get("content") {
                    Some(SqlValue::Text(s)) => s.clone(),
                    other => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            content_column = ?other,
                            "pending-events: scheduled event has invalid content"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };

                summary.scanned += 1;

                let trigger_at_str = properties
                    .as_ref()
                    .and_then(|p| p.get("trigger_at"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Parsed as `DateTime<FixedOffset>`, not straight to
                // `DateTime<Utc>`, so the caller's original offset survives
                // repeat advancement instead of being silently rewritten to
                // UTC. Uses the relaxed RFC 3339 grammar matching the write
                // boundary, not the strict parser, since already-persisted
                // strings may use the relaxed form. See "Offset preservation
                // and relaxed RFC 3339 parsing" in
                // `crates/khive-mcp/docs/pending-events.md`.
                let trigger_at_fixed = match trigger_at_str.parse::<DateTime<FixedOffset>>() {
                    Ok(dt) => dt,
                    Err(_) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip note {id}: unparseable trigger_at {trigger_at_str:?}"
                            );
                        }
                        summary.skipped_not_due += 1;
                        continue;
                    }
                };
                let trigger_at = trigger_at_fixed.with_timezone(&Utc);
                let trigger_offset = *trigger_at_fixed.offset();
                // Owned copy of the exact bytes this page snapshot saw, so the
                // claim below can fence on them however `properties` is
                // borrowed or moved in between.
                let snapshot_trigger_at = trigger_at_str.to_string();

                if trigger_at > now {
                    summary.skipped_not_due += 1;
                    continue;
                }

                // ── Missed-event grace policy (ADR-106 amendment) ─────────
                // An event overdue by more than `grace` is never dispatched:
                // agent-facing side effects (outbound mail, spawned actions)
                // must not fire late en masse after a daemon outage or a
                // first boot against a large stale backlog. See the
                // module-level "Missed-event policy" docs.
                let overdue = now.signed_duration_since(trigger_at);
                let is_missed = overdue > grace;

                // ── Determine what to dispatch ───────────────────────────
                let event_type = properties
                    .as_ref()
                    .and_then(|p| p.get("event_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("remind");

                // Resolve replay/delivery authority only from the immutable
                // pack-written provenance event. `created_by_actor` remains
                // display metadata and never an authority source; the generic
                // KG mutation fence separately prevents a valid provenance
                // record from authorizing rewritten executable intent.
                // Both event kinds require provenance before the missed path:
                // a grace-policy receipt still identifies the creator, and a
                // legacy repeat must not rearm without a verified recipient.
                let creator = match verified_creator_for_event(rt, ns_str, id, event_type).await {
                    Ok(actor) => actor,
                    Err(e) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] creator provenance lookup failed for note \
                                 {id}: {e}"
                            );
                        }
                        summary.failed += 1;
                        continue;
                    }
                };
                let reminder_actor = if event_type == "remind" && !is_missed {
                    creator.as_ref().map(|actor| actor.recipient_id.clone())
                } else {
                    None
                };
                let action_dsl: Option<String> = if is_missed {
                    None
                } else if event_type == "schedule" {
                    properties
                        .as_ref()
                        .and_then(|p| p.get("payload"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    reminder_actor
                        .as_deref()
                        .map(|actor| reminder_delivery_action(actor, &content))
                };

                // ── Determine repeat (read before claim) ──
                // This value gates ADMISSION only — which finalize branch is
                // eligible. It must never reach anything WRITTEN: the value the
                // finalizer schedules from is re-derived at the write, from the
                // same fresh read the CAS is guarded on. See the re-derivation
                // just before `final_properties_after_dispatch`.
                let repeat = properties
                    .as_ref()
                    .and_then(|p| p.get("repeat"))
                    .and_then(Value::as_str)
                    .map(str::to_string);

                // `properties` above is a page-query snapshot; CAS-claim
                // pending -> firing now so a concurrent `schedule.cancel`
                // cannot land between the read and this point (whichever
                // side wins the CAS proceeds; the loser skips). The same
                // claim gates the missed path too. The claim also fences on
                // the snapshot's `trigger_at`, so a writer that reschedules
                // the event in that same window makes the claim a no-op
                // instead of stamping this occurrence id onto a row that is
                // now scheduled for a different instant.
                let occurrence_id = dispatch_occurrence_id(id, trigger_at);
                let receipt_actor = creator
                    .as_ref()
                    .map(|creator| creator.audit_actor.clone())
                    .unwrap_or_else(|| "anonymous:local".to_string());
                #[cfg(test)]
                race_seam::pause_before_claim().await;
                let claim = match claim_pending_event(
                    rt,
                    ns_str,
                    id,
                    occurrence_id,
                    &snapshot_trigger_at,
                    &receipt_actor,
                    lease,
                )
                .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] claim failed for note {id}: {e}");
                        }
                        summary.failed += 1;
                        continue;
                    }
                };
                let Some(claim) = claim else {
                    if verbose {
                        eprintln!(
                            "[pending-events] skip note {id}: no longer pending (concurrent \
                             cancel or claim)"
                        );
                    }
                    summary.skipped_race += 1;
                    continue;
                };

                if repeat.as_deref().is_some_and(|repeat| {
                    khive_pack_schedule::repeat::parse_repeat(repeat).is_err()
                }) {
                    let error = "scheduled event uses an unsupported repeat expression; it is not one the executor can advance";
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "unsupported-repeat").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    props["status"] = json!("failed");
                    let (error_key, error_at_key) = dispatch_error_property_keys(&props);
                    props[error_key] = json!(error);
                    props[error_at_key] = json!(Utc::now().to_rfc3339());
                    let completed_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        completed_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        completed_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => summary.skipped_race += 1,
                        Err(error) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: unsupported-repeat finalization failed"
                        ),
                    }
                    continue;
                }

                if creator.is_none() {
                    let error = if event_type == "remind" {
                        "reminder is missing immutable creator provenance; no recipient selected and delivery refused; create a new reminder with schedule.remind"
                    } else {
                        "scheduled action is missing immutable creator provenance; row cannot be replayed safely"
                    };
                    tracing::error!(
                        scheduled_event_id = %id,
                        "pending-events: refusing unattributed scheduled event"
                    );
                    if verbose {
                        eprintln!("[pending-events] dispatch refused for note {id}: {error}");
                    }
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "failed-identity").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    props["status"] = json!("failed");
                    let (error_key, error_at_key) = dispatch_error_property_keys(&props);
                    props[error_key] = json!(error);
                    props[error_at_key] = json!(Utc::now().to_rfc3339());
                    let updated_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        updated_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        updated_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => tracing::error!(
                            scheduled_event_id = %id,
                            "pending-events: failed-identity finalization lost its firing claim"
                        ),
                        Err(e) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %e,
                            "pending-events: failed-identity finalization failed"
                        ),
                    }
                    continue;
                }

                if is_missed {
                    // ── Missed path: never dispatch. Mark terminally
                    // "missed", or (for a repeat) re-arm past every
                    // accumulated occurrence to the next future one — no
                    // catch-up bursts. ─────────────────────────────────────
                    if verbose {
                        eprintln!(
                            "[pending-events] note {id} overdue by {}s (grace {}s): marking \
                             missed, not dispatching",
                            overdue.num_seconds(),
                            grace.num_seconds()
                        );
                    }
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "missed").await
                    else {
                        summary.failed += 1;
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        summary.failed += 1;
                        continue;
                    };
                    props["missed_at"] = json!(now.timestamp_micros());
                    let repeat_for_finalize = props
                        .get("repeat")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let mut recurrence_error = None;
                    match advance_repeat_past_missed_for_event(
                        &mut props,
                        &repeat_for_finalize,
                        trigger_at,
                        now,
                    ) {
                        Ok(Some(next_at)) => {
                            // Repeating event: skip this occurrence, re-arm
                            // pending at the next future one, rendered at the
                            // original offset, not UTC.
                            props["trigger_at"] =
                                json!(next_at.with_timezone(&trigger_offset).to_rfc3339());
                            props["status"] = json!("pending");
                        }
                        Ok(None) => {
                            // Non-repeating: terminal "missed". Unsupported
                            // recurrence was rejected before this path.
                            // `fired_at` stays null/untouched.
                            props["status"] = json!("missed");
                        }
                        Err(error) => {
                            mark_recurrence_failure(&mut props, error, &Utc::now().to_rfc3339());
                            recurrence_error = Some(error);
                            tracing::error!(
                                scheduled_event_id = %id,
                                error = %error,
                                "pending-events: missed recurrence could not advance"
                            );
                        }
                    }
                    let updated_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::Missed,
                        updated_at,
                        None,
                    );

                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        updated_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => {
                            summary.finalized += 1;
                            if recurrence_error.is_some() {
                                summary.failed += 1;
                            } else {
                                summary.missed.push(id);
                            }
                        }
                        Ok(false) => {
                            if verbose {
                                eprintln!(
                                    "[pending-events] finalize no-op for {id}: row no longer in \
                                     \"firing\" state"
                                );
                            }
                            summary.failed += 1;
                        }
                        Err(e) => {
                            if verbose {
                                eprintln!("[pending-events] finalize failed for {id}: {e}");
                            }
                            summary.failed += 1;
                        }
                    }
                    continue;
                }

                // ── Dispatch the action ──────────────────────────────────
                let dispatch_actor = creator
                    .as_ref()
                    .expect("checked above")
                    .request_actor
                    .clone();
                let Some(dsl) = action_dsl.as_deref() else {
                    let error = "scheduled event has no executable payload";
                    tracing::error!(
                        scheduled_event_id = %id,
                        event_type,
                        "pending-events: refusing empty scheduled-event dispatch"
                    );
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "empty-payload").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    let (error_key, error_at_key) = dispatch_error_property_keys(&props);
                    props[error_key] = json!(error);
                    props[error_at_key] = json!(Utc::now().to_rfc3339());
                    props["status"] = json!("failed");
                    let completed_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        completed_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        completed_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => summary.skipped_race += 1,
                        Err(error) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: empty-payload finalization failed"
                        ),
                    }
                    continue;
                };
                if event_type == "schedule" && stored_action_is_non_single(dsl) {
                    let error = "scheduled action contains multiple operations or a chain; legacy batches are not replayable because partial success cannot be retried safely";
                    tracing::error!(
                        scheduled_event_id = %id,
                        "pending-events: refusing non-single scheduled action"
                    );
                    summary.failed += 1;
                    let Some(expected_properties) =
                        current_properties_for_finalize(rt, ns_str, id, "non-single-action").await
                    else {
                        continue;
                    };
                    let Some(mut props) = expected_properties_value(&expected_properties, id)
                    else {
                        continue;
                    };
                    props["dispatch_error"] = json!(error);
                    props["dispatch_failed_at"] = json!(Utc::now().to_rfc3339());
                    props["status"] = json!("failed");
                    let completed_at = Utc::now().timestamp_micros();
                    props["dispatch_receipt"] = claim.completed_without_invocation_receipt(
                        DispatchReceiptState::NotInvoked,
                        completed_at,
                        Some(error),
                    );
                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        completed_at,
                        &claim,
                        &expected_properties,
                    )
                    .await
                    {
                        Ok(true) => summary.finalized += 1,
                        Ok(false) => summary.skipped_race += 1,
                        Err(error) => tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: non-single-action finalization failed"
                        ),
                    }
                    continue;
                }
                match mark_dispatch_invoking(rt, ns_str, id, &claim, lease).await {
                    Ok(true) => {}
                    Ok(false) => {
                        summary.failed += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: could not persist invocation-start receipt"
                        );
                        summary.failed += 1;
                        continue;
                    }
                }

                summary.invoked += 1;
                let (completion, persisted_outcome) = dispatch_with_renewable_lease(
                    DispatchLeaseTarget {
                        rt,
                        namespace: ns_str,
                        scheduled_event_id: id,
                        claim: &claim,
                    },
                    lease,
                    dsl,
                    dispatch_actor,
                    server,
                    verbose,
                )
                .await;
                let completion_error = match &completion {
                    DispatchCompletion::Succeeded => None,
                    DispatchCompletion::Failed(error)
                    | DispatchCompletion::Indeterminate(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            event_type,
                            recipient_actor = reminder_actor.as_deref(),
                            error = %khive_runtime::secret_gate::bounded_masked_log_text(
                                &error.to_string()
                            ),
                            "pending-events: scheduled event delivery failed"
                        );
                        Some(error.as_str().to_string())
                    }
                };

                // The dispatch helper keeps lease renewal active through this
                // outcome write. No secondary audit await occurs before it.
                let receipt = match persisted_outcome {
                    Ok(Some(receipt)) => {
                        summary.outcomes_persisted += 1;
                        receipt
                    }
                    Ok(None) => {
                        summary.failed += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: dispatch outcome receipt persistence failed"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };
                if event_type == "remind" {
                    if let Some(error) = completion_error.as_deref() {
                        append_reminder_delivery_failure_event(
                            server,
                            ns_str,
                            id,
                            &receipt_actor,
                            reminder_actor.as_deref().unwrap_or("local"),
                            error,
                        )
                        .await;
                    }
                }
                // Re-read the row's CURRENT properties immediately before
                // finalizing, and guard the terminal write on exact equality
                // to that read (mirroring `finalize_corrupt_receipt`'s
                // `selected_properties` guard, #7 in the RMW census). Dispatch
                // may have run for an arbitrary duration and this same process
                // may have renewed the lease meanwhile, so the pre-dispatch
                // `properties` snapshot captured at claim time is expected to
                // have moved; only a read taken right here — after this
                // process's own intervening writes have already landed —
                // can distinguish "nothing else touched this row since I last
                // looked" from a genuine concurrent writer.
                //
                // The race seam parks HERE, not before the claim: a test that
                // pauses earlier lands its concurrent write before the
                // candidate-page snapshot is taken, so the page already carries
                // that write and the test passes whether finalization rebuilds
                // from the stale page or from this fresh read. Parked here, the
                // write is genuinely between the claim and this read, which is
                // the only window that separates the two behaviours.
                #[cfg(test)]
                race_seam::pause_before_finalize_read().await;
                let expected_properties = match current_note_properties_text(rt, ns_str, id).await {
                    Ok(Some(text)) => text,
                    Ok(None) => {
                        summary.failed += 1;
                        continue;
                    }
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: could not read current properties before finalization"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };
                let Some(expected_value) = expected_properties_value(&expected_properties, id)
                else {
                    summary.failed += 1;
                    continue;
                };
                // Re-derive the SCHEDULING inputs from the fresh read too, not
                // just the properties blob. `trigger_at`, `trigger_offset` and
                // `repeat` above came from the pre-claim page snapshot, and
                // guarding the write on the fresh properties text protects the
                // blob while still letting a stale scheduling decision be
                // computed from it: `final_properties_after_dispatch` uses
                // these three to write the next `trigger_at` and the terminal
                // `status`. A writer that changed `repeat` or `trigger_at`
                // between the page snapshot and the fresh read would have its
                // value retained as the CAS base and then immediately
                // contradicted by a next-occurrence computed from the value it
                // replaced.
                //
                // The earlier values keep their job: they gate ADMISSION (is
                // this due, is it inside the grace window), which is a decision
                // about whether to dispatch at all and is correctly made from
                // what was observed before the claim. What must not come from
                // them is anything WRITTEN.
                let trigger_at_fresh_str = expected_value
                    .get("trigger_at")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let (trigger_at, trigger_offset) = match trigger_at_fresh_str
                    .parse::<DateTime<FixedOffset>>()
                {
                    Ok(fixed) => (fixed.with_timezone(&Utc), *fixed.offset()),
                    Err(_) => {
                        // The row's own `trigger_at` stopped being parseable
                        // between the page read and here. Refuse rather than
                        // fall back to the stale pair: falling back is
                        // exactly the silent-overwrite this guard exists to
                        // prevent, and the dispatch has already happened, so
                        // the honest outcome is a failed finalization that
                        // recovery will re-examine.
                        tracing::error!(
                            scheduled_event_id = %id,
                            trigger_at = %trigger_at_fresh_str,
                            "pending-events: trigger_at not parseable at finalization; refusing \
                             to finalize from the pre-claim snapshot"
                        );
                        summary.failed += 1;
                        continue;
                    }
                };
                // The receipt persisted at claim time names an occurrence
                // derived from the trigger the page query saw. If the row is
                // now scheduled for a different instant, writing a terminal row
                // would pair that receipt with a trigger it does not describe —
                // and terminal rows are past the reach of recovery, whose scan
                // fences on `status = 'firing'`, so nothing would ever
                // re-examine it. Refuse for the same reason and in the same
                // shape as the unparseable-trigger branch above: the dispatch
                // has happened, so the honest outcome is a failed finalization
                // that leaves the row `firing` for the receipt validator to
                // adjudicate once the lease expires.
                let fresh_occurrence_id = dispatch_occurrence_id(id, trigger_at);
                if fresh_occurrence_id != claim.occurrence_id {
                    tracing::error!(
                        scheduled_event_id = %id,
                        trigger_at = %trigger_at_fresh_str,
                        claimed_occurrence_id = %claim.occurrence_id,
                        fresh_occurrence_id = %fresh_occurrence_id,
                        "pending-events: the event was rescheduled after its dispatch was \
                         claimed; refusing to finalize a terminal row whose receipt names a \
                         different occurrence"
                    );
                    summary.failed += 1;
                    continue;
                }

                let repeat = expected_value
                    .get("repeat")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let (final_props, disposition) = final_properties_after_dispatch(
                    expected_value,
                    receipt,
                    &completion,
                    trigger_at,
                    trigger_offset,
                    &repeat,
                );
                if disposition == FinalDisposition::RecurrenceFailed {
                    tracing::error!(
                        scheduled_event_id = %id,
                        error = %final_props["recurrence_error"].as_str().unwrap_or(UNADVANCEABLE_REPEAT),
                        "pending-events: recurrence advancement failed after dispatch"
                    );
                }
                match finalize_fired_event(
                    rt,
                    ns_str,
                    id,
                    &final_props,
                    Utc::now().timestamp_micros(),
                    &claim,
                    &expected_properties,
                )
                .await
                {
                    Ok(true) => {
                        apply_final_disposition(&mut summary, disposition);
                        if !matches!(completion, DispatchCompletion::Succeeded)
                            && disposition != FinalDisposition::RecurrenceFailed
                        {
                            summary.failed += 1;
                        }
                    }
                    Ok(false) => summary.failed += 1,
                    Err(error) => {
                        tracing::error!(
                            scheduled_event_id = %id,
                            error = %error,
                            "pending-events: finalization failed after durable outcome"
                        );
                        summary.failed += 1;
                    }
                }
            }

            if page_len < PAGE_SIZE as usize {
                break;
            }
        }
    }

    Ok(summary)
}
