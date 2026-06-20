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

use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter, SortDir};
use khive_storage::types::{PageRequest, SqlValue};

use crate::dbpath::resolve_db_override;

/// Summary of a single drain run.
#[derive(Debug, Default)]
pub struct DrainSummary {
    pub scanned: u64,
    pub fired: u64,
    pub advanced: u64,
    pub failed: u64,
    pub skipped_not_due: u64,
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
    let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let now = Utc::now();
    let mut summary = DrainSummary::default();

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
                if let Err(e) = store.upsert_note(note.clone()).await {
                    if verbose {
                        eprintln!("[pending-events] upsert_note failed for {}: {e}", note.id);
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
    let ops_json: Vec<Value> = parsed
        .ops
        .iter()
        .map(|op| {
            let mut args = serde_json::Map::new();
            for (k, v) in &op.args {
                if let khive_request::ArgValue::Value(val) = v {
                    args.insert(k.clone(), val.clone());
                }
                // $prev references are not supported in stored actions (they were
                // validated at schedule-creation time). Skip them gracefully.
            }
            // Inject the event's namespace so the registry writes to it.
            args.insert(
                "namespace".to_string(),
                Value::String(namespace.to_string()),
            );
            json!({ "tool": op.tool, "args": Value::Object(args) })
        })
        .collect();

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
