# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
