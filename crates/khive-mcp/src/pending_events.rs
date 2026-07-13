//! Scheduled event drain — `kkernel exec --pending-events` (one-shot) and the
//! daemon-resident tick (ADR-106, [`schedule_tick_loop`]).
//!
//! Scans all `scheduled_event` notes with `status="pending"` whose `trigger_at`
//! is at or before now, dispatches scheduled actions or delivers reminders to
//! their creating actors through `comm.send`, marks each as `"fired"`, and
//! advances repeating events to their next occurrence. Events overdue by more
//! than the configured grace window are never dispatched. See "Missed-event
//! policy" below.
//!
//! This module lives in `khive-mcp` (not `kkernel`, where it originated)
//! because the daemon tick loop needs to call it in-process from
//! `khive-mcp::serve`, and `khive-runtime` (where the daemon's socket/accept
//! loop lives) cannot depend back on `khive-mcp` (`khive-mcp` already depends
//! on `khive-runtime` — a dependency the other way would cycle). `kkernel`
//! already depends on `khive-mcp`, so its `exec --pending-events` entry point
//! simply calls [`run_pending_events`] here instead of a local module.
//!
//! ## Invocation modes
//!
//! - **One-shot** (`kkernel exec --pending-events`, cron-friendly): call
//!   [`run_pending_events`] directly. Suitable for `* * * * * kkernel exec
//!   --pending-events` to achieve minute-granularity delivery.
//! - **Daemon-resident tick** (ADR-106): [`schedule_tick_loop`] calls
//!   [`run_pending_events_on`] against the daemon's own resolved `KhiveRuntime`
//!   handle on a fixed interval for the lifetime of the warm `khived` daemon
//!   process. Spawned only by the daemon role (mirrors the
//!   `is_daemon_role` gate `khive-mcp::serve` already uses for the email
//!   channel loops), never by a short-lived stdio client. Running both an
//!   external cron entry and the daemon tick at once is safe: the drain's
//!   `pending -> firing` CAS claim (`claim_pending_event`) makes concurrent or
//!   overlapping invocations harmless by construction — at most one caller
//!   ever wins a given row.
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
//!
//! ## Missed-event policy (ADR-106 amendment)
//!
//! An event is "missed" when it is discovered overdue by more than
//! `KHIVE_FIRE_GRACE_SECS` (default 300s / 5 minutes). A missed event is
//! **never dispatched** — it is marked `status="missed"` with `missed_at`
//! stamped (epoch µs) and `fired_at` left null. A missed *repeating* event is
//! skipped for this occurrence and re-armed at the next occurrence strictly
//! after now (looping past every accumulated occurrence) — it never fires a
//! catch-up burst. This means a daemon that was offline for a long stretch
//! (or a first boot against a store with a large stale backlog) marks the
//! entire overdue backlog missed on its first tick and dispatches zero of
//! them. See the ADR-106 amendment for the full rationale and the prior-art
//! comparison.

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Months, Utc};
use serde_json::{json, Value};

use crate::server::KhiveMcpServer;
use crate::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{EventKind, EventOutcome, SubstrateKind};

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

/// Default grace window (seconds): an event discovered overdue by more than
/// this is "missed" rather than fired late. Overridable via
/// `KHIVE_FIRE_GRACE_SECS`. See the module-level "Missed-event policy" docs.
const DEFAULT_FIRE_GRACE_SECS: i64 = 300;

/// Resolve the missed-event grace window from `KHIVE_FIRE_GRACE_SECS`,
/// falling back to [`DEFAULT_FIRE_GRACE_SECS`] when unset or unparseable as a
/// non-negative integer.
fn fire_grace_from_env() -> Duration {
    let secs = std::env::var("KHIVE_FIRE_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|&s| s >= 0)
        .unwrap_or(DEFAULT_FIRE_GRACE_SECS);
    Duration::seconds(secs)
}

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
    /// IDs of `scheduled_event` notes marked `"missed"` (or re-armed past a
    /// missed occurrence) this pass — never dispatched. See the module-level
    /// "Missed-event policy" docs.
    pub missed: Vec<uuid::Uuid>,
}

/// One-shot drain: fire all pending, due scheduled events.
///
/// - Scans for `scheduled_event` notes with `status="pending"` and
///   `trigger_at <= now`.
/// - Dispatches the stored action DSL or reminder inbox delivery in the event's namespace.
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
    // Resolve through the SAME multi-backend-aware construction the daemon
    // boot path uses (`khive-mcp::serve::build_server_with_explicit_namespace`),
    // rather than a throwaway `RuntimeConfig::default()` (codex PR #782
    // review round 2, High finding continuation): `RuntimeConfig::default()`
    // is env-only — it never consulted `khive.toml` (`[[backends]]`,
    // `[actor] id`, `[packs.*].backend`) at all, so a project with a
    // declared multi-backend config or a tier-3 config-file actor identity
    // was silently invisible to this one-shot CLI path even though
    // `kkernel mcp --daemon` (and, for ordinary ops, `kkernel exec`'s own
    // `resolve_runtime_config` call) both resolve it. The wrapper also
    // returns the fully-wired `KhiveMcpServer` for the resolved pack set
    // (single- or multi-backend), so replayed actions route through the
    // correct per-pack backend exactly like the daemon tick now does — not a
    // single runtime standing in for every pack (the same High finding this
    // fix-round closes for the daemon-resident tick). `kkernel exec` has no
    // `--config` flag today (see `kkernel::exec::run_exec`'s own
    // `resolve_runtime_config` call), so this mirrors that: `config: None`
    // still triggers `khive.toml`'s standard cwd/home search order inside
    // `resolve_runtime_config`.
    //
    // This does NOT call `crate::serve::build_server` directly (PR #782
    // review round 4, High finding): `build_server` derives BOTH
    // `namespace_explicit` and `actor_explicit` from `resolve_cli_namespace`,
    // which treats "a namespace value is present" and "the operator typed
    // `--actor`/`--namespace`" as the same fact — true for a real CLI parse,
    // where there is no other way a namespace value could appear. This
    // wrapper's `namespace` argument is not a CLI flag the operator typed;
    // it is a plain default this function was called with (`"local"` unless
    // the caller passed something else), and `resolve_runtime_config`
    // (`serve.rs`) treats a genuine explicit actor override as authoritative
    // — it clears any configured `[actor] id` for the resolved-to-"local"
    // case rather than falling through to it. Routing this default namespace
    // through `build_server` therefore silently discarded a project's
    // configured `[actor] id`, contradicting Amendment B's claim that this
    // CLI path honors it, and — under strict actor mode with the comm pack —
    // could make server construction itself fail despite a valid config.
    // `build_server_with_explicit_namespace` is the seam that lets this
    // caller assert the narrower, correct semantic instead: the namespace
    // *is* a real default (`namespace_explicit: true`, so it still becomes
    // `default_namespace` and fills `actor_id` when non-"local"), but it is
    // NOT an actor override (`actor_explicit: false`), so a `"local"`
    // resolution keeps falling through to the project/db/env actor tiers —
    // exactly the shape `kkernel exec`/`kkernel reindex` already use via
    // their own direct `resolve_runtime_config` calls (see
    // `RuntimeConfigInputs::actor_explicit`'s field doc).
    let ns = Namespace::parse(namespace)
        .map_err(|e| anyhow::anyhow!("pending-events: invalid namespace {namespace:?}: {e}"))?;
    let args = crate::args::Args {
        db: db.map(str::to_string),
        actor: None,
        namespace: None,
        no_embed: false,
        pack: Vec::new(),
        config: None,
        daemon: false,
        transport: None,
        bind: None,
        brain_profile: None,
        resumed_generation: None,
    };
    let (server, schedule_rt) =
        crate::serve::build_server_with_explicit_namespace(&args, ns, true, false)
            .map_err(|e| anyhow::anyhow!("pending-events: build server: {e}"))?;
    let rt = schedule_rt.ok_or_else(|| {
        anyhow::anyhow!(
            "pending-events: resolved pack set does not include \"schedule\"; nothing to drain"
        )
    })?;
    run_pending_events_on(&rt, &server, verbose).await
}

/// One-shot drain against an already-constructed [`KhiveRuntime`] +
/// [`KhiveMcpServer`] pair (ADR-106 fix-round: codex PR #782 review).
///
/// [`run_pending_events`] is the CLI-facing entry point (`kkernel exec
/// --pending-events`); it now resolves both `rt` and `server` via
/// `khive-mcp::serve::build_server_with_explicit_namespace`, the same multi-backend-aware
/// construction the daemon boot path uses, one fresh pair per invocation —
/// correct for a short-lived cron-invoked process.
///
/// The daemon-resident tick ([`schedule_tick_loop`]) must NOT build its own
/// pair: the daemon boot path (`khive-mcp::serve::build_server` /
/// `build_registry_for_multi_backend`) already resolves `--config`,
/// `[[backends]]`, actor identity, and `--pack` selection once at startup.
/// This function therefore takes both by reference — the caller supplies the
/// already-resolved, already-validated pair so its storage target, actor
/// identity, and pack set are always identical to the server it is ticking
/// for (or, for the one-shot path, to what `build_server` just resolved).
///
/// `rt` and `server` serve two different roles that must NOT be collapsed
/// into one (round 1 of this fix-round did exactly that, and codex's round-2
/// review caught it): `rt` is the **schedule pack's own runtime** — the scan/
/// claim/finalize SQL below reads and CAS-writes `scheduled_event` notes
/// directly through it, so it must point at whichever backend the `schedule`
/// pack is wired to. `server` is the **daemon's live, fully-wired
/// `KhiveMcpServer`** — every pack registered against its own backend per
/// `[[backends]]`/`[packs.*].backend` — used only for `dispatch_action`
/// (replaying a stored action's DSL). Building a second `KhiveMcpServer` from
/// `rt` alone (`KhiveMcpServer::new(rt.clone())`) would register EVERY pack
/// against the schedule backend, so a replayed `comm.send` (or any other
/// pack's action) would silently dispatch into the schedule backend instead
/// of that pack's configured one in a multi-backend deployment — passing the
/// real `server` in is what keeps replay routing identical to a live request.
pub async fn run_pending_events_on(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
) -> Result<DrainSummary> {
    let now = Utc::now();
    let grace = fire_grace_from_env();
    let mut summary = DrainSummary::default();

    // ── Step 0: reclaim rows abandoned mid-fire by a crashed/killed drain ──
    // Runs before namespace discovery so any row reclaimed here (firing ->
    // pending) is picked up by the normal pending scan below in this same
    // pass, in whichever namespace it belongs to.
    let stale_before = now
        .timestamp_micros()
        .saturating_sub(STALE_FIRING_TIMEOUT_MICROS);
    summary.reclaimed = reclaim_stale_firing_events(rt, stale_before).await?;
    if verbose && summary.reclaimed > 0 {
        eprintln!(
            "[pending-events] reclaimed {} stale \"firing\" row(s) back to \"pending\"",
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

        // Bounded, mutation-immune keyset pagination (codex PR #782 review
        // round 2, Medium finding #2 — a continuation of round 1's fix).
        //
        // Round 1 snapshotted every `status="pending"` row for the namespace
        // into one `Vec` before any mutation, which fixed the LIMIT/OFFSET
        // skip bug (mutating a row out of the `status="pending"` predicate
        // mid-page shifted every subsequent page) but introduced a new
        // failure mode: the snapshot filter checked only `status`, not
        // `trigger_at`, so a namespace with one due event buried in a large
        // FUTURE schedule pulled the entire future backlog into memory every
        // tick. This version instead:
        //   1. pushes the due-ness predicate (`trigger_at <= now`) into the
        //      SQL `WHERE` clause directly, via a raw statement (bypassing
        //      `NoteFilter`, whose `order_by`/property-filter surface can
        //      only express JSON-path predicates, not compare a JSON path
        //      against a bind parameter with `<=`) — future events are never
        //      fetched at all, so the working set is bounded by the due
        //      backlog, not the namespace's total schedule size;
        //   2. pages via a `(created_at, id)` keyset cursor instead of
        //      `LIMIT/OFFSET`. Both columns are immutable — this drain never
        //      rewrites `created_at` or `id` — so a row's claim/dispatch/
        //      finalize mutation between pages can never shift a later
        //      page's boundary (the round-1 bug class), and at most
        //      `PAGE_SIZE` rows are held in memory at once (never the whole
        //      namespace).
        const PAGE_SIZE: u32 = 200;
        let now_rfc = now.to_rfc3339();
        let mut cursor: Option<(i64, String)> = None;
        loop {
            let (sql, params): (String, Vec<SqlValue>) = match &cursor {
                //
                // The due-ness predicate compares via SQLite's `datetime()`,
                // not a raw string `<=` (PR #782 review round 3, High finding): stored
                // `trigger_at` values are NOT normalized to UTC —
                // `khive-pack-schedule`'s `handle_remind`/`handle_schedule`
                // deliberately round-trip the caller's original string
                // (offset included, H5), and `validate_at` accepts any RFC
                // 3339 offset. A raw lexicographic `<=` against a UTC
                // `now`-string therefore mis-ranks any non-UTC-offset
                // `trigger_at`: e.g. `"2026-07-10T02:00:00+04:00"`
                // (chronologically `2026-07-09T22:00:00Z`, overdue) sorts
                // AFTER a UTC `now` string like
                // `"2026-07-10T00:47:00.123+00:00"` as raw text, so it would
                // never be fetched — never fire, never get marked missed,
                // forever. `datetime(...)` normalizes both sides to UTC
                // before comparing, so the predicate is chronological
                // regardless of the stored string's offset. Storage itself
                // is unchanged — only this fetch-bound comparison is
                // normalized; the original string still round-trips
                // faithfully (H5).
                //
                // `datetime()` returns NULL for a value it cannot parse, and
                // NULL <= anything is NULL (never true) — the OR clause below
                // keeps an unparseable `trigger_at` row in the candidate set
                // instead of silently dropping it, so the existing Rust-side
                // unparseable-`trigger_at` branch (which logs and advances
                // the cursor past it) still sees it. `validate_at` rejects
                // unparseable `trigger_at` at write time, so this only
                // matters for a hand-written or pre-validation row.
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
                let mut properties: Option<Value> = match row.get("properties") {
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

                // Parse and check trigger_at.
                let trigger_at_str = properties
                    .as_ref()
                    .and_then(|p| p.get("trigger_at"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let trigger_at = match trigger_at_str.parse::<DateTime<Utc>>() {
                    Ok(ts) => ts,
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

                let reminder_actor = if event_type == "remind" && !is_missed {
                    let stored_actor = properties
                        .as_ref()
                        .and_then(|p| p.get("created_by_actor"))
                        .and_then(Value::as_str);
                    if stored_actor.is_none() {
                        tracing::warn!(
                            scheduled_event_id = %id,
                            "pending-events: legacy reminder has no created_by_actor; using the \
                             scheduler actor"
                        );
                    }
                    Some(
                        stored_actor
                            .or_else(|| server.actor_id())
                            .unwrap_or("local")
                            .to_string(),
                    )
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

                // ── Determine repeat (read before claim; only informs which
                // finalize branch runs below, never mutates the row read) ──
                let repeat = properties
                    .as_ref()
                    .and_then(|p| p.get("repeat"))
                    .and_then(Value::as_str)
                    .map(str::to_string);

                // ── Claim the row before dispatch (issue #462, fire side) ──
                // `properties` above is a page-query snapshot; a concurrent
                // `schedule.cancel` could have transitioned the row to
                // "cancelled" since then. CAS-claim pending -> firing now so
                // that: (a) a concurrent cancel's own CAS (which only
                // matches status='pending') fails once we've claimed it, and
                // (b) if cancel already won the race, our claim fails and we
                // skip — the drain can no longer clobber a cancel that
                // landed between the read and this point. The same claim
                // guards the missed path: a missed event still needs
                // exclusive ownership before it can be marked "missed" or
                // re-armed to a future occurrence.
                let claimed_firing_at = match claim_pending_event(rt, ns_str, id).await {
                    Ok(c) => c,
                    Err(e) => {
                        if verbose {
                            eprintln!("[pending-events] claim failed for note {id}: {e}");
                        }
                        summary.failed += 1;
                        continue;
                    }
                };
                let Some(claimed_firing_at) = claimed_firing_at else {
                    if verbose {
                        eprintln!(
                            "[pending-events] skip note {id}: no longer pending (concurrent \
                             cancel or claim)"
                        );
                    }
                    summary.skipped_race += 1;
                    continue;
                };

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
                    let mut props = properties.clone().unwrap_or_else(|| json!({}));
                    props["missed_at"] = json!(now.timestamp_micros());
                    match advance_repeat_past_missed(&repeat, trigger_at, now) {
                        Some(next_at) => {
                            // Repeating event: skip this occurrence, re-arm
                            // pending at the next future one.
                            props["trigger_at"] = json!(next_at.to_rfc3339());
                            props["status"] = json!("pending");
                        }
                        None => {
                            // Non-repeating (or non-advancing cron): terminal
                            // "missed". `fired_at` stays null/untouched.
                            props["status"] = json!("missed");
                        }
                    }
                    let updated_at = Utc::now().timestamp_micros();

                    match finalize_fired_event(
                        rt,
                        ns_str,
                        id,
                        &props,
                        updated_at,
                        claimed_firing_at,
                    )
                    .await
                    {
                        Ok(true) => {
                            summary.missed.push(id);
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
                let mut reminder_delivery_error = None;
                if let Some(dsl) = &action_dsl {
                    let dispatch_result = dispatch_action(dsl, ns_str, server, verbose).await;
                    if let Err(e) = dispatch_result {
                        tracing::error!(
                            scheduled_event_id = %id,
                            event_type,
                            recipient_actor = reminder_actor.as_deref(),
                            error = %e,
                            "pending-events: scheduled event delivery failed"
                        );
                        if verbose {
                            eprintln!("[pending-events] dispatch failed for note {id}: {e}");
                        }
                        summary.failed += 1;
                        if event_type == "remind" {
                            let error = e.to_string();
                            append_reminder_delivery_failure_event(
                                server,
                                ns_str,
                                id,
                                reminder_actor.as_deref().unwrap_or("local"),
                                &error,
                            )
                            .await;
                            reminder_delivery_error = Some(error);
                        }
                        // Per-event failure does NOT abort the drain. Continue.
                        // Still mark as fired so the drain doesn't retry infinitely
                        // on a permanently broken action. The error is reported
                        // in the summary.
                        // (Callers can inspect fired_at + a future dispatch_error
                        // field to distinguish clean fires from error fires.)
                    }
                }

                let fired_at_rfc = Utc::now().to_rfc3339();
                let mut props = properties.clone().unwrap_or_else(|| json!({}));
                if event_type == "remind" {
                    if let Some(error) = reminder_delivery_error {
                        props["delivery_error"] = json!(error);
                        props["delivery_failed_at"] = json!(fired_at_rfc);
                    } else if let Some(obj) = props.as_object_mut() {
                        obj.remove("delivery_error");
                        obj.remove("delivery_failed_at");
                    }
                }
                let updated_at;

                match next_trigger_at(&repeat, trigger_at) {
                    Some(next_at) => {
                        // Repeating event: advance to next occurrence.
                        props["trigger_at"] = json!(next_at.to_rfc3339());
                        props["status"] = json!("pending");
                        props["fired_at"] = json!(fired_at_rfc);
                        properties = Some(props);
                        updated_at = Utc::now().timestamp_micros();
                        summary.advanced += 1;
                    }
                    None => {
                        // Non-repeating (or cron — deferred): mark as fired.
                        props["status"] = json!("fired");
                        props["fired_at"] = json!(fired_at_rfc);
                        properties = Some(props);
                        updated_at = Utc::now().timestamp_micros();
                        summary.fired += 1;
                    }
                }

                // ── Persist the updated note ─────────────────────────────
                // Conditional on status='firing' (set by the claim above)
                // instead of a full-row `upsert_note`, so this write can never
                // clobber a cancel that (impossibly, given the claim above,
                // but defensively) raced in after the claim.
                let final_props = properties.clone().unwrap_or_else(|| json!({}));
                match finalize_fired_event(
                    rt,
                    ns_str,
                    id,
                    &final_props,
                    updated_at,
                    claimed_firing_at,
                )
                .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        if verbose {
                            eprintln!(
                                "[pending-events] finalize no-op for {id}: row no longer in \
                                 \"firing\" state"
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
                            eprintln!("[pending-events] finalize failed for {id}: {e}");
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

            if page_len < PAGE_SIZE as usize {
                break;
            }
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

/// Advance a missed repeating event's `trigger_at` past every occurrence at
/// or before `now`, landing on the first occurrence strictly after `now`
/// (ADR-106 missed-event amendment). This is what makes a missed repeat
/// re-arm without ever firing a catch-up burst: a daily reminder that was
/// due 10 times while the daemon was down skips straight to tomorrow's
/// occurrence instead of firing 10 times in a row.
///
/// Returns `None` when the event does not advance at all (no `repeat`, or an
/// unsupported cron form per [`next_trigger_at`]) — the caller then marks the
/// event terminally `"missed"` instead of re-arming it.
///
/// Terminates because [`next_trigger_at`]'s named-alias arms are always
/// strictly increasing (`current + positive duration`), so each loop
/// iteration moves `current` forward; `now` is fixed, so the loop reaches
/// `next > now` in a bounded number of steps.
fn advance_repeat_past_missed(
    repeat: &Option<String>,
    current: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let mut current = current;
    loop {
        let next = next_trigger_at(repeat, current)?;
        if next > now {
            return Some(next);
        }
        current = next;
    }
}

fn reminder_delivery_action(actor: &str, content: &str) -> String {
    let action = json!([{
        "tool": "comm.send",
        "args": {
            "to": actor,
            "subject": reminder_subject(content),
            "content": content,
        }
    }]);
    serde_json::to_string(&action).expect("reminder delivery action is JSON-serializable")
}

fn reminder_subject(content: &str) -> String {
    const MAX_HEAD_CHARS: usize = 80;
    let collapsed = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = collapsed.chars();
    let head: String = chars.by_ref().take(MAX_HEAD_CHARS).collect();
    if chars.next().is_some() {
        format!("[Reminder] {head}…")
    } else if head.is_empty() {
        "[Reminder]".to_string()
    } else {
        format!("[Reminder] {head}")
    }
}

async fn append_reminder_delivery_failure_event(
    server: &KhiveMcpServer,
    namespace: &str,
    scheduled_event_id: uuid::Uuid,
    recipient_actor: &str,
    error: &str,
) {
    let Some(store) = server.event_store() else {
        return;
    };
    let event = khive_storage::Event::new(
        namespace,
        "schedule.remind.fire",
        EventKind::Audit,
        SubstrateKind::Note,
        recipient_actor,
    )
    .with_outcome(EventOutcome::Error)
    .with_target(scheduled_event_id)
    .with_payload(json!({
        "scheduled_event_id": scheduled_event_id,
        "recipient_actor": recipient_actor,
        "error": error,
    }));
    if let Err(trace_error) = store.append_event(event).await {
        tracing::error!(
            scheduled_event_id = %scheduled_event_id,
            error = %trace_error,
            "pending-events: reminder delivery failure event append failed"
        );
    }
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
/// across all namespaces up front. The `trigger_at` comparison is done via
/// SQLite's `datetime(...)`, not a raw string comparison (PR #782 review
/// round 3, High finding): `khive-pack-schedule` round-trips the caller's
/// original `trigger_at` string verbatim, offset included (H5), and
/// `validate_at` in `handlers.rs` accepts any RFC 3339 offset — a raw-text
/// `<=` only matches chronological order when every stored string happens to
/// share `now`'s UTC offset, which is not guaranteed. `datetime(...)`
/// normalizes both sides to UTC before comparing, making the comparison
/// chronological regardless of offset; storage still round-trips the
/// caller's original string unchanged (H5 unaffected). The Rust layer
/// downstream still re-parses and re-checks each candidate row with
/// `DateTime<Utc>` as the final authority — this SQL predicate is a fetch
/// bound, not the last word.
async fn discover_pending_namespaces(rt: &KhiveRuntime, now: DateTime<Utc>) -> Result<Vec<String>> {
    use khive_storage::types::{SqlStatement, SqlValue};

    let sql_access = rt.sql();
    let mut reader = sql_access
        .reader()
        .await
        .context("pending-events: open SQL reader")?;

    // Select distinct namespaces with at least one potentially-due event.
    // We do a broad filter on `status` here; the Rust layer applies the
    // parsed-timestamp check. This is a pre-filter gate for the per-namespace
    // candidate scan below, not the final due-ness decision — but a
    // namespace excluded HERE never reaches that scan at all, so it must be
    // held to the same correctness bar as the candidate-page queries
    // (`datetime(...)` normalization, PR #782 review round 3 High finding): comparing
    // `trigger_at` against `now` as raw TEXT is only chronologically correct
    // when every stored string happens to share `now`'s UTC offset.
    // `khive-pack-schedule` round-trips the caller's original `trigger_at`
    // string verbatim (offset included, H5), so a non-UTC-offset value can
    // sort on the wrong side of a raw-text comparison and silently exclude
    // its entire namespace from every future pass — not just skip one row.
    // `datetime(...)` normalizes both sides to UTC before comparing; the `OR
    // ... IS NULL` clause keeps a namespace with an unparseable `trigger_at`
    // visible rather than silently dropped, matching the candidate-page
    // queries' same NULL-safety rider.
    let now_rfc = now.to_rfc3339();
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT DISTINCT namespace \
                  FROM notes \
                  WHERE kind = 'scheduled_event' \
                    AND deleted_at IS NULL \
                    AND json_extract(properties, '$.status') = 'pending' \
                    AND ( \
                      datetime(json_extract(properties, '$.trigger_at')) <= datetime(?1) \
                      OR datetime(json_extract(properties, '$.trigger_at')) IS NULL \
                    )"
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
        "missed_count": summary.missed.len(),
        "missed_ids": summary.missed.iter().map(uuid::Uuid::to_string).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&json).expect("serialize")
    );
}

// ── Need a reference to `rt.sql()` — check the public API ────────────────────

// KhiveRuntime exposes `sql()` as an accessor to the SqlAccess trait object.
// We use it here for the namespace-discovery query.

/// Default interval between daemon-resident schedule ticks, in seconds.
/// Matches the cadence the module doc already documents for the external-cron
/// invocation (`* * * * * kkernel exec --pending-events` is minute-grain;
/// 60s is the same order of magnitude for the in-daemon tick).
const DEFAULT_TICK_INTERVAL_SECS: u64 = 60;

/// Resolve the daemon tick interval from `KHIVE_SCHEDULE_TICK_SECS`, falling
/// back to `DEFAULT_TICK_INTERVAL_SECS` (60s) when unset or not a positive
/// integer.
pub fn tick_interval_from_env() -> std::time::Duration {
    let secs = std::env::var("KHIVE_SCHEDULE_TICK_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_TICK_INTERVAL_SECS);
    std::time::Duration::from_secs(secs)
}

/// Daemon-resident periodic drain loop (ADR-106).
///
/// Runs [`run_pending_events_on`] on a fixed interval for as long as the
/// daemon process lives. Only the daemon role spawns this loop (mirrors the
/// `is_daemon_role` gate `khive-mcp::serve` already applies to the email
/// channel loops, #602) — a short-lived `kkernel exec`/stdio client process
/// never calls this, so there is exactly one tick loop per live daemon.
///
/// `rt` is the daemon's own resolved runtime handle for the `"schedule"` pack
/// — for a single-backend boot, the one `KhiveRuntime` the whole daemon
/// shares; for a multi-backend boot (ADR-028 `[[backends]]`), the specific
/// per-pack runtime `schedule` was wired to. `khive-mcp::serve::build_server`
/// resolves this once at daemon boot (the same `--config`/`[[backends]]`/
/// actor-identity/`--pack` resolution the live server itself uses) and passes
/// it through here, so every tick drains the SAME storage target under the
/// SAME actor identity and pack set as the daemon it belongs to — never a
/// silently-reconstructed `RuntimeConfig::default()` (codex PR #782 review,
/// High finding: a config-backed daemon's tick could otherwise drain
/// `$HOME/.khive/khive.db` instead of the configured backend, trip
/// strict-actor-mode failures the live server never has, or dispatch stored
/// actions through packs the daemon never loaded). `rt.clone()` is cheap
/// (`KhiveRuntime` is `Arc`-wrapped internally) — every tick reuses the same
/// warm connection pool rather than opening a fresh one.
///
/// Ticks on a fixed `tokio::time::interval` with
/// [`tokio::time::MissedTickBehavior::Skip`] rather than sleeping `interval`
/// AFTER each drain: a sleep-after-drain loop's effective cadence is
/// `interval + drain_duration`, which drifts further behind on every pass
/// that finds a nontrivial backlog (codex PR #782 review, Medium finding
/// "tick cadence deviates from the accepted interval contract" — ADR-106
/// specifies a fixed interval). The first tick fires after one full
/// `interval` has elapsed (via `interval_at(now + interval, interval)`),
/// matching the original sleep-based boot behavior instead of draining
/// immediately at daemon start.
///
/// `server` is the daemon's own live [`KhiveMcpServer`] (cloned — cheap,
/// `Arc`-wrapped internally), used ONLY for replaying a fired event's stored
/// action DSL (`dispatch_action`, inside [`run_pending_events_on`]). This is
/// a SEPARATE handle from `rt` on purpose (codex PR #782 review round 2, High
/// finding): round 1 of this fix-round built a fresh `KhiveMcpServer::new(rt
/// .clone())` from the schedule runtime alone, which registered EVERY pack
/// against the schedule backend — correct for scanning `scheduled_event`
/// rows (which do live on the schedule backend), but wrong for dispatch: a
/// replayed `comm.send` (or any other pack's action) would then have run
/// against the schedule backend instead of that pack's own configured
/// backend in a multi-backend deployment. Passing the daemon's actual,
/// already-multi-backend-wired `server` through here — the SAME one
/// `build_server` handed back alongside `rt` at boot — keeps replayed-action
/// routing identical to a live request against this daemon.
///
/// A per-tick failure (e.g. a transient SQL error) is logged and does not
/// stop the loop — the next tick simply tries again.
pub async fn schedule_tick_loop(
    rt: KhiveRuntime,
    server: KhiveMcpServer,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        match run_pending_events_on(&rt, &server, false).await {
            Ok(summary) => {
                if summary.fired > 0
                    || summary.advanced > 0
                    || summary.failed > 0
                    || !summary.missed.is_empty()
                {
                    tracing::info!(
                        scanned = summary.scanned,
                        fired = summary.fired,
                        advanced = summary.advanced,
                        missed = summary.missed.len(),
                        failed = summary.failed,
                        reclaimed = summary.reclaimed,
                        "schedule tick: drain pass complete"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "schedule tick: drain pass failed");
            }
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use khive_runtime::{Gate, GateDecision, GateError, GateRequest, RuntimeConfig};
    use khive_storage::event::EventFilter;
    use khive_storage::types::PageRequest;
    use tempfile::NamedTempFile;

    #[derive(Debug)]
    struct DenyCommSendGate;

    impl Gate for DenyCommSendGate {
        fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
            if request.verb == "comm.send" {
                Ok(GateDecision::deny(
                    "comm.send denied by delivery-failure test",
                ))
            } else {
                Ok(GateDecision::allow())
            }
        }
    }

    fn tmp_db() -> (NamedTempFile, String) {
        let f = NamedTempFile::new().expect("tempfile");
        let path = f.path().to_str().expect("utf8 path").to_string();
        (f, path)
    }

    /// An RFC 3339 timestamp a few seconds in the past — due, but comfortably
    /// inside the default 300s missed-event grace window (ADR-106 amendment),
    /// so tests exercising the normal fire/advance path aren't swept into the
    /// missed path by a fixed year-2000 sentinel. Tests exercising the missed
    /// path itself use their own far-past or `now`-relative timestamps.
    fn due_rfc3339() -> String {
        (Utc::now() - Duration::seconds(5)).to_rfc3339()
    }

    /// A UTC "now" RFC 3339 string, formatted the same way the candidate-page
    /// query's bind parameter is (`now.to_rfc3339()` on a `DateTime<Utc>`).
    /// Used only by the offset-sorting regressions below to assert their own
    /// test fixtures actually exercise the raw-text lexicographic-ordering
    /// bug class they're named for, independent of the real query's own
    /// `now` capture (a few milliseconds of drift between the two calls is
    /// irrelevant next to the multi-hour offset margins those tests use).
    fn now_rfc3339_for_ordering_check() -> String {
        Utc::now().to_rfc3339()
    }

    async fn make_rt(db_path: &str) -> KhiveRuntime {
        make_rt_with_actor(db_path, None).await
    }

    async fn make_rt_with_actor(db_path: &str, actor_id: Option<&str>) -> KhiveRuntime {
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            actor_id: actor_id.map(str::to_string),
            ..Default::default()
        };
        KhiveRuntime::new(cfg).expect("runtime")
    }

    /// Drive one drain pass directly through [`run_pending_events_on`] against
    /// a fresh `make_rt`-built runtime, bypassing [`run_pending_events`] (the
    /// CLI-facing one-shot entrypoint this test module used to call directly).
    ///
    /// `run_pending_events` now resolves through `khive-mcp::serve::build_server`
    /// (codex PR #782 review round 2, High finding continuation), which is
    /// TOML-aware (`KhiveConfig::load_with_home_fallback`) so that `kkernel
    /// exec --pending-events` honors a project's `[[backends]]`/`[actor]`
    /// config exactly like the daemon does. That makes it depend on process
    /// `HOME`/cwd, which the tests in this module don't isolate (unlike
    /// `serve.rs`'s own `SeatEnv`-guarded `build_server` tests) — on a
    /// developer machine with a real `~/.khive/config.toml` declaring
    /// `[[backends]]`, calling `run_pending_events` with a scratch `--db` path
    /// would hit "cannot be combined with [[backends]]" instead of exercising
    /// the drain logic these tests actually target. These tests are about
    /// drain semantics (claim/dispatch/finalize/pagination/cadence), not CLI
    /// config resolution, so they build their own runtime + server directly —
    /// exactly what `run_pending_events_on` itself required even before this
    /// fix-round, and what `run_pending_events` did internally, unconditionally,
    /// prior to this fix-round's config-resolution fix.
    async fn drain_for_test(db_path: &str) -> Result<DrainSummary> {
        let rt = make_rt(db_path).await;
        let server = KhiveMcpServer::new(rt.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        run_pending_events_on(&rt, &server, false).await
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
        let ns = Namespace::parse(namespace).expect("ns");
        let token = rt.authorize(ns).expect("authorize");
        let props = json!({
            "trigger_at": trigger_at,
            "repeat": repeat,
            "status": "pending",
            "event_type": event_type,
            "created_by_actor": token.actor().id.clone(),
            "payload": action_dsl,
            "fired_at": null,
            "cancelled_at": null,
        });

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

    async fn inbound_reminder_messages(rt: &KhiveRuntime, actor: &str) -> Vec<(String, Value)> {
        let mut reader = rt.sql().reader().await.expect("open SQL reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT content, properties FROM notes \
                      WHERE kind = 'message' \
                        AND json_extract(properties, '$.direction') = 'inbound' \
                        AND json_extract(properties, '$.to_actor') = ?1 \
                      ORDER BY created_at ASC, id ASC"
                    .to_string(),
                params: vec![SqlValue::Text(actor.to_string())],
                label: Some("test_inbound_reminder_messages".into()),
            })
            .await
            .expect("query reminder messages");
        rows.into_iter()
            .map(|row| {
                let content = match row.get("content") {
                    Some(SqlValue::Text(value)) => value.clone(),
                    other => panic!("unexpected content column: {other:?}"),
                };
                let properties = match row.get("properties") {
                    Some(SqlValue::Text(value)) => {
                        serde_json::from_str(value).expect("message properties JSON")
                    }
                    other => panic!("unexpected properties column: {other:?}"),
                };
                (content, properties)
            })
            .collect()
    }

    async fn make_repeat_due_again(rt: &KhiveRuntime, id: uuid::Uuid) {
        let mut writer = rt.sql().writer().await.expect("open SQL writer");
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE notes \
                      SET properties = json_set(properties, '$.trigger_at', ?1) \
                      WHERE id = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(due_rfc3339()),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test_repeat_due_again".into()),
            })
            .await
            .expect("make repeat due again");
        assert_eq!(rows, 1, "repeat fixture row updated");
    }

    #[test]
    fn reminder_subject_marks_and_truncates_the_content_head() {
        let content = format!("  {}\n tail", "x".repeat(90));
        let subject = reminder_subject(&content);
        assert!(subject.starts_with("[Reminder] "));
        assert!(subject.ends_with('…'));
        assert_eq!(subject.chars().count(), "[Reminder] ".chars().count() + 81);
    }

    #[tokio::test]
    async fn fired_reminder_delivers_to_creator_after_daemon_actor_changes() {
        let (_tmp, db_path) = tmp_db();
        let creator = "lambda:reminder-owner";
        let daemon_actor = "lambda:replacement-daemon";
        let id = {
            let creator_rt = make_rt_with_actor(&db_path, Some(creator)).await;
            let creator_server = KhiveMcpServer::new(creator_rt.clone()).expect("creator server");
            let remind_ops = serde_json::to_string(&json!([{
                "tool": "schedule.remind",
                "args": {
                    "content": "test reminder",
                    "at": "2099-01-01T00:00:00Z"
                }
            }]))
            .expect("serialize reminder op");
            let result = creator_server
                .dispatch_request_local(RequestParams {
                    ops: remind_ops,
                    ..Default::default()
                })
                .await
                .expect("create reminder through schedule.remind");
            let result: Value = serde_json::from_str(&result).expect("reminder result JSON");
            assert_eq!(result["results"][0]["ok"], true, "{result}");
            let id = result["results"][0]["result"]["full_id"]
                .as_str()
                .expect("reminder full_id")
                .parse()
                .expect("reminder UUID");
            let props = get_note_props(&creator_rt, id).await;
            assert_eq!(props["created_by_actor"], creator, "{props}");
            make_repeat_due_again(&creator_rt, id).await;
            id
        };

        let rt = make_rt_with_actor(&db_path, Some(daemon_actor)).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("replacement daemon server");

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain");

        assert_eq!(summary.fired, 1);
        assert_eq!(summary.failed, 0);
        let messages = inbound_reminder_messages(&rt, creator).await;
        let daemon_messages = inbound_reminder_messages(&rt, daemon_actor).await;
        let local_messages = inbound_reminder_messages(&rt, "local").await;
        assert_eq!(
            messages.len(),
            1,
            "one inbound delivery for the creator; daemon={daemon_messages:?}, local={local_messages:?}"
        );
        assert_eq!(messages[0].0, "test reminder");
        assert_eq!(messages[0].1["direction"], "inbound");
        assert_eq!(messages[0].1["to_actor"], creator);
        assert_eq!(messages[0].1["subject"], "[Reminder] test reminder");
        assert!(daemon_messages.is_empty());
        assert!(local_messages.is_empty());
        let props = get_note_props(&rt, id).await;
        assert_eq!(props["status"], "fired");
        assert!(props["fired_at"].as_str().is_some());
    }

    #[tokio::test]
    async fn repeating_reminder_delivers_on_consecutive_fires() {
        let (_tmp, db_path) = tmp_db();
        let actor = "lambda:repeat-owner";
        let rt = make_rt_with_actor(&db_path, Some(actor)).await;
        let server = KhiveMcpServer::new(rt.clone()).expect("server");
        let id =
            create_scheduled_event(&rt, "local", &due_rfc3339(), None, Some("daily"), "remind")
                .await;

        let first = run_pending_events_on(&rt, &server, false)
            .await
            .expect("first drain");
        assert_eq!(first.advanced, 1);
        assert_eq!(inbound_reminder_messages(&rt, actor).await.len(), 1);

        make_repeat_due_again(&rt, id).await;
        let second = run_pending_events_on(&rt, &server, false)
            .await
            .expect("second drain");

        assert_eq!(second.advanced, 1);
        assert_eq!(second.failed, 0);
        assert_eq!(
            inbound_reminder_messages(&rt, actor).await.len(),
            2,
            "each fire delivers one inbound message"
        );
    }

    #[tokio::test]
    async fn reminder_delivery_failure_is_persisted_audited_and_drain_continues() {
        let (_tmp, db_path) = tmp_db();
        let actor = "lambda:failure-owner";
        let cfg = RuntimeConfig {
            db_path: Some(std::path::PathBuf::from(&db_path)),
            default_namespace: Namespace::parse("local").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(DenyCommSendGate),
            actor_id: Some(actor.to_string()),
            ..Default::default()
        };
        let rt = KhiveRuntime::new(cfg).expect("runtime");
        let packs = vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()];
        let server = KhiveMcpServer::with_packs(rt.clone(), &packs)
            .expect("server with required reminder delivery pack");
        let id = create_scheduled_event(&rt, "local", &due_rfc3339(), None, None, "remind").await;
        let action_id = create_scheduled_event(
            &rt,
            "local",
            &due_rfc3339(),
            Some("stats()"),
            None,
            "schedule",
        )
        .await;
        let mut writer = rt.sql().writer().await.expect("open SQL writer");
        let reordered = writer
            .execute(SqlStatement {
                sql: "UPDATE notes SET created_at = CASE id WHEN ?1 THEN 1 WHEN ?2 THEN 2 END \
                      WHERE id IN (?1, ?2)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(action_id.to_string()),
                ],
                label: Some("test_reminder_failure_precedes_valid_action".into()),
            })
            .await
            .expect("order reminder before action");
        assert_eq!(reordered, 2);
        drop(writer);

        let summary = run_pending_events_on(&rt, &server, false)
            .await
            .expect("drain continues after failure");

        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.fired, 2);
        assert!(inbound_reminder_messages(&rt, actor).await.is_empty());
        let props = get_note_props(&rt, id).await;
        assert!(
            props["delivery_error"]
                .as_str()
                .is_some_and(|error| error.contains("denied by delivery-failure test")),
            "delivery error must be visible on the reminder row: {props:?}"
        );
        assert!(props["delivery_failed_at"].as_str().is_some());
        let action_props = get_note_props(&rt, action_id).await;
        assert_eq!(action_props["status"], "fired");
        assert!(action_props["fired_at"].as_str().is_some());

        let token = rt.authorize(Namespace::local()).expect("authorize");
        let events = rt
            .events(&token)
            .expect("event store")
            .query_events(
                EventFilter {
                    verbs: vec!["schedule.remind.fire".to_string()],
                    ..Default::default()
                },
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .expect("query reminder failure events");
        assert!(events
            .items
            .iter()
            .any(|event| { event.outcome == EventOutcome::Error && event.target_id == Some(id) }));
    }

    #[tokio::test]
    async fn due_event_is_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Create a past-due schedule event. Use stats() as the action since it's
        // a valid, registered verb that has no side-effects that need a
        // namespace argument check. `due_rfc3339` is only a few seconds
        // overdue — inside the missed-event grace window — so this exercises
        // the normal fire path, not the ADR-106 missed path.
        let past = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

        let summary = drain_for_test(&db_path).await.expect("drain");

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

        let summary = drain_for_test(&db_path).await.expect("drain");

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

    /// A due event whose `trigger_at` carries a POSITIVE offset must still
    /// fire (PR #782 review round 3, High finding).
    ///
    /// `khive-pack-schedule` round-trips the caller's original `trigger_at`
    /// string verbatim, offset included (H5) — it is never normalized to
    /// UTC in storage. The candidate-page SQL predicate used to compare
    /// `trigger_at` against `now` as raw TEXT (`<=`), which is only
    /// chronologically correct when every stored string happens to share the
    /// same offset as the bind parameter. This event is chronologically due
    /// (10s ago, well inside the default grace window) but stored at a
    /// `+04:00` wall-clock offset, whose string sorts LEXICOGRAPHICALLY
    /// AFTER a UTC `now` string (a later-looking hour digit) even though it
    /// is chronologically earlier — under the pre-fix raw-text predicate
    /// this row would never be fetched by the candidate query at all, so it
    /// would never fire, never even reach the Rust-side missed check: it
    /// would sit `pending` forever. The fix wraps both sides of the SQL
    /// predicate in `datetime(...)`, which normalizes to UTC before
    /// comparing, making the fetch chronologically correct regardless of the
    /// stored string's offset.
    #[tokio::test]
    async fn due_event_with_positive_offset_trigger_at_fires() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Chronologically 10s overdue (well inside the default 300s grace
        // window), but formatted at +04:00 wall time so the RFC 3339 string
        // sorts AFTER a UTC `now` string as raw text.
        let trigger_instant = Utc::now() - Duration::seconds(10);
        let plus_four = FixedOffset::east_opt(4 * 3600).expect("valid offset");
        let trigger_at = trigger_instant.with_timezone(&plus_four).to_rfc3339();
        assert!(
            trigger_at.as_str() > now_rfc3339_for_ordering_check().as_str(),
            "test setup: {trigger_at:?} must sort AFTER a UTC now-string as raw text \
             for this to exercise the lexicographic-ordering bug"
        );

        let id =
            create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule")
                .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "a due event stored with a positive offset must still fire, got {summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        let status = props["status"].as_str().unwrap_or("");
        assert!(
            status == "fired" || status == "pending",
            "status must be fired or pending (repeat), got {status:?}"
        );
    }

    /// A FUTURE event whose `trigger_at` carries a NEGATIVE offset — whose
    /// RFC 3339 string sorts BEFORE a UTC `now` string as raw text, a false
    /// POSITIVE under the pre-fix raw-text predicate — must NOT fire.
    ///
    /// This exercises the other direction of the same lexicographic-ordering
    /// bug class: a negative-offset string can make a genuinely FUTURE event
    /// look due to a raw-text `<=` comparison. The SQL predicate's
    /// `datetime(...)` normalization correctly excludes it from the
    /// candidate page; even if it were fetched, the retained Rust-side
    /// `trigger_at > now` re-check is the belt-and-suspenders backstop that
    /// already made this direction benign before the SQL fix (PR #782 review round 3:
    /// "Negative-offset strings produce false POSITIVES, which the retained
    /// Rust re-check filters — benign").
    #[tokio::test]
    async fn future_event_with_negative_offset_trigger_at_is_not_fired() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // Chronologically 2h in the future, but formatted at -08:00 wall
        // time so the RFC 3339 string sorts BEFORE a UTC `now` string as raw
        // text (a false positive under naive lexicographic comparison).
        let trigger_instant = Utc::now() + Duration::hours(2);
        let minus_eight = FixedOffset::west_opt(8 * 3600).expect("valid offset");
        let trigger_at = trigger_instant.with_timezone(&minus_eight).to_rfc3339();
        assert!(
            trigger_at.as_str() < now_rfc3339_for_ordering_check().as_str(),
            "test setup: {trigger_at:?} must sort BEFORE a UTC now-string as raw text \
             for this to exercise the false-positive path"
        );

        let id =
            create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule")
                .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(
            summary.fired, 0,
            "a chronologically future event must not be fired, got {summary:?}"
        );
        assert_eq!(
            summary.advanced, 0,
            "a chronologically future event must not be advanced, got {summary:?}"
        );

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

        let past = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

        // First drain — fires the event.
        let s1 = drain_for_test(&db_path).await.expect("drain 1");
        assert!(s1.scanned >= 1);

        // Second drain — event is now status="fired", not "pending"; must not re-fire.
        let s2 = drain_for_test(&db_path).await.expect("drain 2");
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

        // Use a past (but in-grace) trigger_at with daily repeat.
        let past = due_rfc3339();
        let id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

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
        let past = due_rfc3339();

        let id_a =
            create_scheduled_event(&rt, ns_a, &past, Some("stats()"), None, "schedule").await;

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

        let summary = drain_for_test(&db_path).await.expect("drain");

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

        // Create a past-due (but in-grace) event with an invalid action DSL
        // (verb not registered).
        let past = due_rfc3339();
        let _id_bad = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats()"), // valid — but let's add a second event with a broken action
            None,
            "schedule",
        )
        .await;
        // Second event with broken action.
        let id_bad2 = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("this_verb_does_not_exist(foo=\"bar\")"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path)
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
    ///
    /// Issue #575: a single drain pass can legitimately report `failed >= 1`
    /// for this exact payload with no logic bug involved. `claim_pending_event`
    /// checks out the pool's single writer connection via
    /// `WriterPool::writer()`, which is `parking_lot::Mutex::try_lock_for(
    /// checkout_timeout)` (default 5s, `khive-db/src/pool.rs`) — a bounded
    /// wait, not a logic gate. On a CPU-oversubscribed CI runner (`cargo test
    /// --workspace` runs dozens of test binaries, each further parallelized,
    /// against 2-4 physical cores), a task can be scheduled off-CPU for longer
    /// than the checkout timeout while queued for that mutex, so the checkout
    /// times out *before the claim's SQL `UPDATE` ever runs*: the drain loop
    /// counts `summary.failed += 1` and the row stays in `status="pending"`,
    /// retryable on the next cron drain. (This retryability is specific to
    /// claim-time checkout failure. Once a claim succeeds, a later
    /// dispatch-time error is counted as failed but the event is still
    /// finalized — a non-repeating event is marked fired, not returned to
    /// pending.) Confirmed live: this test passed 100/100 serial runs, 8/8
    /// full-suite runs, and 3/3 `cargo llvm-cov` runs on a 12-core box, yet
    /// failed on CI on a commit whose kkernel source did not change from a
    /// passing run — the signature of scheduler contention, not a
    /// deterministic dispatch defect.
    ///
    /// Rather than weakening the assertion (retries could mask a genuine
    /// first-drain dispatch regression), remove the contention boundary
    /// deterministically: run serially and raise the checkout timeout for
    /// the duration of the test, keeping the original single-drain
    /// zero-failure contract intact.
    #[tokio::test]
    #[serial_test::serial]
    async fn replayable_action_dispatches_without_failure_at_trigger_time() {
        struct RestoreTimeout(Option<String>);
        impl Drop for RestoreTimeout {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", v),
                    None => std::env::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS"),
                }
            }
        }
        let _restore = RestoreTimeout(std::env::var("KHIVE_CHECKOUT_TIMEOUT_SECS").ok());
        std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", "120");

        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = due_rfc3339();
        let id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("schedule.remind(content=\"ping\", at=\"2099-01-01T00:00:00Z\")"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

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

        let past = due_rfc3339();
        let _id = create_scheduled_event(
            &rt,
            "local",
            &past,
            Some("stats() | get(id=$prev.id)"),
            None,
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path)
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

        let past = due_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &past, Some("stats()"), None, "schedule").await;

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

        let summary = drain_for_test(&db_path).await.expect("drain");

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

        let summary = drain_for_test(&db_path).await.expect("drain");

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

    /// Round-2 regression: finalize must be bound to the owning claim. This
    /// reproduces the exact stale-claimant-resumes
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

    // ── ADR-106 missed-event policy ─────────────────────────────────────────

    /// Deterministic unit test for `advance_repeat_past_missed`: 14 daily
    /// occurrences accumulated while an event was undrained must be skipped
    /// in a single advance to the first occurrence strictly after `now` —
    /// never a multi-fire catch-up burst.
    #[test]
    fn advance_repeat_past_missed_skips_all_accumulated_occurrences() {
        let now: DateTime<Utc> = "2026-06-15T09:00:00Z".parse().unwrap();
        let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        let next = advance_repeat_past_missed(&Some("daily".to_string()), original, now).unwrap();
        assert!(next > now, "advanced occurrence must be strictly future");
        assert!(
            next <= now + Duration::days(1),
            "must land on the very next occurrence, not skip further than one interval past now"
        );
        assert_eq!(
            next,
            original + Duration::days(15),
            "must be exactly the first daily occurrence after now (single advance, no burst)"
        );
    }

    /// No `repeat` (or an unsupported cron form) never advances — the caller
    /// must fall back to marking the event terminally `"missed"`.
    #[test]
    fn advance_repeat_past_missed_no_repeat_returns_none() {
        let now: DateTime<Utc> = "2026-06-15T09:00:00Z".parse().unwrap();
        let original: DateTime<Utc> = "2026-06-01T09:00:00Z".parse().unwrap();
        assert!(advance_repeat_past_missed(&None, original, now).is_none());
    }

    /// The headline regression: 9 non-repeating events overdue well beyond
    /// the default grace window (300s) must ALL be marked `"missed"` and
    /// NONE dispatched. This is the first-boot-against-a-large-backlog
    /// scenario the ADR-106 amendment calls out explicitly. The action DSL is
    /// deliberately a genuinely side-effecting verb (`create`, writing a
    /// distinctively-tagged `observation` note) rather than the read-only
    /// `stats()`, and the test asserts that note is ABSENT after the drain —
    /// not just that `summary.fired`/`summary.advanced` read zero. A
    /// regression that accidentally fires the missed path would otherwise be
    /// caught only by the summary counters, which is weaker evidence than
    /// confirming the action's own write never landed (codex PR #782 review,
    /// Low finding: the previous fixture used `stats()`, contradicting this
    /// comment's claim of a side-effecting action).
    #[tokio::test]
    async fn nine_overdue_events_beyond_grace_are_missed_with_zero_dispatch() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        let past = "2000-01-01T00:00:00Z";
        let marker = "nine-overdue-zero-dispatch-marker";
        let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
        let mut ids = Vec::new();
        for _ in 0..9 {
            let id = create_scheduled_event(
                &rt,
                "local",
                past,
                Some(action_dsl.as_str()),
                None,
                "schedule",
            )
            .await;
            ids.push(id);
        }

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(summary.scanned, 9, "all 9 overdue rows must be scanned");
        assert_eq!(summary.fired, 0, "zero dispatches: nothing may be fired");
        assert_eq!(
            summary.advanced, 0,
            "zero dispatches: nothing may be advanced"
        );
        assert_eq!(summary.failed, 0, "the missed path is not a failure");
        assert_eq!(
            summary.missed.len(),
            9,
            "all 9 overdue rows must be marked missed, got summary={summary:?}"
        );
        for id in &ids {
            assert!(
                summary.missed.contains(id),
                "missed list must name every overdue id"
            );
        }

        for id in ids {
            let props = get_note_props(&rt, id).await;
            assert_eq!(
                props["status"].as_str(),
                Some("missed"),
                "note {id} must end in status=missed, got {props:?}"
            );
            assert!(
                props["missed_at"].as_i64().is_some(),
                "note {id} must have missed_at stamped, got {props:?}"
            );
            assert!(
                props["fired_at"].is_null(),
                "note {id} must never have fired_at set (never dispatched), got {props:?}"
            );
        }

        // Strongest evidence: the side-effecting action's own output record
        // must be entirely absent — not merely "summary says zero fired".
        let ns = Namespace::parse("local").unwrap();
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.notes(&token).expect("notes");
        let page = store
            .query_notes(
                "local",
                Some("observation"),
                PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .expect("query observation notes");
        let marker_hits: Vec<_> = page.items.iter().filter(|n| n.content == marker).collect();
        assert!(
            marker_hits.is_empty(),
            "the missed action must never dispatch: found {} marker note(s): {marker_hits:?}",
            marker_hits.len()
        );
    }

    /// An event overdue by less than the grace window must still fire
    /// normally — the missed policy only applies beyond the grace threshold.
    #[tokio::test]
    async fn overdue_within_grace_still_fires() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // 60s overdue is comfortably inside the 300s default grace window.
        let trigger_at = (Utc::now() - Duration::seconds(60)).to_rfc3339();
        let id =
            create_scheduled_event(&rt, "local", &trigger_at, Some("stats()"), None, "schedule")
                .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert!(
            summary.missed.is_empty(),
            "an event within grace must never be marked missed, got summary={summary:?}"
        );
        assert!(
            summary.fired >= 1 || summary.advanced >= 1,
            "an event within grace must be dispatched normally, got summary={summary:?}"
        );

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("fired"),
            "non-repeating in-grace event must end fired, got {props:?}"
        );
        assert!(
            props["fired_at"].as_str().is_some(),
            "in-grace event must have fired_at set, got {props:?}"
        );
    }

    /// End-to-end (drain-level) confirmation that a missed *repeating* event
    /// is re-armed at a future occurrence instead of ending terminally
    /// missed — complements the deterministic
    /// `advance_repeat_past_missed_skips_all_accumulated_occurrences` unit
    /// test above with the full claim/finalize wiring.
    #[tokio::test]
    async fn missed_repeat_is_rearmed_at_next_future_occurrence() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        // 10 days overdue with a daily repeat: 10 accumulated occurrences,
        // all missed, must collapse into exactly one future re-arm.
        let original_trigger: DateTime<Utc> = Utc::now() - Duration::days(10);
        let id = create_scheduled_event(
            &rt,
            "local",
            &original_trigger.to_rfc3339(),
            Some("stats()"),
            Some("daily"),
            "schedule",
        )
        .await;

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(summary.fired, 0, "a missed repeat must not fire");
        assert_eq!(
            summary.advanced, 0,
            "a missed repeat's re-arm is counted as missed, not advanced"
        );
        assert_eq!(
            summary.missed.len(),
            1,
            "exactly one missed occurrence recorded"
        );
        assert!(summary.missed.contains(&id));

        let props = get_note_props(&rt, id).await;
        assert_eq!(
            props["status"].as_str(),
            Some("pending"),
            "a missed repeat must be re-armed to pending, not left terminal, got {props:?}"
        );
        assert!(
            props["missed_at"].as_i64().is_some(),
            "missed_at must be stamped even though the row is re-armed, got {props:?}"
        );
        let new_trigger: DateTime<Utc> = props["trigger_at"]
            .as_str()
            .expect("trigger_at must be set")
            .parse()
            .expect("parseable trigger_at");
        let now = Utc::now();
        assert!(
            new_trigger > now,
            "re-armed trigger_at must be strictly in the future, got {new_trigger} (now={now})"
        );
        assert!(
            new_trigger <= now + Duration::days(1),
            "re-armed trigger_at must be the very next occurrence, not skip further \
             (no catch-up burst), got {new_trigger} (now={now})"
        );
    }

    /// A backlog larger than the drain's internal page size (200) must be
    /// fully processed in ONE drain pass, not silently truncated at the page
    /// boundary (codex PR #782 review, Medium finding: the previous
    /// implementation paged `status="pending"` with `LIMIT/OFFSET` while
    /// simultaneously mutating rows out of that predicate, so once the first
    /// page's 200 rows left `"pending"`, the page-2 query at `OFFSET 200`
    /// undercounted and silently skipped every row beyond the first page).
    /// 201 rows — one more than `PAGE_SIZE` — reproduces the exact boundary
    /// codex identified.
    #[tokio::test]
    async fn backlog_larger_than_page_size_is_fully_drained_in_one_pass() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        const OVERDUE_ROW_COUNT: usize = 201; // PAGE_SIZE (200) + 1
        let past = "2000-01-01T00:00:00Z"; // far beyond the missed-event grace window
        let mut ids = Vec::with_capacity(OVERDUE_ROW_COUNT);
        for _ in 0..OVERDUE_ROW_COUNT {
            let id =
                create_scheduled_event(&rt, "local", past, Some("stats()"), None, "schedule").await;
            ids.push(id);
        }

        let summary = drain_for_test(&db_path).await.expect("drain");

        assert_eq!(
            summary.scanned, OVERDUE_ROW_COUNT as u64,
            "every overdue row across both pages must be scanned in one pass, got \
             summary={summary:?}"
        );
        assert_eq!(
            summary.missed.len(),
            OVERDUE_ROW_COUNT,
            "every overdue row across both pages must be marked missed in one pass \
             (the page-boundary row must not be skipped), got summary={summary:?}"
        );
        for id in &ids {
            assert!(
                summary.missed.contains(id),
                "missed list must name every row, including ones beyond the first page"
            );
        }
        for id in ids {
            let props = get_note_props(&rt, id).await;
            assert_eq!(
                props["status"].as_str(),
                Some("missed"),
                "note {id} must end in status=missed (not left pending past the page \
                 boundary), got {props:?}"
            );
        }
    }

    /// Two concurrent drain passes over the same store must never double-fire
    /// a row: the `pending -> firing` CAS claim (`claim_pending_event`) makes
    /// exactly one of the two concurrent callers win each row (codex PR #782
    /// review round 1, Medium finding: Amendment B claims Acceptance
    /// Criterion 2 is met, but no regression exercised concurrent drains
    /// until now).
    ///
    /// Each row's action is a genuinely side-effecting `create` writing a
    /// row-distinct marker `observation` note, rather than the read-only
    /// `stats()` the round-1 version of this test used (codex PR #782 review
    /// round 2, Medium finding: a read-only action makes the summary
    /// counters the ONLY signal, which cannot distinguish "claimed once,
    /// dispatched once" from "claimed once, dispatched TWICE, only one
    /// finalize succeeded" — the exact double-dispatch-one-finalize
    /// regression this test exists to catch). After both drains, the test
    /// asserts exactly ONE marker note per scheduled event exists — not just
    /// that the summary counters sum to `ROW_COUNT`.
    #[tokio::test]
    async fn concurrent_drains_fire_each_row_exactly_once() {
        let (_tmp, db_path) = tmp_db();
        let rt = make_rt(&db_path).await;

        const ROW_COUNT: usize = 20;
        let past = due_rfc3339(); // in-grace: exercises the normal fire path, not missed
        let mut ids = Vec::with_capacity(ROW_COUNT);
        let mut markers = Vec::with_capacity(ROW_COUNT);
        for i in 0..ROW_COUNT {
            let marker = format!("concurrent-drain-marker-{i}");
            let action_dsl = format!("create(kind=\"observation\", content=\"{marker}\")");
            let id = create_scheduled_event(
                &rt,
                "local",
                &past,
                Some(action_dsl.as_str()),
                None,
                "schedule",
            )
            .await;
            ids.push(id);
            markers.push(marker);
        }

        let db_path_a = db_path.clone();
        let db_path_b = db_path.clone();
        let (summary_a, summary_b) = tokio::join!(
            async move { drain_for_test(&db_path_a).await },
            async move { drain_for_test(&db_path_b).await },
        );
        let summary_a = summary_a.expect("drain A");
        let summary_b = summary_b.expect("drain B");

        let total_dispatched =
            summary_a.fired + summary_a.advanced + summary_b.fired + summary_b.advanced;
        assert_eq!(
            total_dispatched, ROW_COUNT as u64,
            "every row must be dispatched exactly once across both concurrent drains, \
             got a={summary_a:?} b={summary_b:?}"
        );
        assert_eq!(
            summary_a.failed + summary_b.failed,
            0,
            "the CAS claim must make the losing drain skip cleanly (skipped_race), \
             never fail: a={summary_a:?} b={summary_b:?}"
        );

        for id in &ids {
            let props = get_note_props(&rt, *id).await;
            assert_eq!(
                props["status"].as_str(),
                Some("fired"),
                "note {id} must end fired exactly once, got {props:?}"
            );
        }

        // Strongest evidence: exactly one marker note per row. A
        // double-dispatch-one-finalize bug would leave the CAS-tracked
        // `status`/summary counters looking clean while still writing the
        // action's side effect twice for the row that raced — this is the
        // only assertion that would catch it.
        let ns = Namespace::parse("local").unwrap();
        let token = rt.authorize(ns).expect("authorize");
        let store = rt.notes(&token).expect("notes");
        let page = store
            .query_notes(
                "local",
                Some("observation"),
                PageRequest {
                    limit: (ROW_COUNT as u32) + 10,
                    offset: 0,
                },
            )
            .await
            .expect("query observation notes");
        for marker in &markers {
            let hits: Vec<_> = page.items.iter().filter(|n| &n.content == marker).collect();
            assert_eq!(
                hits.len(),
                1,
                "marker {marker:?} must appear exactly once (double-dispatch check), \
                 found {}: {hits:?}",
                hits.len()
            );
        }
    }

    // ── PR #782 review round 4, High finding: `run_pending_events`'s wrapper
    //    seam must not misread a default namespace as an explicit actor
    //    override ──────────────────────────────────────────────────────────
    //
    // These tests exercise the REAL config-discovery path (process cwd /
    // `HOME`), exactly like `serve.rs`'s own ADR-096 Fork 2 regressions —
    // that module's `SeatEnv`/`write_config` helpers are private to its own
    // `#[cfg(test)]` module, so this module carries its own copies rather
    // than exporting test-only scaffolding across a crate boundary that
    // doesn't otherwise exist between these two files.

    /// RAII guard: temporarily redirects process cwd to `project_root` and
    /// `HOME` to an isolated, empty tempdir (so tier 4 — `~/.khive/config.toml`
    /// — never reaches whatever the real machine running this suite happens
    /// to have configured globally). Restores both on drop, even on
    /// panic/unwind. Mirrors `serve.rs`'s own `SeatEnv`.
    struct SeatEnv {
        original_cwd: std::path::PathBuf,
        original_home: Option<std::ffi::OsString>,
        _isolated_home: tempfile::TempDir,
    }

    impl SeatEnv {
        fn enter(project_root: &std::path::Path) -> Self {
            let original_cwd = std::env::current_dir().expect("read cwd");
            let original_home = std::env::var_os("HOME");
            let isolated_home = tempfile::tempdir().expect("isolated HOME tempdir");
            std::env::set_current_dir(project_root).expect("chdir into seat project root");
            std::env::set_var("HOME", isolated_home.path());
            Self {
                original_cwd,
                original_home,
                _isolated_home: isolated_home,
            }
        }
    }

    impl Drop for SeatEnv {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original_cwd);
            match &self.original_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Write a project-local `.khive/config.toml` declaring `[actor] id`.
    fn write_project_actor_config(project_root: &std::path::Path, actor_id: &str) {
        std::fs::create_dir_all(project_root.join(".khive")).expect("mkdir .khive");
        std::fs::write(
            project_root.join(".khive/config.toml"),
            format!("[actor]\nid = \"{actor_id}\"\n"),
        )
        .expect("write project actor config");
    }

    /// The wrapper seam (`build_server_with_explicit_namespace`, called by
    /// `run_pending_events` with `namespace_explicit: true, actor_explicit:
    /// false`) must let a `"local"`-resolved default namespace fall through
    /// to the project-configured actor — never clear it the way a genuine
    /// `--actor`/`--namespace` CLI override would (`build_server`'s own,
    /// correctly-narrower semantic). Regression for PR #782 review round 4's
    /// High finding: before this fix, `run_pending_events` called
    /// `build_server` directly with a synthesized `namespace: Some("local")`,
    /// which `resolve_cli_namespace` reported as `explicit = true` and
    /// `build_server` then fed into BOTH `namespace_explicit` AND
    /// `actor_explicit`, tripping the "genuinely explicit actor tier
    /// requesting anonymous" branch in `resolve_runtime_config` and silently
    /// discarding the configured `[actor] id`.
    #[test]
    #[serial_test::serial]
    fn wrapper_seam_falls_through_to_project_actor_instead_of_clearing_it() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
        let _seat_env = SeatEnv::enter(seat_dir.path());

        let args = crate::args::Args {
            db: Some(":memory:".to_string()),
            actor: None,
            namespace: None,
            no_embed: false,
            pack: Vec::new(),
            config: None,
            daemon: false,
            transport: None,
            bind: None,
            brain_profile: None,
            resumed_generation: None,
        };
        let ns = Namespace::parse("local").expect("local namespace");

        // The seam `run_pending_events` actually calls: namespace is a real
        // default (`namespace_explicit: true`) but NOT an actor override
        // (`actor_explicit: false`).
        let (_server, schedule_rt) =
            crate::serve::build_server_with_explicit_namespace(&args, ns, true, false)
                .expect("build_server_with_explicit_namespace must succeed");
        let rt = schedule_rt.expect("\"schedule\" pack is in the default pack set");
        assert_eq!(
            rt.config().actor_id.as_deref(),
            Some("lambda:pending-events-tenant"),
            "a default namespace resolving to \"local\" must fall through to the \
             project-configured [actor] id, not clear it as if it were an explicit \
             --actor/--namespace override"
        );
    }

    /// Positive control for the failure mode the fix above closes: routing
    /// the same inputs through `build_server` (the genuine CLI-flag seam,
    /// unchanged by this fix-round) DOES clear the actor, because there a
    /// present namespace value really does mean "the operator typed
    /// --namespace". This documents why `run_pending_events` must not reuse
    /// that entry point for a synthesized, non-CLI-parsed namespace default.
    #[test]
    #[serial_test::serial]
    fn build_server_cli_seam_clears_actor_for_explicit_local_namespace() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
        let _seat_env = SeatEnv::enter(seat_dir.path());

        let args = crate::args::Args {
            db: Some(":memory:".to_string()),
            actor: None,
            namespace: Some("local".to_string()),
            no_embed: false,
            pack: Vec::new(),
            config: None,
            daemon: false,
            transport: None,
            bind: None,
            brain_profile: None,
            resumed_generation: None,
        };

        let (_server, schedule_rt) =
            crate::serve::build_server(&args).expect("build_server must succeed");
        let rt = schedule_rt.expect("\"schedule\" pack is in the default pack set");
        assert_eq!(
            rt.config().actor_id,
            None,
            "build_server's genuine CLI-flag seam must still treat a present --namespace \
             value as an explicit actor override and clear the actor for \"local\" — this \
             is correct CLI behavior, unaffected by the wrapper-seam fix"
        );
    }

    /// `run_pending_events` (the actual `kkernel exec --pending-events`
    /// entry point, not the lower-level `drain_for_test` helper) must
    /// succeed under strict actor mode when a project `[actor] id` is
    /// configured — proving the wrapper's server construction no longer
    /// spuriously trips `enforce_strict_actor_mode` the way routing through
    /// `build_server`'s actor-clearing path would have.
    #[tokio::test]
    #[serial_test::serial]
    async fn wrapper_succeeds_under_strict_actor_mode_with_configured_project_actor() {
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_PACKS");
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");

        let seat_dir = tempfile::tempdir().expect("seat tempdir");
        write_project_actor_config(seat_dir.path(), "lambda:pending-events-tenant");
        let _seat_env = SeatEnv::enter(seat_dir.path());

        let result = run_pending_events(Some(":memory:"), "local", false).await;

        match prev_strict {
            Some(v) => std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", v),
            None => std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"),
        }

        result.expect(
            "run_pending_events must succeed under strict actor mode when a project \
             [actor] id is configured — the same config a live `kkernel mcp --daemon` \
             boot in this project would resolve",
        );
    }
}
