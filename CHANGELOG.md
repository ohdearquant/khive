# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/ohdearquant/khive/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/ohdearquant/khive/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/ohdearquant/khive/compare/v0.1.2...v0.1.4
[0.1.2]: https://github.com/ohdearquant/khive/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ohdearquant/khive/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ohdearquant/khive/releases/tag/v0.1.0
