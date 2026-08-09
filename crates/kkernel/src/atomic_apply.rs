//! ADR-099 Slice B3 — the `--atomic` execution path for `kkernel exec
//! --ops-file`: CLI-boundary orchestrator running admissibility check ->
//! prepare pass -> commit pass -> post-commit reindex. See
//! `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`
//! for the full pipeline, why `propose`/`review`/`withdraw`/`merge` are
//! rejected pre-runtime rather than partially supported, and the gtd-adapter
//! ownership split with `khive-pack-gtd`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_pack_gtd::handlers::{ensure_audit_schema, write_audit_record_with_status};
use khive_pack_gtd::schema::{is_terminal, normalize_status};
use khive_runtime::atomic_plan::{
    AffectedRowGuard, GtdCompletePlan, GtdTransitionPlan, PlanStatement, PostCommitEffect,
};
use khive_runtime::atomic_runner::{AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use khive_runtime::pack::{PackRegistry, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{
    EdgeListFilter, KhiveConfig, KhiveRuntime, LinkSpec, NamespaceToken, Resolved, RuntimeConfig,
};
use khive_storage::EdgeRelation;
#[cfg(test)]
use khive_storage::{types::SqlValue, SqlStatement};

use crate::exec::OpsFileEntry;

fn add_post_commit_embedding_warning(
    result: &mut Value,
    effect: Option<&PostCommitEffect>,
    outcomes: &[khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome],
) {
    // More than one atomic update may schedule the same target effect. Treat
    // those outcomes as one aggregate advisory: a late model registration can
    // make a later duplicate reindex truncate even when the first did not, and
    // first-match lookup would silently lose that real outcome.
    let truncated = effect.is_some_and(|effect| {
        outcomes
            .iter()
            .filter(|outcome| &outcome.effect == effect)
            .any(|outcome| outcome.truncation.any_truncated())
    });
    if !truncated {
        return;
    }
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "warnings".to_string(),
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        );
    }
}

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

    // Guard cold construction (migrations) the same way every other local
    // `kkernel exec` path does — see `crate::exec::acquire_local_construction_guard`.
    // Dropped right after `KhiveRuntime::new` returns rather than held for the
    // whole atomic run: the race this closes is cold-boot schema init, not the
    // prepare/commit passes below.
    let boot_guard = crate::exec::acquire_local_construction_guard(&cfg)?;
    let namespace = cfg.default_namespace.clone();
    let runtime = KhiveRuntime::new(cfg).context("build in-process runtime for --atomic")?;
    drop(boot_guard);
    let token = runtime
        .authorize(namespace)
        .context("authorize namespace for --atomic")?;

    // ADR-099 B3: a `VerbRegistry` built from every
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
    // Canonical server startup installs the aggregate before dispatch. Atomic
    // preparation calls the same runtime endpoint validator directly, so it
    // must receive pack extensions too (notably GTD's task->task depends_on
    // rule) rather than silently behaving like a kg-only runtime.
    runtime.install_edge_rules(verb_registry.all_edge_rules());
    // #750: every other entry point that
    // builds a `VerbRegistry` from a freshly constructed `KhiveRuntime`
    // (`khive-mcp`'s `serve.rs`/`server.rs`) calls this so a pack-installed
    // note-mutation hook (e.g. khive-pack-memory's warm ANN invalidation)
    // is actually wired into the runtime handle used for the rest of this
    // process's lifetime. `--atomic` built its own registry without this
    // call, so `fire_note_mutation_hook` was a guaranteed no-op for the
    // whole `--atomic` process regardless of whether a call site invoked
    // it. This closes the in-process half of the gap; the note-mutation
    // hook's effect (a bumped `AnnState` generation) is itself process-
    // local, so it still cannot reach a separately-running daemon's warm
    // cache — see the cross-process analysis in #750.
    verb_registry.call_register_note_mutation_hooks(&runtime);

    // ── async prepare pass (reads only, no writes) ───────────────────────────
    let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(ops.len());
    // ADR-099 B3: the exact args each op's plan was
    // built from (post id-resolution for update/delete/link) —
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
    // post-commit result-rendering pass below still needs each
    // op's plan (target ids, canonical link endpoints, gtd post-commit
    // effects) to build its `result` payload.
    let outcome =
        khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), plans.clone())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    let total = ops.len();
    let envelope = match outcome {
        AtomicRunOutcome::Committed { post_commit } => {
            // GAP-5 (ADR-099 B3): `GtdAudit` effects are applied HERE,
            // not inside `khive_runtime::atomic_prepare::apply_post_commit_effects`
            // (crate-direction: `khive-pack-gtd` depends on `khive-runtime`,
            // not the other way around — that function treats `GtdAudit` as
            // a no-op, see its match arm). `kkernel` already depends on both
            // crates, so it calls the SAME canonical `ensure_audit_schema`/
            // `write_audit_record_with_status` functions the non-atomic `gtd.transition`/
            // `gtd.complete` handlers call, rather than re-deriving the
            // DDL/INSERT. Best-effort: append errors are logged and returned
            // as per-task booleans for result rendering — a missing audit row
            // never fails an already-committed atomic unit, but is visible.
            let gtd_audit_outcomes =
                apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
            let embedding_outcomes =
                khive_runtime::atomic_prepare::apply_post_commit_effects_with_report(
                    &runtime,
                    &token,
                    post_commit,
                )
                .await
                .context("post-commit reindex after atomic unit commit")?;
            // ADR-099 B3: render each committed op's
            // canonical-shaped `result` payload (ADR-099 D4 requires
            // `results[i].result`; the pre-fix envelope carried only
            // `{ok, tool, op_index}`). Result rendering is itself a READ —
            // safe post-commit, same reasoning as the reindex pass above.
            let mut results: Vec<Value> = Vec::with_capacity(ops.len());
            for (idx, op) in ops.iter().enumerate() {
                let mut result = build_op_result(
                    &runtime,
                    &token,
                    &op.tool,
                    &op.args,
                    &resolved_args_list[idx],
                    &plans[idx],
                    &gtd_audit_outcomes,
                )
                .await
                .with_context(|| {
                    format!(
                        "op {idx} (`{}`) committed but result rendering failed",
                        op.tool
                    )
                })?;
                let embedding_effect = match &plans[idx] {
                    AtomicOpPlan::Update(plan) => Some(plan.post_commit()),
                    _ => None,
                };
                add_post_commit_embedding_warning(
                    &mut result,
                    embedding_effect,
                    &embedding_outcomes,
                );
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

/// Applies every [`PostCommitEffect::GtdAudit`] via the canonical gtd audit
/// functions (GAP-5, ADR-099 B3); best-effort, cannot fail. Returns each
/// affected task's append outcome so response rendering can expose degradation. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`
/// for why this lives in `kkernel` rather than `khive-runtime`.
async fn apply_gtd_audit_post_commit_effects(
    runtime: &KhiveRuntime,
    effects: &[PostCommitEffect],
) -> HashMap<Uuid, bool> {
    let mut outcomes = HashMap::new();
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
            let persisted = write_audit_record_with_status(
                runtime,
                *task_id,
                from_status,
                to_status,
                note.as_deref(),
                namespace,
            )
            .await;
            outcomes.insert(*task_id, persisted);
        }
    }
    outcomes
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
/// atomic-admissible write verbs, BEFORE building any plan — by reusing each
/// canonical handler's own `#[serde(deny_unknown_fields)]` param struct. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`
/// for why this exists and why it reuses rather than reimplements.
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
/// internally via the canonical gtd resolver) and for any tool
/// with no id-bearing fields; for `update`/`delete`/`link` it is the
/// id-rewritten form `resolve_kg_ids_in_args` produces, carried forward so
/// the post-commit result-rendering pass can re-derive natural
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
        "update" => {
            let mut resolved = resolve_kg_ids_in_args(runtime, token, tool, args).await?;
            let expected_kind = update_expected_kind(&resolved, registry)?;
            if resolved
                .get("entity_kind")
                .is_some_and(|value| !value.is_null())
            {
                anyhow::bail!(
                    "entity_kind is immutable; to change kind, delete then re-create the entity, \
                     or use merge() if this is a deduplication correction"
                );
            }
            let id = resolved
                .get("id")
                .and_then(Value::as_str)
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .ok_or_else(|| anyhow::anyhow!("resolved update id must be a full UUID"))?;
            let resolved_target = runtime
                .resolve_by_id(token, id)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let plan = if let Some(Resolved::Note(note)) = resolved_target {
                // Canonical KG dispatch checks an explicit kind mismatch
                // before invoking a pack hook. Preserve that error ordering:
                // a request aimed at the wrong kind must not be normalized as
                // though it targeted this task.
                khive_runtime::atomic_prepare::validate_note_update_expected_kind(
                    &note,
                    &expected_kind,
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
                registry
                    .prepare_note_update_hook(runtime, token, &note, &mut resolved)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                khive_runtime::atomic_prepare::prepare_update_from_note_snapshot(
                    runtime,
                    token,
                    &resolved,
                    expected_kind,
                    note,
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
            } else {
                khive_runtime::atomic_prepare::prepare_update(
                    runtime,
                    token,
                    &resolved,
                    expected_kind,
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?
            };
            Ok((plan, resolved))
        }
        "link" => {
            let resolved = resolve_kg_ids_in_args(runtime, token, tool, args).await?;
            let plan = khive_runtime::atomic_prepare::prepare_op(runtime, token, tool, &resolved)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let source_id = resolved
                .get("source_id")
                .and_then(Value::as_str)
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .ok_or_else(|| anyhow::anyhow!("resolved link source_id must be a full UUID"))?;
            let target_id = resolved
                .get("target_id")
                .and_then(Value::as_str)
                .and_then(|raw| Uuid::parse_str(raw).ok())
                .ok_or_else(|| anyhow::anyhow!("resolved link target_id must be a full UUID"))?;
            let relation = resolved
                .get("relation")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("resolved link relation must be a string"))?
                .parse::<EdgeRelation>()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let spec = LinkSpec {
                namespace: Some(token.namespace().as_str().to_owned()),
                source_id,
                target_id,
                relation,
                weight: resolved
                    .get("weight")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0),
                metadata: resolved.get("metadata").cloned(),
            };
            registry
                .validate_link_hooks(runtime, token, std::slice::from_ref(&spec))
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
/// `Uuid::parse_str` (ADR-099 B3). Canonical KG handlers resolve through
/// `resolve_uuid_unfiltered`
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

/// Resolves a caller-supplied `delete(kind=...)` into `AtomicDeleteKind`. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`.
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

/// Resolves a caller-supplied `update(kind=...)` into `AtomicUpdateKind`;
/// mirrors [`delete_expected_kind`] above. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`.
fn update_expected_kind(
    args: &Value,
    registry: &VerbRegistry,
) -> anyhow::Result<Option<khive_runtime::atomic_prepare::AtomicUpdateKind>> {
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
            khive_runtime::atomic_prepare::AtomicUpdateKind::Entity { specific },
        )),
        khive_pack_kg::handlers::KindSpec::Note { specific } => Ok(Some(
            khive_runtime::atomic_prepare::AtomicUpdateKind::Note { specific },
        )),
        khive_pack_kg::handlers::KindSpec::Edge => {
            Ok(Some(khive_runtime::atomic_prepare::AtomicUpdateKind::Edge))
        }
        khive_pack_kg::handlers::KindSpec::Event | khive_pack_kg::handlers::KindSpec::Proposal => {
            Err(anyhow::anyhow!(
                "kind {raw:?} not supported under --atomic update; only entity/note/edge \
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

/// Render a committed op's canonical-shaped `result` payload (ADR-099 B3:
/// the pre-fix envelope carried
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
    gtd_audit_outcomes: &HashMap<Uuid, bool>,
) -> anyhow::Result<Value> {
    match (tool, plan) {
        // Canonical shape: `normalize_entity_timestamps(to_json(&updated))`
        // (update.rs:209-211 entity, :242-244 note) — the full updated
        // entity/note row with ISO-8601 timestamps.
        // ADR-099 B3: a
        // symmetric edge update carries `edge_natural_key` and MUST be
        // rendered from a fresh post-commit natural-key lookup, never from
        // `p.target_id` — that field is prepare-time-only (the caller's
        // requested id), and the SAME staleness that made the write path
        // unsafe to branch on at prepare time makes it unsafe to render
        // from too. This mirrors the `link` arm below (same reasoning), but
        // uses the deleted-inclusive natural-key lookup, not `list_edges`:
        // ADR-039's DO NOTHING conflict-absorption arm can commit leaving the
        // surviving canonical row tombstoned,
        // and `list_edges` unconditionally filters `deleted_at IS NULL` — it
        // would report "not found" for exactly the row that was just
        // committed, turning a successful, correct commit into a spurious
        // post-commit error.
        ("update", AtomicOpPlan::Update(p)) if p.edge_natural_key().is_some() => {
            let key = p.edge_natural_key().expect("checked by guard above");
            let edge = runtime
                .get_edge_by_natural_key_including_deleted(
                    token,
                    key.namespace(),
                    key.canon_source_id(),
                    key.canon_target_id(),
                    key.relation(),
                )
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "atomic update result: committed symmetric edge not found by natural key \
                         ({}, {}, {})",
                        key.canon_source_id(),
                        key.canon_target_id(),
                        key.relation()
                    )
                })?;
            Ok(serde_json::to_value(&edge)?)
        }
        ("update", AtomicOpPlan::Update(p)) => match runtime
            .resolve_by_id(token, p.target_id())
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
            // ADR-099 B3 r6: `Resolved` has no `Edge` variant, so a
            // non-symmetric edge update's `p.target_id` (unambiguous — see
            // `prepare_update_edge`'s non-symmetric branch, which never
            // changes the edge's own id) falls through here. Canonical
            // shape: `to_json(&edge)` with no `normalize_entity_timestamps`
            // wrapper (update.rs:220 — entity/note timestamps are ISO-8601
            // strings needing normalization; `Edge`'s `created_at`/
            // `updated_at` already serialize as RFC3339 via its own
            // `Serialize` impl).
            None => {
                let edge = runtime
                    .get_edge(token, p.target_id())
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "atomic update result: target {} not found post-commit",
                            p.target_id()
                        )
                    })?;
                Ok(serde_json::to_value(&edge)?)
            }
            _ => anyhow::bail!(
                "atomic update result: target {} not found post-commit",
                p.target_id()
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
                        source_id: Some(p.source_id()),
                        target_id: Some(p.target_id()),
                        relations: vec![relation],
                        ..Default::default()
                    },
                    1,
                    0,
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
        // :1107-1118 (transitioned). The plan carries an explicit no-op bit:
        // same-status plans now contain a guarded snapshot assertion rather
        // than an empty statement list.
        ("gtd.transition", AtomicOpPlan::GtdTransition(p)) => {
            let note = runtime
                .notes(token)?
                .get_note(p.task_id())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("atomic gtd.transition result: task not found post-commit")
                })?;
            let task = khive_pack_gtd::handlers::render_task(&note);
            if p.is_idempotent_noop() {
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
                    gtd_audit_from_to(p.post_commit()).ok_or_else(|| {
                        anyhow::anyhow!("atomic gtd.transition result: missing audit effect")
                    })?;
                let audit_persisted =
                    gtd_audit_outcomes
                        .get(&p.task_id())
                        .copied()
                        .ok_or_else(|| {
                            anyhow::anyhow!("atomic gtd.transition result: missing audit outcome")
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
                    "audit_persisted": audit_persisted,
                }))
            }
        }
        // Canonical shape: handlers.rs:918-926.
        ("gtd.complete", AtomicOpPlan::GtdComplete(p)) => {
            let note = runtime
                .notes(token)?
                .get_note(p.task_id())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("atomic gtd.complete result: task not found post-commit")
                })?;
            let task = khive_pack_gtd::handlers::render_task(&note);
            let (from_status, to_status) = gtd_audit_from_to(p.post_commit()).ok_or_else(|| {
                anyhow::anyhow!("atomic gtd.complete result: missing audit effect")
            })?;
            let audit_persisted =
                gtd_audit_outcomes
                    .get(&p.task_id())
                    .copied()
                    .ok_or_else(|| {
                        anyhow::anyhow!("atomic gtd.complete result: missing audit outcome")
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
                "audit_persisted": audit_persisted,
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

/// Decide-step wiring for `gtd.transition` (ADR-099 B3 r6 second pass): both
/// this function and `khive-pack-gtd`'s `handle_transition` call
/// `khive_pack_gtd::handlers::prepare_transition` — the ONE place the
/// normalize/validate/secret-gate/load/idempotent-check/lifecycle-guard
/// decision logic lives. This function's only job is turning that decision
/// into an `AtomicOpPlan`: the idempotent no-op case produces a guarded
/// mutation-free assertion, and the write case turns the decided patch into a
/// `PlanStatement` via `khive_pack_gtd::handlers::gtd_transition_statement`
/// — the same DML builder canonical's `atomic_gtd_transition` calls.
async fn prepare_gtd_transition(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let raw_id = require_str(args, "id")?;
    let raw_status = require_str(args, "status")?;
    let note_arg = args
        .as_object()
        .and_then(|o| o.get("note"))
        .and_then(|v| v.as_str());

    let decision =
        khive_pack_gtd::handlers::prepare_transition(runtime, token, raw_id, raw_status, note_arg)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    match decision {
        khive_pack_gtd::handlers::TransitionDecision::NoOp { note, current, .. } => {
            let statement = khive_pack_gtd::handlers::gtd_noop_assertion_statement(&note, &current)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
                note.id,
                vec![PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                true,
                PostCommitEffect::None,
            )))
        }
        khive_pack_gtd::handlers::TransitionDecision::Write {
            note,
            current,
            target,
            props,
            updated_at,
            transition_note,
        } => {
            let statement = khive_pack_gtd::handlers::gtd_transition_statement(
                &note, &current, &target, &props, updated_at,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;

            Ok(AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
                note.id,
                vec![PlanStatement {
                    statement,
                    guard: Some(AffectedRowGuard::exactly(1)),
                }],
                false,
                PostCommitEffect::GtdAudit {
                    task_id: note.id,
                    from_status: current,
                    to_status: target,
                    note: transition_note,
                    namespace: token.namespace().as_str().to_string(),
                },
            )))
        }
    }
}

/// Decide-step wiring for `gtd.complete` — same pattern as
/// [`prepare_gtd_transition`] above: `khive_pack_gtd::handlers::
/// prepare_complete` is the single decide step both this function and
/// `handle_complete` call.
async fn prepare_gtd_complete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> anyhow::Result<AtomicOpPlan> {
    let raw_id = require_str(args, "id")?;
    let status_arg = args
        .as_object()
        .and_then(|o| o.get("status"))
        .and_then(|v| v.as_str());
    let result_arg = args
        .as_object()
        .and_then(|o| o.get("result"))
        .and_then(|v| v.as_str());

    let decision =
        khive_pack_gtd::handlers::prepare_complete(runtime, token, raw_id, status_arg, result_arg)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    let statement = khive_pack_gtd::handlers::gtd_transition_statement(
        &decision.note,
        &decision.current,
        decision.target,
        &decision.props,
        decision.updated_at,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(AtomicOpPlan::GtdComplete(GtdCompletePlan::new(
        decision.note.id,
        vec![PlanStatement {
            statement,
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        PostCommitEffect::GtdAudit {
            task_id: decision.note.id,
            from_status: decision.current,
            to_status: decision.target.to_string(),
            note: None,
            namespace: token.namespace().as_str().to_string(),
        },
    )))
}

/// ADR-099 B3 fix (deny_unknown_fields parity): `validate_atomic_args`
/// unit coverage. These are syntactic-only checks (no runtime/db needed) —
/// full end-to-end "typo doesn't mutate the row" coverage lives in
/// `kkernel::exec::tests::atomic_update_unknown_field_is_rejected_and_does_not_mutate_row`.
#[cfg(test)]
mod validate_atomic_args_tests {
    use super::{add_post_commit_embedding_warning, validate_atomic_args, PostCommitEffect, Uuid};
    use serde_json::json;

    #[test]
    fn atomic_update_result_uses_matching_post_commit_truncation_outcome() {
        let note_id = Uuid::new_v4();
        let effect = PostCommitEffect::ReindexNote { note_id };
        let outcomes = vec![khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome {
            effect: effect.clone(),
            truncation: khive_runtime::retrieval::EmbeddingTruncationReport {
                truncated: 1,
                discarded_bytes: 17,
            },
        }];
        let mut result = json!({"id": note_id});

        add_post_commit_embedding_warning(&mut result, Some(&effect), &outcomes);

        assert_eq!(
            result["warnings"],
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING])
        );

        let mut unrelated = json!({"id": Uuid::new_v4()});
        let other_effect = PostCommitEffect::ReindexEntity {
            entity_id: Uuid::new_v4(),
        };
        add_post_commit_embedding_warning(&mut unrelated, Some(&other_effect), &outcomes);
        assert!(unrelated.get("warnings").is_none());
    }

    #[test]
    fn atomic_duplicate_update_effect_aggregates_later_truncation_outcome() {
        let note_id = Uuid::new_v4();
        let effect = PostCommitEffect::ReindexNote { note_id };
        let outcomes = vec![
            khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome {
                effect: effect.clone(),
                truncation: khive_runtime::retrieval::EmbeddingTruncationReport::default(),
            },
            khive_runtime::atomic_prepare::PostCommitEmbeddingOutcome {
                effect: effect.clone(),
                truncation: khive_runtime::retrieval::EmbeddingTruncationReport {
                    truncated: 1,
                    discarded_bytes: 23,
                },
            },
        ];
        let mut first_result = json!({"id": note_id});
        let mut second_result = json!({"id": note_id});

        add_post_commit_embedding_warning(&mut first_result, Some(&effect), &outcomes);
        add_post_commit_embedding_warning(&mut second_result, Some(&effect), &outcomes);

        let expected = json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
        assert_eq!(first_result["warnings"], expected);
        assert_eq!(second_result["warnings"], expected);
    }

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

    fn full_registry(runtime: &KhiveRuntime) -> VerbRegistry {
        let mut builder = VerbRegistryBuilder::new();
        let pack_names: Vec<String> = PackRegistry::discovered_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        PackRegistry::register_packs(&pack_names, runtime.clone(), &mut builder)
            .expect("register packs");
        builder.build().expect("registry")
    }

    async fn assert_task_update_then_lifecycle_rolls_back(lifecycle_tool: &str) {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let before = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");

        let (update, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({"id": task_id.to_string(), "content": "new mirrored body"}),
        )
        .await
        .expect("prepare generic task update");
        let lifecycle_args = match lifecycle_tool {
            "gtd.transition" => json!({"id": task_id.to_string(), "status": "next"}),
            "gtd.complete" => json!({"id": task_id.to_string(), "result": "shipped"}),
            other => panic!("unexpected lifecycle tool {other}"),
        };
        let (lifecycle, _) =
            prepare_one(&runtime, &token, &registry, lifecycle_tool, &lifecycle_args)
                .await
                .unwrap_or_else(|error| panic!("prepare {lifecycle_tool}: {error}"));

        // Both plans were decided from the same pre-unit task snapshot. The
        // generic update runs first and advances its revision; the lifecycle
        // plan must then fail its exact-snapshot guard. The atomic runner must
        // roll the first write back instead of committing stale lifecycle
        // properties over its newly mirrored description.
        let outcome = khive_runtime::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![update, lifecycle],
        )
        .await
        .expect("atomic runner");
        assert!(
            matches!(
                &outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 1,
                    failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
                }
            ),
            "update -> {lifecycle_tool} must fail closed and roll back; got: {outcome:?}"
        );

        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        assert_eq!(persisted.updated_at, before.updated_at);
        assert_eq!(persisted.content, "atomic-gtd-test-task");
        assert_eq!(
            task_properties(&persisted)
                .get("status")
                .and_then(Value::as_str),
            Some("inbox")
        );
        assert!(task_properties(&persisted).get("description").is_none());
    }

    #[tokio::test]
    async fn atomic_generic_task_update_synchronizes_content_and_description() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);

        let (plan, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({"id": task_id.to_string(), "content": "atomic body"}),
        )
        .await
        .expect("prepare atomic task content update");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit content update");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

        let after_content = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(after_content.content, "atomic body");
        assert_eq!(
            task_properties(&after_content)
                .get("description")
                .and_then(Value::as_str),
            Some("atomic body")
        );

        let (plan, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({
                "id": task_id.to_string(),
                "properties": {"description": "atomic property body"},
            }),
        )
        .await
        .expect("prepare atomic task description update");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit description update");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

        let after_description = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(after_description.content, "atomic property body");
        assert_eq!(
            task_properties(&after_description)
                .get("description")
                .and_then(Value::as_str),
            Some("atomic property body")
        );

        let (plan, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({"id": task_id.to_string(), "content": null, "properties": null}),
        )
        .await
        .expect("prepare atomic null/no-op patch");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit null/no-op patch");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
        let after_null = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(after_null.content, "atomic property body");
        assert_eq!(
            task_properties(&after_null)
                .get("description")
                .and_then(Value::as_str),
            Some("atomic property body")
        );

        let (plan, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({
                "id": task_id.to_string(),
                "properties": {"description": null},
            }),
        )
        .await
        .expect("prepare atomic description clear");
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit description clear");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
        let after_clear = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("get task")
            .expect("task exists");
        assert_eq!(after_clear.content, "atomic-gtd-test-task");
        assert!(task_properties(&after_clear)
            .get("description")
            .is_some_and(Value::is_null));
    }

    #[tokio::test]
    async fn atomic_task_update_rejects_title_clear_before_description_clear_can_write() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let before = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task before rejected update")
            .expect("task exists");

        let err = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({
                "id": task_id.to_string(),
                "name": null,
                "properties": {"description": null},
            }),
        )
        .await
        .expect_err("a task title cannot be cleared under --atomic");
        assert!(
            err.to_string().contains("task title cannot be cleared"),
            "title-clear error must identify the task invariant; got: {err}"
        );

        let after = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task after rejected update")
            .expect("task exists");
        assert_eq!(after, before, "rejected preparation must not write");
    }

    #[tokio::test]
    async fn atomic_task_update_rejects_lifecycle_properties_during_prepare() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let before = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task before rejected update")
            .expect("task exists");

        let err = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({
                "id": task_id.to_string(),
                "properties": {"status": "done"},
            }),
        )
        .await
        .expect_err("atomic prepare must share lifecycle-owned property rejection");
        assert_eq!(
            err.to_string(),
            "invalid input: properties.status is lifecycle-owned and cannot be patched on a task; use gtd.transition for lifecycle changes or gtd.complete for terminal completion"
        );
        let after = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task after rejected update")
            .expect("task exists");
        assert_eq!(after, before, "rejected atomic prepare must not write");
    }

    #[tokio::test]
    async fn atomic_task_update_checks_explicit_kind_before_running_task_hook() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);

        // The mirror fields deliberately conflict. If the task hook ran
        // before the explicit-kind check, its "must match" error would mask
        // canonical dispatch's expected NotFound result.
        let err = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({
                "id": task_id.to_string(),
                "kind": "observation",
                "content": "one body",
                "properties": {"description": "another body"},
            }),
        )
        .await
        .expect_err("wrong explicit note kind must fail before task normalization");
        let message = err.to_string();
        assert!(message.contains("not found: note"), "got: {message}");
        assert!(
            !message.contains("must match"),
            "task hook must not run before kind mismatch rejection: {message}"
        );
    }

    #[tokio::test]
    async fn atomic_task_update_guard_refuses_snapshot_changed_after_prepare() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let (plan, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({"id": task_id.to_string(), "content": "prepared body"}),
        )
        .await
        .expect("prepare task update");

        let mut concurrent = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        concurrent.content = "concurrent body".to_string();
        concurrent.properties =
            Some(json!({"status": "inbox", "priority": "p2", "description": "concurrent body"}));
        concurrent.updated_at = concurrent.updated_at.saturating_add(10);
        runtime
            .notes(&token)
            .expect("note store")
            .upsert_note(concurrent)
            .await
            .expect("concurrent write");

        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("atomic runner");
        assert!(
            matches!(
                &outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 0,
                    failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
                }
            ),
            "stale prepared update must roll back; got: {outcome:?}"
        );
        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        assert_eq!(persisted.content, "concurrent body");
        assert_eq!(
            task_properties(&persisted)
                .get("description")
                .and_then(Value::as_str),
            Some("concurrent body")
        );
    }

    #[tokio::test]
    async fn repeated_atomic_task_updates_fail_closed_without_projected_state() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let (first, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({"id": task_id.to_string(), "content": "first body"}),
        )
        .await
        .expect("prepare first update");
        let (second, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "update",
            &json!({
                "id": task_id.to_string(),
                "properties": {"description": "second body"},
            }),
        )
        .await
        .expect("prepare second update");

        // Both plans intentionally share the same pre-unit snapshot. The
        // first advances its revision; the second must then trip its guard,
        // rolling the whole unit back instead of overwriting the first write
        // with an independently prepared full-row image.
        let outcome = khive_runtime::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![first, second],
        )
        .await
        .expect("atomic runner");
        assert!(
            matches!(
                &outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 1,
                    failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
                }
            ),
            "repeated target must fail closed; got: {outcome:?}"
        );
        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        assert_eq!(persisted.content, "atomic-gtd-test-task");
        assert!(task_properties(&persisted).get("description").is_none());
    }

    #[tokio::test]
    async fn atomic_generic_update_then_transition_rolls_back_on_shared_snapshot() {
        assert_task_update_then_lifecycle_rolls_back("gtd.transition").await;
    }

    #[tokio::test]
    async fn atomic_generic_update_then_complete_rolls_back_on_shared_snapshot() {
        assert_task_update_then_lifecycle_rolls_back("gtd.complete").await;
    }

    /// Every op is prepared from the pre-unit snapshot. A same-status
    /// transition prepared second must therefore revalidate that snapshot
    /// after an earlier real transition on the same task, not silently
    /// commit without checking its now-stale hypothesis.
    #[tokio::test]
    async fn atomic_transition_then_stale_noop_rolls_back_whole_unit() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let before = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");

        let (transition, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "gtd.transition",
            &json!({"id": task_id.to_string(), "status": "next"}),
        )
        .await
        .expect("prepare real transition");
        let (stale_noop, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "gtd.transition",
            &json!({"id": task_id.to_string(), "status": "inbox"}),
        )
        .await
        .expect("prepare same-status transition from the original snapshot");

        let outcome = khive_runtime::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![transition, stale_noop],
        )
        .await
        .expect("atomic runner");
        assert!(
            matches!(
                &outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 1,
                    failure: AtomicOpFailure::GuardFailed {
                        statement_label: Some(label),
                        observed: 0,
                        ..
                    },
                } if label == "gtd_atomic_noop_assertion"
            ),
            "the stale no-op assertion must fail at op 1; got: {outcome:?}"
        );

        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists after rollback");
        assert_eq!(persisted.updated_at, before.updated_at);
        assert_eq!(task_properties(&persisted)["status"], "inbox");
    }

    /// A deletion earlier in the unit must likewise invalidate a no-op that
    /// was prepared while the task still existed. The no-op's assertion is
    /// what forces the delete to roll back as part of the whole unit.
    #[tokio::test]
    async fn atomic_delete_then_stale_noop_rolls_back_whole_unit() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        let task_id = seed_task(&runtime, &token, "inbox").await;
        let registry = full_registry(&runtime);
        let before = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");

        let (delete, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "delete",
            &json!({"id": task_id.to_string(), "hard": true}),
        )
        .await
        .expect("prepare hard delete");
        let (stale_noop, _) = prepare_one(
            &runtime,
            &token,
            &registry,
            "gtd.transition",
            &json!({"id": task_id.to_string(), "status": "inbox"}),
        )
        .await
        .expect("prepare same-status transition before delete applies");

        let outcome = khive_runtime::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![delete, stale_noop],
        )
        .await
        .expect("atomic runner");
        assert!(
            matches!(
                &outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 1,
                    failure: AtomicOpFailure::GuardFailed {
                        statement_label: Some(label),
                        observed: 0,
                        ..
                    },
                } if label == "gtd_atomic_noop_assertion"
            ),
            "the delete must invalidate the no-op assertion at op 1; got: {outcome:?}"
        );

        let persisted = runtime
            .notes(&token)
            .expect("note store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("hard delete must roll back with the unit");
        assert_eq!(persisted, before);
    }

    /// ADR-099 B3: atomic `gtd.transition`
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

    /// ADR-099 B3: a secret in the
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

    /// ADR-099 B3: a secret in the
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
        // Use the default inbox state to preserve direct-complete parity with
        // the accepted lifecycle table (inbox -> done is legal).
        let task_id = seed_task(&runtime, &token, "inbox").await;
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
            Some("inbox"),
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

    /// GAP-3 (ADR-099 B3): atomic `gtd.transition(status="finished")`
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

    /// GAP-6 (ADR-099 B3): an idempotent atomic `gtd.transition` (current ==
    /// target after `normalize_status`) must perform no persisted mutation.
    /// Its guarded no-effect statement only revalidates the snapshot. This
    /// matches canonical when no note was supplied; canonical note-bearing
    /// no-ops have a separate note-event contract outside atomic v1. The
    /// pre-fix atomic prepare only special-cased `current != target` inside
    /// its `can_transition` guard, so a current==target call fell through
    /// to an unconditional `UPDATE` that bumped `updated_at` for nothing.
    #[tokio::test]
    async fn atomic_gtd_transition_idempotent_noop_performs_no_mutation() {
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
        let AtomicOpPlan::GtdTransition(noop_plan) = &plan else {
            panic!("expected gtd transition plan")
        };
        assert!(noop_plan.is_idempotent_noop());
        assert_eq!(noop_plan.statements().len(), 1);

        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .expect("commit ok");
        let post_commit = match outcome {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("idempotent no-op must still succeed as Committed, got {other:?}"),
        };
        assert!(
            post_commit.as_slice().is_empty(),
            "an idempotent no-op transition must produce no post-commit effect (no audit row \
             either — canonical no-note no-ops never reach their own audit helper): {post_commit:?}"
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
            "an idempotent transition must not change updated_at"
        );
    }

    #[tokio::test]
    async fn atomic_same_status_transition_with_note_remains_mutation_and_audit_free() {
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
            .expect("read task")
            .expect("task exists");
        let args = json!({
            "id": task_id.to_string(),
            "status": "next",
            "note": "canonical-only note event",
        });
        let plan = prepare_gtd_transition(&runtime, &token, &args)
            .await
            .expect("prepare atomic same-status no-op");
        let outcome = khive_runtime::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![plan.clone()],
        )
        .await
        .expect("commit guarded no-op assertion");
        let post_commit = match outcome {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected committed no-op, got {other:?}"),
        };
        assert!(post_commit.as_slice().is_empty());

        let audit_outcomes = HashMap::new();
        let result = build_op_result(
            &runtime,
            &token,
            "gtd.transition",
            &args,
            &args,
            &plan,
            &audit_outcomes,
        )
        .await
        .expect("render no-op result");
        assert_eq!(result["transitioned"], false);
        assert!(result.get("note_recorded").is_none());
        assert!(result.get("audit_persisted").is_none());

        let after = runtime
            .notes(&token)
            .expect("notes store")
            .get_note(task_id)
            .await
            .expect("read task")
            .expect("task exists");
        assert_eq!(after.updated_at, before.updated_at);
        assert!(task_properties(&after).get("transition_note").is_none());
    }

    /// GAP-5 (ADR-099 B3): a committed atomic `gtd.transition` AND a
    /// committed atomic `gtd.complete` must each write a
    /// `gtd_lifecycle_audit` row — parity with `handle_transition`/
    /// `handle_complete`'s best-effort `ensure_audit_schema` +
    /// `write_audit_record_with_status` calls. The
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
        let transition_audit =
            apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
        assert_eq!(transition_audit.get(&transition_task), Some(&true));

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
        let complete_audit =
            apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
        assert_eq!(complete_audit.get(&complete_task), Some(&true));

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

    #[tokio::test]
    async fn atomic_complete_result_reports_failed_audit_append() {
        let runtime = scratch_runtime();
        let token = runtime
            .authorize(Namespace::parse("local").expect("ns"))
            .expect("authorize");
        ensure_audit_schema(&runtime).await;
        {
            let mut writer = runtime.sql().writer().await.expect("writer");
            writer
                .execute_script(
                    "CREATE TRIGGER reject_atomic_gtd_audit_insert \
                     BEFORE INSERT ON gtd_lifecycle_audit \
                     BEGIN SELECT RAISE(FAIL, 'forced atomic audit failure'); END;"
                        .to_string(),
                )
                .await
                .expect("failure-injection trigger");
        }

        let task_id = seed_task(&runtime, &token, "next").await;
        let args = json!({"id": task_id.to_string(), "result": "shipped"});
        let plan = prepare_gtd_complete(&runtime, &token, &args)
            .await
            .expect("prepare complete");
        let outcome = khive_runtime::atomic_runner::run_atomic_unit(
            runtime.sql().as_ref(),
            vec![plan.clone()],
        )
        .await
        .expect("commit domain write");
        let post_commit = match outcome {
            AtomicRunOutcome::Committed { post_commit } => post_commit,
            other => panic!("expected committed completion, got {other:?}"),
        };
        let audit_outcomes =
            apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
        assert_eq!(audit_outcomes.get(&task_id), Some(&false));

        let result = build_op_result(
            &runtime,
            &token,
            "gtd.complete",
            &args,
            &args,
            &plan,
            &audit_outcomes,
        )
        .await
        .expect("render committed completion");
        assert_eq!(result["completed"], true);
        assert_eq!(result["audit_persisted"], false);
    }
}
