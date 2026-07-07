//! ADR-099 Slice B3 — the `--atomic` execution path for `kkernel exec
//! --ops-file`.
//!
//! This module is the CLI-boundary orchestrator: it runs the parse-time
//! admissibility check ([`khive_request::atomic::check_atomic_admissible`],
//! B1) and the op-count guard BEFORE building any runtime or touching the
//! database, then drives the async prepare pass (KG-substrate verbs via
//! [`khive_runtime::atomic_prepare::prepare_op`]; `gtd.transition` /
//! `gtd.complete` via the two `prepare_gtd_*` functions below, kept in this
//! crate because their lifecycle vocabulary lives in `khive-pack-gtd`, which
//! depends on `khive-runtime` — not the other way around), the synchronous
//! commit pass ([`khive_runtime::atomic_runner::run_atomic_unit`], B2), and
//! finally the async post-commit reindex pass
//! ([`khive_runtime::atomic_prepare::apply_post_commit_effects`]).
//!
//! `propose` / `review` / `withdraw` are admissible per
//! [`khive_types::pack::ATOMIC_ADMISSIBLE_VERBS`] (ADR-099 D3 intends them to
//! gain a seam) but have no prepare implementation yet. B3 fix round (codex
//! REJECT, Medium finding): they are now rejected by the SAME pre-runtime
//! `check_atomic_admissible` static guard above, as
//! [`khive_types::pack::AtomicRejectionReason::KnownUnimplemented`] — never
//! reaching `KhiveRuntime::new` or `prepare_one` at all. `prepare_op`'s own
//! `prepare_governance_unimplemented` fallback (see that module's doc
//! comment) is unreachable through this CLI path and remains only as
//! defense-in-depth for any other caller of `prepare_op`.
//!
//! `merge` joined this same deferred bucket in the B3 fix round (Leo
//! refinement, codex REJECT Blocker 2): a full-parity atomic merge prepare
//! was drafted and unit-tested against `khive_runtime::atomic_prepare`
//! directly, but its edge-conflict resolution cannot be expressed in
//! ADR-099's static predicate/guard plan shape (see that crate's
//! `atomic_prepare` module doc), so it is rejected here rather than shipped
//! partially-scoped — `merge is not yet supported under --atomic; use the
//! non-atomic merge verb` is the message callers see.
//!
//! The returned envelope is additive-only and lives entirely outside
//! `dispatch_request_local`'s response shape: non-atomic `--ops-file` runs
//! (and every other exec path) are untouched by this module.

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_pack_gtd::schema::{allowed_transitions, can_transition, is_actionable, is_terminal};
use khive_runtime::atomic_plan::{
    AffectedRowGuard, GtdCompletePlan, GtdTransitionPlan, PlanStatement,
};
use khive_runtime::atomic_runner::{AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use khive_runtime::{KhiveConfig, KhiveRuntime, NamespaceToken, RuntimeConfig};
use khive_storage::types::SqlValue;
use khive_storage::SqlStatement;

use crate::exec::OpsFileEntry;

/// Run `ops` as ONE ADR-099 atomic unit against a freshly built in-process
/// runtime. Returns the additive result envelope
/// (`{"results", "summary", "atomic"}`) on success or a rolled-back run; the
/// only `Err` cases are the parse-time admissibility rejection, the
/// op-count guard, an unsupported multi-backend config, or a genuine
/// `atomic_unit` seam failure (`AtomicRunnerError`) — every one of these
/// happens before any write.
pub(crate) async fn execute_atomic_ops_file(
    ops: Vec<OpsFileEntry>,
    cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    max_ops: usize,
) -> Result<Value> {
    // ── parse-time admissibility (before any runtime / any write) ──────────
    let parsed_for_check: Vec<khive_request::ParsedOp> = ops
        .iter()
        .map(|op| khive_request::ParsedOp {
            tool: op.tool.clone(),
            args: std::collections::BTreeMap::new(),
        })
        .collect();
    let rejections = khive_request::atomic::check_atomic_admissible(&parsed_for_check);
    if !rejections.is_empty() {
        let messages: Vec<String> = rejections.iter().map(|r| r.to_string()).collect();
        anyhow::bail!(
            "--atomic rejected {} op(s) before any write:\n{}",
            messages.len(),
            messages.join("\n")
        );
    }

    // ── op-count guard (before any runtime / any write) ─────────────────────
    if ops.len() > max_ops {
        anyhow::bail!(
            "--atomic op count {} exceeds the configured maximum {max_ops}; \
             split the file or raise --atomic-max-ops",
            ops.len()
        );
    }

    // ── v1 restriction: single-backend topology only ────────────────────────
    if !khive_cfg.backends.is_empty() {
        anyhow::bail!(
            "--atomic does not support a multi-backend [[backends]] topology in v1; \
             found {} declared backend(s)",
            khive_cfg.backends.len()
        );
    }

    let namespace = cfg.default_namespace.clone();
    let runtime = KhiveRuntime::new(cfg).context("build in-process runtime for --atomic")?;
    let token = runtime
        .authorize(namespace)
        .context("authorize namespace for --atomic")?;

    // ── async prepare pass (reads only, no writes) ───────────────────────────
    let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(ops.len());
    for (op_index, op) in ops.iter().enumerate() {
        let plan = prepare_one(&runtime, &token, &op.tool, &op.args)
            .await
            .with_context(|| format!("op {op_index} (`{}`) failed to prepare", op.tool))?;
        plans.push(plan);
    }

    // ── synchronous commit pass (ADR-099 D1 phase 2, B2) ────────────────────
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), plans)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let total = ops.len();
    let envelope = match outcome {
        AtomicRunOutcome::Committed { post_commit } => {
            khive_runtime::atomic_prepare::apply_post_commit_effects(&runtime, &token, post_commit)
                .await
                .context("post-commit reindex after atomic unit commit")?;
            let results: Vec<Value> = ops
                .iter()
                .enumerate()
                .map(|(idx, op)| json!({"ok": true, "tool": op.tool, "op_index": idx}))
                .collect();
            json!({
                "results": results,
                "summary": {"total": total, "succeeded": total, "failed": 0},
                "atomic": {
                    "committed": true,
                    "rolled_back": false,
                    "failed_op_index": Value::Null,
                    "error": Value::Null,
                },
            })
        }
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            let error_message = describe_failure(&failure);
            let results: Vec<Value> = ops
                .iter()
                .enumerate()
                .map(|(idx, op)| {
                    if idx == failed_op_index {
                        json!({"ok": false, "tool": op.tool, "op_index": idx, "error": error_message})
                    } else {
                        json!({"ok": false, "tool": op.tool, "op_index": idx, "error": "not applied: whole atomic unit rolled back"})
                    }
                })
                .collect();
            json!({
                "results": results,
                "summary": {"total": total, "succeeded": 0, "failed": total},
                "atomic": {
                    "committed": false,
                    "rolled_back": true,
                    "failed_op_index": failed_op_index,
                    "error": error_message,
                },
            })
        }
    };

    Ok(envelope)
}

fn describe_failure(failure: &AtomicOpFailure) -> String {
    match failure {
        AtomicOpFailure::GuardFailed {
            statement_label,
            expected,
            observed,
        } => format!(
            "guard failed on statement {statement_label:?}: expected {}..{:?} affected rows, observed {observed}",
            expected.expected_min, expected.expected_max
        ),
        AtomicOpFailure::SqlError {
            statement_label,
            message,
        } => format!("sql error on statement {statement_label:?}: {message}"),
    }
}

async fn prepare_one(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tool: &str,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    match tool {
        "gtd.transition" => prepare_gtd_transition(runtime, token, args)
            .await
            .map_err(|e| anyhow::anyhow!("{e}")),
        "gtd.complete" => prepare_gtd_complete(runtime, token, args)
            .await
            .map_err(|e| anyhow::anyhow!("{e}")),
        _ => khive_runtime::atomic_prepare::prepare_op(runtime, token, tool, args)
            .await
            .map_err(|e| anyhow::anyhow!("{e}")),
    }
}

// ---------------------------------------------------------------------------
// GTD prepare (kept in kkernel — see module doc for the crate-direction
// rationale)
// ---------------------------------------------------------------------------

fn require_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.as_object()
        .and_then(|o| o.get(key))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required field {key:?}"))
}

fn require_uuid(args: &Value, key: &str) -> anyhow::Result<Uuid> {
    let raw = require_str(args, key)?;
    Uuid::parse_str(raw).map_err(|_| anyhow::anyhow!("{key} must be a full UUID; got {raw:?}"))
}

/// Load a task note by id, verifying kind="task" and non-deleted — mirrors
/// `khive-pack-gtd::handlers::load_task`'s checks (reimplemented here: that
/// function is `pub(crate)` to the gtd pack crate).
async fn load_task(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> anyhow::Result<khive_storage::note::Note> {
    let note = runtime
        .notes(token)?
        .get_note(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("task not found: {id}"))?;
    if note.namespace != token.namespace().as_str() {
        anyhow::bail!("task not found: {id}");
    }
    if note.kind != "task" {
        anyhow::bail!("expected kind=\"task\", got {:?}", note.kind);
    }
    if note.deleted_at.is_some() {
        anyhow::bail!("task deleted: {id}");
    }
    Ok(note)
}

fn task_status(props: Option<&Value>) -> String {
    props
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("inbox")
        .to_string()
}

/// Validate the target terminal status for `gtd.complete` (mirrors
/// `khive-pack-gtd::handlers::complete_target_status`, private to that
/// crate): accepts `None`/`"done"` -> `"done"`, `"cancelled"` -> itself.
fn complete_target_status(status: Option<&str>) -> anyhow::Result<&'static str> {
    match status {
        None | Some("done") => Ok("done"),
        Some("cancelled") => Ok("cancelled"),
        Some(other) => {
            anyhow::bail!("complete: status must be \"done\" or \"cancelled\"; got {other:?}")
        }
    }
}

async fn prepare_gtd_transition(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;
    let target = require_str(args, "status")?;
    let note = load_task(runtime, token, id).await?;
    let current = task_status(note.properties.as_ref());

    if is_terminal(&current) {
        anyhow::bail!("task {id} is in terminal state {current:?}; no further transitions allowed");
    }
    if current != target && !can_transition(&current, target) {
        let allowed = allowed_transitions(&current);
        anyhow::bail!(
            "cannot transition from {current:?} to {target:?}; allowed from {current:?}: {}",
            if allowed.is_empty() {
                "(none)".to_string()
            } else {
                allowed.join(", ")
            }
        );
    }

    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = props.as_object_mut() {
        obj.insert("status".into(), json!(target));
        if target == "done" {
            obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
        }
    }
    let updated_at = Utc::now().timestamp_micros();

    let statement = SqlStatement {
        sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
              WHERE id = ?3 AND json_extract(properties, '$.status') = ?4 \
              AND deleted_at IS NULL"
            .to_string(),
        params: vec![
            SqlValue::Text(serde_json::to_string(&props)?),
            SqlValue::Integer(updated_at),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(current),
        ],
        label: Some("atomic-gtd-transition".to_string()),
    };

    Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan {
        task_id: id,
        statements: vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
    }))
}

async fn prepare_gtd_complete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;
    let status_arg = args
        .as_object()
        .and_then(|o| o.get("status"))
        .and_then(|v| v.as_str());
    let target = complete_target_status(status_arg)?;
    let result_arg = args
        .as_object()
        .and_then(|o| o.get("result"))
        .and_then(|v| v.as_str());

    let note = load_task(runtime, token, id).await?;
    let current = task_status(note.properties.as_ref());

    if is_terminal(&current) {
        anyhow::bail!("task {id} is in terminal state {current:?}; no further transitions allowed");
    }
    if !is_actionable(&current) {
        anyhow::bail!(
            "complete: task in {current:?}; transition to 'next' or 'active' first, \
             or use gtd.transition(status=done) explicitly"
        );
    }

    let mut props = note.properties.clone().unwrap_or_else(|| json!({}));
    if let Some(obj) = props.as_object_mut() {
        obj.insert("status".into(), json!(target));
        obj.insert("completed_at".into(), json!(Utc::now().to_rfc3339()));
        if let Some(result) = result_arg {
            obj.insert("result".into(), json!(result));
        }
    }
    let updated_at = Utc::now().timestamp_micros();

    let statement = SqlStatement {
        sql: "UPDATE notes SET properties = ?1, updated_at = ?2 \
              WHERE id = ?3 AND json_extract(properties, '$.status') = ?4 \
              AND deleted_at IS NULL"
            .to_string(),
        params: vec![
            SqlValue::Text(serde_json::to_string(&props)?),
            SqlValue::Integer(updated_at),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(current),
        ],
        label: Some("atomic-gtd-complete".to_string()),
    };

    Ok(AtomicOpPlan::GtdComplete(GtdCompletePlan {
        task_id: id,
        statements: vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
    }))
}
