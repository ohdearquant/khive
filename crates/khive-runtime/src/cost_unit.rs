//! ADR-103 Amendment 1: deterministic `cost_unit` for the per-dispatch
//! audit-row `resource` payload enrichment.
//!
//! `cost_unit = base_weight(verb) + per_item_weight(verb) x item_count x
//! model_count`, computed with checked `i64` arithmetic and clamped to
//! `i64::MAX` on overflow rather than omitted (ADR-103 Amendment 1 Part 1).
//!
//! Amendment 1 commits only to the formula's *shape* and to
//! `per_item_weight(verb) = 0` for every verb outside its closed
//! embedding-bearing family. The `base_weight` / `per_item_weight`
//! magnitudes are left as "deterministic, hand-set constants ... fixed at
//! implementation time and not measured". This module ships `base_weight =
//! 1` uniformly across every verb and `per_item_weight = 1` for every
//! embedding-bearing verb, as the documented default pending a dedicated
//! per-verb weights table (see the PR body that introduced this module).

use serde_json::Value;

/// True when `verb` is one of ADR-103 Amendment 1's closed
/// embedding-bearing verb families, given the request's own top-level
/// params.
///
/// `params` is needed only to tell a singleton `create` from a bulk
/// `create(items=[...])`: the amendment explicitly carves bulk create out as
/// non-embedding-bearing (`create_many` intentionally skips embedding and
/// backfills vectors later via a separate `reindex` call,
/// `crates/khive-runtime/src/operations.rs:4698-4709`), regardless of its
/// own `created`/`attempted` summary counts.
fn is_embedding_bearing(verb: &str, params: &Value) -> bool {
    match verb {
        "create" => params.get("items").is_none(),
        "update" | "memory.remember" | "memory.recall" | "knowledge.search"
        | "knowledge.compose" | "knowledge.index" => true,
        _ => false,
    }
}

/// `base_weight(verb)`: every verb dispatch, `1` (documented default; see
/// module docs: the amendment leaves specific weight VALUES unspecified).
fn base_weight(_verb: &str) -> i64 {
    1
}

/// `per_item_weight(verb)`: `1` for every embedding-bearing verb family
/// (documented default; see module docs), `0` for everything else. The `0`
/// case is not a default: it is ADR-103 Amendment 1's normative
/// requirement that a non-embedding-bearing verb's `cost_unit` reduces to
/// `base_weight(verb)` alone, `item_count`/`model_count` playing no role.
fn per_item_weight(verb: &str, params: &Value) -> i64 {
    if is_embedding_bearing(verb, params) {
        1
    } else {
        0
    }
}

/// `item_count` for one dispatch, per ADR-103 Amendment 1's per-verb-family
/// table. Only meaningful (and only called by [`cost_unit_for_dispatch`])
/// when `per_item_weight` is nonzero for this verb.
///
/// - `create` singleton, `memory.remember`, `update`, `memory.recall`,
///   `knowledge.search`, `knowledge.compose`: always `1`, each is a single
///   entity/note write or a single query embedding, never a batch.
/// - `knowledge.index`: `result["total"]`, the full paged corpus count
///   computed across all internally paged reads, never the internal
///   `batch_size` chunk ceiling (`clamp(1, 1000)` on the embed-grouping
///   page size only, not the dispatch's total work).
fn item_count(verb: &str, result: &Value) -> i64 {
    if verb == "knowledge.index" {
        result.get("total").and_then(Value::as_i64).unwrap_or(0)
    } else {
        1
    }
}

/// `model_count` for one dispatch, per ADR-103 Amendment 1's per-verb-family
/// table. Only meaningful (and only called by [`cost_unit_for_dispatch`])
/// when `per_item_weight` is nonzero for this verb.
///
/// `registered_model_count` is evaluated lazily via `FnOnce`, called only
/// for the two verb families whose `model_count` is not a per-dispatch
/// constant (`memory.remember`'s implicit-model case, and singleton
/// `create`), so every other dispatch never touches the runtime's embedder
/// registry.
///
/// `0` is a valid, deliberate result when no embedding model is registered
/// at all: no embed call is issued, so the whole
/// `per_item_weight x item_count x model_count` term is `0` and
/// `cost_unit` reduces to `base_weight(verb)` alone. The dispatch still
/// happened; no embedding work backs its cost.
fn model_count(verb: &str, params: &Value, registered_model_count: impl FnOnce() -> i64) -> i64 {
    match verb {
        "memory.remember" => {
            let explicit_single_model = params
                .get("embedding_model")
                .and_then(Value::as_str)
                .is_some();
            if explicit_single_model {
                1
            } else {
                registered_model_count()
            }
        }
        "create" => registered_model_count(),
        // update, memory.recall, knowledge.search / compose, knowledge.index:
        // each invokes exactly one embedding model (a query-embedding model,
        // or the single configured default embedder), never a fan-out.
        _ => 1,
    }
}

/// Checked-arithmetic `cost_unit`:
/// `base_weight + per_item_weight x item_count x model_count`.
///
/// `checked_mul` at each product step, `checked_add` for the final sum: any
/// overflow at any step clamps the WHOLE expression to `i64::MAX` rather
/// than omitting the field (ADR-103 Amendment 1 Part 1). All inputs are
/// non-negative in practice, so overflow can only occur in the positive
/// direction.
fn compute(base_weight: i64, per_item_weight: i64, item_count: i64, model_count: i64) -> i64 {
    let term = per_item_weight
        .checked_mul(item_count)
        .and_then(|p| p.checked_mul(model_count))
        .unwrap_or(i64::MAX);
    base_weight.checked_add(term).unwrap_or(i64::MAX)
}

/// Compute `resource.cost_unit` for one successful dispatch.
///
/// `params` is the original request's top-level arguments (`GateRequest::args`,
/// already in scope, read-only, at the audit-row emission seam in
/// `crates/khive-runtime/src/pack.rs`); `result` is the dispatch's own
/// successful `Value`; `registered_model_count` reads
/// `PackRuntime::registered_embedding_model_names().len()` for the pack that
/// owns `verb`, lazily.
///
/// Callers MUST only invoke this for a successful (`Ok`) dispatch result.
/// Error-outcome dispatches omit `resource.cost_unit` entirely (ADR-103
/// Amendment 1's "absence has exactly two meanings" rule: a pre-amendment
/// event, or a dispatch that errored) and must never call into this
/// function: there is no successful handler `Value` to read `item_count`
/// from for an errored dispatch.
pub fn cost_unit_for_dispatch(
    verb: &str,
    params: &Value,
    result: &Value,
    registered_model_count: impl FnOnce() -> i64,
) -> i64 {
    let weight = per_item_weight(verb, params);
    if weight == 0 {
        return compute(base_weight(verb), 0, 0, 0);
    }
    let items = item_count(verb, result);
    let models = model_count(verb, params, registered_model_count);
    compute(base_weight(verb), weight, items, models)
}

/// Build the `resource` payload object, `{"work_class": "interactive",
/// "cost_unit": N}`, for one successful verb dispatch.
///
/// Every dispatch through `VerbRegistry::dispatch*` is `work_class:
/// "interactive"` (ADR-103 Decision (a): "Request-driven synchronous verb
/// dispatch. Default for all handlers."). Background phase work (embedder
/// warmup, ANN rebuild, etc.) uses the separate `PhaseStarted` /
/// `PhaseCompleted` / `PhaseCancelled` event family and never this payload.
///
/// `request_id` (khive#948) is the caller-supplied correlation id threaded in
/// from the daemon frame via `RequestIdentity`, stamped alongside
/// `work_class`/`cost_unit` when the caller supplied one. Its absence has
/// exactly one meaning (no id was supplied, e.g. a pre-#948 client or an
/// internal/non-benchmark caller) — unlike `cost_unit`, it is never
/// conditionally omitted on an otherwise-successful row.
pub fn resource_payload(
    verb: &str,
    params: &Value,
    result: &Value,
    registered_model_count: impl FnOnce() -> i64,
    request_id: Option<u64>,
) -> Value {
    let cost_unit = cost_unit_for_dispatch(verb, params, result, registered_model_count);
    let mut payload = serde_json::json!({ "work_class": "interactive", "cost_unit": cost_unit });
    if let Some(id) = request_id {
        if let Value::Object(ref mut map) = payload {
            map.insert("request_id".to_string(), serde_json::json!(id));
        }
    }
    stamp_usage_units(&mut payload);
    payload
}

/// ADR-103 Amendment 2: freeze the dispatch-accounting context (first freeze
/// wins) and stamp the snapshot as `resource.units`. The resource payload is
/// built immediately before the enclosing audit row is appended, which is
/// exactly the amendment's snapshot point — the same frozen object is what
/// the response envelope later reads. No armed context (direct registry
/// callers, background work) means no `units` key; reporting never fails the
/// dispatch.
fn stamp_usage_units(payload: &mut Value) {
    if let Some(ctx) = crate::usage::current() {
        if let Value::Object(map) = payload {
            map.insert("units".to_string(), ctx.freeze());
        }
    }
}

/// Build the `resource` payload object for a dispatch that did not resolve
/// `Ok`: `{"work_class": "interactive"}`, with no `cost_unit` key.
///
/// ADR-103 Decision (a) stamps the closed `work_class` enum on every event,
/// with no exception for a denied, errored, or unknown-verb dispatch. Only
/// `resource.cost_unit` is scoped to a successful dispatch by Amendment 1's
/// "absence has exactly two meanings" rule (a pre-amendment event, or a
/// dispatch that errored): `work_class` itself is not one of those two
/// omission cases, so it must still be present. Every dispatch through
/// `VerbRegistry::dispatch*` is `work_class: "interactive"` regardless of
/// outcome; there is no non-interactive outcome for a verb dispatch.
///
/// `request_id` (khive#948) is stamped the same way on this payload as on
/// [`resource_payload`]'s: failure rows must be joinable by the same key as
/// success rows.
pub fn base_resource_payload(request_id: Option<u64>) -> Value {
    let mut payload = serde_json::json!({ "work_class": "interactive" });
    if let Some(id) = request_id {
        if let Value::Object(ref mut map) = payload {
            map.insert("request_id".to_string(), serde_json::json!(id));
        }
    }
    stamp_usage_units(&mut payload);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn unreachable_model_count() -> i64 {
        panic!("registered_model_count must not be called for a non-embedding-bearing verb")
    }

    // ---- Formula arithmetic ----

    #[test]
    fn non_embedding_verb_is_base_weight_only() {
        let cost = cost_unit_for_dispatch("stats", &json!({}), &json!({}), unreachable_model_count);
        assert_eq!(
            cost, 1,
            "non-embedding-bearing verb must be base_weight(verb) alone"
        );
    }

    #[test]
    fn link_is_base_weight_only_regardless_of_bulk_shape() {
        let singleton = cost_unit_for_dispatch(
            "link",
            &json!({"source_id": "a", "target_id": "b", "relation": "extends"}),
            &json!({"id": "edge-1"}),
            unreachable_model_count,
        );
        let bulk = cost_unit_for_dispatch(
            "link",
            &json!({"links": [{}, {}, {}]}),
            &json!({"attempted": 3, "created": 3}),
            unreachable_model_count,
        );
        assert_eq!(singleton, 1);
        assert_eq!(
            bulk, 1,
            "link has no embedding-bearing path, singleton or bulk"
        );
    }

    #[test]
    fn create_singleton_scales_with_registered_model_count() {
        let cost = cost_unit_for_dispatch("create", &json!({"kind": "concept"}), &json!({}), || 3);
        // base_weight(1) + per_item_weight(1) * item_count(1) * model_count(3)
        assert_eq!(cost, 4);
    }

    // ---- Zero-model vanishing ----

    #[test]
    fn zero_registered_models_vanishes_the_term() {
        let cost = cost_unit_for_dispatch("create", &json!({"kind": "concept"}), &json!({}), || 0);
        assert_eq!(
            cost, 1,
            "no embedding model registered -> cost_unit reduces to base_weight(verb) alone"
        );
    }

    #[test]
    fn memory_remember_zero_registered_models_vanishes_the_term() {
        let cost = cost_unit_for_dispatch("memory.remember", &json!({}), &json!({}), || 0);
        assert_eq!(cost, 1);
    }

    // ---- Bulk create is base-only ----

    #[test]
    fn bulk_create_is_base_weight_only_never_touches_model_count() {
        let cost = cost_unit_for_dispatch(
            "create",
            &json!({"items": [{"kind": "concept", "name": "a"}, {"kind": "concept", "name": "b"}]}),
            &json!({"attempted": 2, "created": 2}),
            unreachable_model_count,
        );
        assert_eq!(
            cost, 1,
            "bulk create(items=[...]) skips embedding entirely -> base_weight(verb) alone"
        );
    }

    #[test]
    fn bulk_create_is_base_only_regardless_of_created_count() {
        // The amendment is explicit: this holds "regardless of its
        // created/attempted summary counts" -- a large batch must not
        // change the result.
        let items: Vec<Value> = (0..250)
            .map(|i| json!({"kind": "concept", "name": format!("item-{i}")}))
            .collect();
        let cost = cost_unit_for_dispatch(
            "create",
            &json!({"items": items}),
            &json!({"attempted": 250, "created": 250}),
            unreachable_model_count,
        );
        assert_eq!(cost, 1);
    }

    // ---- knowledge.index full total, not batch_size ceiling ----

    #[test]
    fn knowledge_index_uses_full_paged_total_not_batch_size_ceiling() {
        // batch_size only bounds the internal SQL page / embed-grouping
        // size; the dispatch can process far more than 1000 items in one
        // call, and item_count must reflect that full total.
        let cost = cost_unit_for_dispatch(
            "knowledge.index",
            &json!({"batch_size": 1000}),
            &json!({"indexed": 4500, "skipped": 0, "failed": 0, "total": 4500}),
            || 1,
        );
        // base_weight(1) + per_item_weight(1) * item_count(4500) * model_count(1)
        assert_eq!(cost, 4501);
    }

    #[test]
    fn knowledge_index_missing_total_defaults_to_zero_items_not_a_panic() {
        let cost = cost_unit_for_dispatch("knowledge.index", &json!({}), &json!({}), || 1);
        assert_eq!(cost, 1);
    }

    #[test]
    fn knowledge_index_model_count_is_constant_one_never_reads_registry() {
        let cost = cost_unit_for_dispatch(
            "knowledge.index",
            &json!({}),
            &json!({"total": 10}),
            unreachable_model_count,
        );
        assert_eq!(cost, 11);
    }

    // ---- memory.remember explicit-model override ----

    #[test]
    fn memory_remember_explicit_model_overrides_registry_count() {
        let cost = cost_unit_for_dispatch(
            "memory.remember",
            &json!({"content": "x", "embedding_model": "paraphrase"}),
            &json!({}),
            unreachable_model_count,
        );
        // explicit single model -> model_count = 1, registry never consulted
        assert_eq!(cost, 2);
    }

    #[test]
    fn memory_remember_no_explicit_model_reads_registered_count() {
        let cost = cost_unit_for_dispatch(
            "memory.remember",
            &json!({"content": "x"}),
            &json!({}),
            || 4,
        );
        assert_eq!(cost, 5);
    }

    // ---- Constant-model-count families never touch the registry ----

    #[test]
    fn update_and_recall_and_search_never_touch_registry() {
        for verb in [
            "update",
            "memory.recall",
            "knowledge.search",
            "knowledge.compose",
        ] {
            let cost =
                cost_unit_for_dispatch(verb, &json!({}), &json!({}), unreachable_model_count);
            assert_eq!(cost, 2, "verb {verb}: base_weight(1) + 1*1*1");
        }
    }

    // ---- Overflow clamps to i64::MAX ----

    #[test]
    fn compute_clamps_multiplication_overflow_to_i64_max() {
        assert_eq!(compute(1, i64::MAX, 2, 1), i64::MAX);
    }

    #[test]
    fn compute_clamps_addition_overflow_to_i64_max() {
        assert_eq!(compute(i64::MAX, 1, 1, 1), i64::MAX);
    }

    #[test]
    fn knowledge_index_extreme_total_clamps_to_i64_max() {
        let cost = cost_unit_for_dispatch(
            "knowledge.index",
            &json!({}),
            &json!({"total": i64::MAX}),
            || 2,
        );
        assert_eq!(cost, i64::MAX);
    }

    // ---- resource_payload shape ----

    #[test]
    fn resource_payload_shape_is_work_class_and_cost_unit_only() {
        let payload = resource_payload(
            "stats",
            &json!({}),
            &json!({}),
            unreachable_model_count,
            None,
        );
        assert_eq!(
            payload,
            json!({"work_class": "interactive", "cost_unit": 1}),
            "resource payload must be exactly {{work_class, cost_unit}}, no request_id key \
             when the caller supplied none"
        );
    }

    #[test]
    fn resource_payload_stamps_request_id_when_supplied() {
        let payload = resource_payload(
            "stats",
            &json!({}),
            &json!({}),
            unreachable_model_count,
            Some(42),
        );
        assert_eq!(
            payload,
            json!({"work_class": "interactive", "cost_unit": 1, "request_id": 42}),
        );
    }

    #[test]
    fn base_resource_payload_omits_request_id_when_absent() {
        assert_eq!(
            base_resource_payload(None),
            json!({"work_class": "interactive"}),
        );
    }

    #[test]
    fn base_resource_payload_stamps_request_id_when_supplied() {
        assert_eq!(
            base_resource_payload(Some(7)),
            json!({"work_class": "interactive", "request_id": 7}),
        );
    }
}
