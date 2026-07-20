# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- The brain pack (`khive-pack-brain`, `brain.*` verbs — profile lifecycle,
  feedback ingestion, resolution/binding) moved to a commercial extension
  distributed separately and is no longer part of this distribution's
  default 11-pack, 70-verb surface. `khive-brain-core` (posterior math types)
  stays public: it remains a runtime dependency of `memory.recall`'s ranking
  hooks, which run plain when no brain-providing pack is registered. This
  repository's git history retains the extracted crate's past commits under
  their original license terms; this change relocates future development and
  does not alter the status of already-published history.
- Moved the transport channel crates (`khive-channel`, `khive-channel-email`,
  `khive-channel-telegram`) and their `channel-email`/`channel-telegram`
  `khive-mcp`/`kkernel` cargo features to a commercial extension; no longer
  part of this repository or the open-source build.
- Moved `khive-pack-knowledge` (the `knowledge.` verb pack: atom/domain corpus, TF-IDF
  and embedding-rerank search, composition, section feedback) to a commercial extension.
  It is no longer part of the open-source distribution or its default pack set. The
  remaining 11 packs are unaffected; `memory.recall` runs standalone (no dependency on
  the knowledge corpus).
- Moved the formal-methods ontology pack (`khive-pack-formal`) and the code-quality
  ontology pack (`khive-pack-code`), along with the `kkernel code-ingest` admin CLI
  subcommand, to a commercial extension. The open-source build no longer registers
  either pack by default.

## [0.5.0] - 2026-07-13

### Added

- BlobStore content-addressed blob capability (ADR-111, #922).
- GQL WHERE operators `CONTAINS`, `STARTS WITH`, `IN`, `IS NOT NULL` (#892).
- ADR-104 Stage C entity-anchored recall candidate extraction (#881).
- `resource.cost_unit` emission on runtime operations (ADR-103 Amendment 1, #927).
- Atomic proposal create plans in `khive-runtime` (#904, #928).
- Pack-declared entity-type subtype composition at boot (ADR-017, #925).
- Vamana ADR-110 Layer A feature-gated parallelism with deterministic serial fallback (#896).
- Request correlation id threaded across daemon frames and audit events (#948, #951).
- Slow-request and timeout logging for `knowledge` pack compose (#915).

### Changed

- Daemon strict mode now fails a request on fallback instead of degrading silently (#947, #949).
- Publish pipeline uses topological crate order with path-only cyclic dev-deps (#901).
- `lattice-embed` and `lattice-fann` dependencies bumped to 0.6.0 (#885).

### Fixed

- `gtd` preserves RFC 3339 due-date fidelity in agent-mode presentation (#956).
- `brain.event_counts` normalizes the actor filter and adds `counts_by_verb` (#943, #944).
- `pack-kg` exposes effective list limits (#894, #930).
- `memory.recall` emits `recall_executed` events (#866, #929).
- FTS5 metacharacters sanitized in `khive-db` query construction (#916, #932).
- Fired schedule reminders deliver to the creating actor's inbox (#897).
- ADR-091 Plank 1 background age sweep over `tx_registry` (#921).
- `memory.recall` bounded by a fail-soft deadline (#919).
- `pack-comm` health and probe honor the injected namespace (#914).
- `pack-schedule` preserves the ISO timezone offset in remind create-response rendering (#911).
- `pack-kg` create help schema includes the `resource` entity kind (#909).
- `pack-git` masking applied uniformly to all external-origin ingest fields (#910).
- Secret gate uses token-boundary trigger matching to stop path-slug false positives (#888).
- Batched `neighbors` results keyed by the requested node (#891).

## [0.4.0] - 2026-07-12

Published to crates.io on 2026-07-12; backfilled here as the tag and changelog entry did
not accompany that publish.

### Added

- Workspace entity kind with `contains` edges to git/gtd/session notes (#873, #874).
- `khive-pack-code` v0 admin-only code-ingest path (ADR-085 Amendment 3, #848).
- Daemon-resident schedule drain tick with missed-event policy (#782).
- `resolve_reference` capability and recently-referenced ring (#762).
- `git.digest` paged ingest with URL clone cache (ADR-088 Amendment 1, #761).
- ADR-104 Stage A/B serve-time profile projection and bounded per-entity posterior term in
  recall scoring (#743, #745).
- Auto-extraction of `entity_names` from the recall query when the caller omits them (#738).
- `brain.event_counts` windowed event-counts read verb (ADR-103 Stage 1, #737).
- Daemon audit `duration_us` and phase telemetry, plus `comm.health` resource self-report
  (ADR-103 Stage 1, #732).
- MCP bridge protocol mismatch self-heal via in-place re-exec (#731).
- `khive-changeset` op-list model and NDJSON-delta codec (ADR-101, #715), with envelope
  `batch_id` and field-scoped update preimage (#725) and a `kg commit` tier-2 primitive (#721).
- Five configurable rule classes for `kg validate` (#712).
- Lifecycle events, checkpoint pressure telemetry, severity ladder, and link-verb audit
  enrichment (#703).
- `khive-pack-git` v0: commit/issue/PR ingestion with provenance edges (ADR-088, #692).
- Store backup tooling (ADR-100, #677) with per-job retention knobs (#684).
- ADR-099 atomic CLI surface for `kkernel exec --ops-file` (B1-B3, #678, #680).
- Single-writer `WriterTask` core with a bounded write queue; all write paths route through it
  (ADR-067 Component A, #670).
- Per-request identity served over one warm daemon registry (ADR-096 Fork 1, #660).
- Read-only `comm.health()` verb with daemon-persisted heartbeat rows (#615).
- ADR-091 Plank 0/2: open-transaction registry, WAL checkpoint instrumentation, and WAL
  TRUNCATE escalation with rate-limited guards (#591, #593).
- `context` verb: entity-anchored graph context in one call (ADR-089, #588).
- Recall serve-time attribution wired into the serve ledger (ADR-081 §5, #583).
- ADR-081 retune-driver substrate: implicit weight, bounded-mass fold gate, serve ledger (#497).
- `khive-pack-session` T1 verb surface (store/list/resume/export) and a ChatGPT export mirror
  source (#411, #525).

### Changed

- `khive-runtime`, `khive-db`, and `kkernel` share canonical/atomic decision-step and DML code
  paths across the ADR-099 B3 series, closing duplication between the two execution modes.
- `begin_tx` retired; session ingest routes through `atomic_unit` (ADR-099 D5, #673).
- Edge-relation error hints now derive from installed pack `EDGE_RULES` (#621).

### Fixed

- FTS5 metacharacters sanitized in recall/search query construction (#880).
- Malformed-policy output masked from Gate deny reasons and audit logs (#853).
- Typed validation errors instead of panics on invalid `khive-quant` train/encode shapes (#854).
- `resource` entity kind accepted in JSON/NDJSON import adapters (#856).
- Every published crate declares `rust-version` (MSRV 1.91.0) (#855).
- Bounded ANN wait in `memory.recall` with lexical fallback (#859).
- Exact-name entity lookup checked before hybrid fallback in `resolve` (#852).
- Substrate node labels (`entity`/`note`) made satisfiable in GQL/SPARQL (#857).
- `pack-git` scratch-cache ENOENT race closed on the macOS flake family (#847).
- `link()`/`link_many()` guarded against concurrent hard-delete (#826).
- LIKE wildcards escaped in entity name-prefix resolution and Vamana snapshot invalidation
  (#834, #824).
- `FeedbackExplicit` signal observation decoded from `target_id` (#831).
- Credentials masked in `pack-git` issue titles and PR notes without dropping content
  (#835, #785).
- Daemon confirms a dead process before killing it, closing a recovery race (#838).
- `comm.probe` cursor made commit-order safe (#827).
- DSL container-nesting depth and input length bounded (#823).
- `gtd` next/tasks push status/assignee/priority filters into SQL (#825).
- Unreachable strong-count checkpoint exit replaced with a watch signal (#822).
- Stale ANN served during rebuild so the recall request path is not blocked (#812).
- Compact hex prefixes normalized before LIKE-scanning hyphenated ids (#816).
- Generation-check on ANN install closes a stale-build race (#815).
- `ensure_clone` refuses unowned cache-key directories (#788).
- IMAP UID cursor progress persisted for the email channel (#784).
- GQL result truncation warns at the 500-row cap (#802).
- Punctuated identifiers split correctly in FTS5 query sanitization (#790).
- RFC 3339 timezone offsets honored in relative-time display (#800).
- OAuth token refresh bounded by a timeout under the cache lock (#787).
- Over-cap commit embeddings truncated in `pack-git` (#789).
- Comm/GTD backlog burn: inbox sender filters, thread cursor pagination, message tags (#757).
- `brain.resolve` defaults the actor from the caller's dispatch identity (#742).
- Exactly-once forwarding, stale-daemon recovery, and cold-boot FTS guard in the daemon (#698).
- Tier-2 actor+namespace bindings resolved on the `brain` feedback path (#699).
- Multi-backend `annotates`→edge resolution, tier-3 config anchor, curation merge SQL
  unification (#695).
- Silent local-dispatch fallback eliminated; config_id topology parity with graduated
  fail-loud behavior (#698).

### Performance

- Shared MCP measurement client for the benchmark program (#865).
- Flagship coverage manifest and validator for benchmark tracking (#862).
- Throwaway readiness socket dropped and unknown-verb listing cached (#647).
- Neighbor queries for `direction=both` halved via a single `UNION ALL` expansion (#648).

## [0.3.0] - 2026-07-01

### Added

- Email channel transport (ADR-056): SMTP/IMAP adapter, app-only OAuth2
  (XOAUTH2) authentication, an outbound delivery loop with `Message-ID` and
  reply-to-actor routing, and an inbound round-trip (greeting, maintainer
  match, reply correlation).
- Session pack `khive-pack-session` (ADR-080): OSS session storage with a live
  daemon mirror of Claude Code sessions and Codex CLI transcript mirroring.
- Brain router seam: feature-gated lattice-fann router (M1), a
  `brain.register_adapter` integrity verb, `build_context_vector` reading live
  posteriors, and `router_state`/`adapter_set` snapshot persistence.
- ANN persistence (ADR-079): persist and warm-load v2 ANN segments so the
  daemon warm window is bounded by load cost rather than a full rebuild; ANN
  warming degrades to FTS-only instead of erroring.
- Output-format axis (ADR-078): `OutputFormat` (`json` / `auto` / `table`) with
  shape-aware rendering, orthogonal to presentation mode.
- Batch `create_many` for bulk entity creation; optional `entity_type` on
  `neighbors` and properties on `traverse`; property/tag filters on note search.
- Pack core-backend accessor (ADR-073) and a `SubstrateCoordinator`
  cross-backend link with federated search (ADR-029 Phase 2).

### Changed

- `kkernel exec` now defaults to `Verbose` presentation per ADR-045 §2 (the
  scripted / operator surface); the MCP `request` tool keeps the `Agent`
  default.
- Subhandler verbs are gated by wire origin rather than globally.
- Traverse performance: a recursive-CTE join-order fix yields a large speedup,
  and graph-traversal queries are batched to remove N+1 lookups.
- Namespace model (ADR-007 Rev 6): attribution-only namespaces, a per-actor
  episodic memory carve-out, and namespace-blind by-ID storage.
- Bumped `lattice-embed` to 0.4.2 and `lattice-fann` to 0.4.2.

### Fixed

- `knowledge`: `compose` reads resolved section posteriors; recall never
  returns a silent empty result while the ANN index is warming; a poisoned
  warming mutex is recovered instead of aborting the server.
- `retrieval`: property/tag predicates are pushed below result truncation.
- `runtime`: char-boundary-safe secret gate (no UTF-8/CJK panic); the
  configured actor is threaded into the gate request.
- `comm`: actor-addressed delivery (ADR-057) fixes cross-actor messaging; an
  anonymous inbox read leak is closed.
- `mcp`: the embedding-env warning fires only when a `[[engines]]` block
  overrides the `KHIVE_EMBEDDING_MODEL` / `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
  pair, not when that pair is the applied fallback.
- Storage hardening: WAL-checkpoint discipline, BM25 poisoned-lock recovery,
  and `expires_at` honored in recall with `memory.prune` / `memory.vacuum`.

### Docs

- Per-crate READMEs, a crate-README template, and a full configuration
  reference.
- Stale `kkernel call` references replaced with `kkernel exec`.
- New and updated ADRs: 067/068 (cloud topology), 069/072 (Subject model), 073,
  074, 075, 076 (relation-set calculability), 078, 079, and 080.

## [0.2.11] - 2026-06-13

### Fixed

- Cross-platform compile: `DaemonRequestFrame` and `compute_config_id` imports
  in `kkernel/src/exec.rs` gated with `#[cfg(unix)]` to match their declaration
  in `khive-runtime`

## [0.2.10] - 2026-06-13

Full crates.io publish (all 29 workspace crates).

### Fixed

- `khive-brain-core` added to publish dependency order — unblocks
  `khive-pack-brain` on crates.io
- All inter-crate version references bumped consistently

## [0.2.9] - 2026-06-11

GitHub release only — crates.io remains at 0.2.8.

### Added

- Write-time secret detection gate — credential plaintext is hard-blocked from
  content-bearing verbs with a masked reason (#76, #83)
- Type-differentiated salience + decay defaults for memory writes: episodic
  0.3/0.02, semantic 0.5/0.005 (#70, #84)
- `knowledge.get` `include_sections` param (#89); draft atoms excluded from
  knowledge search by default with `include_drafts` opt-in (#78, #90)
- `brain_profile` config knob with 3-tier feedback resolution: explicit →
  namespace-bound → global (#52, ADR-035)
- Vendored JSON/JSONL data-leak pre-commit + CI check (#61)
- Reindex progress bars and domain mirror backfill (#19)

### Fixed

- FTS coverage gap: reindex now backfills pre-existing notes (#88) and entities
  (#96) into FTS; new canonical `entity_fts_document` constructor shared by all
  entity FTS write paths
- Embed-intent prefixes wired across all call sites — instruction-tuned
  embedding models receive `query:`/`passage:` correctly (#95)
- Hard delete purges soft-deleted records (#82)
- `kkernel kg validate` enforces closed-taxonomy schema checks (#41)
- khive-merge compiles again and is hardened (#21, #42)
- `kkernel exec` routes through the warm daemon when available (#63, #64);
  ANN warm removed from stdio — daemon owns hot state (#20)
- Nondeterministic HashMap ordering + startup robustness (#45)
- FTS UPDATE triggers narrowed to indexed columns — stops WAL bloat from
  embedding updates (#19)

### Security

- serde boundaries reject non-finite/NaN and invalid values (#49)
- gate-rego: entrypoint trimmed and validated to avoid latent fail-open (#43);
  tracing dependency restored (#66)
- Remote URLs redacted from git clone error messages (#40)
- brain `section_signals` validated; replay rows quarantined (#46)

### Changed

- Schema DDL moved from inline Rust strings to `.sql` files per ADR-015 (#51)
- Workspace dependency discipline; unused deps removed; `#[allow]` REASON form (#53)
- Oversized production files split; long functions extracted (#35, #56)
- Crate-doc shape + rustdoc hygiene pass (#36, #55)
- ADR freshness pass: ADR-019/023/024/030/051 (#48)

## [0.2.0] - 2026-05-22

### Added

- **kkernel binary** — new Rust admin/management CLI (ADR-076). Subcommands:
  `kkernel sync` (build real SQLite DB from NDJSON), `kkernel pack list`,
  `kkernel pack handler <name>` (pack introspection)
- **81-issue sweep** — resolved 77 issues across 12 parallel plays via `/show`
- ADR-065 through ADR-077 (13 new ADRs covering plugin intent routing,
  cross-plugin workflows, marketplace adaptation, note merge, batch conflict
  detection, bulk link creation, remote entity resolution, sync content-hash
  verification, communication/schedule packs, KG swarm self-correction,
  agent-driven PR workflow, kernel/MCP split, binary packaging strategy)
- DispatchHook trait for brain event emission (issue #158)
- PackTunable for MemoryPack with 3 tunable parameters (#159)
- entity_kind and note_kind in search response (#160)
- Properties filter for search verb (#163)
- Neighbor/traverse enrichment with entity name + kind (#162)
- Memory plugin, GTD plan/process skills, KG agent improvements
- CHANGELOG.md, CONTRIBUTING.md, SECURITY.md
- 25+ regression tests across the audit-correction round
- Deno CLI: kg diff, kg log, kg stats, kg doctor commands

### Changed

- **neighbors/traverse response**: `node_id` → `id` on the JSON wire (#148).
  Internal Rust still uses `.node_id`. Legacy `node_id` accepted as input alias.
- FTS5 score normalization: linear rescaling within result set (0.05, 1.0]
  replaces the collapsed `1/(1+|rank|)` formula (#149)
- VCS crate restructured: superseded modules removed per ADR-048, foundational
  primitives (hash, types, error) retained
- CI script runs Deno tests from `cli/` directory (fixes import map resolution)
- Clippy enforced with `--all-targets` (catches test-only dead code)
- `khive kg sync` now shells out to `kkernel sync` for real SQLite DB build
  (replaces the dishonest JSON-as-DB stub)

### Fixed

- Flaky tracing test: global subscriber + unique gate_impl name filter (#161)
- MemoryPack::active_config was dead code — tuning had no effect (#159)
- Pagination offset hardcoded to 0 for entity/note list (#145)
- Contract tests: query row wrapping + field rename handling (#138)
- annotates edge source-must-be-note constraint documented in ADR-002 (#146)

## [0.1.4] - 2026-05-20

### Added

- Brain pack with event-driven auto-tuning (ADR-064)
- Configurable recall pipeline (ADR-062)
- Retrieval objectives for vector, text, and graph proximity scoring (ADR-061)
- Bayesian fold extensions: precision tracking and epistemic weight (ADR-059)
- Fold cognitive primitives crate (ADR-058)
- Dynamic pack loading with inventory-based self-registration (ADR-063)

### Changed

- Pack system now uses inventory-based self-registration; packs declare themselves
  at compile time and are discovered at runtime without manual wiring

## [0.1.2] - 2026-05-17

Maintenance release. Pack architecture documentation updates and workspace version alignment.

## [0.1.1] - 2026-05-16

Maintenance release.

## [0.1.0] - 2026-05-16

### Added

- Initial release
- Core crates: `khive-types`, `khive-score`, `khive-storage`, `khive-db`,
  `khive-query`, `khive-runtime`, `khive-request`
- Pack system with built-in packs: `kg`, `gtd`, `memory`
- MCP server (`khive-mcp`) exposing a single `request` tool that dispatches
  verbs through the loaded pack registry
- Deno CLI for git-native knowledge-graph operations
- Marketplace plugins for KG and GTD workflows

[Unreleased]: https://github.com/ohdearquant/khive/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ohdearquant/khive/compare/v0.2.11...v0.3.0
[0.2.0]: https://github.com/ohdearquant/khive/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/ohdearquant/khive/compare/v0.1.2...v0.1.4
[0.1.2]: https://github.com/ohdearquant/khive/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ohdearquant/khive/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ohdearquant/khive/releases/tag/v0.1.0
