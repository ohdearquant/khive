# khive-pack-memory Design

## ADR Compliance

### ADR-021: Memory Pack (remember / recall verbs)

- Implements `memory.remember` (commissive — creates a memory note with salience and decay)
  and `memory.recall` (assertive — retrieves memory notes via decay-aware hybrid ranking).
- Registers the `memory` note kind; depends on the `kg` pack.
- Decay formula (exponential model):
  $$\text{effective\_salience} = \text{salience} \times e^{-\text{decay\_factor} \times \text{age\_days}}$$
  The note's own `decay_factor` field controls the rate; `temporal_half_life_days` is used only
  by the independent temporal recency score, not by salience decay.
- Default `decay_factor = 0.01` gives a ~69-day half-life: $e^{-0.01 \times 69.3} \approx 0.5$.
- `memory_type` property is always written (default `"episodic"`); only `"episodic"` and
  `"semantic"` are accepted — other values are rejected at validation time.
- `decay_factor` must be finite and `>= 0`; no upper clamp is required.
- Final composite score must be in `[0, 1]`.

### ADR-025: Illocutionary Verb Classification

- `memory.remember` — Commissive: commits the caller to a persistent change in the namespace.
- `memory.recall` — Assertive: retrieves and presents the current state of affairs.
- Sub-handlers (`recall_embed`, `recall_candidates`, `recall_fuse`, `recall_rerank`, `recall_score`)
  are all Assertive.

### ADR-027: Inventory Self-Registration

- `MemoryPackFactory` is submitted via `inventory::submit!` so the pack is auto-discovered at
  startup without requiring explicit registration at the call site.
- The factory declares `requires = ["kg"]` so the runtime enforces dependency ordering.

### ADR-032: Brain-Tunable Parameters

- `MemoryPack` implements `PackTunable` so the brain pack can adjust recall scoring weights
  based on observed usage patterns.
- The three parameters (`memory::relevance_weight`, `memory::salience_weight`,
  `memory::temporal_weight`) correspond to the three Beta posteriors in `BalancedRecallState`.
  Posterior means flow directly into `RecallConfig`.

### ADR-033: Recall Reranking and Weighted Fusion

- A rerank stage sits between fusion and final scoring in the recall pipeline.
- When `reranker_weights` is non-empty in `RecallConfig`, weighted reranking replaces the
  default archive score for each candidate.
- For `"weighted"` fusion strategy requested via the API: weight values always come from pack
  config (`RecallConfig.reranker_weights`), not from the incoming request body.
- Sub-handler `memory.recall_rerank` exposes the rerank stage as a dotted sub-verb.

### ADR-043: CJK Routing for Multilingual Embedding

- When the recall query is primarily CJK text and a multilingual embedding model is registered,
  that model is preferred over other registered models.
- CJK routing only activates when a multilingual model is actually present; detection falls
  back to all registered models when none is found, ensuring CJK queries still return results.
- The model preference is configured via `ScoringConfig.cjk_model` or by matching registered
  model names against known multilingual substrings.

### ADR-002: Edge Ontology — Supersedes Suppression

- Memory recall suppresses candidates that have an inbound `supersedes` edge (i.e., the memory
  has been explicitly superseded by a newer one).
- This uses the same graph-edge mechanism as `search_notes` in the runtime: agents create
  supersession via `link(source=new_note, target=old_note, relation="supersedes")`.
- A property shortcut (`superseded_by` in `properties`) provides a secondary suppression path
  for archive-import compatibility when graph edges are not available.

### ADR-023: Dotted Sub-Handler Naming

- Recall pipeline stages are exposed as sub-handlers using dotted names:
  `memory.recall_embed`, `memory.recall_candidates`, `memory.recall_fuse`,
  `memory.recall_rerank`, `memory.recall_score`.
- These have `Visibility::Subhandler` and are not listed in public verb catalogs,
  per ADR-023 (Pack Verb Surface, Visibility, and Composition) §"Handlers vs. verbs".

## Consistency Notes

- `handlers.rs` is intentionally large (~3100 LOC). The full recall pipeline
  (parameter parsing, FTS/ANN retrieval, fusion, reranking, scoring) is kept as a single
  coherent unit because the intermediate types (`NoteCandidate`, `RerankFeatures`,
  `ScoreBreakdown`) are deeply coupled and private. Splitting would require exporting
  dozens of internal types. Refactoring is tracked as a longer-term work item.
- `config.rs` is intentionally large (~1080 LOC). All recall configuration types
  (`RecallConfig`, `DecayModel`, `RecallFtsGatherConfig`, `BrainProfileHint`) are kept
  together because their validation logic is mutually dependent.
- `brain_profile` integration in `RecallConfig` is partially implemented: the configuration
  field is live but the runtime lookup (cross-pack call to `brain.profile`) is not yet wired
  at the handler level. See TODO(#484).
