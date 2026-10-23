use std::sync::atomic::{AtomicI64, Ordering};

use khive_pack_kg::handlers::{search_rank_fields, SearchSubstrate, ValidatedSearchRequest};
use khive_request::{ArgValue, ParsedOp, PrevFailure};
use khive_runtime::presentation::{present_with_policy_at, NoteContentScope, PresentationNow};
use khive_runtime::{
    DomainDisposition, InterceptedDispatchResult, PresentationMode, RuntimeError,
    VerbPresentationPolicy, VerbRegistry,
};
use khive_storage::EdgeRelation;
use serde_json::{json, Value};

use crate::coordinator::CoordinatorService;

use super::batch_types::{
    error_disposition, error_with_disposition, failure_entry, DispatchFailure,
};
use super::search_diagnostics::{
    backend_errors_value, search_arm_participation_value, search_incomplete_error, OpSuccess,
    SearchDegradation, SearchStatus,
};
#[cfg(doc)]
use super::KhiveMcpServer;

/// Request-derived policy stays outside the public response envelope and is
/// limited to actual note results. Caller content never supplies a policy marker.
pub(super) fn parse_content_requested(op: &ParsedOp, prev_result: Option<&Value>) -> bool {
    matches!(op.tool.as_str(), "get" | "list")
        && op.args.get("parse_content").and_then(|arg| match arg {
            ArgValue::Value(value) => Some(value.clone()),
            _ => prev_result.and_then(|prev| arg.resolve_all(prev)),
        }) == Some(Value::Bool(true))
}

pub(super) fn note_content_scope(
    enabled: bool,
    tool: &str,
    result: &Value,
    registry: &VerbRegistry,
) -> NoteContentScope {
    if !enabled {
        return NoteContentScope::None;
    }
    let note_kinds = registry.all_note_kinds();
    let is_note = |record: &Value| {
        // By-ID get and broad note lists can read a persisted kind whose pack
        // is no longer loaded. Its versioned Note shape is still a note; do not
        // let that one row disable opacity for an otherwise ordinary note page.
        let versioned_note = record.get("version").and_then(Value::as_u64).is_some()
            && record.get("id").is_some_and(Value::is_string)
            && record.get("created_at").is_some_and(Value::is_string)
            && record.get("updated_at").is_some_and(Value::is_string);
        record
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| note_kinds.contains(&kind) || versioned_note)
            && record.get("content").is_some()
    };
    if tool == "get" && is_note(result) {
        return NoteContentScope::Record;
    }
    if tool == "list" {
        for (key, scope) in [
            ("items", NoteContentScope::Items),
            ("notes", NoteContentScope::Notes),
        ] {
            if result
                .get(key)
                .and_then(Value::as_array)
                .is_some_and(|records| !records.is_empty() && records.iter().all(is_note))
            {
                return scope;
            }
        }
    }
    NoteContentScope::None
}

/// Route a `link` or `search` verb through `coord` when in multi-backend mode.
/// Shared logic behind both dispatch sites (`dispatch_op` chain mode and the
/// parallel/single closure in `run_parsed`). Returns `Some(Ok(OpSuccess))` when
/// the coordinator handled the op, `Some(Err(DispatchFailure))` on a
/// coordinator error (including fail-closed namespace rejection), `None` to
/// fall through to the registry. Must apply the exact same fail-closed
/// namespace rule as `VerbRegistry::dispatch` (RUNTIME-AUD-002, #433) — see
/// `crates/khive-mcp/docs/api/coordinator.md`.
pub(super) async fn dispatch_via_coordinator_inner(
    coord: &dyn CoordinatorService,
    registry: &VerbRegistry,
    tool: &str,
    args_value: &Value,
    identity: Option<&khive_runtime::RequestIdentity>,
) -> Option<Result<OpSuccess, DispatchFailure>> {
    // Only link/search are ever intercepted here.
    if !matches!(tool, "link" | "search") {
        return None;
    }

    match tool {
        "link" => {
            if args_value.get("help").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            // Only intercept single-link form (not bulk `links` array).
            // Bulk link falls through to the registry for now.
            if args_value.get("links").is_some() {
                return None;
            }
            // Normalize the `source`/`target`/`kind` aliases as the handler does, so both
            // spellings take this path; a call the handler would refuse falls through to it.
            let mut link_args = args_value.clone();
            khive_pack_kg::handlers::normalize_link_params(&mut link_args).ok()?;
            let source_str = link_args.get("source_id")?.as_str()?;
            let target_str = link_args.get("target_id")?.as_str()?;
            let relation_str = link_args.get("relation")?.as_str()?;

            // Only intercept when both endpoints are parseable UUIDs.
            // Name/prefix resolution requires single-backend context — fall through.
            let source_id: uuid::Uuid = source_str.parse().ok()?;
            let target_id: uuid::Uuid = target_str.parse().ok()?;
            let relation: EdgeRelation = relation_str.parse().ok()?;
            let weight = link_args
                .get("weight")
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let metadata = link_args.get("metadata").cloned();
            let dependency_kind = link_args.get("dependency_kind").cloned();
            let resurrect = link_args
                .get("resurrect")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let result = registry
                .dispatch_intercepted_with_metadata_and_disposition(
                    tool,
                    args_value,
                    identity,
                    |namespace| async move {
                        // Use the handler's schema after admission, before a coordinated write.
                        // The registry consumes namespace before ordinary pack dispatch too.
                        if let Some(fields) = link_args.as_object_mut() {
                            fields.remove("namespace");
                        }
                        let _: khive_pack_kg::handlers::LinkParams =
                            khive_runtime::deser_params(link_args)?;
                        // The coordinator receives the same metadata as the KG
                        // handler: a top-level dependency_kind fills the key only
                        // when metadata does not already contain it.
                        let dependency_kind = match dependency_kind {
                            None | Some(Value::Null) => None,
                            Some(Value::String(value)) => Some(value),
                            Some(_) => {
                                return Err(RuntimeError::InvalidInput(
                                    "dependency_kind must be a string".into(),
                                ));
                            }
                        };
                        let metadata =
                            khive_runtime::merge_entry_metadata(metadata, dependency_kind)?;
                        let coord_result = coord
                            .link(
                                &namespace, source_id, target_id, relation, weight, metadata,
                                resurrect,
                            )
                            .await
                            .map_err(RuntimeError::from)?;
                        let mut raw = serde_json::to_value(&coord_result.edge)
                            .unwrap_or_else(|e| json!({"error": format!("serialize edge: {e}")}));
                        if relation.is_symmetric() {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert("source_id".to_string(), json!(source_id.to_string()));
                                obj.insert("target_id".to_string(), json!(target_id.to_string()));
                            }
                        }
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("mutation".to_string(), json!(coord_result.mutation.name()));
                        }
                        Ok(InterceptedDispatchResult::new(raw, ()))
                    },
                )
                .await;
            Some(
                result
                    .map(|outcome| OpSuccess::complete(outcome.result))
                    .map_err(|error| DispatchFailure::from_dispatch(tool, error)),
            )
        }
        "search" => {
            if args_value.get("help").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let mut handler_args = args_value.clone();
            if let Some(fields) = handler_args.as_object_mut() {
                fields.remove("namespace");
            }
            // Preserve the coordinated read scope and the actor resolved by
            // the gate in one sealed token through backend fan-out.
            let extra_visible = coordinator_search_visibility(registry, args_value, identity);
            let result = registry
                .dispatch_intercepted_with_token_and_disposition(
                    tool,
                    args_value,
                    identity,
                    |token| async move {
                        // Match normal registry dispatch ordering: the gate has
                        // already authorized this namespace before handler-level
                        // search validation runs inside the intercepted closure.
                        let request = ValidatedSearchRequest::from_value(handler_args, registry)?;
                        let coord_result = coord
                            .fan_out_search_scoped(&request, &token, args_value, &extra_visible)
                            .await?;
                        khive_storage::ensure_request_read_active("search")?;
                        // Preserve the coordinator search response's compatibility
                        // fields, and add the KG single-backend handler's canonical
                        // row fields for shape parity (MIN-1): `kind` (duplicates
                        // entity_kind/note_kind), `name`, `created_at`, `updated_at`,
                        // and `version`. Shared KG rank conversion also adds
                        // `score`, `rank_score`, `rank_score_kind`, and `signals`.
                        let result_val = if request.substrate() == SearchSubstrate::Note {
                            let items: Vec<Value> = coord_result
                                .note_hits
                                .iter()
                                .filter(|h| h.score >= request.min_rank_score())
                                .filter_map(|h| {
                                    let version = coord_result.note_versions.get(&h.note_id)?;
                                    let note_kind = coord_result.note_kinds.get(&h.note_id);
                                    let name =
                                        coord_result.note_names.get(&h.note_id).cloned().flatten();
                                    let created_at = coord_result
                                        .note_created_at
                                        .get(&h.note_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let updated_at = coord_result
                                        .note_updated_at
                                        .get(&h.note_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let ranking =
                                        search_rank_fields(h.score, h.rank_score_kind, h.signals);
                                    let mut row = json!({
                                        "id": h.note_id.to_string(),
                                        "kind": note_kind,
                                        "note_kind": note_kind,
                                        "name": name,
                                        "source": h.source.as_str(),
                                        "title": h.title,
                                        "snippet": h.snippet,
                                        "created_at": created_at,
                                        "updated_at": updated_at,
                                        "version": version,
                                    });
                                    row.as_object_mut().expect("search row object").extend(
                                        ranking.as_object().expect("ranking fields object").clone(),
                                    );
                                    Some(row)
                                })
                                .collect();
                            serde_json::to_value(items).unwrap_or_else(|_| json!([]))
                        } else {
                            let items: Vec<Value> = coord_result
                                .entity_hits
                                .iter()
                                .filter(|h| h.score >= request.min_rank_score())
                                .map(|h| {
                                    let entity_kind = coord_result.entity_kinds.get(&h.entity_id);
                                    let created_at = coord_result
                                        .entity_created_at
                                        .get(&h.entity_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let updated_at = coord_result
                                        .entity_updated_at
                                        .get(&h.entity_id)
                                        .map(|micros| khive_runtime::micros_to_iso(*micros));
                                    let ranking =
                                        search_rank_fields(h.score, h.rank_score_kind, h.signals);
                                    let version = coord_result.entity_versions.get(&h.entity_id);
                                    let mut row = json!({
                                        "id": h.entity_id.to_string(),
                                        "kind": entity_kind,
                                        "entity_kind": entity_kind,
                                        "name": h.title,
                                        "source": h.source.as_str(),
                                        "title": h.title,
                                        "snippet": h.snippet,
                                        "created_at": created_at,
                                        "updated_at": updated_at,
                                        "version": version,
                                    });
                                    row.as_object_mut().expect("search row object").extend(
                                        ranking.as_object().expect("ranking fields object").clone(),
                                    );
                                    row
                                })
                                .collect();
                            serde_json::to_value(items).unwrap_or_else(|_| json!([]))
                        };
                        let degradation = SearchDegradation::from_result(
                            &coord_result,
                            &result_val,
                            request.text_mode_name(),
                        );

                        Ok(InterceptedDispatchResult::new(result_val, degradation))
                    },
                )
                .await;
            Some(match result {
                Ok(outcome) => {
                    let is_empty = outcome
                        .result
                        .as_array()
                        .map(|items| items.is_empty())
                        .unwrap_or(true);
                    // ADR-130 §1: a backend failure with zero surviving hits
                    // (post server-side filtering, min_rank_score included) is a
                    // failed operation, not a successful empty result — the
                    // "no match" reading is not established when the answer
                    // may be sitting on the backend that never responded.
                    if outcome.metadata.is_partial() && is_empty {
                        let mut error = search_incomplete_error(outcome.metadata);
                        error
                            .as_object_mut()
                            .expect("structured search error")
                            .insert("domain_result".into(), outcome.result);
                        Err(DispatchFailure::committed(tool, error))
                    } else {
                        Ok(OpSuccess {
                            result: outcome.result,
                            degradation: outcome.metadata,
                        })
                    }
                }
                Err(error) => Err(DispatchFailure::from_dispatch(tool, error)),
            })
        }
        _ => None,
    }
}

/// Resolve the coordinator search boundary's extra read-visibility set
/// (MAJ-3 fix), mirroring the normal registry dispatch path's default-case
/// widening to `['local'] ∪ visible_namespaces`
/// (`khive_runtime::pack::VerbRegistry::dispatch_with_identity`,
/// `crates/khive-runtime/src/pack.rs`). Without this, `fan_out_search`
/// authorizes each backend token against the resolved primary namespace
/// alone, so a namespace visible only through `visible_namespaces` silently
/// drops out of coordinator search results even though the same caller's
/// non-coordinator (single-backend) search would see it.
///
/// An explicit `namespace=` request parameter intentionally narrows
/// visibility to that one namespace — this returns an empty set in that
/// case, unwidened, exactly like the registry path's `explicit_namespace`
/// branch.
pub(super) fn coordinator_search_visibility(
    registry: &VerbRegistry,
    args_value: &Value,
    identity: Option<&khive_runtime::RequestIdentity>,
) -> Vec<khive_runtime::Namespace> {
    let explicit_namespace = args_value.get("namespace").is_some();
    if explicit_namespace {
        return Vec::new();
    }
    let mut extra_visible: Vec<khive_runtime::Namespace> = match identity {
        Some(id) => id
            .visible_namespaces
            .iter()
            .filter_map(|s| match khive_runtime::Namespace::parse(s) {
                Ok(parsed) => Some(parsed),
                Err(e) => {
                    tracing::warn!(
                        target: "khive_mcp::server",
                        namespace = %s,
                        error = %e,
                        "coordinator_search_visibility: skipping invalid visible_namespace \
                         entry from per-request identity"
                    );
                    None
                }
            })
            .collect(),
        None => registry.visible_namespaces().to_vec(),
    };
    extra_visible.push(khive_runtime::Namespace::local());
    extra_visible
}

/// Every runtime variant is explicitly covered. Dispatch provenance, not the
/// variant, determines whether the domain handler ran successfully, except for
/// the named write outcomes, which carry their own domain proof.
/// Apply MCP transport limits after the shared typed projection. The boundary's
/// original disposition decides whether an obligation result may be retained;
/// named error overrides remain exactly as emitted by the shared projection.
pub(super) fn runtime_error_value(error: RuntimeError, disposition: DomainDisposition) -> Value {
    let projected = khive_runtime::runtime_error_value(error, disposition);
    let projected_disposition = error_disposition(&projected);
    let mut value = error_with_disposition(projected, disposition);
    value["domain_disposition"] = json!(projected_disposition.as_str());
    value
}

/// Returns `true` when a raw handler `result` value's container nesting is
/// within [`khive_request::NESTING_DEPTH_LIMIT`]. Callers MUST call this on
/// the raw value straight out of coordinator/registry dispatch, before any
/// recursive `Value` operation (clone, serialize, presentation transform)
/// touches it — see `crates/khive-mcp/docs/design.md` (Result depth guard).
pub(super) fn result_within_depth_limit(result: &Value) -> bool {
    khive_request::value_nesting_within_limit(result, khive_request::NESTING_DEPTH_LIMIT)
}

/// Per-op error payload for a handler result that failed
/// [`result_within_depth_limit`]. Carries only the configured depth limit,
/// never the oversized value itself.
fn depth_error_payload(context: &str) -> Value {
    json!({
        "kind": "result_too_deep",
        "code": "result_too_deep",
        "message": format!(
            "op result nesting depth exceeds max {}{context}",
            khive_request::NESTING_DEPTH_LIMIT
        ),
    })
}

/// Build the `{ok: true, tool, result}` envelope for a successful op,
/// without re-serializing an already-owned `Value` through `json!` (which
/// would call `serde_json::to_value` and recurse over the whole tree
/// again). The depth check must already have passed before this is called.
pub(super) fn ok_envelope(tool: String, success: OpSuccess) -> Value {
    let OpSuccess {
        result,
        degradation,
    } = success;
    let SearchDegradation {
        status,
        retryable: _,
        arm_participation,
        retry_after_ms: _,
        missing_backends,
        backend_errors,
        backend_errors_omitted,
    } = degradation;
    let is_partial = status == Some(SearchStatus::Partial);
    let extra_fields = usize::from(status.is_some())
        + usize::from(arm_participation.is_some())
        + if is_partial {
            3 + usize::from(backend_errors_omitted > 0) * 2
        } else {
            0
        };
    let mut map = serde_json::Map::with_capacity(3 + extra_fields);
    map.insert("ok".to_string(), Value::Bool(true));
    map.insert("tool".to_string(), Value::String(tool));
    map.insert("result".to_string(), result);
    // ADR-130 §1: `status` is present on every successful search envelope
    // (complete or partial); absent for every other verb.
    if let Some(status) = status {
        map.insert(
            "status".to_string(),
            Value::String(status.as_str().to_string()),
        );
    }
    if let Some(participation) = arm_participation {
        map.insert(
            "arm_participation".to_string(),
            search_arm_participation_value(participation),
        );
    }
    // Legacy `partial`/`missing_backends` alias, compatibility-release only
    // (ADR-130 §Compatibility) — omitted for `status="complete"`.
    if is_partial {
        map.insert("partial".to_string(), Value::Bool(true));
        map.insert(
            "missing_backends".to_string(),
            Value::Array(missing_backends.into_iter().map(Value::String).collect()),
        );
        map.insert(
            "backend_errors".to_string(),
            backend_errors_value(&backend_errors),
        );
        if backend_errors_omitted > 0 {
            map.insert("backend_errors_truncated".to_string(), Value::Bool(true));
            map.insert(
                "backend_errors_omitted".to_string(),
                json!(backend_errors_omitted),
            );
        }
    }
    Value::Object(map)
}

/// Discard a rejected over-limit `Value` without native recursion.
///
/// `Value`'s derived `Drop` walks nested containers the same way `Clone`
/// and `Serialize` do, so simply letting a pathologically deep `result`
/// fall out of scope after the depth guard rejects it would trade a stack
/// overflow during serialization for one during drop. Draining containers
/// onto an explicit heap-allocated worklist keeps each removal O(1) on the
/// call stack regardless of nesting depth.
pub(super) fn drop_value_iteratively(value: Value) {
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            Value::Array(items) => stack.extend(items),
            Value::Object(map) => stack.extend(map.into_values()),
            _ => {}
        }
    }
}

/// Builds a `substitution_error` payload for a `$prev` argument that failed
/// to resolve (`resolve_all` returned `None`). Uses [`ArgValue::find_prev_failure`]
/// to identify exactly which lookup failed and why — a missing field/index, a
/// path segment applied to the wrong JSON type, or unsupported bracket syntax
/// — each worded differently so the caller isn't left with one generic
/// "not found" for three different mistakes. Falls back to a generic message
/// only if `find_prev_failure` cannot explain a miss `resolve_all` reported
/// (defensive; the two are expected to always agree).
pub(super) fn substitution_error_payload(name: &str, arg_val: &ArgValue, prev: &Value) -> Value {
    let Some(failure) = arg_val.find_prev_failure(prev) else {
        let fields_hint = if let Value::Object(map) = prev {
            let mut fields: Vec<&str> = map.keys().map(String::as_str).collect();
            fields.sort_unstable();
            format!(" Available top-level fields: [{}]", fields.join(", "))
        } else {
            String::new()
        };
        return json!({
            "kind": "substitution_error",
            "reason": "path_not_found",
            "message": format!(
                "argument {name:?}: one or more $prev paths not found in prior result.{fields_hint}"
            ),
        });
    };
    let reason = match &failure {
        PrevFailure::NotFound { .. } => "path_not_found",
        PrevFailure::WrongType { .. } => "path_wrong_type",
        PrevFailure::Unsupported { .. } => "path_unsupported",
    };
    json!({
        "kind": "substitution_error",
        "reason": reason,
        "message": format!(
            "argument {name:?}: {failure}. $prev resolves only against the immediately \
             preceding op's result — a non-adjacent dependency cannot be expressed inside \
             one chain; split into separate calls and carry the value across yourself."
        ),
    })
}

/// ADR-103 Amendment 2: stamp the per-op envelope entry with the dispatch's
/// frozen usage snapshot when its counters remain complete. A marked context
/// omits the key even after freeze. Best-effort — never alters ok/error status.
pub(super) fn stamp_usage(entry: &mut Value, ctx: &khive_runtime::usage::UsageContext) {
    if let Value::Object(map) = entry {
        match ctx.shipping_snapshot() {
            Some(snapshot) => {
                map.insert("usage".to_string(), snapshot);
            }
            None => {
                map.remove("usage");
            }
        }
    }
}

/// Add host-owned ticker liveness to the schedule pack's canonical agenda
/// payload. The pack owns scheduled intent; the MCP host owns the daemon loop,
/// so this decoration stays at their dispatch boundary instead of persisting a
/// process heartbeat in schedule data.
pub(super) fn decorate_schedule_agenda_with_ticker_health(
    tool: &str,
    is_help: bool,
    mut result: Value,
    last_tick_micros: &AtomicI64,
) -> Value {
    if tool != "schedule.agenda" || is_help {
        return result;
    }
    let last_tick = last_tick_micros.load(Ordering::Acquire);
    let last_tick_at = (last_tick > 0).then(|| khive_runtime::micros_to_iso(last_tick));
    if let Some(result) = result.as_object_mut() {
        result.insert(
            "ticker".to_string(),
            json!({ "last_tick_at": last_tick_at }),
        );
    }
    result
}

/// Chain-mode (`dispatch_op`) success path: check the raw handler `result`
/// against the depth guard before it is ever cloned into `$prev` context or
/// wrapped in the response envelope. On violation returns a `result_too_deep`
/// error that does not embed the oversized value, and discards the rejected
/// value iteratively so its own drop can't overflow the stack either.
pub(super) fn chain_ok_envelope_or_depth_error(
    tool: String,
    success: OpSuccess,
) -> Result<Value, DispatchFailure> {
    if !result_within_depth_limit(&success.result) {
        drop_value_iteratively(success.result);
        return Err(DispatchFailure::committed(
            tool,
            depth_error_payload("; cannot be used as $prev chain context"),
        ));
    }
    Ok(ok_envelope(tool, success))
}

/// Parallel/single-mode success path: check the raw handler `result` against
/// the depth guard *before* it is handed to `present` (which recurses
/// natively over `Value` in agent mode) or wrapped in the response envelope.
/// On violation returns a `result_too_deep` per-op error entry that does not
/// embed the oversized value, and discards the rejected value iteratively
/// (see [`drop_value_iteratively`]).
pub(super) fn present_ok_envelope_or_depth_error(
    tool: String,
    mut success: OpSuccess,
    mode: PresentationMode,
    now_unix: impl Into<PresentationNow>,
    policy: VerbPresentationPolicy,
    content_scope: NoteContentScope,
) -> Value {
    if !result_within_depth_limit(&success.result) {
        drop_value_iteratively(success.result);
        return failure_entry(tool, depth_error_payload(""), DomainDisposition::Committed);
    }
    success.result = content_scope.protect(success.result, |value| {
        present_with_policy_at(value, mode, now_unix.into(), policy)
    });
    ok_envelope(tool, success)
}

/// Returns `true` if a dispatched op's canonical `result` field nests
/// container values (`[`/`{`) deeper than [`khive_request::NESTING_DEPTH_LIMIT`].
///
/// This is a second, defense-in-depth check retained on the chain-mode
/// aggregation path in [`KhiveMcpServer::run_parsed`]: by the time it runs,
/// [`chain_ok_envelope_or_depth_error`] has already screened the same
/// `result` field inside `dispatch_op`, so this should never trip in
/// practice. It stays cheap (iterative, not recursive) so keeping it costs
/// nothing and catches a future refactor that bypasses the earlier guard.
pub(super) fn result_exceeds_depth_limit(result_obj: &Value) -> bool {
    result_obj
        .get("result")
        .is_some_and(|v| !result_within_depth_limit(v))
}

/// Chain-mode aggregation-loop seam in [`KhiveMcpServer::run_parsed`]:
/// defense-in-depth depth check on a dispatched op's full `result_obj`
/// envelope (should never trip — `dispatch_op` already screened `result`).
/// Returns the unchanged envelope on success, or an already-built error
/// entry on rejection. See `crates/khive-mcp/docs/design.md` (Result depth
/// guard) for why the rejected envelope is drained iteratively.
pub(super) fn chain_aggregation_depth_reject(result_obj: Value) -> Result<Value, Value> {
    if result_exceeds_depth_limit(&result_obj) {
        let tool_name = result_obj
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let error_entry = failure_entry(
            tool_name,
            depth_error_payload("; cannot be used as $prev chain context"),
            DomainDisposition::Committed,
        );
        drop_value_iteratively(result_obj);
        return Err(error_entry);
    }
    Ok(result_obj)
}

/// Apply the presentation transform to the `result` field of a successful
/// per-op envelope, leaving error envelopes unchanged.
///
/// Error envelopes are never transformed — only successful `result` fields.
pub(super) fn apply_presentation_to_result(
    mut result_obj: Value,
    mode: PresentationMode,
    now_unix: impl Into<PresentationNow>,
    policy: VerbPresentationPolicy,
    content_scope: NoteContentScope,
) -> Value {
    if result_obj.get("ok").and_then(Value::as_bool) == Some(true) {
        if let Some(result_field) = result_obj.get("result").cloned() {
            let presented = content_scope.protect(result_field, |value| {
                present_with_policy_at(value, mode, now_unix.into(), policy)
            });
            if let Some(obj) = result_obj.as_object_mut() {
                obj.insert("result".to_string(), presented);
            }
        }
    }
    result_obj
}
