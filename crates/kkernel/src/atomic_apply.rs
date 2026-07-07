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

use khive_pack_gtd::handlers::{ensure_audit_schema, write_audit_record};
use khive_pack_gtd::schema::{
    allowed_transitions, can_transition, is_actionable, is_terminal, is_valid_status,
    normalize_status,
};
use khive_runtime::atomic_plan::{
    AffectedRowGuard, GtdCompletePlan, GtdTransitionPlan, PlanStatement, PostCommitEffect,
};
use khive_runtime::atomic_runner::{AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use khive_runtime::pack::{PackRegistry, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{
    EdgeListFilter, KhiveConfig, KhiveRuntime, NamespaceToken, Resolved, RuntimeConfig,
};
use khive_storage::types::SqlValue;
use khive_storage::{EdgeRelation, SqlStatement};

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

    // ADR-099 B3 fix round 5, finding 1: a `VerbRegistry` built from every
    // discovered pack, reusing the REAL runtime just constructed above (via
    // `.clone()` — `KhiveRuntime` derives `Clone`) rather than a second
    // throwaway one (the pattern `kkernel::pack_introspect::build_registry`
    // uses for introspection). This is what makes `resolve_kind_spec`
    // reachable at this seam: `khive-runtime` cannot depend on
    // `khive-pack-kg`/`khive-pack-gtd` (packs depend on the runtime, not
    // vice versa), so `resolve_kind_spec`'s vocab lookup (granular
    // entity_kind/note_kind names from every loaded pack) can only be done
    // here, where both the runtime and the packs are visible.
    let mut verb_registry_builder = VerbRegistryBuilder::new();
    let pack_names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    PackRegistry::register_packs(&pack_names, runtime.clone(), &mut verb_registry_builder)
        .map_err(|n| anyhow::anyhow!("pack {n:?} declared in inventory but factory missing"))?;
    let verb_registry = verb_registry_builder
        .build()
        .context("building VerbRegistry for --atomic kind resolution")?;

    // ── async prepare pass (reads only, no writes) ───────────────────────────
    let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(ops.len());
    // ADR-099 B3 fix round 5, finding 4: the exact args each op's plan was
    // built from (post id-resolution for update/delete/link — finding 3) —
    // carried alongside the plan so the post-commit result-rendering pass
    // can re-derive natural keys (e.g. a link's canonical edge lookup)
    // without re-parsing the ops file.
    let mut resolved_args_list: Vec<Value> = Vec::with_capacity(ops.len());
    for (op_index, op) in ops.iter().enumerate() {
        let (plan, resolved_args) =
            prepare_one(&runtime, &token, &verb_registry, &op.tool, &op.args)
                .await
                .with_context(|| format!("op {op_index} (`{}`) failed to prepare", op.tool))?;
        plans.push(plan);
        resolved_args_list.push(resolved_args);
    }

    // ── synchronous commit pass (ADR-099 D1 phase 2, B2) ────────────────────
    // `plans` is cloned here: `run_atomic_unit` consumes it by value, but the
    // post-commit result-rendering pass below (finding 4) still needs each
    // op's plan (target ids, canonical link endpoints, gtd post-commit
    // effects) to build its `result` payload.
    let outcome =
        khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), plans.clone())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    let total = ops.len();
    let envelope = match outcome {
        AtomicRunOutcome::Committed { post_commit } => {
            // GAP-5 (B3 fix round 4): `GtdAudit` effects are applied HERE,
            // not inside `khive_runtime::atomic_prepare::apply_post_commit_effects`
            // (crate-direction: `khive-pack-gtd` depends on `khive-runtime`,
            // not the other way around — that function treats `GtdAudit` as
            // a no-op, see its match arm). `kkernel` already depends on both
            // crates, so it calls the SAME canonical `ensure_audit_schema`/
            // `write_audit_record` functions the non-atomic `gtd.transition`/
            // `gtd.complete` handlers call, rather than re-deriving the
            // DDL/INSERT. Best-effort: errors are logged inside those
            // functions and never propagated — a missing audit row must
            // never fail an already-committed atomic unit.
            apply_gtd_audit_post_commit_effects(&runtime, &post_commit).await;
            khive_runtime::atomic_prepare::apply_post_commit_effects(&runtime, &token, post_commit)
                .await
                .context("post-commit reindex after atomic unit commit")?;
            // ADR-099 B3 fix round 5, finding 4: render each committed op's
            // canonical-shaped `result` payload (ADR-099 D4 requires
            // `results[i].result`; the pre-fix envelope carried only
            // `{ok, tool, op_index}`). Result rendering is itself a READ —
            // safe post-commit, same reasoning as the reindex pass above.
            let mut results: Vec<Value> = Vec::with_capacity(ops.len());
            for (idx, op) in ops.iter().enumerate() {
                let result = build_op_result(
                    &runtime,
                    &token,
                    &op.tool,
                    &op.args,
                    &resolved_args_list[idx],
                    &plans[idx],
                )
                .await
                .with_context(|| {
                    format!(
                        "op {idx} (`{}`) committed but result rendering failed",
                        op.tool
                    )
                })?;
                results
                    .push(json!({"ok": true, "tool": op.tool, "op_index": idx, "result": result}));
            }
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

/// Apply every [`PostCommitEffect::GtdAudit`] in `effects` by calling the
/// SAME `ensure_audit_schema`/`write_audit_record` functions the canonical
/// `handle_transition`/`handle_complete` handlers call (GAP-5, B3 fix round
/// 4). Lives in `kkernel` rather than `khive-runtime::atomic_prepare`
/// because those two functions are owned by `khive-pack-gtd`, which
/// depends on `khive-runtime` — not the other way around; `kkernel` is the
/// first crate in the dependency graph that can see both. Non-`GtdAudit`
/// effects are ignored here (they are `khive_runtime::atomic_prepare::
/// apply_post_commit_effects`'s job, called separately). Best-effort by
/// construction: both callee functions log-and-swallow their own errors, so
/// this function itself cannot fail.
async fn apply_gtd_audit_post_commit_effects(runtime: &KhiveRuntime, effects: &[PostCommitEffect]) {
    for effect in effects {
        if let PostCommitEffect::GtdAudit {
            task_id,
            from_status,
            to_status,
            note,
            namespace,
        } = effect
        {
            ensure_audit_schema(runtime).await;
            write_audit_record(
                runtime,
                *task_id,
                from_status,
                to_status,
                note.as_deref(),
                namespace,
            )
            .await;
        }
    }
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

/// ADR-099 B3 parity fix: reject unknown/typo'd arg keys on the five v1
/// atomic-admissible write verbs, BEFORE building any plan.
///
/// Canonical (non-atomic) `handle_update`/`handle_delete`/`handle_link`
/// (`khive-pack-kg`) and `handle_transition`/`handle_complete`
/// (`khive-pack-gtd`) all deserialize their args through a
/// `#[serde(deny_unknown_fields)]` param struct, so a typo like
/// `conten` (for `content`) is rejected with `bad params: unknown field
/// 'conten'` rather than silently ignored. The pre-fix `--atomic` path had
/// no equivalent gate: each `prepare_*` fn only read the keys it knew
/// about via `obj(args)?.get(...)`, so a typo'd key was dropped on the
/// floor and the op reported `ok:true` with every OTHER field reset to its
/// current value — the caller's intended change silently lost.
///
/// This is Approach A (reuse, not reimplement): `kkernel` already depends
/// on both `khive-pack-kg` and `khive-pack-gtd` directly (see
/// `kkernel/Cargo.toml`) — no crate-graph inversion is needed to reach
/// their param structs, which are now re-exported `pub` (widened from
/// `pub(crate)`/private specifically for this seam; see the doc comments
/// on `UpdateParams`/`DeleteParams`/`LinkParams`
/// [`khive_pack_kg::handlers::params`] and
/// `TransitionParams`/`CompleteParams` [`khive_pack_gtd::handlers`]).
/// Deserializing an op's args through the SAME struct the canonical
/// handler uses reproduces its `deny_unknown_fields` rejection AND its
/// exact error message shape for free, with no duplicated key list to
/// drift out of sync. The deserialized value is discarded — the
/// `prepare_*` fns below still read from the raw `Value` map unchanged;
/// this is a pure additive gate in front of them.
///
/// `merge`, `create`, and the read/governance verbs are out of scope here:
/// `merge` and the embedding-bearing/read/governance verbs are already
/// rejected earlier, at `check_atomic_admissible` (before this function is
/// ever reached) or are not part of the v1 admissible set at all.
fn validate_atomic_args(tool: &str, args: &Value) -> anyhow::Result<()> {
    fn reject<T: serde::de::DeserializeOwned>(args: &Value) -> anyhow::Result<()> {
        serde_json::from_value::<T>(args.clone())
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("bad params: {e}"))
    }

    match tool {
        // kg substrate verbs — `UpdateParams` covers both update-entity and
        // update-note (the canonical handler resolves which from `id`, not
        // from a separate struct); same struct, so one branch covers both.
        "update" => reject::<khive_pack_kg::handlers::UpdateParams>(args),
        "delete" => reject::<khive_pack_kg::handlers::DeleteParams>(args),
        "link" => reject::<khive_pack_kg::handlers::LinkParams>(args),
        // gtd verbs.
        "gtd.transition" => reject::<khive_pack_gtd::handlers::TransitionParams>(args),
        "gtd.complete" => reject::<khive_pack_gtd::handlers::CompleteParams>(args),
        _ => Ok(()),
    }
}

/// Returns `(plan, resolved_args)` — `resolved_args` is `args` for
/// `gtd.transition`/`gtd.complete` (their own prepare fns resolve `id`
/// internally via the canonical gtd resolver, finding 3) and for any tool
/// with no id-bearing fields; for `update`/`delete`/`link` it is the
/// id-rewritten form `resolve_kg_ids_in_args` produces, carried forward so
/// the post-commit result-rendering pass (finding 4) can re-derive natural
/// keys (e.g. a link's canonical edge lookup) without re-resolving ids.
async fn prepare_one(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    tool: &str,
    args: &Value,
) -> anyhow::Result<(AtomicOpPlan, Value)> {
    validate_atomic_args(tool, args)?;
    match tool {
        "gtd.transition" => {
            let plan = prepare_gtd_transition(runtime, token, args)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((plan, args.clone()))
        }
        "gtd.complete" => {
            let plan = prepare_gtd_complete(runtime, token, args)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((plan, args.clone()))
        }
        "update" | "link" => {
            let resolved = resolve_kg_ids_in_args(runtime, token, tool, args).await?;
            let plan = khive_runtime::atomic_prepare::prepare_op(runtime, token, tool, &resolved)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((plan, resolved))
        }
        "delete" => {
            let resolved = resolve_kg_ids_in_args(runtime, token, tool, args).await?;
            let expected_kind = delete_expected_kind(&resolved, registry)?;
            let plan = khive_runtime::atomic_prepare::prepare_delete(
                runtime,
                token,
                &resolved,
                expected_kind,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((plan, resolved))
        }
        _ => {
            let plan = khive_runtime::atomic_prepare::prepare_op(runtime, token, tool, args)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((plan, args.clone()))
        }
    }
}

/// Rewrite an op's KG-substrate id fields (`id` for update/delete;
/// `source_id`/`target_id` for link) to resolved full UUIDs before handing
/// args to `khive_runtime::atomic_prepare`, which only accepts bare
/// `Uuid::parse_str` (ADR-099 B3 fix round 5, finding 3 — codex r3 REJECT
/// High). Canonical KG handlers resolve through `resolve_uuid_unfiltered`
/// (full UUID -> 8+ hex prefix -> entity-name fallback, common.rs:270; the
/// `_including_deleted` variant for hard delete, mirroring
/// `handle_delete`'s `hard` branch at update.rs:268-271) — both are now
/// `pub` specifically for this seam. Resolution is a READ, so it belongs in
/// the async prepare phase; the suspend-free commit-phase invariant is
/// untouched. A field that is absent or not a string is left unchanged —
/// the downstream `prepare_*` fn's own "missing required field"/"must be a
/// full UUID" error still fires with its existing message shape.
async fn resolve_kg_ids_in_args(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tool: &str,
    args: &Value,
) -> anyhow::Result<Value> {
    let mut out = args.clone();
    let hard = out
        .as_object()
        .and_then(|o| o.get("hard"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    async fn rewrite(
        obj: &mut serde_json::Map<String, Value>,
        key: &str,
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        including_deleted: bool,
    ) -> anyhow::Result<()> {
        let Some(Value::String(raw)) = obj.get(key).cloned() else {
            return Ok(());
        };
        let resolved = if including_deleted {
            khive_pack_kg::handlers::resolve_uuid_unfiltered_including_deleted(&raw, runtime, token)
                .await
        } else {
            khive_pack_kg::handlers::resolve_uuid_unfiltered(&raw, runtime, token).await
        }
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        obj.insert(key.to_string(), json!(resolved.to_string()));
        Ok(())
    }

    let obj = out
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("op args must be a JSON object"))?;
    match tool {
        "update" => rewrite(obj, "id", runtime, token, false).await?,
        "delete" => rewrite(obj, "id", runtime, token, hard).await?,
        "link" => {
            rewrite(obj, "source_id", runtime, token, false).await?;
            rewrite(obj, "target_id", runtime, token, false).await?;
        }
        _ => {}
    }
    Ok(out)
}

/// Resolve a caller-supplied `delete(kind=...)` string into the
/// [`khive_runtime::atomic_prepare::AtomicDeleteKind`] `prepare_delete`
/// enforces, using the SAME canonical `resolve_kind_spec` the non-atomic
/// `handle_delete` calls (ADR-099 B3 fix round 5, finding 1 — codex r3
/// REJECT Blocker). `kind` absent -> `Ok(None)` (no check, parity with
/// canonical's own optional discriminator). `kind` resolving to `Edge` maps
/// to `AtomicDeleteKind::Edge` (ADR-099 B3 r6 — closes the round-4 codex
/// REJECT: `Edge` used to be rejected here even though
/// `ATOMIC_ADMISSIBLE_VERBS` already allows `delete(kind="edge")`).
/// `Event`/`Proposal` remain a fail-loud rejection BEFORE `prepare_delete`
/// (and therefore before any write): those substrates are not v1-admissible
/// for atomic delete at all.
fn delete_expected_kind(
    args: &Value,
    registry: &VerbRegistry,
) -> anyhow::Result<Option<khive_runtime::atomic_prepare::AtomicDeleteKind>> {
    let raw = match args
        .as_object()
        .and_then(|o| o.get("kind"))
        .and_then(|v| v.as_str())
    {
        Some(k) => k,
        None => return Ok(None),
    };
    let spec = khive_pack_kg::handlers::resolve_kind_spec(raw, registry)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    match spec {
        khive_pack_kg::handlers::KindSpec::Entity { specific } => Ok(Some(
            khive_runtime::atomic_prepare::AtomicDeleteKind::Entity { specific },
        )),
        khive_pack_kg::handlers::KindSpec::Note { specific } => Ok(Some(
            khive_runtime::atomic_prepare::AtomicDeleteKind::Note { specific },
        )),
        khive_pack_kg::handlers::KindSpec::Edge => {
            Ok(Some(khive_runtime::atomic_prepare::AtomicDeleteKind::Edge))
        }
        khive_pack_kg::handlers::KindSpec::Event | khive_pack_kg::handlers::KindSpec::Proposal => {
            Err(anyhow::anyhow!(
                "kind {raw:?} not supported under --atomic delete; only entity/note/edge \
                 substrates are v1-admissible"
            ))
        }
    }
}

/// Extract `(from_status, to_status)` from a gtd lifecycle post-commit
/// effect — used by [`build_op_result`] below.
fn gtd_audit_from_to(effect: &PostCommitEffect) -> Option<(String, String)> {
    match effect {
        PostCommitEffect::GtdAudit {
            from_status,
            to_status,
            ..
        } => Some((from_status.clone(), to_status.clone())),
        _ => None,
    }
}

/// Render a committed op's canonical-shaped `result` payload (ADR-099 B3 fix
/// round 5, finding 4 — codex r3 REJECT High: the pre-fix envelope carried
/// only `{ok, tool, op_index}`, dropping the `results[i].result` ADR-099 D4
/// specifies). Result rendering is a pure READ, run strictly after the
/// commit pass — safe for the same reason the post-commit reindex pass is.
///
/// `original_args`: the op's args exactly as the caller supplied them
/// (needed for delete's `id`/`kind` echo, and gtd.transition's raw
/// `status`). `resolved_args`: the id-rewritten form `resolve_kg_ids_in_args`
/// produced for update/delete/link (`== original_args` for gtd ops, whose
/// own prepare fns resolve `id` internally).
async fn build_op_result(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tool: &str,
    original_args: &Value,
    resolved_args: &Value,
    plan: &AtomicOpPlan,
) -> anyhow::Result<Value> {
    match (tool, plan) {
        // Canonical shape: `normalize_entity_timestamps(to_json(&updated))`
        // (update.rs:209-211 entity, :242-244 note) — the full updated
        // entity/note row with ISO-8601 timestamps.
        ("update", AtomicOpPlan::Update(p)) => match runtime
            .resolve_by_id(token, p.target_id)
            .await?
        {
            Some(Resolved::Entity(entity)) => {
                Ok(khive_pack_kg::handlers::normalize_entity_timestamps(
                    serde_json::to_value(&entity)?,
                ))
            }
            Some(Resolved::Note(note)) => Ok(khive_pack_kg::handlers::normalize_entity_timestamps(
                serde_json::to_value(&note)?,
            )),
            // ADR-099 B3 r6: `Resolved` has no `Edge` variant, so an edge
            // update's `p.target_id` (the SURVIVING edge id — see
            // `prepare_update_edge`'s symmetric-conflict-absorption branch,
            // which may differ from the caller's original `id`) falls
            // through here. Canonical shape: `to_json(&edge)` with no
            // `normalize_entity_timestamps` wrapper (update.rs:220 —
            // entity/note timestamps are ISO-8601 strings needing
            // normalization; `Edge`'s `created_at`/`updated_at` already
            // serialize as RFC3339 via its own `Serialize` impl).
            None => {
                let edge = runtime.get_edge(token, p.target_id).await?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "atomic update result: target {} not found post-commit",
                        p.target_id
                    )
                })?;
                Ok(serde_json::to_value(&edge)?)
            }
            _ => anyhow::bail!(
                "atomic update result: target {} not found post-commit",
                p.target_id
            ),
        },
        // Canonical shape: `{"deleted": deleted, "id": p.id, "kind": p.kind}`
        // (update.rs:327/:356/:360) — `p.id`/`p.kind` are the CALLER's
        // original strings (pre id-resolution), not the resolved UUID.
        ("delete", AtomicOpPlan::Delete(_)) => {
            let id_val = original_args
                .as_object()
                .and_then(|o| o.get("id"))
                .cloned()
                .unwrap_or(Value::Null);
            let kind_val = original_args
                .as_object()
                .and_then(|o| o.get("kind"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(json!({"deleted": true, "id": id_val, "kind": kind_val}))
        }
        // Canonical shape: `to_json(&edge)` with `source_id`/`target_id`
        // swapped back to the CALLER's order for a symmetric relation
        // (link.rs:183-189). The atomic INSERT is a natural-key upsert, so
        // the prepare-time-generated edge id may not be the committed row's
        // id on a conflict — look the edge up post-commit by
        // `(canonical_source, canonical_target, relation)` instead of
        // trusting it.
        ("link", AtomicOpPlan::Link(p)) => {
            let relation_str = resolved_args
                .as_object()
                .and_then(|o| o.get("relation"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("atomic link result: missing relation"))?;
            let relation: EdgeRelation = relation_str
                .parse()
                .map_err(|e| anyhow::anyhow!("atomic link result: unknown relation: {e}"))?;
            let edges = runtime
                .list_edges(
                    token,
                    EdgeListFilter {
                        source_id: Some(p.source_id),
                        target_id: Some(p.target_id),
                        relations: vec![relation],
                        ..Default::default()
                    },
                    1,
                )
                .await?;
            let edge = edges.into_iter().next().ok_or_else(|| {
                anyhow::anyhow!("atomic link result: committed edge not found by natural key")
            })?;
            let mut raw = serde_json::to_value(&edge)?;
            if relation.is_symmetric() {
                if let Some(obj) = raw.as_object_mut() {
                    let orig_source = resolved_args
                        .as_object()
                        .and_then(|o| o.get("source_id"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let orig_target = resolved_args
                        .as_object()
                        .and_then(|o| o.get("target_id"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    obj.insert("source_id".to_string(), orig_source);
                    obj.insert("target_id".to_string(), orig_target);
                }
            }
            Ok(raw)
        }
        // Canonical shapes: handlers.rs:1030-1037 (idempotent no-op) /
        // :1107-1118 (transitioned). `p.statements.is_empty()` is exactly
        // the idempotent-no-op signal `prepare_gtd_transition` encodes
        // (current == target after `normalize_status`, GAP-6).
        ("gtd.transition", AtomicOpPlan::GtdTransition(p)) => {
            let note = runtime
                .notes(token)?
                .get_note(p.task_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("atomic gtd.transition result: task not found post-commit")
                })?;
            let task = khive_pack_gtd::handlers::render_task(&note);
            if p.statements.is_empty() {
                let raw_status = original_args
                    .as_object()
                    .and_then(|o| o.get("status"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("atomic gtd.transition result: missing status")
                    })?;
                let target = normalize_status(raw_status);
                Ok(json!({
                    "transitioned": false,
                    "id": task["id"],
                    "full_id": task["full_id"],
                    "from": target,
                    "to": target,
                    "note": "already in target status",
                }))
            } else {
                let (from_status, to_status) =
                    gtd_audit_from_to(&p.post_commit).ok_or_else(|| {
                        anyhow::anyhow!("atomic gtd.transition result: missing audit effect")
                    })?;
                Ok(json!({
                    "transitioned": true,
                    "id": task["id"],
                    "full_id": task["full_id"],
                    "from": from_status,
                    "to": to_status,
                    "is_terminal": is_terminal(&to_status),
                    "title": task["title"],
                    "priority": task["priority"],
                    "assignee": task["assignee"],
                    "due": task["due"],
                }))
            }
        }
        // Canonical shape: handlers.rs:918-926.
        ("gtd.complete", AtomicOpPlan::GtdComplete(p)) => {
            let note = runtime
                .notes(token)?
                .get_note(p.task_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("atomic gtd.complete result: task not found post-commit")
                })?;
            let task = khive_pack_gtd::handlers::render_task(&note);
            let (from_status, to_status) = gtd_audit_from_to(&p.post_commit).ok_or_else(|| {
                anyhow::anyhow!("atomic gtd.complete result: missing audit effect")
            })?;
            let completed_at = note
                .properties
                .as_ref()
                .and_then(|props| props.get("completed_at"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!("atomic gtd.complete result: missing completed_at")
                })?;
            Ok(json!({
                "completed": true,
                "id": task["id"],
                "full_id": task["full_id"],
                "from": from_status,
                "to": to_status,
                "completed_at": completed_at,
                "is_terminal": is_terminal(&to_status),
            }))
        }
        (other, _) => anyhow::bail!(
            "atomic result rendering: no canonical-shape renderer for {other:?} \
             (this is a bug — every v1 --atomic-admissible verb must have one)"
        ),
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

/// Resolve a gtd task id (full UUID or 8+ hex prefix), mirroring canonical
/// GTD lifecycle verbs' resolution (`khive_pack_gtd::handlers::resolve_uuid`,
/// handlers.rs:270 — ADR-099 B3 fix round 5, finding 3 — codex r3 REJECT
/// High: the pre-fix version below (`require_uuid`) only accepted a bare
/// full UUID, rejecting the short ids `gtd.assign` itself returns).
async fn resolve_gtd_id(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    key: &str,
) -> anyhow::Result<Uuid> {
    let raw = require_str(args, key)?;
    khive_pack_gtd::handlers::resolve_uuid(raw, runtime, token)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
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
    let id = resolve_gtd_id(runtime, token, args, "id").await?;
    let raw_status = require_str(args, "status")?;

    // GAP-3 (B3 fix round 4): parity with `handle_transition`
    // (handlers.rs:980-987) — normalize aliases (finished/completed->done,
    // in_progress->active, todo->inbox, blocked->waiting, later->someday,
    // ...) and reject an unknown status BEFORE any DB read/write. The
    // pre-fix atomic prepare ran `can_transition` on the raw, unnormalized
    // string with no `is_valid_status` gate at all.
    let target = normalize_status(raw_status);
    if !is_valid_status(target) {
        anyhow::bail!(
            "invalid status {raw_status:?} — valid: inbox, next, waiting, someday, active, done, \
             cancelled (aliases: in_progress, todo, blocked, later, finished)"
        );
    }

    let note_arg = args
        .as_object()
        .and_then(|o| o.get("note"))
        .and_then(|v| v.as_str());

    // Parity with `khive-pack-gtd::handlers::handle_transition`
    // (handlers.rs:988): secret-gate the caller-supplied transition note
    // BEFORE any DB read/write (codex r2 High finding 3).
    if let Some(n) = note_arg {
        khive_runtime::secret_gate::check(n)?;
    }

    let note = load_task(runtime, token, id).await?;
    let current = task_status(note.properties.as_ref());

    // GAP-6 (B3 fix round 4): parity with `handle_transition`
    // (handlers.rs:995-1005) — an idempotent transition (current == target
    // after normalization) is checked BEFORE the terminal-state guard and
    // performs NO write and NO audit row, exactly mirroring canonical's
    // early return. The pre-fix atomic prepare only special-cased
    // `current != target` inside the `can_transition` check, so a
    // current==target call fell through to an unconditional (and
    // unnecessary) `UPDATE`.
    if current == target {
        return Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan {
            task_id: id,
            statements: vec![],
            post_commit: PostCommitEffect::None,
        }));
    }

    if is_terminal(&current) {
        anyhow::bail!("task {id} is in terminal state {current:?}; no further transitions allowed");
    }
    if !can_transition(&current, target) {
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
        // Parity with handlers.rs:1028 — persist the caller-supplied
        // transition note under `properties.transition_note`.
        if let Some(n) = note_arg {
            obj.insert("transition_note".into(), json!(n));
        }
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
            SqlValue::Text(current.clone()),
        ],
        label: Some("atomic-gtd-transition".to_string()),
    };

    Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan {
        task_id: id,
        statements: vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        // GAP-5 (B3 fix round 4): mirrors handlers.rs:1062-1071's
        // best-effort `write_audit_record` call.
        post_commit: PostCommitEffect::GtdAudit {
            task_id: id,
            from_status: current,
            to_status: target.to_string(),
            note: note_arg.map(str::to_string),
            namespace: token.namespace().as_str().to_string(),
        },
    }))
}

async fn prepare_gtd_complete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let id = resolve_gtd_id(runtime, token, args, "id").await?;
    let status_arg = args
        .as_object()
        .and_then(|o| o.get("status"))
        .and_then(|v| v.as_str());
    let target = complete_target_status(status_arg)?;
    let result_arg = args
        .as_object()
        .and_then(|o| o.get("result"))
        .and_then(|v| v.as_str());

    // Parity with `khive-pack-gtd::handlers::handle_complete` (handlers.rs:803):
    // secret-gate the caller-supplied result BEFORE any DB read/write
    // (codex r2 High finding 3).
    if let Some(result) = result_arg {
        khive_runtime::secret_gate::check(result)?;
    }

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
            SqlValue::Text(current.clone()),
        ],
        label: Some("atomic-gtd-complete".to_string()),
    };

    Ok(AtomicOpPlan::GtdComplete(GtdCompletePlan {
        task_id: id,
        statements: vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        // GAP-5 (B3 fix round 4): mirrors handlers.rs:873-883's best-effort
        // `write_audit_record` call — `complete` never persists a
        // transition note (canonical passes `None`).
        post_commit: PostCommitEffect::GtdAudit {
            task_id: id,
            from_status: current,
            to_status: target.to_string(),
            note: None,
            namespace: token.namespace().as_str().to_string(),
        },
    }))
}

/// ADR-099 B3 fix (deny_unknown_fields parity): `validate_atomic_args`
/// unit coverage. These are syntactic-only checks (no runtime/db needed) —
/// full end-to-end "typo doesn't mutate the row" coverage lives in
/// `kkernel::exec::tests::atomic_update_unknown_field_is_rejected_and_does_not_mutate_row`.
#[cfg(test)]
mod validate_atomic_args_tests {
    use super::validate_atomic_args;
    use serde_json::json;

    #[test]
    fn update_rejects_unknown_field() {
        let err = validate_atomic_args("update", &json!({"id": "x", "conten": "hello"}))
            .expect_err("typo'd `conten` must be rejected");
        assert!(err.to_string().contains("unknown field"), "error: {err}");
    }

    #[test]
    fn update_accepts_well_formed_args() {
        validate_atomic_args("update", &json!({"id": "x", "content": "hello"}))
            .expect("well-formed update args must be accepted");
    }

    #[test]
    fn delete_rejects_unknown_field() {
        let err = validate_atomic_args("delete", &json!({"id": "x", "hardd": true}))
            .expect_err("typo'd `hardd` must be rejected");
        assert!(err.to_string().contains("unknown field"), "error: {err}");
    }

    #[test]
    fn delete_accepts_well_formed_args() {
        validate_atomic_args("delete", &json!({"id": "x", "hard": true}))
            .expect("well-formed delete args must be accepted");
    }

    #[test]
    fn link_rejects_unknown_field() {
        let err = validate_atomic_args(
            "link",
            &json!({
                "source_id": "a",
                "target_id": "b",
                "relation": "extends",
                "targt_backend": "x",
            }),
        )
        .expect_err("typo'd `targt_backend` must be rejected");
        assert!(err.to_string().contains("unknown field"), "error: {err}");
    }

    #[test]
    fn link_accepts_well_formed_args() {
        validate_atomic_args(
            "link",
            &json!({"source_id": "a", "target_id": "b", "relation": "extends"}),
        )
        .expect("well-formed link args must be accepted");
    }

    #[test]
    fn gtd_transition_rejects_unknown_field() {
        let err = validate_atomic_args(
            "gtd.transition",
            &json!({"id": "x", "status": "next", "notee": "typo"}),
        )
        .expect_err("typo'd `notee` must be rejected");
        assert!(err.to_string().contains("unknown field"), "error: {err}");
    }

    #[test]
    fn gtd_transition_accepts_well_formed_args() {
        validate_atomic_args(
            "gtd.transition",
            &json!({"id": "x", "status": "next", "note": "ok"}),
        )
        .expect("well-formed gtd.transition args must be accepted");
    }

    #[test]
    fn gtd_complete_rejects_unknown_field() {
        let err = validate_atomic_args("gtd.complete", &json!({"id": "x", "resutl": "typo"}))
            .expect_err("typo'd `resutl` must be rejected");
        assert!(err.to_string().contains("unknown field"), "error: {err}");
    }

    #[test]
    fn gtd_complete_accepts_well_formed_args() {
        validate_atomic_args("gtd.complete", &json!({"id": "x", "result": "ok"}))
            .expect("well-formed gtd.complete args must be accepted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use khive_types::Namespace;

    fn scratch_runtime() -> KhiveRuntime {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("atomic_apply_gtd.db");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("runtime");
        std::mem::forget(dir);
        rt
    }

    /// Seed a live GTD task note directly (bypassing `gtd.assign`'s handler,
    /// which lives one crate over) with the flat properties shape
    /// `load_task`/`task_status` expect: `kind = "task"`,
    /// `properties.status`.
    async fn seed_task(runtime: &KhiveRuntime, token: &NamespaceToken, status: &str) -> Uuid {
        let mut note = khive_storage::note::Note::new("local", "task", "atomic-gtd-test-task");
        note.name = Some("atomic-gtd-test-task".to_string());
        note.properties = Some(json!({"status": status, "priority": "p2"}));
        let id = note.id;
        runtime
            .notes(token)
            .expect("notes store")
            .upsert_note(note)
            .await
            .expect("seed task");
        id
    }

    fn task_properties(note: &khive_storage::note::Note) -> &Value {
        note.properties
            .as_ref()
            .expect("task must carry properties")
    }

    /// B3 fix round 3 (codex r2 High finding 3): atomic `gtd.transition`
    /// must persist a caller-supplied `note` as `properties.transition_note`
    /// — parity with `khive-pack-gtd::handlers::handle_transition`
    /// (handlers.rs:1028), which the pre-fix atomic prepare silently
    /// dropped (it never read the `note` arg at all).
    #[tokio::test]
    async fn atomic_gtd_transition_persists_transition_note() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;

        let plan = prepare_gtd_transition(
            &runtime,
            &token,
            &json!({"id": task_id.to_string(), "status": "next", "note": "handed off to reviewer"}),
        )
        .await
        .expect("prepare transition");

        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

        let note = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must still exist");
        let props = task_properties(&note);
        assert_eq!(props.get("status").and_then(|v| v.as_str()), Some("next"));
        assert_eq!(
            props.get("transition_note").and_then(|v| v.as_str()),
            Some("handed off to reviewer"),
            "transition_note must be persisted into properties: {props:?}"
        );
    }

    /// B3 fix round 3 (codex r2 High finding 3): a secret in the
    /// `gtd.transition` `note` arg must be REJECTED at prepare, before any
    /// DB write — parity with `handle_transition`'s pre-write secret_gate
    /// check (handlers.rs:988).
    #[tokio::test]
    async fn atomic_gtd_transition_rejects_secret_in_note_before_any_write() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;

        let err = prepare_gtd_transition(
            &runtime,
            &token,
            &json!({
                "id": task_id.to_string(),
                "status": "next",
                "note": "leaked key AKIAFAKEKEY1234567890",
            }),
        )
        .await
        .expect_err("a secret in the transition note must be rejected at prepare");
        assert!(
            err.to_string().contains("write blocked"),
            "expected a secret_gate rejection, got: {err}"
        );

        // No write must have happened: status is still "inbox".
        let note = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must still exist");
        assert_eq!(
            task_properties(&note)
                .get("status")
                .and_then(|v| v.as_str()),
            Some("inbox"),
            "rejected prepare must not have mutated the task"
        );
    }

    /// B3 fix round 3 (codex r2 High finding 3): a secret in the
    /// `gtd.complete` `result` arg must be REJECTED at prepare, before any
    /// DB write — parity with `handle_complete`'s pre-write secret_gate
    /// check (handlers.rs:803); a clean result persists normally
    /// (handlers.rs:832 parity).
    #[tokio::test]
    async fn atomic_gtd_complete_rejects_secret_in_result_and_persists_clean_result() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        // (a) secret in `result` rejected before any write.
        let task_id = seed_task(&runtime, &token, "next").await;
        let err = prepare_gtd_complete(
            &runtime,
            &token,
            &json!({
                "id": task_id.to_string(),
                "result": "shipped using AKIAFAKEKEY1234567890",
            }),
        )
        .await
        .expect_err("a secret in the complete result must be rejected at prepare");
        assert!(
            err.to_string().contains("write blocked"),
            "expected a secret_gate rejection, got: {err}"
        );
        let note = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must still exist");
        assert_eq!(
            task_properties(&note)
                .get("status")
                .and_then(|v| v.as_str()),
            Some("next"),
            "rejected prepare must not have mutated the task"
        );

        // (b) a clean result persists.
        let plan = prepare_gtd_complete(
            &runtime,
            &token,
            &json!({"id": task_id.to_string(), "result": "shipped clean"}),
        )
        .await
        .expect("prepare complete");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

        let note = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must still exist");
        let props = task_properties(&note);
        assert_eq!(props.get("status").and_then(|v| v.as_str()), Some("done"));
        assert_eq!(
            props.get("result").and_then(|v| v.as_str()),
            Some("shipped clean")
        );
    }

    /// GAP-3 (B3 fix round 4): atomic `gtd.transition(status="finished")`
    /// on an active task must SUCCEED with the alias normalized to "done"
    /// — parity with the `normalize_status`/`is_valid_status` gate in
    /// `handle_transition` (handlers.rs:980-987). The pre-fix atomic
    /// prepare ran `can_transition` on the raw unnormalized string, which
    /// rejects "finished" outright (it is not itself a lifecycle state
    /// name).
    #[tokio::test]
    async fn atomic_gtd_transition_normalizes_status_alias() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "active").await;

        let plan = prepare_gtd_transition(
            &runtime,
            &token,
            &json!({"id": task_id.to_string(), "status": "finished"}),
        )
        .await
        .expect("prepare transition with aliased status must succeed");

        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

        let note = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must still exist");
        assert_eq!(
            task_properties(&note)
                .get("status")
                .and_then(|v| v.as_str()),
            Some("done"),
            "the \"finished\" alias must normalize to \"done\", parity with canonical"
        );
    }

    /// GAP-6 (B3 fix round 4): an idempotent `gtd.transition` (current ==
    /// target after `normalize_status`) must perform NO write — parity with
    /// `handle_transition`'s early return (handlers.rs:995-1005). The
    /// pre-fix atomic prepare only special-cased `current != target` inside
    /// its `can_transition` guard, so a current==target call fell through
    /// to an unconditional `UPDATE` that bumped `updated_at` for nothing.
    #[tokio::test]
    async fn atomic_gtd_transition_idempotent_noop_performs_no_write() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "next").await;

        let before = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must exist");
        let updated_at_before = before.updated_at;

        let plan = prepare_gtd_transition(
            &runtime,
            &token,
            &json!({"id": task_id.to_string(), "status": "next"}),
        )
        .await
        .expect("prepare idempotent transition must succeed (no-op, not an error)");

        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        let post_commit = match outcome {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("idempotent no-op must still succeed as Committed, got {other:?}"),
        };
        assert!(
            post_commit.is_empty(),
            "an idempotent no-op transition must produce no post-commit effect (no audit row \
             either — canonical never reaches its own write_audit_record call): {post_commit:?}"
        );

        let after = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get_note")
            .expect("task must still exist");
        assert_eq!(
            after.updated_at, updated_at_before,
            "an idempotent transition must not touch updated_at — no write happened"
        );
    }

    /// GAP-5 (B3 fix round 4): a committed atomic `gtd.transition` AND a
    /// committed atomic `gtd.complete` must each write a
    /// `gtd_lifecycle_audit` row — parity with `handle_transition`/
    /// `handle_complete`'s best-effort `ensure_audit_schema` +
    /// `write_audit_record` calls (handlers.rs:1062-1071, :873-883). The
    /// pre-fix atomic prepare wrote no audit row at all.
    #[tokio::test]
    async fn atomic_gtd_transition_and_complete_write_lifecycle_audit_rows() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");

        // (a) transition inbox -> next, with a transition note.
        let transition_task = seed_task(&runtime, &token, "inbox").await;
        let plan = prepare_gtd_transition(
            &runtime,
            &token,
            &json!({"id": transition_task.to_string(), "status": "next", "note": "audit me"}),
        )
        .await
        .expect("prepare transition");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        let post_commit = match outcome {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected Committed, got {other:?}"),
        };
        apply_gtd_audit_post_commit_effects(&runtime, &post_commit).await;

        // (b) complete next -> done.
        let complete_task = seed_task(&runtime, &token, "next").await;
        let plan = prepare_gtd_complete(
            &runtime,
            &token,
            &json!({"id": complete_task.to_string(), "result": "shipped"}),
        )
        .await
        .expect("prepare complete");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        let post_commit = match outcome {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected Committed, got {other:?}"),
        };
        apply_gtd_audit_post_commit_effects(&runtime, &post_commit).await;

        let mut reader = runtime.sql().reader().await.expect("reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT note_id, from_state, to_state, namespace FROM gtd_lifecycle_audit \
                      ORDER BY at ASC"
                    .to_string(),
                params: vec![],
                label: Some("test-gtd-audit-rows".to_string()),
            })
            .await
            .expect("query gtd_lifecycle_audit");
        assert_eq!(
            rows.len(),
            2,
            "both the transition and the complete must each write exactly one audit row: {rows:?}"
        );

        let transition_task_str = transition_task.to_string();
        let complete_task_str = complete_task.to_string();

        let transition_row_present = rows.iter().any(|r| {
            matches!(r.get("note_id"), Some(SqlValue::Text(id)) if id == &transition_task_str)
                && matches!(r.get("from_state"), Some(SqlValue::Text(s)) if s == "inbox")
                && matches!(r.get("to_state"), Some(SqlValue::Text(s)) if s == "next")
                && matches!(r.get("namespace"), Some(SqlValue::Text(ns)) if ns == "local")
        });
        assert!(
            transition_row_present,
            "expected an audit row for the transition op: {rows:?}"
        );

        let complete_row_present = rows.iter().any(|r| {
            matches!(r.get("note_id"), Some(SqlValue::Text(id)) if id == &complete_task_str)
                && matches!(r.get("from_state"), Some(SqlValue::Text(s)) if s == "next")
                && matches!(r.get("to_state"), Some(SqlValue::Text(s)) if s == "done")
                && matches!(r.get("namespace"), Some(SqlValue::Text(ns)) if ns == "local")
        });
        assert!(
            complete_row_present,
            "expected an audit row for the complete op: {rows:?}"
        );
    }
}
