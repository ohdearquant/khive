# ADR-042: Composable Rerank Pipeline (local cross-encoder + salience + graph-proximity)

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive
**Depends on**:

- ADR-011 (Embedding and Inference Architecture)
- ADR-021 (Memory Pack)
- ADR-031 (Multi-Engine Retrieval)
- ADR-032 (Brain Profile Orchestration — LoRA-class profile state)
- ADR-033 (Recall Pipeline)

---

## Context

khive's retrieval today is bi-encoder + fusion: FTS5 produces text candidates, lattice
embeddings produce vector candidates, RRF fuses them, the memory pack's scoring
formula (ADR-033 §1) applies relevance/salience/temporal weights. There is no
cross-encoder rerank — no model that scores each candidate against the query directly.

ADR-011 notes this:

> Cross-encoder rerank (query, candidate → score) is deferred. When lattice publishes
> a rerank crate, `khive-runtime` adds a `rerank()` method that calls it directly —
> same pattern as embedding. No HTTP, no service abstraction.

Current shipped state is narrower than this ADR's native-rerank target. ADR-033
ships a memory-pack weighted feature rerank path: `RecallConfig.reranker_weights`
uses the feature keys `relevance`, `salience`, `temporal`, `text_match`, and
`vector_match`; when non-empty, that weighted score replaces the default final
`rank_score`.

Native lattice/cross-encoder rerank, LoRA hook resolution, runtime rerank config,
latency disable behavior, and memory-path `RerankExecuted` emission are deferred.
`khive-retrieval` contains a native cross-encoder scaffold and an empty
`native-rerank` feature only.

### What this ADR does

- Defines the deferred native rerank design: a local cross-encoder/lattice tier,
  LoRA hook routing, and native rerank events.
- Establishes that embeddings are not LoRA-adapted; only future LLM-based rerank
  and similar inference call sites may consume LoRA hooks.
- Reserves the same shape for future call sites (query paraphraser, synthesizer).

### What this ADR does NOT do

- Change the shipped ADR-033 weighted feature rerank contract.
- Claim that `lattice-inference`, LoRA hooks, runtime `rerank_model_id`, or
  memory-path `RerankExecuted` emission are currently shipped.
- Add query paraphrasing or synthesis call sites.

---

## Decision

### Ownership and resolution

**Ownership**: ADR-042 owns:

- The `Reranker` trait (and cross-encoder / bi-encoder / pure-math variants)
- Rerank-stage configuration (`RerankConfig`)
- lattice-inference integration for local rerank

ADR-030 provides retrieval engines and low-level fusion primitives; it does NOT define
reranker traits or rerank weights. Those belong here.

**Resolution**: shipped weighted feature rerank does not resolve brain profiles or LoRA
hooks. Native rerank profile resolution for `consumer_kind="rerank"` is deferred.

**Score shape**: shipped `memory.recall_rerank` returns:

- `rerank_scores`: per-feature weighted contributions keyed by feature name.
- `rerank_score`: the normalized weighted feature score.

In full `memory.recall`, when `reranker_weights` is non-empty, `rerank_score` becomes
the final `rank_score` directly. It is not further blended with RRF/salience/temporal.

### 1. Shipped weighted-rerank subhandler and deferred native stage

The shipped memory-pack subhandler is:

| Handler                | Visibility | Input                   | Output                                                                   |
| ---------------------- | ---------- | ----------------------- | ------------------------------------------------------------------------ |
| `memory.recall_rerank` | Subhandler | `{candidates, config?}` | `{reranked: [{note_id, rerank_scores, rerank_score}], active_rerankers}` |

It is an internal feature-weight subhandler. It does not accept `hook`, `profile_id`, or
`model_id`, and it does not return `hook_applied`.

The `hook` parameter is an optional profile id — when provided, the handler resolves
the profile and passes its `LoraHook` to the rerank forward (§4 below). When omitted,
the rerank runs with `NoopLoraHook`.

`memory.recall_rerank` is `Internal` per ADR-023 §2 visibility rules — agents don't
call it directly. The orchestrating `memory.recall` handler invokes it when the
deployment has a rerank model configured.

### 2. Model selection

Rerank model choice is a deployment configuration parameter, not a hardcoded constant.
`RuntimeConfig` gains:

````rust
pub struct RuntimeConfig {
    // … existing fields …
    // NOTE: embedding_model is NOT carried here. Embedding generation is the
    // caller's responsibility per ADR-031:342-348. The caller produces embeddings
    // and passes pre-computed vectors to the rerank stage if needed.

    /// Active rerank model. `None` disables the rerank stage — recall returns
    /// fused candidates directly to the score stage (ADR-033 behavior unchanged).
    /// References a `RegisteredModel.id` in lattice-tune's registry (lattice ADR-029).
    pub rerank_model_id: Option<Uuid>,
}

/// Rerank-specific configuration, separate from RuntimeConfig.
/// Embedding model is NOT carried here — embedding generation is the caller's
No shipped `RerankConfig`, `RuntimeConfig.rerank_model_id`, `RuntimeConfig.rerank_top_n`,
or `KHIVE_RERANK_*` environment variable surface exists. The shipped opt-in is
per-call or active `RecallConfig.reranker_weights` in the memory pack. Native model
selection remains deferred.

### 3. Latency budget

Rerank is on the recall hot path — every recall call goes through it (when enabled).
The budget is **≤50ms per call on a typical workstation GPU**, **≤200ms on CPU**.
Recall is interactive; longer than 200ms breaks the agent feedback loop.

This implies practical constraints:

- Rerank model size: ≤1B parameters in f16, or ≤2B in int8 (lattice ADR-018 quantized
  vectors / quantization paths apply).
- Candidate set: top-N from fuse before rerank. v1 default `N = 32`. Larger sets
  blow the budget; smaller sets reduce rerank's signal.
- Batching: lattice's continuous batching (lattice ADR-048) handles concurrent
  recall calls. Per-call serial latency dominates for single-user deployments.
- GPU presence: deployments that load a rerank model SHOULD have GPU. CPU-only
  deployments leave `rerank_model_id = None` and rely on bi-encoder fusion only.

If a rerank call exceeds 500ms, the runtime emits a warning event and disables
rerank for the rest of the process lifetime. The next process start re-attempts;
the operator can `KHIVE_RERANK_DISABLED=1` to force-skip.

### 4. LoRA hook resolution

Native LoRA hook resolution is deferred. Shipped `brain.resolve` exists, but the memory
weighted-rerank path does not resolve rerank profiles, does not call `resolve_rerank_hook`,
and does not depend on lattice LoRA hook types.

```rust
async fn handle_recall_rerank(
    &self,
    query:        &str,
    fused_hits:   Vec<FusedHit>,
    caller_ctx:   &CallerContext,
    runtime:      &KhiveRuntime,
) -> RuntimeResult<Vec<RerankedHit>> {
    let Some(model_id) = runtime.config().rerank_model_id else {
        // Rerank disabled — pass through.
        return Ok(fused_hits.into_iter().map(Into::into).collect());
    };

    let hook: Box<dyn LoraHook> = runtime
        .brain()
        .map(|b| b.resolve_rerank_hook(caller_ctx, model_id))
        .unwrap_or_else(|| Box::new(NoopLoraHook));

    let rerank_inputs = fused_hits.iter()
        .take(self.config.rerank_top_n)
        .map(|h| RerankInput { query, candidate: &h.content })
        .collect();

    let scores = lattice_inference::rerank(
        model_id,
        rerank_inputs,
        Some(&*hook),
    ).await?;

    Ok(merge_scores(fused_hits, scores))
}
````

`brain.resolve_rerank_hook(caller_ctx, model_id)` is the §4 resolution chain plus the
target-model-id check, returning `Box<NoopLoraHook>` on any mismatch.

### 5. Emitted event

When rerank runs, the runtime emits a `RerankExecuted` event after the rerank call
completes:

```rust
EventKind::RerankExecuted

payload = {
    served_by_profile_id: Option<String>,     // ADR-032 §3 — None if no hook applied
    model_id:             Uuid,
    candidates:           Vec<Uuid>,           // input ids (top-N from fuse)
    reranked:             Vec<(Uuid, HashMap<&'static str, f32>)>,  // per-reranker scores per item (audit/debug)
    final_scores:         Vec<(Uuid, f32)>,    // ordered output (id, weighted-sum final_score for ordering)
    latency_us:           u64,
    hook_applied:         bool,
    hook_target_match:    bool,                // false ⇒ profile present but model mismatched
}
```

ADR-041 (Event Provenance Projection) projects this event:

- `Candidate` rows for each input (positions match input order)
- `Selected` rows for the rerank output (positions match output order, top-K only)

Brain profiles fold over these events the same way they fold over `RecallExecuted`
— see ADR-032 §5a (Bayesian profile) and §5b (LoRA evolver consumes feedback signals
that reference rerank outputs).

### 6. Failure modes and fallbacks

| Condition                                                                   | Behavior                                                                                                             |
| --------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `rerank_model_id = None`                                                    | Rerank stage skipped; fused hits pass through to score stage.                                                        |
| `lattice-inference` rerank call returns `Err` (model not loaded, OOM, etc.) | Log error event; pass fused hits through; if 3 errors within 60s window, disable rerank for process lifetime.        |
| Rerank latency > 500ms                                                      | Warn event; result is still used; if 5 warnings within 5min, disable for process lifetime.                           |
| Hook target_model_id mismatch                                               | Drop the hook; rerank with `NoopLoraHook`; emit event with `hook_target_match = false`.                              |
| Profile resolution returns `NoProfileResolved`                              | Same as no brain loaded — `NoopLoraHook`. Not an error.                                                              |
| Feature `lattice-tune/inference-hook` not enabled at compile time           | Boot-time error if any brain profile is `LoRA`-class. Pure-Bayesian deployments compile and run without the feature. |

The rerank stage is degraded-mode-tolerant by design — fused hits are always a valid
fallback because they're what the pipeline used before this ADR existed.

### 7. Native reranker trait — deferred

The shipped memory pack does not instantiate native reranker objects and does not ship
`cross_encoder`, `salience`, or `graph_proximity` model/pure-math rerankers behind a
memory-pack `Reranker` trait.

Shipped rerank computes a single weighted score from five feature keys:

| Feature name   | Source                               |
| -------------- | ------------------------------------ |
| `relevance`    | Fused retrieval score                |
| `salience`     | Decay-adjusted note salience         |
| `temporal`     | Half-life recency score              |
| `text_match`   | Candidate appeared in text results   |
| `vector_match` | Candidate appeared in vector results |

The generic retrieval `Reranker` trait and native cross-encoder scaffold exist in
`khive-retrieval`, but native cross-encoder use remains deferred until the inference
port lands.

If a reranker errors, its scores default to `0.0` for that batch — the rest of the
pipeline proceeds. This preserves the §6 degraded-mode-tolerance contract per
reranker.

#### Configuration (replaces `rerank_weight` in ADR-033 RecallConfig)

```rust
pub struct RecallConfig {
    // ... existing fields ...

    /// Per-reranker weights. Missing keys default to 0.0 (reranker not used in
    /// scoring even if it ran). To enable a reranker, set its weight > 0.0.
    /// Keys: "cross_encoder", "salience", "graph_proximity", or any future-registered name.
    pub reranker_weights: HashMap<String, f64>,

    /// Per-reranker config (anchor entities for graph_proximity, salience α, etc.).
    pub reranker_params: HashMap<String, serde_json::Value>,
}
```

`RecallConfig.rerank_weight: f64` from the earlier draft of this ADR is removed in
favor of the keyed map. ADR-033 §1 RecallConfig MUST also drop the standalone
`rerank_weight` field and adopt the map. Pack defaults set the cross-encoder
weight to 0.0 (rerank disabled by default); operators or brain profiles enable it
per deployment.

### 8. Future call sites (reserved shape)

The Reranker trait covers in-pipeline rerank. Other lattice-inference call sites
remain future work and add their own stages (NOT new Reranker variants):

| Future call site              | Stage location                              | Adapter target         |
| ----------------------------- | ------------------------------------------- | ---------------------- |
| Query paraphraser             | Before `recall.embed`                       | Paraphrase model id    |
| Result synthesizer            | After selector                              | Synthesis model id     |
| Memory consolidator (offline) | Outside recall — batch pack-internal worker | Consolidation model id |

Each gets its own ADR mirroring this one. They are NOT Rerankers because they
operate on different inputs/outputs — query rewriting is `&str → &str`, synthesis
is `&[Candidate] → String`, consolidation is `&[Note] → Vec<Note>`. Forcing them
into the Reranker trait would dilute it.

---

## Rationale

### Why shipped weighted rerank replaces the default score

ADR-033's shipped behavior is REPLACE, not blend. `RecallConfig.reranker_weights`
defaults to `{}`, so behavior is unchanged when no weighted feature rerank is configured.
When one or more supported feature weights are configured, the formula becomes:

```text
rank_score = Σ(weights[feature] × feature_value) / Σ(positive recognized weights)
```

The supported feature keys are `relevance`, `salience`, `temporal`, `text_match`, and
`vector_match`. The existing default scoring path is skipped for that request because
the weighted feature score already covers the same base axes plus retrieval-source
features.

### Why rerank, not query paraphrasing, first

Tightest feedback loop. Rerank's input (fused candidates) and output (reranked order)
are both observable in the same event payload. `RecallSelected` events directly score
whether rerank picked the right item. The brain-LoRA feedback signal for rerank is
unambiguous: did the user/agent act on what rerank put on top, or on something
further down?

Query paraphrasing's signal is indirect — paraphrase quality is measured by
downstream recall hit rate, which involves the entire pipeline's behavior, not just
the paraphrase. Less clean for the v1 LoRA-adapter training loop.

### Why ≤50ms / ≤200ms budget

Recall is interactive. Agents tend to chain multiple recall calls per turn — a slow
rerank multiplies. 50ms is "barely noticeable" on local hardware; 200ms is "noticeable
but acceptable." Beyond 200ms recall feels broken.

The constraint cascades to model size, candidate count, and GPU presence. Operators
who want better quality at the cost of latency can override `RecallConfig.rerank_top_n`
to feed more candidates — at their own latency budget.

### Why target_model_id match check, not implicit compatibility

LoRA adapters store rank-r matrices indexed by `(layer_idx, module_name)`. Applying a
matrix shaped for model A's layer dimensions to model B silently produces nonsense
outputs — the math doesn't error, it just yields garbage. The explicit
`target_model_id` check at hook resolution catches this at boundary, before garbage
propagates into recall scoring.

The cost is one Uuid equality check per recall — negligible. The benefit is
catching the most common LoRA misconfig (deployed a new rerank model, forgot to
re-bind / retrain the adapter) before it corrupts the feedback signal.

### Why fall back to NoopLoraHook on mismatch (not error)

Errors at the rerank boundary block recall. The deployment is functional without the
hook — rerank still works, just unadapted. Surface the mismatch via the
`RerankExecuted` event's `hook_target_match = false` field, so operators see it in
audit logs without losing recall functionality.

---

## Alternatives Considered

| Alternative                                          | Why rejected                                                                                      |
| ---------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Defer rerank further (status quo)                    | Loses ADR-032 §5b's first real consumer; brain LoRA profiles stay aspirational.                   |
| Rerank as a separate process (RPC)                   | Adds network hop on hot path; violates ADR-011's zero-service deployment.                         |
| Hard-code Qwen3-0.8B as the rerank model             | Locks deployments into one model; can't adapt to better small models.                             |
| Rerank ALL fused hits (not top-N)                    | Latency explosion at fused-hit counts ≥100; 32-default is the empirical sweet spot.               |
| Apply LoRA hook to embedding model instead           | ADR-032 §5b explicitly forbids — silently misaligns stored vectors.                               |
| Use bi-encoder rerank (no cross-encoder model)       | Bi-encoder is what fuse already does; cross-encoder rerank is the marginal improvement.           |
| Error on hook target_model_id mismatch               | Loses functionality unnecessarily; degraded mode (NoopLoraHook) is preferable.                    |
| No latency budget; let operator set arbitrary models | Pipeline-wide latency contracts depend on rerank fitting in budget; unbounded rerank breaks them. |

---

## Consequences

### Positive

- ADR-032 §5b LoRA-class profiles get their v1 consumer — the typology becomes
  shippable, not aspirational.
- Recall quality gains a cross-encoder signal without breaking the existing scoring
  formula.
- The pattern for adapter-aware lattice-inference call sites is established — future
  paraphraser/synthesizer ADRs reuse this shape verbatim.
- Bi-encoder-only deployments continue to work unchanged (rerank defaults to disabled).
- ADR-041 (Event Provenance Projection) gets another emit-projection pattern to
  validate against.

### Negative

- Adds dependency on `lattice-inference` from `khive-runtime` (was previously only
  `lattice-embed`). The dependency is opt-in via `rerank_model_id` config but the
  link is mandatory at compile.
- Rerank latency dominates recall when enabled. Operators must pick model + GPU
  configuration that fits the budget.
- The fallback paths (no model, mismatch, latency) are degraded-mode behaviors that
  need monitoring — a deployment that silently runs with hook_target_match=false is
  losing brain-tuned quality without erroring.

### Neutral

- `lattice-tune/inference-hook` feature flag becomes mandatory for deployments that
  load LoRA-class brain profiles. Pure-Bayesian deployments unaffected.
- ADR-033's `RecallConfig` gains `reranker_weights: HashMap<String, f64>` (default `{}`
  — no rerankers active) and the orchestrating handler gains the rerank step.
  Backward-compatible — recall with no config change behaves as before.
- The rerank model loads on first use via lattice's lazy-init pattern (same as
  embedding). Idle deployments pay no cost.

---

## Implementation

### Config

- `RuntimeConfig.rerank_model_id: Option<Uuid>` — references lattice-tune registry.
- `RuntimeConfig.rerank_top_n: u32` — default 32, configurable.
- `RecallConfig.reranker_weights: HashMap<String, f64>` — default `HashMap::new()`, ADR-033 update.
  Example: `{"cross_encoder": 1.0}` to enable only cross-encoder rerank.
- Env vars: `KHIVE_RERANK_MODEL_ID`, `KHIVE_RERANK_TOP_N`, `KHIVE_RERANK_DISABLED`.

### Crate dependencies

```toml
# khive-runtime/Cargo.toml
lattice-inference = "X"
lattice-tune      = { version = "X", features = ["inference-hook"] }
```

The `inference-hook` feature flag is mandatory at compile because the brain pack
references `LoraHook` types from lattice-tune unconditionally. Pure-Bayesian
deployments compile the flag in but never load a LoRA profile.

### Handler

- `crates/khive-pack-memory/src/handlers.rs`: add `handle_recall_rerank`.
- `crates/khive-pack-memory/src/lib.rs`: register `recall_rerank` handler at
  `Visibility::Internal`.
- `crates/khive-pack-brain/src/lib.rs`: add `resolve_rerank_hook(caller_ctx,
  model_id) -> Box<dyn LoraHook>`.

### Events

`EventKind::RerankExecuted` and DB projection support exist, but the shipped memory
weighted-rerank path does not emit `RerankExecuted`. Event emission is reserved for the
native rerank path or a future explicit instrumentation change.

### Tests

Shipped tests cover:

| Scenario                          | Assert                                                                               |
| --------------------------------- | ------------------------------------------------------------------------------------ |
| Empty `reranker_weights`          | Weighted rerank is pass-through and default scoring behavior is unchanged.           |
| Configured feature weights        | Weighted feature score changes result ordering and replaces the default final score. |
| `memory.recall_rerank` subhandler | Returns `{reranked, active_rerankers}` with per-feature `rerank_scores`.             |

Native model, LoRA hook, latency-disable, process-wide disable, and emitted-event tests
are deferred with native rerank.

---

## Open Questions

1. **Multi-engine rerank**. ADR-031 multi-engine retrieval emits candidates from N
   engines. v1 reranks the post-fusion list (one rerank pass over fused candidates).
   Should there be a per-engine rerank with engine-specific adapters? Defer — fan-out
   rerank is its own complexity; one rerank pass over fused candidates is the v1
   contract.

2. **Rerank caching**. Identical `(query, candidate)` pairs across calls could be
   cached. Lattice's inference cache (ADR-015) handles embedding caching; an analogous
   rerank-score cache would be a separate cache layer in `khive-runtime`. Defer — the
   query side has high cardinality and rerank caches typically miss.

3. **Rerank as the only signal**. Some deployments may want rerank to dominate the
   score (e.g., a research deployment with very high signal from feedback). The
   `reranker_weights = {"cross_encoder": 1.0}` config covers it (only the keyed entry is active). No additional handler
   needed; just config tuning.

4. **Query paraphraser ADR**. The exact shape (one rewrite or N alternatives, applied
   before embed or as a separate query side-channel) is open. Resolve when the use case
   sharpens.

5. **Online training loop maturity**. ADR-032 §5b notes that lattice does not ship
   `adapt_step` for online gradient steps; khive implements it in
   `khive-pack-brain::lora::sgd_step`. v1's online training is exploratory — should
   it be gated behind a feature flag until the math is validated? Tentative: yes,
   `khive-pack-brain/online-lora` feature, default off in v1.

---

## References

- [ADR-008 (lattice)](../../../../lattice/docs/adr/ADR-008-lora-injection.md): `LoraHook` trait — the per-layer per-module adapter
  injection point this ADR consumes.
- [ADR-009 (lattice)](../../../../lattice/docs/adr/ADR-009-model-architectures.md): Qwen3 architecture — the v1 sentinel rerank model
  class.
- [ADR-029 (lattice)](../../../../lattice/docs/adr/ADR-029-model-registry.md): Model Registry — the `rerank_model_id` references
  RegisteredModel.id.
- [ADR-031 (lattice)](../../../../lattice/docs/adr/ADR-031-lora-adapter-management.md): LoRA Adapter Management — `LoraAdapter:
  LoraHook` impl behind `inference-hook` feature.
- [ADR-011](ADR-011-embedding-and-inference.md): Embedding and Inference Architecture
  — establishes the lattice-inference dependency pattern this ADR extends.
- [ADR-021](ADR-021-memory-pack.md): Memory Pack — the `recall` verb this ADR adds a
  stage to.
- [ADR-031](ADR-031-multi-engine-retrieval.md): Multi-Engine Retrieval — rerank consumes
  the fused output of multi-engine recall.
- [ADR-032](ADR-032-brain-profile-orchestration.md) §5b: LoRA-class profile state —
  this ADR is its v1 consumer.
- [ADR-033](ADR-033-recall-pipeline.md): Recall Pipeline — extended with the
  `recall.rerank` stage and `reranker_weights` config field.
- [ADR-041](ADR-041-event-provenance-projection.md): Event Provenance Projection —
  `RerankExecuted` events project candidates + selected via per-kind decoder.
- `crates/khive-pack-memory/src/handlers.rs`: rerank handler.
- `crates/khive-pack-brain/src/lib.rs`: `resolve_rerank_hook`.
- `crates/khive-runtime/src/runtime.rs`: `rerank_model_id` config wiring.
