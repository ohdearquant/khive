//! ADR-099 Slice B3 — the `--atomic` execution path for `kkernel exec
//! --ops-file`: CLI-boundary orchestrator running admissibility check ->
//! prepare pass -> commit pass -> post-commit reindex. See
//! `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`
//! for the full pipeline, why `propose`/`review`/`withdraw`/`merge` are
//! rejected pre-runtime rather than partially supported.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use khive_runtime::atomic_runner::{AtomicOpFailure, AtomicOpPlan, AtomicRunOutcome};
use khive_runtime::pack::{PackRegistry, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{
    EdgeListFilter, KhiveConfig, KhiveRuntime, NamespaceToken, Resolved, RuntimeConfig,
};
use khive_storage::EdgeRelation;

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
    // op's plan (target ids, canonical link endpoints) to build its
    // `result` payload.
    let outcome =
        khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), plans.clone())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

    let total = ops.len();
    let envelope = match outcome {
        AtomicRunOutcome::Committed { post_commit } => {
            khive_runtime::atomic_prepare::apply_post_commit_effects(&runtime, &token, post_commit)
                .await
                .context("post-commit reindex after atomic unit commit")?;
            // ADR-099 B3: render each committed op's
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
        _ => Ok(()),
    }
}

/// Returns `(plan, resolved_args)` — `resolved_args` is `args` for any tool
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
        "update" => {
            let resolved = resolve_kg_ids_in_args(runtime, token, tool, args).await?;
            let expected_kind = update_expected_kind(&resolved, registry)?;
            let plan = khive_runtime::atomic_prepare::prepare_update(
                runtime,
                token,
                &resolved,
                expected_kind,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((plan, resolved))
        }
        "link" => {
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

/// Render a committed op's canonical-shaped `result` payload (ADR-099 B3:
/// the pre-fix envelope carried
/// only `{ok, tool, op_index}`, dropping the `results[i].result` ADR-099 D4
/// specifies). Result rendering is a pure READ, run strictly after the
/// commit pass — safe for the same reason the post-commit reindex pass is.
///
/// `original_args`: the op's args exactly as the caller supplied them
/// (needed for delete's `id`/`kind` echo). `resolved_args`: the
/// id-rewritten form `resolve_kg_ids_in_args` produced for
/// update/delete/link.
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
        // ADR-099 B3: a
        // symmetric edge update carries `edge_natural_key` and MUST be
        // rendered from a fresh post-commit natural-key lookup, never from
        // `p.target_id` — that field is prepare-time-only (the caller's
        // requested id), and the SAME staleness that made the write path
        // unsafe to branch on at prepare time makes it unsafe to render
        // from too. This mirrors the `link` arm below (same reasoning), but
        // uses the deleted-inclusive natural-key lookup, not `list_edges`:
        // ADR-039's DO NOTHING conflict-absorption arm can commit leaving the
        // surviving canonical row tombstoned (khive#1213/#1214 fix round),
        // and `list_edges` unconditionally filters `deleted_at IS NULL` — it
        // would report "not found" for exactly the row that was just
        // committed, turning a successful, correct commit into a spurious
        // post-commit error.
        ("update", AtomicOpPlan::Update(p)) if p.edge_natural_key().is_some() => {
            let key = p.edge_natural_key().expect("checked by guard above");
            let edge = runtime
                .get_edge_by_natural_key_including_deleted(
                    token,
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
        (other, _) => anyhow::bail!(
            "atomic result rendering: no canonical-shape renderer for {other:?} \
             (this is a bug — every v1 --atomic-admissible verb must have one)"
        ),
    }
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
}
