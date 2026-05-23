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
formula (ADR-033 §1) applies relevance/importance/temporal weights. There is no
cross-encoder rerank — no model that scores each candidate against the query directly.

ADR-011 notes this:

> Cross-encoder rerank (query, candidate → score) is deferred. When lattice publishes
> a rerank crate, `khive-runtime` adds a `rerank()` method that calls it directly —
> same pattern as embedding. No HTTP, no service abstraction.

That deferral is no longer right. Lattice ships the full inference stack today —
Qwen3, BERT, GQA/RoPE/KV cache/paged-KV/continuous-batching (lattice ADRs 001, 009,
047, 048). A small reasoning model (Qwen3-class, ~0.8B parameters) running locally
via `lattice-inference` is the missing rerank tier. And it is the **first** khive
call site that consumes `LoraHook` from lattice ADR-008 — which makes it the v1
entry point for ADR-032 §5b LoRA-class brain profiles.

ADR-032 §5b table makes the boundary explicit: the embedding model is NOT
LoRA-adapted (online adaptation would silently misalign stored vectors against
newly-produced ones). The reranker IS. This ADR specifies that consumer.

### What this ADR does

- Adds a **composable** rerank stage to the recall pipeline (ADR-033) between fuse
  and score. The stage runs one or more named rerankers, each producing a per-candidate
  score; their contributions feed `recall.score`'s weighted sum.
- Defines the `Reranker` trait and three v1 built-in implementations:
  cross-encoder (lattice-inference), salience-weighted (pure math from memory
  metadata), and graph-proximity (hop-decay over KG structure).
- Defines the lattice-inference cross-encoder call signature, model selection,
  and latency budget — the heaviest single reranker. (§§1–6)
- Wires brain-resolved LoRA hooks into the cross-encoder forward pass — LoRA
  adapts only LLM-based rerankers; pure-math rerankers (salience, graph-proximity)
  carry no adapter.
- Specifies the failure modes (no rerank model loaded, no profile bound,
  target-model mismatch) — each falls back gracefully.
- Reserves the same shape for future call sites (query paraphraser, synthesizer) so
  they reuse the dispatch pattern.

### What this ADR does NOT do

- Add query paraphrasing or synthesis call sites — separate future ADRs.
- Adapt the embedding model (forbidden by ADR-032 §5b).
- Specify training a rerank LoRA from scratch — training pipelines live in
  `lattice-tune::train` (lattice ADR-026); brain consumes already-trained adapters
  and tunes them online via `khive-pack-brain::lora::sgd_step` (ADR-032 §5b).
- Replace the existing scoring formula (ADR-033 §1). Rerank is an _additional_ signal
  the scoring pipeline can weigh; the existing RRF + importance + temporal terms
  remain.

---

## Decision

### Ownership and resolution

**Ownership**: ADR-042 owns:

- The `Reranker` trait (and cross-encoder / bi-encoder / pure-math variants)
- Rerank-stage configuration (`RerankConfig`)
- lattice-inference integration for local rerank

ADR-030 provides retrieval engines and low-level fusion primitives; it does NOT define
reranker traits or rerank weights. Those belong here.

**Resolution**: the rerank stage resolves `consumer_kind="rerank"` against the brain
profile binding chain (ADR-032 §10), unless the upstream recall profile pins a
`rerank_profile_id`. See ADR-033 §8.2 for the full recall→rerank profile resolution
precedence.

**Score shape**: `RerankedHit` carries two score fields:

- `rerank_scores: HashMap<&'static str, f32>` — per-reranker score keyed by reranker
  name (e.g., `"cross_encoder"`, `"salience"`, `"graph_proximity"`). Used by
  `recall.score` via `RecallConfig.reranker_weights` and available for audit.
- `final_score: f32` — weighted combination of per-reranker scores computed by the
  rerank stage for ordering purposes. `recall.score` may further blend this with the
  RRF + importance + temporal terms.

Both fields are always present after the rerank stage runs. Downstream stages use
`final_score` for ordering and `rerank_scores` for per-reranker audit/debug.

### 1. New stage: `recall.rerank` between `recall.fuse` and `recall.score`

ADR-033 §2 defines five pipeline stages. This ADR inserts a sixth between fuse and
score:

```text
recall(query, namespace, limit, config?):
  recall.embed        →  {embeddings: [{engine_id, model_id, vector}]}     (ADR-033, ADR-031)
  recall.candidates   →  {text_hits, vector_hits_by_engine}                 (ADR-033, ADR-031)
  recall.fuse         →  {fused_hits}                                       (ADR-033)
  recall.rerank       →  {reranked: [{id, rerank_scores: HashMap<&'static str, f32>, final_score: f32}]}  (this ADR)
  recall.score        →  {scored: [{id, score, breakdown}]}                 (ADR-033)
  selector            →  top-K under budget                                 (ADR-033)
```

The rerank stage is a new memory-pack-owned handler:

| Handler                | Visibility | Input                                               | Output                                                                                                |
| ---------------------- | ---------- | --------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `memory.recall_rerank` | Internal   | `{query, fused_hits, hook?: profile_id, model_id?}` | `{reranked: [{id, rerank_scores: HashMap<&'static str, f32>, final_score: f32}], hook_applied: bool}` |

The `hook` parameter is an optional profile id — when provided, the handler resolves
the profile and passes its `LoraHook` to the rerank forward (§4 below). When omitted,
the rerank runs with `NoopLoraHook`.

`memory.recall_rerank` is `Internal` per ADR-023 §2 visibility rules — agents don't
call it directly. The orchestrating `memory.recall` handler invokes it when the
deployment has a rerank model configured.

### 2. Model selection

Rerank model choice is a deployment configuration parameter, not a hardcoded constant.
`RuntimeConfig` gains:

```rust
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
/// responsibility per ADR-031:342-348. The reranker receives pre-computed
/// embeddings if cross-encoder reranking needs them.
pub struct RerankConfig {
    pub rerankers: Vec<RerankerDef>,
    pub reranker_weights: HashMap<String, f64>,
    pub top_k: usize,
}
```

Default `None`. Deployments opt in by setting `KHIVE_RERANK_MODEL_ID` env var or the
config field. v1 sentinel default model: a Qwen3-class small model (~0.8B) loaded via
`lattice-inference`. The exact model is chosen by the deployment — lattice ships
multiple Qwen3 variants and supports BERT-class cross-encoders too. khive does not
prescribe a single model; it prescribes the loading path (`lattice-tune::registry`)
and the inference path (`lattice-inference::forward`).

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

When the recall pipeline reaches `recall.rerank`, brain (if loaded) resolves a
profile via `brain.resolve(actor, namespace, consumer_kind="rerank")`. The resolved
profile's `inference_hook` (ADR-032 §2) returns `Option<Box<dyn LoraHook>>`:

- **No brain loaded**: rerank runs with `NoopLoraHook`.
- **Brain loaded, no profile bound for this context**: same — `NoopLoraHook`.
- **Profile bound, non-LoRA state class**: `profile.inference_hook` is `None` for
  Bayesian, Trajectory, etc. classes — `NoopLoraHook`.
- **Profile bound, LoRA class, target_model_id mismatch**: drop the hook with a debug
  log; rerank with `NoopLoraHook`. The adapter was trained for a different rerank
  model — applying it would produce nonsense (ADR-032 §5b).
- **Profile bound, LoRA class, target_model_id matches**: pass `state.as_hook()`
  to the rerank forward. The adapter modifies the forward pass per layer per module
  via lattice ADR-008's hook.

The hook is read from brain's per-context `ArcSwap<Box<dyn LoraHook>>` (ADR-032 §5b).
The cost when no adapter is bound: a `Box::new(NoopLoraHook)` with `#[inline(always)]`
empty body — eliminated by the compiler at the forward-pass call sites.

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
```

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

### 7. Reranker trait — the composability surface

§§1–6 specified the cross-encoder reranker. v1 ships three rerankers behind a
single trait so the recall pipeline composes them by configuration, not code
change:

```rust
pub trait Reranker: Send + Sync {
    /// Stable name used in RecallConfig (e.g., "cross_encoder", "salience",
    /// "graph_proximity"). MUST be unique across registered rerankers.
    fn name(&self) -> &'static str;

    /// Score N (query, candidate) pairs. Returns one f32 per candidate in input order.
    /// Pure rerankers (salience, graph_proximity) ignore `query` and `hook`.
    async fn score_batch(
        &self,
        query:      &str,
        candidates: &[RerankCandidate<'_>],
        ctx:        &RerankContext<'_>,
    ) -> Result<Vec<f32>, RerankerError>;
}

pub struct RerankContext<'a> {
    pub namespace:  &'a str,
    pub hook:       Option<&'a dyn LoraHook>,    // None for pure-math rerankers
    pub config:     &'a RerankerConfig,
    // future: deadline, tracing span
}
```

Each Reranker owns its own latency budget; the cross-encoder's ≤50ms/≤200ms (§3)
is one Reranker's policy. Pure-math rerankers (salience, graph-proximity) are
microsecond-scale and impose no budget.

#### v1 built-in rerankers

| Name              | Implementation                                                                                                                                                                                                                 | Adapter?          | Source                          |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------- | ------------------------------- |
| `cross_encoder`   | Calls `lattice_inference::rerank(model_id, inputs, hook)` per §1                                                                                                                                                               | Yes (LoRA via §4) | §§1–6 of this ADR               |
| `salience`        | `score(c) = α + (1 - α) * c.salience` with α default `0.5` (matches old khive ADR-024 salience-weighted rerank). Pure math, no IO.                                                                                             | No                | New in v1                       |
| `graph_proximity` | `score(c) = base * decay^min_hops(c.entity, anchor_entities)` with `decay` default `0.7`. `anchor_entities` come from `RerankContext.config.anchors` (caller-specified). Reads the edge table via `khive-storage::GraphStore`. | No                | Restored from old khive ADR-061 |

`salience` and `graph_proximity` are pure-math rerankers — no model, no inference,
no adapter. They serve as cheap signal that brain can learn to weight up or down
per profile. The cross-encoder is the heavy-but-precise rerankerin this lineup; the
others are cheap-but-shallow.

#### Stage execution

`recall.rerank` runs ALL configured rerankers in parallel (since they are
independent), collects their per-candidate scores, and writes them to the
candidate as `rerank_scores: HashMap<&'static str, f32>` (keyed by reranker name).
The score stage (`recall.score`) consumes this map via per-reranker weights in
`RecallConfig.reranker_weights: HashMap<String, f64>`.

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

### Why insert rerank between fuse and score (not replace score)

`recall.score` applies the v1 weighted formula over `(rrf_score, importance, temporal)`.
Rerank produces an additional signal — `rerank_score` — that the scoring pipeline can
weight alongside the existing terms. Replacing scoring entirely would discard the
working importance and temporal logic; inserting rerank as a stage adds signal without
removing any.

Concretely, ADR-033's `RecallConfig` gains `reranker_weights: HashMap<String, f64>` (default
`{}`, so behavior is unchanged when no reranker is configured). With one or more rerankers
enabled, the formula becomes:

```text
score = relevance_weight * rrf_score
      + importance_weight * effective_importance
      + temporal_weight * temporal
      + Σᵢ reranker_weights[i] * reranker_score[i]
```

The weighted composition is the standard `WeightedObjective` extension (ADR-024).

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

- `crates/khive-types/src/event.rs`: add `EventKind::RerankExecuted`.
- `crates/khive-runtime/src/observations.rs`: add per-kind decoder for
  `RerankExecuted` per ADR-041 (Candidate inputs + Selected outputs).

### Tests

| Scenario                                                                    | Assert                                                                                           |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `rerank_model_id = None`                                                    | Recall pipeline skips rerank stage; results identical to pre-ADR.                                |
| `rerank_model_id = Some(...)`, no brain                                     | Rerank runs with NoopLoraHook; `hook_applied = false`.                                           |
| Brain bound, non-LoRA profile                                               | Rerank runs with NoopLoraHook; event records `served_by_profile_id`.                             |
| Brain bound, LoRA profile, matching target                                  | Rerank runs with adapter hook; event records `hook_applied=true, hook_target_match=true`.        |
| Brain bound, LoRA profile, mismatched target                                | Rerank runs with NoopLoraHook; event records `hook_applied=false, hook_target_match=false`.      |
| Rerank latency > 500ms                                                      | Warning event emitted; rerank still applied for the call.                                        |
| 5 latency warnings in 5min                                                  | Rerank disabled for process lifetime; subsequent calls log "rerank disabled by SLO".             |
| `lattice-tune/inference-hook` disabled at compile + LoRA profile registered | Boot fails with feature-flag error.                                                              |
| Rerank output ordering                                                      | Top-K reranked items appear in `Selected` rows of `RerankExecuted` event with correct positions. |

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
