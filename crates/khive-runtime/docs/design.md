# khive-runtime Design

## ADR Compliance

### ADR-002: Edge Ontology

- 15 closed edge relations; endpoint contract enforced at the runtime layer in `operations.rs`
- Symmetric relations (`competes_with`, `composed_with`) are stored with `source_uuid < target_uuid`
- `annotates` is the only cross-substrate relation: source must be a note, target may be anything
- All 13 base relations require entity→entity; notes cannot be source/target except via `annotates`
- `supersedes` is same-substrate only: entity→entity or note→note, never cross-substrate
- `dependency_kind` metadata key is only valid on `depends_on` edges
- Pack-declared edge endpoint rules are additive only; packs cannot tighten the base contract

### ADR-004 / ADR-005: Event and Storage Capability Traits

- `EventStore::append` is called after each authorized dispatch to record audit events
- The audit payload field holds the full `AuditEvent` envelope (not a bare verb result)
- Top-level event fields follow the ADR-004/ADR-005 schema

### ADR-007: Namespace Strategy (Rev 6)

- Namespace is attribution and gate-policy input, not a storage partition; it is not a
  by-ID access control boundary
- By-ID operations (get, delete, update) resolve globally unique UUIDs directly — no
  `record.namespace == caller_namespace` check at the runtime layer (rule 2; see
  `operations.rs::get_entity`)
- `merge_entity` is the one by-ID operation that still requires a namespace match on
  both sides (it is a same-namespace curation operation, not a generic lookup); it
  rejects the merge when a record's namespace differs from the caller's token namespace
- `actor.id` in config must be a valid namespace string; an invalid value is a startup error
- `NamespaceToken` carries dispatch attribution and the visible-namespace read/write
  scope produced at the gate boundary; it is not a by-ID access guard (historical:
  earlier ADR-007 revisions described it as the storage trust boundary — superseded)

### ADR-009 / ADR-028: Multi-Backend Deployment

- `BackendId` identifies a named backend in multi-backend deployments; single-backend uses `"main"`
- `KhiveRuntime::from_backend` is the preferred boot path for multi-backend deployments
- Cross-backend `merge_entity` is unsupported in v1; both entities must reside on the same backend
- `db_path` and `embedding_model` on `RuntimeConfig` are deprecated in favour of the external-backend path

### ADR-010: KG Versioning / Portability

- Export format is `"khive-kg"` version `"0.1"` (stable identifier for archive parsers)
- Embeddings are excluded from archives (regenerable from text + model)
- Edges are collected by source entity, not by namespace scan, to capture cross-entity relationships
- `edge_id` field on `ExportedEdge` is stable across export/import cycles; old archives without it receive a fresh UUID on import

### ADR-013 / ADR-024: Note Kinds and Annotation

- `annotates` edges targeting a note are validated before any write (atomicity)
- `annotates` targets can be entity, note, edge, or event (cross-substrate by design)
- Note delete cascades annotation edges targeting that note

### ADR-014: Curation Operations

- `merge_entity` enforces same-kind constraint at the runtime layer, not storage
- Namespace isolation is enforced during merge: only records in the caller namespace can be merged
- Symmetric relations are canonicalized (source_uuid < target_uuid) before merge conflict checks
- Soft-delete preserves existing edges; queries filter by `deleted_at IS NULL`
- Entity tombstone records preserve provenance for audit

### ADR-015: Schema Migrations

- Core substrate tables evolve through versioned migrations; pack-auxiliary tables are separate
- Migrations are idempotent; already-applied versions are skipped at runtime startup

### ADR-017: Pack Standard

- Pack verb names in `Visibility::Verb` participate in cross-pack collision detection at boot
- `Visibility::Subhandler` entries are excluded from collision checks and not callable via MCP
- Boot-time collision: two packs declaring the same public verb name produce `RuntimeError::VerbCollision`
- Pack-auxiliary schema plans are collected from all registered packs and applied at startup

### ADR-018: Authorization Gate

- Gate is consulted before every verb dispatch; gate infrastructure failures are fail-open
- `GateDecision::Deny` is hard enforcement: the pack is never invoked on denial
- Namespace token is minted at the dispatch boundary after gate approval
- `namespace` is stripped from params before forwarding to pack handlers
- `VerbRegistry` emits one `gate.check` info trace event per dispatch for observability
- Obligations on `GateDecision::Allow` are serialized as an empty array when there are none

### ADR-002 / ADR-019: Note and Edge Operations

- Three-case relation contract for link operations: annotates, supersedes, and entity→entity base rules
  (ADR-002: Edge Ontology governs the endpoint contract; ADR-019: GTD Pack extends it for task notes).
- The endpoint validation path is centralized in `operations.rs` so both `link` and `update_edge` share the same contract.

### ADR-021: Memory Pack

- Memory decay formula: `effective_salience = salience * exp(-decay_factor * age_days)`
- Default decay rate: 0.01 (~69-day half-life)
- Per-note `decay_factor` is used by `DecayAwareSalienceObjective` rather than the objective's own rate

### ADR-023: Declarative Pack Format

- Verb surface and visibility are declared per-pack; only `Visibility::Verb` entries appear in `help=true` envelopes
- `all_verbs` returns only public verb entries; internal subhandlers require `all_handlers_with_names`

### ADR-025: Pack Dispatch Trait

- `PackRuntime::dispatch` is the async per-verb entry point for each pack
- Packs that do not use an embedder registry may ignore the `register_embedders` hook

### ADR-027: Dynamic Pack Loading

- Pack factories are discovered via `inventory` at link time; missing dependencies are a boot error
- Missing dependencies are not silently auto-added; the requested set must be explicit
- `PackRegistry` performs topological sort of packs using Kahn's algorithm

### ADR-029: Gate Authorization

- `RuntimeConfig::gate` defaults to `AllowAllGate`; production deployments plug in a policy-backed impl

### ADR-030: Layered Retrieval Architecture

- `KindHook` provides per-kind specialization for shared CRUD operations
- The retrieval pipeline composes signal objectives without IO; the runtime layer materialises signal data

### ADR-031: Pack-Extensible Embedder Registry

- Pack-declared embedder providers are registered via `PackRuntime::register_embedders`
- Pack-extensible edge endpoint rules are shared across clones via `Arc<RwLock<_>>`
- Base ADR-002 rules apply independently; pack rules are additive
- `KhiveRuntime::install_edge_rules` is called once by the transport after `VerbRegistry` is built

### ADR-033: Recall Pipeline

- `NoteCandidate` carries pre-computed signals; objectives are pure functions with no IO
- `MemoryRecallPipeline::default()` uses the ADR-021 default decay parameters
- `AmplifiedDecayAwareSalienceObjective` is used when salience should drive ranking more aggressively

### ADR-034: KG Validation Pipelines

- `ValidationRule` carries a `check: RuleFn` and optional `fix: FixFn`
- Severity can be overridden per-rule from `.khive/kg/rules.toml`
- `GraphPatch` is a deferred stub; the auto-fix write path is not yet implemented
- Violations are grouped by rule ID and sorted canonically

### ADR-037: Inter-Pack Dependencies

- Missing pack dependencies are collected and reported as a single `MissingPackDependencies` error
- Circular dependencies are detected during topological sort and reported as `CircularPackDependency`
- Remote resolution errors (`UnknownRemote`, `RemoteCacheMissing`) are part of the same error family

### ADR-049: ANN Warmup

- `KhiveRuntime::warm_ann_index` is intended to run once at startup as a background task.
  The warm-start protocol is owned by the daemon (ADR-049: khived daemon); the runtime
  exposes the `warm_ann_index` hook for the daemon to invoke during startup.
- Warm startup sequence follows steps 2–4 from the ANN warmup spec.

### ADR-045: Verb Response Presentation

- `micros_to_iso` is the single conversion point from internal `i64` microsecond timestamps to ISO-8601
- `Agent` mode: short UUIDs (8-char), relative timestamps within 24h, lifecycle nulls preserved, scores truncated to 3 sig-figs
- `Human` mode at the MCP layer is identical to `Verbose`; terminal formatting is applied by the CLI layer
- `full_id` is explicitly excluded from UUID shortening in Agent mode to preserve chaining handles

### ADR-020: Stable Edge Identity

- `ExportedEdge::edge_id` carries the stable `LinkId` UUID across export/import cycles,
  as specified in ADR-020 (Git-Native KG Implementation) §edge_id.
- Old archives (pre-0.2) omit `edge_id`; `serde(default)` assigns a fresh UUID on import.

### ADR-049: Persistent Daemon

- `khived` is a persistent warm runtime over a Unix socket
- `PackRuntime::warm` is invoked on every registered pack during daemon startup

### ADR-050: Namespace Token Contract

- `NamespaceToken` is sealed to prevent external construction without gate authorization
- Namespace authority governs which namespace(s) a dispatch can read/write (minted at
  the gate boundary); it is not consulted again per-record on by-ID operations (ADR-007
  Rev 6), except `merge_entity`/`merge_note`, which still require a namespace match

## Consistency Notes

- `validation.rs` line 112 references `ADR-020` (git-native write path) in relation to `GraphPatch`. The git-native write path is out of scope for the v0.2 validation cluster; this is accurate documentation of a deferred feature, not a discrepancy.
- `RuntimeConfig::db_path` and `RuntimeConfig::embedding_model` are documented as deprecated (in favour of `from_backend` and `EmbedderRegistry`) but remain in place for backward compatibility with tests and single-binary deployments. They should be removed when all callers migrate to the external-backend boot path.
- `PackRuntime::register_embedders` hook docs reference "ADR-031 extension" — this is the pack-extensible embedder hook added alongside ADR-031, not a separate ADR. The name is stable.
