# khive-pack-memory Design

## ADR Compliance

### Memory Pack (remember / recall verbs) (ADR-021)

- Implements `memory.remember` (commissive — creates a memory note with salience and decay)
  and `memory.recall` (assertive — retrieves memory notes via decay-aware hybrid ranking).
- Registers the `memory` note kind; depends on the `kg` pack.
- Decay formula (exponential model):
  $$\text{effective\_salience} = \text{salience} \times e^{-\text{decay\_factor} \times \text{age\_days}}$$
  The note's own `decay_factor` field controls the rate; `temporal_half_life_days` is used only
  by the independent temporal recency score, not by salience decay.
- Defaults are type-differentiated: `episodic` → `salience=0.3`, `decay_factor=0.02` (~35-day
  half-life); `semantic` → `salience=0.5`, `decay_factor=0.005` (~139-day half-life).
  Explicit caller-supplied values always override the defaults.
- `memory_type` property is always written (default `"episodic"`); only `"episodic"` and
  `"semantic"` are accepted — other values are rejected at validation time.
- `decay_factor` must be finite and `>= 0`; no upper clamp is required.
- Final composite score must be in `[0, 1]`.

### Illocutionary Verb Classification (ADR-025)

- `memory.remember` — Commissive: commits the caller to a persistent change in the namespace.
- `memory.recall` — Assertive: retrieves and presents the current state of affairs.
- Sub-handlers (`recall_embed`, `recall_candidates`, `recall_fuse`, `recall_rerank`, `recall_score`)
  are all Assertive.

### Inventory Self-Registration (ADR-027)

- `MemoryPackFactory` is submitted via `inventory::submit!` so the pack is auto-discovered at
  startup without requiring explicit registration at the call site.
- The factory declares `requires = ["kg"]` so the runtime enforces dependency ordering.

### Brain-Tunable Parameters (ADR-032)

- `MemoryPack` implements `PackTunable` so the brain pack can adjust recall scoring weights
  based on observed usage patterns.
- The three parameters (`memory::relevance_weight`, `memory::salience_weight`,
  `memory::temporal_weight`) correspond to the three Beta posteriors in `BalancedRecallState`.
  Posterior means flow directly into `RecallConfig`.

### Recall Reranking and Weighted Fusion (ADR-033)

- A rerank stage sits between fusion and final scoring in the recall pipeline.
- When `reranker_weights` is non-empty in `RecallConfig`, weighted reranking replaces the
  default archive score for each candidate.
- For `"weighted"` fusion strategy requested via the API: weight values always come from pack
  config (`RecallConfig.reranker_weights`), not from the incoming request body.
- Sub-handler `memory.recall_rerank` exposes the rerank stage as a dotted sub-verb.

### CJK Routing for Multilingual Embedding (ADR-043)

- When the recall query is primarily CJK text and a multilingual embedding model is registered,
  that model is preferred over other registered models.
- CJK routing only activates when a multilingual model is actually present; detection falls
  back to all registered models when none is found, ensuring CJK queries still return results.
- The model preference is configured via `ScoringConfig.multilingual_model` or by matching
  registered model names against known multilingual substrings.

### Edge Ontology — Supersedes Suppression (ADR-002)

- Memory recall suppresses candidates that have an inbound `supersedes` edge (i.e., the memory
  has been explicitly superseded by a newer one).
- This uses the same graph-edge mechanism as `search_notes` in the runtime: agents create
  supersession via `link(source=new_note, target=old_note, relation="supersedes")`.
- A property shortcut (`superseded_by` in `properties`) provides a secondary suppression path
  for archive-import compatibility when graph edges are not available.

### Dotted Sub-Handler Naming (ADR-023)

- Recall pipeline stages are exposed as sub-handlers using dotted names:
  `memory.recall_embed`, `memory.recall_candidates`, `memory.recall_fuse`,
  `memory.recall_rerank`, `memory.recall_score`.
- These have `Visibility::Subhandler` and are not listed in public verb catalogs,
  per ADR-023 (Pack Verb Surface, Visibility, and Composition) §"Handlers vs. verbs".

## Architecture Boundaries

The implementation separates verb concerns into remember, recall, feedback, prune, and dotted
subhandler modules. Shared request types and retrieval coordination remain in `handlers/common.rs`
because both the main recall path and the subhandlers must use identical namespace, fusion, and ANN
fallback rules.

Configuration types stay together in `config.rs`: nested validation must reject an inconsistent
request before retrieval starts. Archive scoring types remain in `scoring.rs`, while the primary
recall pipeline also uses `khive_runtime::MemoryRecallPipeline`; the two models are composed rather
than silently treated as interchangeable.

Serving-profile lookup is live. Recall resolves an explicit profile or actor/namespace binding,
projects its posterior means into request-local weights, applies the bounded per-entity term, and
stamps the same profile ID on results and serve telemetry. A failed optional bound-profile read
degrades to configured defaults; an explicitly named unknown profile is a caller error.

## Technical References

- ANN freshness, snapshots, and rebuild races: `docs/api/ann-lifecycle.md`
- Full request and retrieval flow: `docs/api/recall-pipeline.md`
- Configuration and environment fallbacks: `docs/api/configuration.md`
- Score normalization, entity boosts, and reranking: `docs/api/scoring.md`
- FTS candidate gathering: `docs/api/text-retrieval.md`
- Remember, feedback, prune, and vacuum: `docs/api/memory-lifecycle.md`
- Runtime registration and brain tuning: `docs/api/pack-integration.md`
- Incident-derived guarantees: `docs/recall-reliability.md`
