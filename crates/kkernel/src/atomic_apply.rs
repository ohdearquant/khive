//! ADR-099 Slice B3 — the `--atomic` execution path for `kkernel exec
//! --ops-file`: CLI-boundary orchestrator running admissibility check ->
//! prepare pass -> commit pass -> post-commit reindex. See
//! `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`
//! for the full pipeline, why `propose`/`review`/`withdraw`/`merge` are
//! rejected pre-target-runtime rather than partially supported, and the gtd-adapter
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
    EdgeListFilter, KhiveConfig, KhiveRuntime, LinkSpec, Namespace, NamespaceToken, Resolved,
    RuntimeConfig, RuntimeError,
};
use khive_storage::EdgeRelation;
#[cfg(test)]
use khive_storage::{types::SqlValue, SqlStatement};
use khive_types::RefusalReason;

use crate::exec::OpsFileEntry;

mod arg_validation;
use arg_validation::{normalize_atomic_args, validate_atomic_args};
mod reindex_report;
use reindex_report::{add_post_commit_embedding_warning, model_degradations};
mod gtd_prepare;
use gtd_prepare::{prepare_gtd_complete, prepare_gtd_transition};

#[cfg(test)]
mod partial_indexing_tests;

#[derive(Clone, Debug)]
struct AtomicFailureDetail {
    op_index: usize,
    tool: String,
    error: String,
    reason: Option<RefusalReason>,
}

/// Typed, pre-commit atomic failure returned to the CLI boundary.
///
/// The human `message` remains the command's terminal error text. `envelope`
/// is an additive machine-readable view over the real operation list, allowing
/// the CLI to emit stable refusal reasons without scraping that text.
#[derive(Debug)]
pub(crate) struct AtomicExecFailure {
    message: String,
    envelope: Value,
}

impl AtomicExecFailure {
    fn new(ops: &[OpsFileEntry], message: String, failures: Vec<AtomicFailureDetail>) -> Self {
        let by_index: std::collections::BTreeMap<usize, AtomicFailureDetail> = failures
            .into_iter()
            .map(|failure| (failure.op_index, failure))
            .collect();
        let first_failed = by_index.keys().next().copied();
        let results = ops
            .iter()
            .enumerate()
            .map(|(op_index, op)| {
                if let Some(failure) = by_index.get(&op_index) {
                    let mut entry = json!({
                        "ok": false,
                        "tool": failure.tool.as_str(),
                        "op_index": op_index,
                        "error": failure.error.as_str(),
                    });
                    if let Some(reason) = failure.reason {
                        entry["reason"] = json!(reason.as_str());
                        // This structured policy guard failed during prepare,
                        // before any operation in the atomic unit was applied.
                        if reason == RefusalReason::PolicyRefusal {
                            entry["domain_disposition"] = json!("not_committed");
                        }
                    }
                    entry
                } else {
                    json!({
                        "ok": false,
                        "tool": op.tool.as_str(),
                        "op_index": op_index,
                        "error": "not applied: atomic pre-commit validation rejected another operation",
                    })
                }
            })
            .collect::<Vec<_>>();
        let envelope = json!({
            "results": results,
            "summary": {"total": ops.len(), "succeeded": 0, "failed": ops.len()},
            "atomic": {
                "committed": false,
                "rolled_back": false,
                "failed_op_index": first_failed,
                "error": message.as_str(),
            },
        });
        Self { message, envelope }
    }

    pub(crate) fn envelope(&self) -> Value {
        self.envelope.clone()
    }
}

impl std::fmt::Display for AtomicExecFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AtomicExecFailure {}

#[derive(Clone, Debug)]
struct AtomicDegradation {
    stage: &'static str,
    op_index: Option<usize>,
    tool: Option<String>,
    error: String,
}

impl AtomicDegradation {
    fn post_commit_reindex(error: anyhow::Error) -> Self {
        Self {
            stage: "post_commit_reindex",
            op_index: None,
            tool: None,
            error: format!("post-commit reindex after atomic unit commit: {error:#}"),
        }
    }

    fn result_rendering(op_index: usize, tool: &str, error: anyhow::Error) -> Self {
        Self {
            stage: "result_rendering",
            op_index: Some(op_index),
            tool: Some(tool.to_string()),
            error: format!(
                "op {op_index} (`{tool}`) committed but result rendering failed: {error:#}"
            ),
        }
    }

    fn save_file_publish(error: &anyhow::Error) -> Self {
        Self {
            stage: "save_file_publish",
            op_index: None,
            tool: None,
            error: format!("atomic unit committed but save-file publication failed: {error:#}"),
        }
    }

    fn as_json(&self) -> Value {
        json!({
            "stage": self.stage,
            "op_index": self.op_index,
            "tool": self.tool.as_deref(),
            "error": self.error.as_str(),
        })
    }
}

/// Attach a failed post-commit `--save-file` publication to the authoritative
/// atomic envelope. Returns `true` only when the envelope represents a durable
/// commit; rolled-back envelopes still get printed by the caller but must not
/// be mislabeled as committed degradation.
pub(crate) fn record_save_file_publish_failure(
    envelope: &mut Value,
    error: &anyhow::Error,
) -> bool {
    let Some(atomic) = envelope.get_mut("atomic").and_then(Value::as_object_mut) else {
        return false;
    };
    if atomic.get("committed").and_then(Value::as_bool) != Some(true) {
        return false;
    }

    let degradation = AtomicDegradation::save_file_publish(error).as_json();
    atomic.insert(
        "status".to_string(),
        Value::String("committed_degraded".to_string()),
    );
    atomic.insert("retryable".to_string(), Value::Bool(false));
    match atomic.get_mut("degradations") {
        Some(Value::Array(degradations)) => degradations.push(degradation),
        _ => {
            atomic.insert("degradations".to_string(), Value::Array(vec![degradation]));
        }
    }
    true
}

fn committed_atomic_block(degradations: &[AtomicDegradation]) -> Value {
    let mut atomic = json!({
        "committed": true,
        "rolled_back": false,
        "failed_op_index": Value::Null,
        "error": Value::Null,
    });
    if !degradations.is_empty() {
        atomic["status"] = json!("committed_degraded");
        atomic["retryable"] = json!(false);
        atomic["degradations"] = Value::Array(
            degradations
                .iter()
                .map(AtomicDegradation::as_json)
                .collect(),
        );
    }
    atomic
}

fn committed_degraded_result_entry(
    op_index: usize,
    tool: &str,
    degradation: &AtomicDegradation,
) -> Value {
    json!({
        "ok": true,
        "tool": tool,
        "op_index": op_index,
        "result": Value::Null,
        "status": "committed_degraded",
        "retryable": false,
        "degradation": degradation.as_json(),
    })
}

fn atomic_failure_error(
    ops: &[OpsFileEntry],
    message: String,
    failures: Vec<AtomicFailureDetail>,
) -> anyhow::Error {
    anyhow::Error::new(AtomicExecFailure::new(ops, message, failures))
}

fn atomic_preparation_pack_names(cfg: &RuntimeConfig) -> Vec<String> {
    PackRegistry::discovered_names()
        .into_iter()
        // Telemetry contributes no vocabulary or mutation hooks to atomic preparation.
        // Loading it only when requested preserves all other discovered pack hooks.
        .filter(|name| *name != "telemetry" || cfg.packs.iter().any(|pack| pack == "telemetry"))
        .map(str::to_string)
        .collect()
}

/// Build a metadata-only in-memory registry for exactly the configured pack
/// set. Atomic admissibility must run before the target database is opened,
/// but classifying `verb-refused` requires the same loaded-vs-known distinction
/// normal dispatch gets from `VerbRegistry::has_verb`.
fn build_atomic_preflight_registry(
    cfg: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> Result<(VerbRegistry, KhiveRuntime)> {
    // Pack-registry metadata has no file-backed writer; ADR-194 allows this explicit opt-out.
    let mut metadata_cfg = cfg.clone().for_metadata_registry();
    metadata_cfg.default_namespace =
        Namespace::parse("kkernel-atomic-preflight").unwrap_or_else(|_| Namespace::local());
    let pack_names = metadata_cfg.packs.clone();
    let runtime =
        KhiveRuntime::new(metadata_cfg).context("building --atomic preflight registry runtime")?;
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&pack_names, runtime.clone(), &mut builder)
        .map_err(|error| anyhow::anyhow!("build --atomic preflight registry: {error}"))?;
    for (pack, policy) in &khive_cfg.packs {
        builder.with_disabled_verbs(pack, &policy.verbs_disabled);
    }
    let registry = builder
        .build()
        .context("building --atomic preflight VerbRegistry")?;
    Ok((registry, runtime))
}

fn classify_atomic_preflight(
    ops: &[OpsFileEntry],
    cfg: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
) -> Result<Vec<AtomicFailureDetail>> {
    let parsed: Vec<khive_request::ParsedOp> = ops
        .iter()
        .map(|op| khive_request::ParsedOp {
            tool: op.tool.clone(),
            args: std::collections::BTreeMap::new(),
        })
        .collect();
    let rejections: std::collections::BTreeMap<usize, khive_request::AtomicRejection> =
        khive_request::atomic::check_atomic_admissible(&parsed)
            .into_iter()
            .map(|rejection| (rejection.op_index, rejection))
            .collect();
    let (registry, _runtime) = build_atomic_preflight_registry(cfg, khive_cfg)?;

    let mut failures = Vec::new();
    for (op_index, op) in ops.iter().enumerate() {
        // Deployment policy applies even to statically atomic-admissible verbs.
        // Refuse the entire unit before opening the target or preparing writes.
        if registry.is_verb_disabled(&op.tool) {
            failures.push(AtomicFailureDetail {
                op_index,
                tool: op.tool.clone(),
                error: format!(
                    "op {op_index} (`{}`) cannot run under --atomic: verb is unknown or disabled",
                    op.tool
                ),
                reason: Some(RefusalReason::VerbRefused),
            });
            continue;
        }
        let Some(rejection) = rejections.get(&op_index) else {
            continue;
        };
        if !registry.has_verb(&op.tool) {
            let static_detail = rejections
                .get(&op_index)
                .map(|rejection| format!("; static policy also reports: {rejection}"))
                .unwrap_or_default();
            let error = format!(
                "op {op_index} (`{}`) cannot run under --atomic: verb is unknown or not loaded{static_detail}",
                op.tool
            );
            failures.push(AtomicFailureDetail {
                op_index,
                tool: op.tool.clone(),
                error,
                reason: Some(RefusalReason::VerbRefused),
            });
        } else {
            failures.push(AtomicFailureDetail {
                op_index,
                tool: op.tool.clone(),
                error: rejection.to_string(),
                reason: None,
            });
        }
    }
    Ok(failures)
}

/// Read-only admission shared by the real atomic run and its dry-run preview.
/// Keep the order of these checks identical so a preview cannot accept a unit
/// that the real run would refuse before opening the target runtime.
pub(crate) fn preflight_atomic_ops_file(
    ops: &[OpsFileEntry],
    cfg: &RuntimeConfig,
    khive_cfg: &KhiveConfig,
    max_ops: usize,
) -> Result<()> {
    if ops.len() > max_ops {
        anyhow::bail!(
            "--atomic op count {} exceeds the configured maximum {max_ops}; \
             split the file or raise --atomic-max-ops",
            ops.len()
        );
    }

    if !khive_cfg.backends.is_empty() {
        anyhow::bail!(
            "--atomic does not support a multi-backend [[backends]] topology in v1; \
             found {} declared backend(s)",
            khive_cfg.backends.len()
        );
    }

    let preflight_failures = classify_atomic_preflight(ops, cfg, khive_cfg)?;
    if !preflight_failures.is_empty() {
        let messages: Vec<&str> = preflight_failures
            .iter()
            .map(|failure| failure.error.as_str())
            .collect();
        let message = format!(
            "--atomic rejected {} op(s) before any write:\n{}",
            messages.len(),
            messages.join("\n")
        );
        return Err(atomic_failure_error(ops, message, preflight_failures));
    }

    Ok(())
}

fn refusal_reason_for_prepare_error(error: &anyhow::Error) -> Option<RefusalReason> {
    match error.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::SecretDetected(_)) => Some(RefusalReason::GateRefusal),
        Some(error) if error.is_stream_policy_refusal() => Some(RefusalReason::PolicyRefusal),
        _ => None,
    }
}

/// Run `ops` as ONE ADR-099 atomic unit against a freshly built in-process
/// runtime. Returns the additive result envelope
/// (`{"results", "summary", "atomic"}`) on success or a rolled-back run; the
/// `Err` cases are typed preflight/prepare failures, the op-count guard, an
/// unsupported multi-backend config, or a genuine `atomic_unit` seam failure
/// (`AtomicRunnerError`). Post-commit reindex and result-rendering failures
/// instead return a committed, non-retryable degraded envelope. Preflight and
/// prepare failures happen before any target write and retain a per-operation
/// envelope for the CLI boundary.
pub(crate) async fn execute_atomic_ops_file(
    ops: Vec<OpsFileEntry>,
    cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    max_ops: usize,
) -> Result<Value> {
    execute_atomic_ops_file_with_runtime_setup(ops, cfg, khive_cfg, max_ops, |_| {}).await
}

async fn execute_atomic_ops_file_with_runtime_setup(
    ops: Vec<OpsFileEntry>,
    cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    max_ops: usize,
    setup: impl FnOnce(&KhiveRuntime),
) -> Result<Value> {
    // The dry-run path calls the same read-only admission before reporting
    // success. Keep it ahead of runtime construction and all target writes.
    preflight_atomic_ops_file(&ops, &cfg, khive_cfg, max_ops)?;

    // Guard cold construction (migrations) the same way every other local
    // `kkernel exec` path does — see `crate::exec::acquire_local_construction_guard`.
    // Dropped right after `KhiveRuntime::new` returns rather than held for the
    // whole atomic run: the race this closes is cold-boot schema init, not the
    // prepare/commit passes below.
    let pack_names = atomic_preparation_pack_names(&cfg);
    let boot_guard = crate::exec::acquire_local_construction_guard(&cfg)?;
    let namespace = cfg.default_namespace.clone();
    let runtime = KhiveRuntime::new(cfg).context("build in-process runtime for --atomic")?;
    drop(boot_guard);
    let token = runtime
        .authorize(namespace)
        .context("authorize namespace for --atomic")?;

    // ADR-099 B3: all discovered vocabulary and hooks, excluding only unrequested
    // telemetry (which contributes neither), remain in the executable registry,
    // reusing the REAL runtime just constructed above (via
    // `.clone()` — `KhiveRuntime` derives `Clone`) rather than a second
    // throwaway one (the pattern `kkernel::pack_introspect::build_registry`
    // uses for introspection). This is what makes `resolve_kind_spec`
    // reachable at this seam: `khive-runtime` cannot depend on
    // `khive-pack-kg`/`khive-pack-gtd` (packs depend on the runtime, not
    // vice versa), so `resolve_kind_spec`'s vocab lookup (granular
    // entity_kind/note_kind names from every loaded pack) can only be done
    // here, where both the runtime and the packs are visible.
    let mut verb_registry_builder = VerbRegistryBuilder::new();
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
    setup(&runtime);

    // ── async prepare pass (reads only, no writes) ───────────────────────────
    let mut plans: Vec<AtomicOpPlan> = Vec::with_capacity(ops.len());
    // ADR-099 B3: the exact args each op's plan was
    // built from (post id-resolution for update/delete/link) —
    // carried alongside the plan so the post-commit result-rendering pass
    // can re-derive natural keys (e.g. a link's canonical edge lookup)
    // without re-parsing the ops file.
    let mut resolved_args_list: Vec<Value> = Vec::with_capacity(ops.len());
    for (op_index, op) in ops.iter().enumerate() {
        // Prepared event statements keep parser provenance when the atomic
        // writer later executes outside this request scope. Ops-file values are
        // literal; reference-bearing inputs are rejected by its parser.
        let operation = khive_types::OperationAttribution {
            op_index: u32::try_from(op_index).expect("ops-file bounds operation count"),
            ref_resolution: khive_types::RefResolution::Literal,
        };
        match khive_storage::operation_context::scope_operation_attribution(
            operation,
            prepare_one(&runtime, &token, &verb_registry, &op.tool, &op.args),
        )
        .await
        {
            Ok((plan, resolved_args)) => {
                plans.push(plan);
                resolved_args_list.push(resolved_args);
            }
            Err(error) => {
                let reason = refusal_reason_for_prepare_error(&error);
                let error_message =
                    format!("op {op_index} (`{}`) failed to prepare: {error:#}", op.tool);
                let failure = AtomicFailureDetail {
                    op_index,
                    tool: op.tool.clone(),
                    error: error_message.clone(),
                    reason,
                };
                return Err(atomic_failure_error(&ops, error_message, vec![failure]));
            }
        }
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
            let mut degradations = Vec::new();
            let effects_report =
                khive_runtime::atomic_prepare::apply_post_commit_effects_with_failures(
                    &runtime,
                    &token,
                    post_commit,
                )
                .await;
            // A failed effect must not hide the model failures and truncation
            // advisories of the effects that completed.
            if let Some(error) = effects_report.failure_error() {
                let degradation = AtomicDegradation::post_commit_reindex(anyhow::Error::new(error));
                tracing::warn!(
                    error = %degradation.error,
                    "atomic unit committed but post-commit reindex failed"
                );
                degradations.push(degradation);
            }
            let embedding_outcomes = effects_report.outcomes;
            degradations.extend(model_degradations(&embedding_outcomes));
            // ADR-099 B3: render each committed op's
            // canonical-shaped `result` payload (ADR-099 D4 requires
            // `results[i].result`; the pre-fix envelope carried only
            // `{ok, tool, op_index}`). Result rendering is itself a READ —
            // safe post-commit, same reasoning as the reindex pass above.
            let mut results: Vec<Value> = Vec::with_capacity(ops.len());
            for (idx, op) in ops.iter().enumerate() {
                let rendered = build_op_result(
                    &runtime,
                    &token,
                    &op.tool,
                    &op.args,
                    &resolved_args_list[idx],
                    &plans[idx],
                    &gtd_audit_outcomes,
                )
                .await;
                match rendered {
                    Ok(mut result) => {
                        let embedding_effect = match &plans[idx] {
                            AtomicOpPlan::Update(plan) => Some(plan.post_commit()),
                            _ => None,
                        };
                        add_post_commit_embedding_warning(
                            &mut result,
                            embedding_effect,
                            &embedding_outcomes,
                        );
                        results.push(
                            json!({"ok": true, "tool": op.tool, "op_index": idx, "result": result}),
                        );
                    }
                    Err(error) => {
                        let degradation = AtomicDegradation::result_rendering(idx, &op.tool, error);
                        tracing::warn!(
                            error = %degradation.error,
                            op_index = idx,
                            tool = %op.tool,
                            "atomic unit committed but operation result rendering failed"
                        );
                        results.push(committed_degraded_result_entry(idx, &op.tool, &degradation));
                        degradations.push(degradation);
                    }
                }
            }
            json!({
                "results": results,
                "summary": {"total": total, "succeeded": total, "failed": 0},
                "atomic": committed_atomic_block(&degradations),
            })
        }
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            let error_message = describe_failure(&failure);
            let error_value = match &failure {
                AtomicOpFailure::NoteConflict(conflict) => json!(conflict.clone().into_error()),
                AtomicOpFailure::EntityConflict(conflict) => json!(conflict.clone().into_error()),
                _ => json!(error_message),
            };
            let results: Vec<Value> = ops
                .iter()
                .enumerate()
                .map(|(idx, op)| {
                    if idx == failed_op_index {
                        json!({"ok": false, "tool": op.tool, "op_index": idx, "error": error_value})
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
        AtomicOpFailure::NoteConflict(conflict) => conflict.clone().into_error().to_string(),
        AtomicOpFailure::EntityConflict(conflict) => conflict.clone().into_error().to_string(),
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

/// Returns `(plan, resolved_args)` — `resolved_args` uses canonical alias spellings for
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
    let normalized = normalize_atomic_args(tool, args)?;
    let args = &normalized;
    validate_atomic_args(tool, args)?;
    match tool {
        "gtd.transition" => {
            let plan = prepare_gtd_transition(runtime, token, args).await?;
            Ok((plan, args.clone()))
        }
        "gtd.complete" => {
            let plan = prepare_gtd_complete(runtime, token, args).await?;
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
                .map_err(anyhow::Error::new)?;
                let update_policy = registry
                    .prepare_note_update_policy(runtime, token, &note, &mut resolved)
                    .await
                    .map_err(anyhow::Error::new)?;
                khive_runtime::atomic_prepare::prepare_update_from_note_snapshot(
                    runtime,
                    token,
                    &resolved,
                    expected_kind,
                    note,
                    update_policy,
                    registry,
                )
                .await
                .map_err(anyhow::Error::new)?
                .1
            } else {
                khive_runtime::atomic_prepare::prepare_update(
                    runtime,
                    token,
                    &resolved,
                    expected_kind,
                )
                .await
                .map_err(anyhow::Error::new)?
            };
            Ok((plan, resolved))
        }
        "link" => {
            let resolved = resolve_kg_ids_in_args(runtime, token, tool, args).await?;
            let plan = khive_runtime::atomic_prepare::prepare_op(runtime, token, tool, &resolved)
                .await
                .map_err(anyhow::Error::new)?;
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
                resurrect: resolved
                    .get("resurrect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            registry
                .validate_link_hooks(runtime, token, std::slice::from_ref(&spec))
                .await
                .map_err(anyhow::Error::new)?;
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
            .map_err(anyhow::Error::new)?;
            Ok((plan, resolved))
        }
        _ => {
            let plan = khive_runtime::atomic_prepare::prepare_op(runtime, token, tool, args)
                .await
                .map_err(anyhow::Error::new)?;
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

/// Resolve the common kind expectation before adapting it to update or delete.
fn expected_kind(
    args: &Value,
    registry: &VerbRegistry,
    verb: &str,
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
        khive_pack_kg::handlers::KindSpec::Entity {
            specific,
            entity_type,
        } => Ok(Some(
            khive_runtime::atomic_prepare::AtomicDeleteKind::Entity {
                specific,
                entity_type,
            },
        )),
        khive_pack_kg::handlers::KindSpec::Note { specific } => Ok(Some(
            khive_runtime::atomic_prepare::AtomicDeleteKind::Note { specific },
        )),
        khive_pack_kg::handlers::KindSpec::Edge => {
            Ok(Some(khive_runtime::atomic_prepare::AtomicDeleteKind::Edge))
        }
        khive_pack_kg::handlers::KindSpec::Event | khive_pack_kg::handlers::KindSpec::Proposal => {
            Err(anyhow::anyhow!(
                "kind {raw:?} not supported under --atomic {verb}; only entity/note/edge \
                 substrates are v1-admissible"
            ))
        }
    }
}

/// Resolves a caller-supplied `delete(kind=...)` into `AtomicDeleteKind`. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`.
fn delete_expected_kind(
    args: &Value,
    registry: &VerbRegistry,
) -> anyhow::Result<Option<khive_runtime::atomic_prepare::AtomicDeleteKind>> {
    expected_kind(args, registry, "delete")
}

/// Resolves a caller-supplied `update(kind=...)` into `AtomicUpdateKind`;
/// mirrors [`delete_expected_kind`] above. See
/// `crates/kkernel/docs/design.md#atomic-exec---ops-file---atomic-execution-path-adr-099-slice-b3`.
fn update_expected_kind(
    args: &Value,
    registry: &VerbRegistry,
) -> anyhow::Result<Option<khive_runtime::atomic_prepare::AtomicUpdateKind>> {
    Ok(
        expected_kind(args, registry, "update")?.map(|kind| match kind {
            khive_runtime::atomic_prepare::AtomicDeleteKind::Entity {
                specific,
                entity_type,
            } => khive_runtime::atomic_prepare::AtomicUpdateKind::Entity {
                specific,
                entity_type,
            },
            khive_runtime::atomic_prepare::AtomicDeleteKind::Note { specific } => {
                khive_runtime::atomic_prepare::AtomicUpdateKind::Note { specific }
            }
            khive_runtime::atomic_prepare::AtomicDeleteKind::Edge => {
                khive_runtime::atomic_prepare::AtomicUpdateKind::Edge
            }
        }),
    )
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
/// (needed for delete's `kind` echo). `resolved_args`: the normalized,
/// id-rewritten form `resolve_kg_ids_in_args`
/// produced for update/delete/link (canonical alias spellings for gtd ops, whose
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
        // Canonical update result: entity timestamps are normalized; note
        // timestamps are normalized and the note status is projected to the
        // same top-level status/lifecycle shape as the regular handler.
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
        ("update", AtomicOpPlan::Update(p)) => {
            match runtime.resolve_by_id(token, p.target_id()).await? {
                Some(Resolved::Entity(entity)) => {
                    Ok(khive_pack_kg::handlers::normalize_entity_timestamps(
                        serde_json::to_value(&entity)?,
                    ))
                }
                Some(Resolved::Note(note)) => {
                    let mut value = khive_pack_kg::handlers::remap_note_status(
                        khive_pack_kg::handlers::normalize_entity_timestamps(serde_json::to_value(
                            &note,
                        )?),
                    );
                    if p.is_idempotent_noop() {
                        value["unchanged"] = json!(true);
                    }
                    Ok(value)
                }
                Some(Resolved::Event(_)) | Some(Resolved::PackRecord { .. }) => {
                    Err(anyhow::anyhow!(
                        "atomic update result: target {} resolved to an unsupported record",
                        p.target_id()
                    ))
                }
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
            }
        }
        // Match the canonical handler's resolved UUID. Keep the existing
        // atomic kind projection from the caller's original arguments.
        ("delete", AtomicOpPlan::Delete(p)) => {
            let kind_val = original_args
                .as_object()
                .and_then(|o| o.get("kind"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(json!({"deleted": true, "id": p.target_id().to_string(), "kind": kind_val}))
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
            if let Some(obj) = raw.as_object_mut() {
                obj.insert("mutation".to_string(), json!(p.disposition().name()));
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
                let raw_status = resolved_args
                    .as_object()
                    .and_then(|o| o.get("status"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("atomic gtd.transition result: missing status")
                    })?;
                let target = normalize_status(raw_status);
                // Same shape as the pack handler's no-op arm: the explanation
                // is `reason`, never the caller's `note` key, and
                // `note_recorded` is unconditional. A note supplied with a
                // same-status transition is refused in `prepare_transition`,
                // which this path calls, so it cannot reach here.
                Ok(json!({
                    "transitioned": false,
                    "note_recorded": false,
                    "id": task["id"],
                    "full_id": task["full_id"],
                    "from": target,
                    "to": target,
                    "reason": "already in target status",
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

/// ADR-099 B3 fix (deny_unknown_fields parity): `validate_atomic_args`
/// unit coverage. These are syntactic-only checks (no runtime/db needed) —
/// full end-to-end "typo doesn't mutate the row" coverage lives in
/// `kkernel::exec::tests::atomic_update_unknown_field_is_rejected_and_does_not_mutate_row`.
#[cfg(test)]
#[path = "atomic_apply_validate_atomic_args_tests.rs"]
mod validate_atomic_args_tests;

#[cfg(test)]
#[path = "atomic_apply_tests.rs"]
mod tests;
