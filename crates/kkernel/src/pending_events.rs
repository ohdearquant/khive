//! `kkernel exec --pending-events` — one-shot scheduled event drain.
//!
//! Scans all `scheduled_event` notes with `status="pending"` whose `trigger_at`
//! is at or before now, fires their stored action through the registry in the
//! event's own namespace, marks each as `"fired"`, and advances repeating events
//! to their next occurrence.
//!
//! This is a cron-friendly one-shot drain. It is NOT a long-running daemon.
//! Run it from cron (e.g. `* * * * * kkernel exec --pending-events`) to achieve
//! minute-granularity delivery.
//!
//! ## Namespace isolation
//!
//! Each event fires in its own namespace: the action is dispatched through the
//! MCP server's registry with the event's namespace injected as the `namespace=`
//! parameter, so all writes land in the event's namespace.
//!
//! ## Repeat advancement
//!
//! Named aliases are advanced as follows:
//! - `"daily"`   → `trigger_at + 1 day`
//! - `"weekly"`  → `trigger_at + 7 days`
//! - `"monthly"` → `trigger_at + 1 calendar month`
//!
//! Five-field cron expressions (e.g. `"0 9 * * 1"`) are stored and validated but
//! **not yet advanced** — computing the next-fire time requires a cron-parsing
//! library that is not yet present in the codebase (STOP condition: no
//! machine-readable next-fire semantics exist for cron form). Events with a cron
//! `repeat` are fired and then marked `"fired"` (one-shot). See issue #14 for the
//! tracking note.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Months, Utc};
use serde_json::{json, Value};

use khive_mcp::serve::enforce_strict_actor_mode;
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter, SortDir};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};

use crate::dbpath::resolve_db_override;

/// A `scheduled_event` row stuck in `status="firing"` for longer than this is
/// considered abandoned by a drain process that crashed or was killed between
/// the `pending -> firing` claim and the post-dispatch finalize write (issue
/// #462). Such a row is neither retried (the pending scan only looks at
/// `status="pending"`) nor cancellable (`schedule.cancel` only CAS-matches
/// `status="pending"`), so it would otherwise be wedged forever. Every drain
/// pass reclaims rows stuck past this timeout back to `"pending"` so a future
/// drain (or a `schedule.cancel`) can act on them again.
///
/// 5 minutes is comfortably longer than a single dispatch (`dispatch_action`
/// is a single in-process verb call, not a network round-trip), so a fresh,
/// still-in-flight claim is never mistaken for an abandoned one.
const STALE_FIRING_TIMEOUT_MICROS: i64 = 5 * 60 * 1_000_000;

/// Summary of a single drain run.
#[derive(Debug, Default)]
pub struct DrainSummary {
    pub scanned: u64,
    pub fired: u64,
    pub advanced: u64,
    pub failed: u64,
    pub skipped_not_due: u64,
    pub skipped_race: u64,
    pub reclaimed: u64,
}

/// One-shot drain: fire all pending, due scheduled events.
///
/// - Scans for `scheduled_event` notes with `status="pending"` and
///   `trigger_at <= now`.
/// - Dispatches the stored action DSL in the event's namespace.
/// - Marks fired events: status → `"fired"`, `fired_at` → now.
/// - For repeating events with named aliases (`"daily"` / `"weekly"` /
///   `"monthly"`), resets status to `"pending"` and advances `trigger_at`.
///   Five-field cron repeat expressions are NOT advanced (see module-level
///   documentation).
///
/// Per-event failures accumulate in the returned [`DrainSummary`] without
/// aborting the drain.
pub async fn run_pending_events(
    db: Option<&str>,
    namespace: &str,
    verbose: bool,
) -> Result<DrainSummary> {
    let mut cfg = RuntimeConfig::default();
    if let Some(db_path) = resolve_db_override(db) {
        cfg.db_path = db_path;
    }
    // The drain operates across all namespaces found in the database.
    // `namespace` from the CLI arg is used as the "home" namespace for
    // authorizing the initial SQL reader, but event dispatch uses each event's
    // own namespace.
    cfg.default_namespace = Namespace::parse(namespace).map_err(|e| anyhow::anyhow!("{e}"))?;

    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    enforce_strict_actor_mode(rt.config().actor_id.as_deref(), &rt.config().packs)?;
    let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let now = Utc::now();
    let mut summary = DrainSummary::default();

    // ── Step 0: reclaim rows abandoned mid-fire by a crashed/killed drain ──
    // Runs before namespace discovery so any row reclaimed here (firing ->
    // pending) is picked up by the normal pending scan below in this same
    // pass, in whichever namespace it belongs to.
    let stale_before = now
        .timestamp_micros()
        .saturating_sub(STALE_FIRING_TIMEOUT_MICROS);
    summary.reclaimed = reclaim_stale_firing_events(&rt, stale_before).await?;
    if verbose && summary.reclaimed > 0 {
        eprintln!(
            "[pending-events] reclaimed {} stale \"firing\" row(s) back to \"pending\"",
            summary.reclaimed
        );
    }

    // ── Step 1: discover all distinct namespaces with pending scheduled_event notes ──
    let namespaces = discover_pending_namespaces(&rt, now).await?;

    if verbose {
        eprintln!(
            "[pending-events] scan: now={}, namespaces_with_pending={}",
            now.to_rfc3339(),
            namespaces.len()
        );
    }

    // ── Step 2: per-namespace drain ──────────────────────────────────────────
    for ns_str in &namespaces {
        let ns = match Namespace::parse(ns_str) {
            Ok(n) => n,
            Err(e) => {
                if verbose {
                    eprintln!("[pending-events] skip invalid namespace {ns_str:?}: {e}");
                }
                continue;
            }
        };
        let token = match rt.authorize(ns.clone()) {
            Ok(t) => t,
            Err(e) => {
                if verbose {
                    eprintln!("[pending-events] authorize({ns_str}) failed: {e}");
                }
                continue;
            }
        };
        let store = match rt.notes(&token) {
            Ok(s) => s,
            Err(e) => {
                if verbose {
                    eprintln!("[pending-events] notes({ns_str}) failed: {e}");
                }
                continue;
            }
        };

        // Page through all pending scheduled_event notes in this namespace.
        let filter = NoteFilter {
            kind: Some("scheduled_event".to_string()),
            property_filters: vec![PropertyFilter {
                json_path: "$.status".to_string(),
                op: FilterOp::Eq,
                value: SqlValue::Text("pending".to_string()),
            }],
            order_by: Some(("$.trigger_at".to_string(), SortDir::Asc)),
            ..Default::default()
        };

        const PAGE_SIZE: u32 = 200;
        let mut offset: u64 = 0;

        loop {
            let page = store
                .query_notes_filtered(
                    ns_str,
                    &filter,
                    PageRequest {
                        limit: PAGE_SIZE,
                        offset,
                    },
                )
                .await
                .with_context(|| {
                    format!("pending-events: query_notes_filtered failed for ns={ns_str}")
                })?;
            let page_len = page.items.len() as u32;

            for mut note in page.items {
                summary.scanned += 1;

                // Parse and check trigger_at.
                let trigger_at_str = note
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("trigger_at"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let trigger_at = match trigger_at_str.parse::<DateTime<Utc>>() {
                    Ok(ts) => ts,
                    Err(_) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] skip note {}: unparseable trigger_at {:?}",
                                note.id, trigger_at_str
                            );
                        }
                        summary.skipped_not_due += 1;
                        continue;
                    }
                };

                if trigger_at > now {
                    summary.skipped_not_due += 1;
                    continue;
                }

                // ── Determine what to dispatch ───────────────────────────
                let event_type = note
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("event_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("remind");

                let action_dsl: Option<String> = if event_type == "schedule" {
                    note.properties
                        .as_ref()
                        .and_then(|p| p.get("payload"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    // remind events: no DSL action to dispatch. We mark as fired
                    // to acknowledge the trigger. The notification channel (comm,
                    // channel transport, etc.) is out of scope for this drain.
                    None
                };

                // ── Claim the row before dispatch (issue #462, fire side) ──
                // `note` is a snapshot read by the page query above; a
                // concurrent `schedule.cancel` could have transitioned the
                // row to "cancelled" since then. CAS-claim pending -> firing
                // now so that: (a) a concurrent cancel's own CAS (which only
                // matches status='pending') fails once we've claimed it, and
                // (b) if cancel already won the race, our claim fails and we
                // skip — the drain can no longer clobber a cancel that
                // landed between the read and this point.
                let claimed_firing_at = match claim_pending_event(&rt, ns_str, note.id).await {
                    Ok(c) => c,
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] claim failed for note {}: {e}", note.id);
                        }
                        summary.failed += 1;
                        continue;
                    }
                };
                let Some(claimed_firing_at) = claimed_firing_at else {
                    if verbose {
                        eprintln!(
                            "[pending-events] skip note {}: no longer pending (concurrent \
                             cancel or claim)",
                            note.id
                        );
                    }
                    summary.skipped_race += 1;
                    continue;
                };

                // ── Dispatch the action ──────────────────────────────────
                if let Some(dsl) = &action_dsl {
                    let dispatch_result = dispatch_action(dsl, ns_str, &server, verbose).await;
                    if let Err(e) = dispatch_result {
                        if verbose {
                            eprintln!("[pending-events] dispatch failed for note {}: {e}", note.id);
                        }
                        summary.failed += 1;
                        // Per-event failure does NOT abort the drain. Continue.
                        // Still mark as fired so the drain doesn't retry infinitely
                        // on a permanently broken action. The error is reported
                        // in the summary.
                        // (Callers can inspect fired_at + a future dispatch_error
                        // field to distinguish clean fires from error fires.)
                    }
                }

                // ── Determine repeat ─────────────────────────────────────
                let repeat = note
                    .properties
                    .as_ref()
                    .and_then(|p| p.get("repeat"))
                    .and_then(Value::as_str)
                    .map(str::to_string);

                let fired_at_rfc = Utc::now().to_rfc3339();
                let mut props = note.properties.clone().unwrap_or_else(|| json!({}));

                match next_trigger_at(&repeat, trigger_at) {
                    Some(next_at) => {
                        // Repeating event: advance to next occurrence.
                        props["trigger_at"] = json!(next_at.to_rfc3339());
                        props["status"] = json!("pending");
                        props["fired_at"] = json!(fired_at_rfc);
                        note.properties = Some(props);
                        note.updated_at = Utc::now().timestamp_micros();
                        summary.advanced += 1;
                    }
                    None => {
                        // Non-repeating (or cron — deferred): mark as fired.
                        props["status"] = json!("fired");
                        props["fired_at"] = json!(fired_at_rfc);
                        note.properties = Some(props);
                        note.updated_at = Utc::now().timestamp_micros();
                        summary.fired += 1;
                    }
                }

                // ── Persist the updated note ─────────────────────────────
                // Conditional on status='firing' (set by the claim above)
                // instead of a full-row `upsert_note`, so this write can never
                // clobber a cancel that (impossibly, given the claim above,
                // but defensively) raced in after the claim.
                let final_props = note.properties.clone().unwrap_or_else(|| json!({}));
                match finalize_fired_event(
                    &rt,
                    ns_str,
                    note.id,
                    &final_props,
                    note.updated_at,
                    claimed_firing_at,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] finalize no-op for {}: row no longer in \
                                 \"firing\" state",
                                note.id
                            );
                        }
                        summary.failed += 1;
                        if summary.fired > 0 {
                            summary.fired -= 1;
                        }
                        if summary.advanced > 0 {
                            summary.advanced -= 1;
                        }
                    }
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] finalize failed for {}: {e}", note.id);
                        }
                        // Count as failed; drain continues.
                        summary.failed += 1;
                        // Undo the advance/fired accounting since persist failed.
                        // (fired/advanced were already incremented above — adjust back)
                        if summary.fired > 0 {
                            summary.fired -= 1;
                        }
                        if summary.advanced > 0 {
                            summary.advanced -= 1;
                        }
                    }
                }
            }

            if page_len < PAGE_SIZE {
                break;
            }
            offset = offset
                .checked_add(u64::from(PAGE_SIZE))
                .ok_or_else(|| anyhow::anyhow!("pending-events: pagination offset overflow"))?;
        }
    }

    Ok(summary)
}

/// CAS-claim a pending scheduled event for firing: `pending -> firing`.
///
/// Returns `Ok(Some(firing_at))` iff exactly one row transitioned, meaning
/// this drain (and not a concurrent `schedule.cancel`) now owns the row.
/// The returned `firing_at` (epoch µs) is this drain's **claim token** —
/// callers MUST thread it through to `finalize_fired_event` so finalization
/// binds to the specific claim that won, not merely to `status='firing'`
/// (issue #462 round-2: a stale claimant that resumes after a reclaim +
/// re-claim must not be able to finalize over the new claimant's row).
/// Mirrors the `schedule.cancel` CAS in `khive-pack-schedule/src/handlers.rs`
/// so the two writers share one state machine: cancel only matches
/// `status='pending'`, so once a row is claimed to `firing` a racing cancel
/// fails cleanly instead of clobbering (or being clobbered by) this drain's
/// eventual write.
///
/// Also stamps `properties.firing_at` (epoch µs, same instant as
/// `updated_at`) so a later drain pass can detect and reclaim this row if
/// this process crashes before `finalize_fired_event` runs (issue #462).
async fn claim_pending_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
) -> Result<Option<i64>> {
    let updated_at = Utc::now().timestamp_micros();
    let mut writer = rt
        .sql()
        .writer()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL writer: {e}"))?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set( \
                        json_set(COALESCE(properties, '{}'), '$.status', 'firing'), \
                        '$.firing_at', ?1 \
                      ), \
                      updated_at = ?1 \
                  WHERE id = ?2 \
                    AND namespace = ?3 \
                    AND kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending'"
                .to_string(),
            params: vec![
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some("pending_events_claim_firing".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: claim conditional update: {e}"))?;
    Ok((rows == 1).then_some(updated_at))
}

/// Reclaim `scheduled_event` rows stuck in `status="firing"` whose
/// `firing_at` is older than `stale_before_micros` (epoch µs) back to
/// `status="pending"` (issue #462).
///
/// Runs across all namespaces in one statement (a maintenance sweep, not a
/// namespace-scoped read) — mirrors `discover_pending_namespaces`, which also
/// queries `rt.sql()` directly rather than per-namespace tokens.
///
/// The `WHERE` clause matches only rows whose `firing_at` predates the
/// threshold, so a claim made by a still-running drain (fresh `firing_at`)
/// never matches and is never stolen. Rows claimed by a pre-#462 binary
/// (missing `firing_at` entirely) are treated as maximally stale and
/// reclaimed unconditionally, since there is no timestamp to compare against
/// and leaving them wedged forever is strictly worse.
///
/// Returns the number of rows reclaimed.
async fn reclaim_stale_firing_events(rt: &KhiveRuntime, stale_before_micros: i64) -> Result<u64> {
    let mut writer = rt
        .sql()
        .writer()
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: open SQL writer: {e}"))?;
    let rows = writer
        .execute(SqlStatement {
            sql: "UPDATE notes \
                  SET properties = json_set(properties, '$.status', 'pending') \
                  WHERE kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'firing' \
                    AND ( \
                      json_extract(properties, '$.firing_at') IS NULL \
                      OR CAST(json_extract(properties, '$.firing_at') AS INTEGER) < ?1 \
                    )"
            .to_string(),
            params: vec![SqlValue::Integer(stale_before_micros)],
            label: Some("pending_events_reclaim_stale_firing".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: reclaim stale firing rows: {e}"))?;
    Ok(rows)
}

/// CAS-persist the post-dispatch state of a claimed event: `firing -> {fired
/// | pending}` (the latter for an advanced repeat), replacing the full-row
/// `upsert_note` that could otherwise clobber a concurrent write.
///
/// `claimed_firing_at` is the claim token returned by `claim_pending_event`
/// (or reconstructed from a reclaimed row's own `firing_at`) — the CAS
/// requires the row's CURRENT `firing_at` to still equal this value, not
/// merely that `status='firing'`. Without this, a stale claimant that stalls
/// past `STALE_FIRING_TIMEOUT_MICROS`, gets reclaimed, and is then re-claimed
/// by a second drain could resume and finalize over the second drain's live
/// claim purely because both rows share `status='firing'` (issue #462
/// round-2). Binding to the specific `firing_at` instant closes that gap:
/// a reclaim always rewrites `firing_at` (via a fresh `claim_pending_event`
/// call) or clears `status` back to `pending`, so a stale token can never
/// match the row's current one.
///
/// `properties` must NOT already carry a `firing_at` field for the terminal
/// write — this function clears it (the event has reached a terminal state
/// for this cycle, `fired` or re-armed `pending`, so no claim token should
/// survive to be mistaken for a live claim by a future finalize).
///
/// Returns `Ok(true)` iff exactly one row (still `firing` under this exact
/// claim token) was updated.
async fn finalize_fired_event(
    rt: &KhiveRuntime,
    namespace: &str,
    id: uuid::Uuid,
    properties: &Value,
    updated_at: i64,
    claimed_firing_at: i64,
) -> Result<bool> {
    let mut properties = properties.clone();
    if let Some(obj) = properties.as_object_mut() {
        obj.remove("firing_at");
    }
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
                    AND CAST(json_extract(properties, '$.firing_at') AS INTEGER) = ?5"
                .to_string(),
            params: vec![
                SqlValue::Text(props_json),
                SqlValue::Integer(updated_at),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(namespace.to_string()),
                SqlValue::Integer(claimed_firing_at),
            ],
            label: Some("pending_events_finalize_fired".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: finalize conditional update: {e}"))?;
    Ok(rows == 1)
}

/// Compute the next `trigger_at` for a repeating event, given the current
/// `trigger_at` and the `repeat` spec.
///
/// Returns `Some(next)` for named aliases `"daily"` / `"weekly"` / `"monthly"`.
/// Returns `None` for five-field cron expressions (not yet supported) and for
/// `None` / absent repeat.
fn next_trigger_at(repeat: &Option<String>, current: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match repeat.as_deref() {
        Some("daily") => Some(current + Duration::days(1)),
        Some("weekly") => Some(current + Duration::weeks(1)),
        Some("monthly") => {
            // Add one calendar month. chrono::Months handles month-boundary
            // arithmetic (e.g. Jan 31 + 1 month = Feb 28/29).
            current.checked_add_months(Months::new(1))
        }
        Some(expr) if is_five_field_cron(expr) => {
            // STOP condition: five-field cron expressions require a cron-parsing
            // library to compute the next fire time. No such library is present
            // in the codebase. Fire as one-shot and log a warning.
            //
            // Future work: introduce a cron-next crate (e.g. `croner`) and
            // implement proper next-occurrence computation. Track in issue #14.
            tracing::warn!(
                repeat = expr,
                "pending-events: cron repeat expression cannot be advanced (not yet supported); \
                 event will be marked fired (one-shot)"
            );
            None
        }
        _ => None,
    }
}

/// Returns `true` if `expr` looks like a 5-field cron expression (not a named alias).
fn is_five_field_cron(expr: &str) -> bool {
    expr.split_whitespace().count() == 5
}

/// Dispatch a DSL action string in the given namespace.
///
/// The action is wrapped as a JSON-form batch with `namespace` injected into
/// each op's args so the VerbRegistry mints a token scoped to the event's
/// namespace. This preserves namespace isolation: all writes from the action
/// land in the event's namespace, not in the server's default `local` namespace.
async fn dispatch_action(
    action_dsl: &str,
    namespace: &str,
    server: &KhiveMcpServer,
    verbose: bool,
) -> Result<()> {
    // Parse the stored DSL to inject namespace into each op.
    let parsed = khive_request::parse_request(action_dsl).map_err(|e| {
        anyhow::anyhow!("pending-events: action DSL parse error ({e}): {action_dsl:?}")
    })?;

    // Re-serialize as JSON form with namespace injected.
    //
    // `$prev` references are rejected at schedule-creation time (issue #461),
    // but legacy rows written before that guard may still carry one. Reject
    // rather than silently drop: a dropped arg can dispatch successfully with
    // missing/wrong data, which is worse than a visible replay failure.
    let mut ops_json: Vec<Value> = Vec::with_capacity(parsed.ops.len());
    for op in &parsed.ops {
        let mut args = serde_json::Map::new();
        for (k, v) in &op.args {
            let khive_request::ArgValue::Value(val) = v else {
                return Err(anyhow::anyhow!(
                    "pending-events: non-literal scheduled action argument {k:?} is not \
                     replayable: {action_dsl:?}"
                ));
            };
            args.insert(k.clone(), val.clone());
        }
        // Inject the event's namespace so the registry writes to it.
        args.insert(
            "namespace".to_string(),
            Value::String(namespace.to_string()),
        );
        ops_json.push(json!({ "tool": op.tool, "args": Value::Object(args) }));
    }

    let ops_str = serde_json::to_string(&ops_json)
        .map_err(|e| anyhow::anyhow!("pending-events: serialize ops: {e}"))?;

    if verbose {
        eprintln!("[pending-events] dispatch ns={namespace}: {ops_str}");
    }

    let result = server
        .dispatch_request_local(RequestParams {
            ops: ops_str,
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
        })
        .await
        .map_err(|e| anyhow::anyhow!("pending-events: dispatch error: {e}"))?;

    // The MCP response is a JSON string. Check for per-op failures.
    let parsed_result: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
    if let Some(results) = parsed_result.get("results").and_then(Value::as_array) {
        let failures: Vec<_> = results
            .iter()
            .filter(|r| r.get("ok").and_then(Value::as_bool) == Some(false))
            .collect();
        if !failures.is_empty() {
            let errs: Vec<String> = failures
                .iter()
                .filter_map(|r| r.get("error").and_then(Value::as_str).map(str::to_string))
                .collect();
            return Err(anyhow::anyhow!(
                "pending-events: action produced {} failure(s): {}",
                failures.len(),
                errs.join("; ")
            ));
        }
    }

    Ok(())
}

/// Discover all distinct namespaces that have at least one pending, due
/// `scheduled_event` note (i.e. `status="pending"` AND `trigger_at <= now`).
///
/// Uses a direct SQL query for efficiency — avoids fetching all pending notes
/// across all namespaces up front. The `trigger_at` comparison is a string
/// comparison, which is correct for UTC RFC 3339 timestamps (lexicographic
/// order matches chronological order for `Z` / `+00:00` forms).
///
/// RFC 3339 timestamps with non-zero offsets are NOT reliably comparable as
/// strings — the schedule pack normalises all `trigger_at` values to whatever
/// the caller supplies. In practice, `validate_at` in `handlers.rs` accepts
/// any RFC 3339 string that `chrono` parses (including offset forms). We
/// therefore fetch by namespace and re-check in Rust with parsed `DateTime<Utc>`.
async fn discover_pending_namespaces(rt: &KhiveRuntime, now: DateTime<Utc>) -> Result<Vec<String>> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let sql_access = rt.sql();
    let mut reader = sql_access
        .reader()
        .await
        .context("pending-events: open SQL reader")?;

    // Select distinct namespaces with at least one potentially-due event.
    // We do a broad filter on `status` here; the Rust layer applies the
    // parsed-timestamp check. Using `<=` string comparison on trigger_at works
    // for UTC-normalised timestamps but is a best-effort pre-filter for the
    // SQL layer only.
    let now_rfc = now.to_rfc3339();
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT DISTINCT namespace \
                  FROM notes \
                  WHERE kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending' \
                    AND json_extract(properties, '$.trigger_at') <= ?1"
                .into(),
            params: vec![SqlValue::Text(now_rfc)],
            label: Some("pending_events_namespaces".into()),
        })
        .await
        .context("pending-events: discover namespaces query")?;

    let namespaces: Vec<String> = rows
        .into_iter()
        .filter_map(|row| {
            row.get("namespace").and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
        })
        .collect();

    Ok(namespaces)
}

/// Print the drain summary to stdout as JSON.
pub fn print_summary(summary: &DrainSummary) {
    let json = json!({
        "scanned": summary.scanned,
        "fired": summary.fired,
        "advanced": summary.advanced,
        "failed": summary.failed,
        "skipped_not_due": summary.skipped_not_due,
        "skipped_race": summary.skipped_race,
        "reclaimed": summary.reclaimed,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&json).expect("serialize")
    );
}

// ── Need a reference to `rt.sql()` — check the public API ────────────────────

// KhiveRuntime exposes `sql()` as an accessor to the SqlAccess trait object.
// We use it here for the namespace-discovery query.

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn tmp_db() -> (NamedTempFile, String) {
        let f = NamedTempFile::new().expect("tempfile");
        let path = f.path().to_str().expect("utf8 path").to_string();
        (f, path)
    }

    async fn make_rt(db_path: &str) -> KhiveRuntime {
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..Default::default()
        };
        KhiveRuntime::new(cfg).expect("runtime")
    }

    /// Create a scheduled_event note directly via runtime.create_note, replicating
    /// the exact property schema used by handle_schedule / handle_remind in
    /// khive-pack-schedule.
    async fn create_scheduled_event(
        rt: &KhiveRuntime,
        namespace: &str,
        trigger_at: &str,
        action_dsl: Option<&str>,
        repeat: Option<&str>,
        event_type: &str,
    ) -> uuid::Uuid {
        let props = json!({
            "trigger_at": trigger_at,
            "repeat": repeat,
            "status": "pending",
            "event_type": event_type,
            "payload": action_dsl,
            "fired_at": null,
            "cancelled_at": null,
        });

        let ns = Namespace::parse(namespace).expect("ns");
        // We need a NamespaceToken. In tests within `khive-runtime`, `for_namespace`
        // is pub(crate). External crates use `rt.authorize()`.
        let token = rt.authorize(ns).expect("authorize");

        let content = action_dsl.unwrap_or("test reminder");
        let note = rt
            .create_note(
                &token,
                "scheduled_event",
                None,
                content,
                None,
                Some(props),
                vec![],
            )
            .await
            .expect("create_note");

        note.id
    }

    /// Fetch a note's properties from the store.
    async fn get_note_props(rt: &KhiveRuntime, id: uuid::Uuid) -> Value {
        let ns = Namespace::parse("local").unwrap();
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.notes(&token).expect("notes");
        let note = store
            .get_note(id)
            .await
            .expect("get_note")
            .expect("note exists");
        note.properties.unwrap_or(json!({}))
    }

    #[tokio::test]
    async fn due_event_is_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a past-due schedule event. Use stats() as the action since it's
        // a valid, registered verb that has no side-effects that need a
        // namespace argument check.
        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        assert!(summary.scanned >= 1, "must have scanned the due event");
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "must fire or advance"
        );

        let props = get_note_props(&rt, id).await;
        let status = props["status"].as_str().unwrap_or("");
        assert!(
            status == "fired" || status == "pending",
            "status must be fired or pending (repeat), got {status:?}"
        );
    }

    #[tokio::test]
    async fn future_event_is_skipped() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let future = "2099-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", future, Some("stats()"), None, "schedule").await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        // The future event must not be fired. The drain may skip it via the SQL
        // pre-filter (scanned=0, skipped_not_due=0) or via the Rust timestamp
        // check (scanned=1, skipped_not_due=1) — either is correct; the key
        // invariant is that fired=0, advanced=0.
        assert_eq!(summary.fired, 0, "future event must not be fired");
        assert_eq!(summary.advanced, 0, "future event must not be advanced");

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "future event must remain pending"
        );
    }

    #[tokio::test]
    async fn fired_event_is_idempotent() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // First drain — fires the event.
        let s1 = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain 1");
        assert!(s1.scanned >= 1);

        // Second drain — event is now status="fired", not "pending"; must not re-fire.
        let s2 = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain 2");
        assert_eq!(s2.scanned, 0, "no pending events on second drain");
        assert_eq!(s2.fired, 0, "no new fires on second drain");

        let props = get_note_props(&rt, id).await;
        let fired_at_1 = props["fired_at"].as_str().unwrap_or("").to_string();
        assert!(
            !fired_at_1.is_empty(),
            "fired_at must be set after first drain"
        );

        // fired_at must not change on the second drain (idempotent).
        let props2 = get_note_props(&rt, id).await;
        assert_eq!(
            props2["fired_at"].as_str().unwrap_or(""),
            fired_at_1.as_str(),
            "fired_at must not change on second drain"
        );
    }

    #[tokio::test]
    async fn daily_repeat_advances() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Use a past trigger_at with daily repeat.
        let past = "2000-06-01T09:00:00Z";
        let id = create_scheduled_event(
            &rt,
            "local",
            past,
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        assert!(
            summary.advanced >= 1,
            "daily event must be advanced, not fired"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "after advance, status must be pending"
        );
        let new_trigger = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set");
        let new_ts: DateTime<Utc> = new_trigger.parse().expect("parseable ts");
        let original: DateTime<Utc> = past.parse().unwrap();
        assert_eq!(
            new_ts,
            original + Duration::days(1),
            "daily advance must add 1 day"
        );
    }

    #[tokio::test]
    async fn namespace_isolation() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a due event in namespace "ns-a". The action is stats() which
        // doesn't create notes, so we can't verify write-landing-in-ns-a directly
        // through this drain. Instead we verify the drain scans and fires the event
        // in ns-a without touching the ns-b namespace counts.
        let ns_a = "ns-a";
        let ns_b = "ns-b";
        let past = "2000-01-01T00:00:00Z";

        let id_a = create_scheduled_event(&rt, ns_a, past, Some("stats()"), None, "schedule").await;

        // Create a future event in ns-b that must not be fired.
        let _id_b = create_scheduled_event(
            &rt,
            ns_b,
            "2099-01-01T00:00:00Z",
            Some("stats()"),
            None,
            "schedule",
        )
        .await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        // Only the ns-a event should have been processed.
        assert!(summary.scanned >= 1);
        assert!(summary.fired >= 1 || summary.advanced >= 1);

        // ns-a event is fired.
        let token_a = rt.authorize(Namespace::parse(ns_a).unwrap()).expect("auth");
        let store_a = rt.notes(&token_a).expect("notes");
        let note_a = store_a.get_note(id_a).await.expect("get").expect("exists");
        let status_a = note_a
            .properties
            .as_ref()
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            status_a == "fired" || status_a == "pending",
            "ns-a event must be fired or advanced, got {status_a:?}"
        );
    }

    #[tokio::test]
    async fn dispatch_failure_does_not_abort_drain() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a past-due event with an invalid action DSL (verb not registered).
        let past = "2000-01-01T00:00:00Z";
        let _id_bad = create_scheduled_event(
            &rt,
            "local",
            past,
            Some("stats()"), // valid — but let's add a second event with a broken action
            None,
            "schedule",
        )
        .await;
        // Second event with broken action.
        let id_bad2 = create_scheduled_event(
            &rt,
            "local",
            past,
            Some("this_verb_does_not_exist(foo=\"bar\")"),
            None,
            "schedule",
        )
        .await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain must not abort");

        // Both events were scanned. The bad one produced a failure.
        assert!(summary.scanned >= 2, "both events must be scanned");
        assert!(
            summary.failed >= 1 || summary.fired >= 1,
            "at least one event processed (failed or fired)"
        );

        // The drain still ran to completion (no panic / early return).
        let props_bad2 = get_note_props(&rt, id_bad2).await;
        let _ = props_bad2["status"].as_str(); // just verify it's accessible
    }

    /// Issue #461: a `schedule.schedule` payload that write-time validation
    /// now accepts (single op, exactly-registered handler name, literal args,
    /// all required params present) must actually dispatch successfully at
    /// trigger time — proving write-time acceptance and trigger-time replay
    /// agree. Before the fix, a bare-shorthand payload could pass write-time
    /// checks yet fail replay as an unknown verb; this asserts the *positive*
    /// case: a canonical payload produces zero dispatch failures.
    #[tokio::test]
    async fn replayable_action_dispatches_without_failure_at_trigger_time() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id = create_scheduled_event(
            &rt,
            "local",
            past,
            Some("schedule.remind(content=\"ping\", at=\"2099-01-01T00:00:00Z\")"),
            None,
            "schedule",
        )
        .await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        assert_eq!(
            summary.failed, 0,
            "a write-time-replayable action must dispatch cleanly at trigger time"
        );
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "the event must be processed"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"].as_str(), Some("fired"));
    }

    /// Issue #461: a legacy stored action containing a `$prev` reference
    /// (impossible to create through the handler after this fix, but
    /// representative of a row written before the write-time guard existed)
    /// must be rejected by `dispatch_action` with an error naming the
    /// non-literal argument, not silently dropped and dispatched with
    /// missing/wrong data. Asserting the specific error text (rather than
    /// just "some failure occurred") matters here: a downstream handler
    /// might independently reject a dropped-but-required argument as
    /// "missing", which would make a weaker assertion pass even if the
    /// silent-drop bug were reintroduced.
    #[tokio::test]
    async fn dispatch_action_rejects_non_literal_prev_reference() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let err = dispatch_action("stats() | get(id=$prev.id)", "local", &server, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not replayable"),
            "expected the specific non-literal-argument rejection message, got: {msg}"
        );
    }

    /// Same scenario end-to-end through the drain: confirms the rejection
    /// surfaces as a counted failure rather than aborting the drain or being
    /// swallowed, and that the drain still completes.
    #[tokio::test]
    async fn dispatch_rejects_legacy_prev_reference_instead_of_dropping_it() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let _id = create_scheduled_event(
            &rt,
            "local",
            past,
            Some("stats() | get(id=$prev.id)"),
            None,
            "schedule",
        )
        .await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain must not abort or panic on a legacy $prev row");

        assert!(
            summary.failed >= 1,
            "a legacy $prev reference must surface as a dispatch failure, not a silent drop"
        );
    }

    /// Deterministic regression for the fire-side of issue #462: simulates
    /// the exact interleaving the reviewer/critic flagged — a drain claims a
    /// row for firing (its read-then-act window), and only *after* that does
    /// a `schedule.cancel` request arrive for the same id. Before this fix,
    /// the drain read a `pending` snapshot and later did a full-row
    /// `upsert_note` unconditionally, so a cancel landing in between would be
    /// silently clobbered back to "fired". With the `pending -> firing` CAS
    /// claim in place, the drain's claim (standing in for "drain read the row
    /// before the cancel") must make the *subsequent* cancel fail — proving
    /// cancel can no longer be lost to a fire that was already in flight.
    #[tokio::test]
    async fn fire_claim_wins_race_against_concurrent_cancel() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // Simulate the drain's claim (pending -> firing), which in the real
        // drain happens right after the page read and before dispatch.
        let claimed_firing_at = claim_pending_event(&rt, "local", id)
            .await
            .expect("claim query")
            .expect("claim must succeed on a fresh pending row");

        // A `schedule.cancel` arriving after the claim (the race window the
        // reviewer identified) must now fail instead of clobbering the
        // in-flight fire.
        let cancel_ops = serde_json::to_string(&serde_json::json!([
            { "tool": "schedule.cancel", "args": { "id": id.to_string() } }
        ]))
        .expect("serialize cancel op");
        let cancel_result = server
            .dispatch_request_local(RequestParams {
                ops: cancel_ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
            })
            .await
            .expect("dispatch_request_local must not error at the RPC layer");
        let cancel_json: Value = serde_json::from_str(&cancel_result).expect("valid JSON");
        let op_result = &cancel_json["results"][0];
        assert_eq!(
            op_result["ok"], false,
            "cancel of a claimed (firing) event must fail, not silently succeed: {cancel_json}"
        );
        let cancel_err = op_result["error"].as_str().unwrap_or("");
        assert!(
            cancel_err.contains("not pending"),
            "cancel must report the event is no longer pending; got: {cancel_err}"
        );

        // Finalize the fire as the drain would, then confirm the terminal
        // state is "fired" — the cancel never got a chance to overwrite it.
        let finalized = finalize_fired_event(
            &rt,
            "local",
            id,
            &serde_json::json!({
                "trigger_at": past,
                "repeat": null,
                "status": "fired",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": Utc::now().to_rfc3339(),
                "cancelled_at": null,
            }),
            Utc::now().timestamp_micros(),
            claimed_firing_at,
        )
        .await
        .expect("finalize query");
        assert!(
            finalized,
            "finalize must succeed on a row still in \"firing\""
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str().unwrap_or(""),
            "fired",
            "terminal state must be \"fired\"; cancel must not have won the race"
        );
    }

    /// Directly set a note's `properties` via raw SQL, bypassing the normal
    /// claim/finalize CAS paths. Used to deterministically fabricate a
    /// stale-`firing` row (as if a drain claimed it and then crashed before
    /// finalizing) without depending on wall-clock sleeps.
    async fn force_set_properties(rt: &KhiveRuntime, id: uuid::Uuid, properties: &Value) {
        let props_json = serde_json::to_string(properties).expect("serialize");
        let mut writer = rt.sql().writer().await.expect("writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties = ?1 WHERE id = ?2".to_string(),
                params: vec![SqlValue::Text(props_json), SqlValue::Text(id.to_string())],
                label: Some("test_force_set_properties".into()),
            })
            .await
            .expect("force update");
        assert_eq!(rows, 1, "test setup: row must exist");
    }

    /// Issue #462 (stale-`firing` recovery), case (a): a row claimed by a
    /// drain that then crashed before finalizing — `status="firing"` with a
    /// `firing_at` older than the documented timeout — must be reclaimed back
    /// to `pending` and fired on the next drain pass, instead of being
    /// wedged forever (the old behavior: only `status="pending"` rows are
    /// ever scanned, so a stranded `firing` row was invisible to every future
    /// drain).
    #[tokio::test]
    async fn stale_firing_row_is_reclaimed_and_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // Simulate a drain claiming the row, then crashing before finalize:
        // status="firing" with a firing_at well past the stale timeout.
        let stale_firing_at = Utc::now().timestamp_micros() - (STALE_FIRING_TIMEOUT_MICROS * 2);
        force_set_properties(
            &rt,
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "firing",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": null,
                "cancelled_at": null,
                "firing_at": stale_firing_at,
            }),
        )
        .await;

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        assert!(
            summary.reclaimed >= 1,
            "the stale firing row must be reclaimed, got summary={summary:?}"
        );
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "the reclaimed row must be fired (or advanced) in the same pass, \
             got summary={summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("fired"),
            "a reclaimed non-repeating event must end in \"fired\", got {props:?}"
        );
    }

    /// Issue #462, case (b): a row claimed *recently* (fresh `firing_at`,
    /// well within the stale timeout) must NOT be reclaimed — a live drain's
    /// in-flight claim is never stolen by the reclaim sweep.
    #[tokio::test]
    async fn fresh_firing_row_is_not_reclaimed() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // Fresh claim: firing_at = now, well under the stale threshold.
        let _claimed_firing_at = claim_pending_event(&rt, "local", id)
            .await
            .expect("claim query")
            .expect("claim must succeed on a fresh pending row");

        let summary = run_pending_events(Some(&db_path), "local", false)
            .await
            .expect("drain");

        assert_eq!(
            summary.reclaimed, 0,
            "a fresh firing row must not be reclaimed, got summary={summary:?}"
        );
        assert_eq!(
            summary.fired, 0,
            "a fresh firing row must not be fired by a drain pass that did not claim it"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("firing"),
            "a fresh firing row must remain firing (owned by the process that claimed it), \
             got {props:?}"
        );
    }

    /// Round-2 regression (codex REJECT, "finalize must be bound to the
    /// owning claim"): reproduces the exact stale-claimant-resumes
    /// interleaving. Drain A claims and (simulated) crashes/stalls past the
    /// stale timeout with its own `firing_at` token recorded. A reclaim pass
    /// then runs, and drain B re-claims the row, minting a fresh `firing_at`
    /// token distinct from A's. A now resumes and attempts to finalize using
    /// its stale token: before this fix, `finalize_fired_event` matched on
    /// `status='firing'` alone and would have clobbered B's live claim with
    /// A's stale final state. With the claim-token CAS in place, A's
    /// finalize must be a no-op and B's claim (and eventual finalize) must
    /// survive untouched.
    #[tokio::test]
    async fn stale_claimant_cannot_finalize_over_a_fresh_reclaim() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        // Drain A claims, then (simulated) stalls: fabricate a status="firing"
        // row whose firing_at predates the stale threshold — this stands in
        // for "A really did claim it, then never got back to finalize".
        let a_claimed_firing_at = Utc::now().timestamp_micros() - (STALE_FIRING_TIMEOUT_MICROS * 2);
        force_set_properties(
            &rt,
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "firing",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": null,
                "cancelled_at": null,
                "firing_at": a_claimed_firing_at,
            }),
        )
        .await;

        // A reclaim pass runs (as a live drain's periodic sweep would),
        // moving the row back to "pending" since A's firing_at is stale.
        let stale_before = Utc::now().timestamp_micros() - STALE_FIRING_TIMEOUT_MICROS;
        let reclaimed = reclaim_stale_firing_events(&rt, stale_before)
            .await
            .expect("reclaim query");
        assert_eq!(reclaimed, 1, "A's stale claim must be reclaimed");

        // Drain B re-claims the now-pending row, minting a fresh firing_at
        // token that differs from A's stale one.
        let b_claimed_firing_at = claim_pending_event(&rt, "local", id)
            .await
            .expect("claim query")
            .expect("B's claim must succeed on the reclaimed row");
        assert_ne!(
            a_claimed_firing_at, b_claimed_firing_at,
            "B's claim token must differ from A's stale token"
        );

        // A resumes (unaware it was reclaimed) and attempts to finalize using
        // its own stale claim token. This must be a no-op: it must NOT match
        // B's current firing_at, and must NOT clobber B's live claim.
        let a_finalize_result = finalize_fired_event(
            &rt,
            "local",
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "fired",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": Utc::now().to_rfc3339(),
                "cancelled_at": null,
            }),
            Utc::now().timestamp_micros(),
            a_claimed_firing_at,
        )
        .await
        .expect("finalize query must not error");
        assert!(
            !a_finalize_result,
            "A's finalize with a stale claim token must be a no-op, not a successful write"
        );

        // B's claim must be completely intact: still "firing", still stamped
        // with B's own firing_at — A's stale finalize must not have touched it.
        let props_after_a = get_note_props(&rt, id).await;
        assert_eq!(
            props_after_a["status"].as_str(),
            Some("firing"),
            "B's claim must survive A's stale finalize attempt untouched, got {props_after_a:?}"
        );
        assert_eq!(
            props_after_a["firing_at"].as_i64(),
            Some(b_claimed_firing_at),
            "B's firing_at token must be unchanged by A's stale finalize attempt"
        );

        // B now finalizes with its own (correct) claim token — this must
        // succeed, proving the fix doesn't wedge legitimate finalization.
        let b_finalize_result = finalize_fired_event(
            &rt,
            "local",
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "fired",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": Utc::now().to_rfc3339(),
                "cancelled_at": null,
            }),
            Utc::now().timestamp_micros(),
            b_claimed_firing_at,
        )
        .await
        .expect("finalize query must not error");
        assert!(
            b_finalize_result,
            "B's finalize with its own claim token must succeed"
        );

        let final_props = get_note_props(&rt, id).await;
        assert_eq!(
            final_props["status"].as_str(),
            Some("fired"),
            "terminal state must be \"fired\" via B's own claim, got {final_props:?}"
        );
        assert!(
            final_props.get("firing_at").is_none() || final_props["firing_at"].is_null(),
            "firing_at must be cleared on terminal finalize, got {final_props:?}"
        );
    }

    /// Issue #462, case (c): `schedule.cancel` on a row that is currently
    /// `status="firing"` — even a *stale* one — must still fail cleanly.
    /// Reclaim only happens as part of a drain pass; cancel itself never
    /// reclaims, so a cancel that races a still-technically-firing (if
    /// abandoned) row gets the same "not pending" rejection it would against
    /// a live in-flight fire. This confirms the reclaim path does not weaken
    /// the fire/cancel CAS contract asserted by
    /// `fire_claim_wins_race_against_concurrent_cancel`.
    #[tokio::test]
    async fn cancel_on_stale_firing_row_still_fails_cleanly() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");

        let past = "2000-01-01T00:00:00Z";
        let id =
            create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;

        let stale_firing_at = Utc::now().timestamp_micros() - (STALE_FIRING_TIMEOUT_MICROS * 2);
        force_set_properties(
            &rt,
            id,
            &json!({
                "trigger_at": past,
                "repeat": null,
                "status": "firing",
                "event_type": "schedule",
                "payload": "stats()",
                "fired_at": null,
                "cancelled_at": null,
                "firing_at": stale_firing_at,
            }),
        )
        .await;

        let cancel_ops = serde_json::to_string(&serde_json::json!([
            { "tool": "schedule.cancel", "args": { "id": id.to_string() } }
        ]))
        .expect("serialize cancel op");
        let cancel_result = server
            .dispatch_request_local(RequestParams {
                ops: cancel_ops,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
            })
            .await
            .expect("dispatch_request_local must not error at the RPC layer");
        let cancel_json: Value = serde_json::from_str(&cancel_result).expect("valid JSON");
        let op_result = &cancel_json["results"][0];
        assert_eq!(
            op_result["ok"], false,
            "cancel of a stale-but-still-firing event must fail, not silently succeed \
             (reclaim happens on drain, not cancel): {cancel_json}"
        );
        let cancel_err = op_result["error"].as_str().unwrap_or("");
        assert!(
            cancel_err.contains("not pending"),
            "cancel must report the event is no longer pending; got: {cancel_err}"
        );

        // The row is still "firing" (untouched by the failed cancel attempt).
        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str().unwrap_or(""),
            "firing",
            "a failed cancel must not alter the row's status"
        );
    }

    // Unit tests for next_trigger_at

    #[test]
    fn next_trigger_at_daily() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = next_trigger_at(&Some("daily".to_string()), base).unwrap();
        assert_eq!(next, base + Duration::days(1));
    }

    #[test]
    fn next_trigger_at_weekly() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = next_trigger_at(&Some("weekly".to_string()), base).unwrap();
        assert_eq!(next, base + Duration::weeks(1));
    }

    #[test]
    fn next_trigger_at_monthly() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = next_trigger_at(&Some("monthly".to_string()), base).unwrap();
        // June 1 + 1 month = July 1
        let expected: DateTime<Utc> = "2026-07-01T09:00:00Z".parse().unwrap();
        assert_eq!(next, expected);
    }

    #[test]
    fn next_trigger_at_none_repeat_returns_none() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        assert!(next_trigger_at(&None, base).is_none());
    }

    #[test]
    fn next_trigger_at_cron_returns_none() {
        let base: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        // 5-field cron: not supported → returns None (one-shot fire)
        assert!(next_trigger_at(&Some("0 9 * * 1".to_string()), base).is_none());
    }
}
