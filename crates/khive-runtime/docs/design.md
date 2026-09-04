# khive-runtime Design

## ADR Compliance

### Edge Ontology (ADR-002)

- 17 closed edge relations (15 base relations plus 2 epistemic relations added by ADR-055);
  endpoint contract enforced at the runtime layer in `operations.rs`
- Symmetric relations (`competes_with`, `composed_with`) are stored with `source_uuid < target_uuid`
- `annotates` is the only cross-substrate relation: source must be a note, target may be anything
- All base relations other than `annotates` require entity→entity; notes cannot be source/target
  except via `annotates`
- `supersedes` is same-substrate only: entity→entity or note→note, never cross-substrate
- `dependency_kind` metadata key is only valid on `depends_on` edges
- Pack-declared edge endpoint rules are additive only; packs cannot tighten the base contract

### Event and Storage Capability Traits (ADR-004, ADR-005)

- `EventStore::append` is called after each authorized dispatch to record audit events
- The audit payload field holds the full `AuditEvent` envelope (not a bare verb result)
- Top-level event fields follow the ADR-004/ADR-005 schema

### Shared Blob Hydration (ADR-160 D3)

- Each installed `BlobStore` is paired with one immutable, runtime-owned
  `Arc<BlobHydrator>`; default, core, and pack runtime handles sharing that
  store receive the same hydrator and therefore one aggregate byte budget
- Admission reserves the caller's declared whole-object maximum before backend
  I/O. `[runtime] blob_hydration_bytes` defaults to 256 MiB and startup rejects
  values below the portable 64 MiB object envelope
- `VerifiedBlob` exposes borrowed bytes and retains its weighted lease until
  drop; it has no clone or owned-byte extraction API
- A tracked supervisor owns admitted backend work. Request cancellation drops
  only the waiter, while capacity remains charged until native work ends; daemon
  drain observes the supervisor through ADR-119 background-task accounting
- Raw store access is limited to mutation, stat, existence, delete, and
  maintenance. Production whole-buffer reads route through `BlobHydrator`; the
  unbounded raw read surface was removed in ADR-160 Phase 3

### Role-Keyed Attachments and V21 Cutover (ADR-121, ADR-160 D4)

- Phase 4a separately ships the transactional-GC compatibility gate without
  changing V20 schema/data. Phase-4b runtime builders may cut over only after
  that build converges fleet-wide and every pre-Phase-4a process sharing the
  database/blob root is drained. Every Phase-4a application reader/writer must
  also be quiesced or unable to access the database during cutover; only a
  GC-only worker has narrow completed-V21 compatibility. Phase-4b serving
  starts after exact-current topology validation
- `Entity.content_ref` is a compatibility read projection of attachment role
  `content`; entity writes do not persist a same-named column
- `create_entity_with_attachments` verifies every referenced blob, then commits
  the entity and all initial roles in one backend transaction
- `KhiveRuntime::attachments()` is accepted only on backend `main`. A pack bound
  to a secondary backend routes record-plus-attachment work through `core()`
- Hard entity/note deletion removes attachment rows in the same transaction;
  soft deletion retains them as recoverable liveness anchors
- Direct `KhiveRuntime::new` may finish an empty V21 migration, but refuses a
  legacy application-assisted cutover. Production hosts use
  `from_prepared_backend` only after the async boot coordinator has authenticated
  legacy pack evidence and completed V21

### Namespace Strategy (Rev 6) (ADR-007)

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

### Multi-Backend Deployment (ADR-009, ADR-028)

- `BackendId` identifies a named backend in multi-backend deployments; single-backend uses `"main"`
- Official multi-backend host boot coordinates schema/V21 first, then uses the
  low-level backend assembly seam; unchecked `from_backend` is not itself a
  migration or serving entrypoint
- The host prepares and inventories every secondary before enabling V21 on
  main; any secondary attachment liveness blocks boot because main is the sole
  SQL authority visible to transactional blob GC
- Cross-backend `merge_entity` is unsupported in v1; both entities must reside on the same backend
- `RuntimeConfig::db_path` remains the database input to the supported async
  single-backend host builder. `embedding_model` remains the compatibility
  primary-model shorthand beside `EmbedderRegistry`; neither field makes direct
  `from_backend` assembly a supported production boot path

### KG Versioning / Portability (ADR-010)

- Export format is `"khive-kg"` version `"0.1"` (stable identifier for archive parsers)
- Embeddings are excluded from archives (regenerable from text + model)
- Edges are collected by source entity, not by namespace scan, to capture cross-entity relationships
- `edge_id` field on `ExportedEdge` is stable across export/import cycles; old archives without it receive a fresh UUID on import
- `ExportedEdge::properties` round-trips storage `metadata`, and its independent creation/update
  timestamps are preserved; legacy archives missing timestamps receive import-time defaults

### Note Kinds and Annotation (ADR-013, ADR-024)

- `annotates` edges targeting a note are validated before any write (atomicity)
- `annotates` targets can be entity, note, edge, or event (cross-substrate by design)
- Note delete cascades annotation edges targeting that note

### ADR-014: Curation Operations

- `merge_entity` enforces same-kind constraint at the runtime layer, not storage
- Merge is a same-namespace curation constraint: only records in the caller namespace can be
  merged (this is the one by-ID operation with a namespace check — see ADR-007 below; it is
  not a general by-ID access rule)
- Symmetric relations are canonicalized (source_uuid < target_uuid) before merge conflict checks
- A merge edge-rewire collision keeps the existing natural-key row and records a complete
  preimage of the dropped edge in both `MergeSummary` and the merge audit event
- Dropping a conflicting edge applies the hard-edge-delete cascade recursively; incident
  annotation edges are removed rather than left dangling, and their preimages are nested under
  the conflict preimage so the complete destructive step can be restored
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

- Gate is consulted before every verb dispatch; gate infrastructure failures are audited and
  fail closed with `RuntimeError::GateUnavailable`
- `GateDecision::Deny` is hard enforcement: the pack is never invoked on denial
- Namespace token is minted at the dispatch boundary after gate approval
- `namespace` is stripped from params before forwarding to pack handlers
- `VerbRegistry` emits one `gate.check` info trace event per dispatch for observability
- Obligations on `GateDecision::Allow` are serialized as an empty array when there are none

### Note and Edge Operations (ADR-002, ADR-019)

- Three-case relation contract for link operations: annotates, supersedes, and entity→entity base rules
  (ADR-002 governs the endpoint contract; ADR-019 extends it for task notes).
- The endpoint validation path is centralized in `operations.rs` so both `link` and `update_edge` share the same contract.

### ADR-021: Memory Pack

- Memory decay formula: `effective_salience = salience * exp(-decay_factor * age_days)`
- Default decay rate: 0.01 (~69-day half-life)
- The active memory pipeline applies each note's `decay_factor` before
  `AmplifiedDecayAwareSalienceObjective`; the standalone `DecayAwareSalienceObjective` applies
  its constructor-supplied fixed rate instead

### Declarative Pack Format (ADR-023)

- Verb surface and visibility are declared per-pack; only `Visibility::Verb` entries appear in `help=true` envelopes
- `all_verbs` returns only public verb entries; internal subhandlers require `all_handlers_with_names`

### Pack Dispatch Trait (ADR-025)

- `PackRuntime::dispatch` is the async per-verb entry point for each pack
- Packs that do not use an embedder registry may ignore the `register_embedders` hook

### Dynamic Pack Loading (ADR-027)

- Pack factories are discovered via `inventory` at link time; missing dependencies are a boot error
- Missing dependencies are not silently auto-added; the requested set must be explicit
- A factory whose pack has no public verbs is a boot error unless it explicitly returns `true`
  from `PackFactory::intentionally_verbless`
- `PackRegistry` performs topological sort of packs using Kahn's algorithm
- Startup selection precedence is `--pack` → `KHIVE_PACKS` → `[runtime].packs` → the built-in
  production set; `[packs.<name>]` assigns backends and does not select packs for loading

### Gate Authorization (ADR-029)

- `RuntimeConfig::gate` defaults to `AllowAllGate`; production deployments plug in a policy-backed impl
- An optional operator `[gate]` table installs `CallerEnrollmentGate`: exact
  resolved actor ids come from `granted_actors`, while `grant_unattributed`
  independently governs the anonymous/local caller; an explicit empty table
  denies all and unknown table keys fail startup
- This is a live static policy, not ADR-143's still-unimplemented store-held
  grant and one-time-import model

### Layered Retrieval Architecture (ADR-030)

- `KindHook` provides per-kind specialization for shared CRUD operations
- The retrieval pipeline composes signal objectives without IO; the runtime layer materialises signal data

### Pack-Extensible Embedder Registry (ADR-031)

- Pack-declared embedder providers are registered via `PackRuntime::register_embedders`
- Pack-extensible edge endpoint rules are shared across clones via `Arc<RwLock<_>>`
- Base ADR-002 rules apply independently; pack rules are additive
- `KhiveRuntime::install_edge_rules` is called once by the transport after `VerbRegistry` is built

### Recall Pipeline (ADR-033)

- `NoteCandidate` carries pre-computed signals; objectives are pure functions with no IO
- `MemoryRecallPipeline::default()` uses the ADR-021 default decay parameters
- `AmplifiedDecayAwareSalienceObjective` is used when salience should drive ranking more aggressively

### ADR-034: KG Validation Pipelines

- `ValidationRule` carries a `check: RuleFn` and optional `fix: FixFn`
- Severity can be overridden per-rule from `.khive/kg/rules.toml`
- `GraphPatch` is a deferred stub; the auto-fix write path is not yet implemented
- Violations are grouped by rule ID and sorted canonically

### Inter-Pack Dependencies (ADR-037)

- Missing pack dependencies are collected and reported as a single `MissingPackDependencies` error
- Circular dependencies are detected during topological sort and reported as `CircularPackDependency`
- Remote resolution errors (`UnknownRemote`, `RemoteCacheMissing`) are part of the same error family

### ANN Warmup (ADR-049)

- `KhiveRuntime::warm_ann_index` is intended to run once at startup as a background task.
  The warm-start protocol is owned by the daemon (ADR-049); the runtime
  exposes the `warm_ann_index` hook for the daemon to invoke during startup.
- Warm startup sequence follows steps 2–4 from the ANN warmup spec.

### Verb Response Presentation (ADR-045)

- `micros_to_iso` is the single conversion point from internal `i64` microsecond timestamps to ISO-8601
- `Agent` mode: short UUIDs (8-char) except strict round-trip fields, relative timestamps within 24h, lifecycle nulls preserved, scores truncated to 3 sig-figs
- `Human` mode at the MCP layer is identical to `Verbose`; terminal formatting is applied by the CLI layer
- `full_id`, `context_entity_id`, `thread_id`, `outbound_ref`, `parent_id`, `session_id`, and `project_id` are explicitly excluded from UUID shortening in Agent mode to preserve strict chaining, correlation, ancestry, filtering, and provenance handles
- `memory.feedback` and `comm.delivered` are `AlwaysVerbose` because their generic `target_id` / `id` fields are exact strict-verb inputs

### Stable Edge Identity (ADR-020)

- `ExportedEdge::edge_id` carries the stable `LinkId` UUID across export/import cycles,
  as specified in ADR-020 (Git-Native KG Implementation) §edge_id.
- `ExportedEdge::properties` maps to `Edge::metadata`; `created_at` and `updated_at` are
  exported and imported independently rather than being regenerated.
- Old archives (pre-0.2) omit `edge_id`; `serde(default)` assigns a fresh UUID on import.

### Persistent Daemon (ADR-049)

- `khived` is a persistent warm runtime over a Unix socket
- `PackRuntime::warm` is invoked on every registered pack during daemon startup;
  each hook follows its own assigned backend mode, suppressing writer-bearing
  work for read-only snapshot runtimes (ADR-028 A2)

### Namespace Token Contract (ADR-050)

- `NamespaceToken` is sealed to prevent external construction without gate authorization
- Namespace authority governs which namespace(s) a dispatch can read/write (minted at
  the gate boundary); it is not consulted again per-record on by-ID operations (ADR-007
  Rev 6), except `merge_entity`/`merge_note`, which still require a namespace match

## Consistency Notes

- `validation.rs` line 112 references `ADR-020` (git-native write path) in relation to `GraphPatch`. The git-native write path is out of scope for the v0.2 validation cluster; this is accurate documentation of a deferred feature, not a discrepancy.
- `RuntimeConfig::db_path` remains the database input to the supported async khive-mcp/kkernel host builders and to current-schema tests. Direct `from_backend` assembly is a low-level, already-prepared-backend seam; production boot coordinates V21 first and may use `from_prepared_backend`. `embedding_model` remains the primary-model shorthand beside `EmbedderRegistry`.
- `PackRuntime::register_embedders` hook docs reference "ADR-031 extension" — this is the pack-extensible embedder hook added alongside ADR-031, not a separate ADR. The name is stable.
