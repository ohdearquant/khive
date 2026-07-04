# kkernel Design

**Last reviewed**: 2026-06-06

## ADR Compliance

### ADR-003: System Architecture (kernel/MCP split)

- `kkernel` is the admin/management binary; `khive-mcp` is the MCP stdio server.
- They share the `khive-runtime` crate but have separate entry points.
- Pack crates do not depend on `kkernel` or its coordinator module — they receive
  a single-backend `KhiveRuntime` from the MCP layer.
- The anti-pattern of packs depending on the coordinator is explicitly guarded by the
  module boundary: `coordinator` is `kkernel`-internal.

### ADR-009 / ADR-028: Multi-backend configuration

- `BackendRegistry` holds registered backends. Constructed at boot from `khive.toml`.
- `kkernel backend list` and `kkernel backend info` expose the registry to operators.
- Current v1 implementation exposes a single default backend. Full `khive.toml`-driven
  multi-backend enumeration is deferred to a follow-up milestone.

### ADR-010 / ADR-020: VCS and sync

- `kkernel sync` delegates to `khive_vcs::sync::run_sync` — the NDJSON-to-SQLite
  rebuild logic is owned by `khive-vcs`, not by `kkernel`.
- `kkernel kg fetch` (alias: `kkernel kg sync`) fetches a remote KG archive and
  populates the local remote cache under `.khive/kg/remotes/<remote>/`.

### ADR-015: Schema migrations

- `KhiveRuntime::new()` runs `run_migrations()` internally — constructing the runtime
  is sufficient to apply all pending migrations.
- `kkernel db migrate` wraps this; `kkernel db check` uses a read-only runtime to
  report schema state without writing.

### ADR-017: Pack standard (vocabulary + handlers)

- `pack_introspect` module builds an in-memory `VerbRegistry` from all `inventory!`-
  registered packs and exposes `list_packs()` and `pack_handler(name)`.
- Handler `visibility` distinguishes MCP-exposed `Verb` entries from internal
  `Subhandler` entries (e.g. `memory.recall_embed`).

### ADR-023: Verb namespace contract

- The kg substrate pack owns 17 bare verb names (no dot prefix): `create`, `get`,
  `list`, `stats`, `update`, `delete`, `search`, `link`, `neighbors`, `traverse`,
  `query`, `merge`, `propose`, `review`, `withdraw`, `verbs`, `context` (ADR-089).
- Every other pack must prefix verbs with `<pack>.` (e.g. `memory.recall`).
- Sub-variants use underscore, not nested dots: `memory.recall_embed`, not
  `memory.recall.embed`.
- Enforced by the integration test in `tests/verb_namespace_contract.rs`.

### ADR-027: Dynamic pack loading (self-registration via inventory!)

- Pack crates self-register using `inventory::submit!`. The linker drops crates whose
  symbols aren't referenced, so `kkernel/lib.rs` and the contract test binary both
  include explicit `use PackName as _` anchors to prevent dead-stripping.
- `PackRegistry::discovered_names()` returns all self-registered pack names at runtime.

### ADR-029: SubstrateCoordinator

- `coordinator/mod.rs` implements D1 (BackendRegistry), D2 (LocatorCache), and D3
  (fan-out search with RRF).
- D4 (cross-backend traversal), D5 (WAL cascade delete), and D6 (health map) are
  deferred; sub-modules (`edges`, `traversal`, `curation`, `health`) are reserved.
- See `docs/coordinator.md` for implementation phase detail.

### ADR-034 / ADR-035: KG validation and init

- `kkernel kg validate` runs three built-in structural checks (duplicate UUIDs, sort
  order, referential integrity) plus configurable rules from `rules.toml`.
- `kkernel kg init` creates `.khive/kg/` and writes `khive.toml` with defaults.
- See `docs/kg-rules.md` for the rule TOML format.

### ADR-036 / ADR-037: KG status and fetch/sync alias

- `kkernel kg status` computes a content hash of the DB state and the NDJSON files and
  reports whether they match.
- `kkernel kg fetch` has a `visible_alias = "sync"` so `kkernel kg sync --repin <remote>`
  reaches the same handler.

### ADR-043: Embedding model lifecycle

- `kkernel engine list/status` expose `_embedding_models` table data.
- `kkernel engine migrate` and `kkernel engine drift-check` are deferred to follow-up
  #380 (EmbedMigrationWorker and lattice_transport integration).
- No MCP verbs are exposed for engine management — these are operator-only commands.

### ADR-044: Vector store capabilities and orphan sweep

- `kkernel vector capabilities` emits the sqlite-vec baseline capability flags.
  Values match `SqliteVecStore::capabilities()` in `khive-db`.
- `kkernel vector sweep` is deferred to follow-up #381; `SqliteVecStore` returns
  `Unsupported` for the orphan-sweep operation.

### ADR-046: Proposal lifecycle

- The kg pack exposes `propose`, `review`, and `withdraw` verbs as part of the
  17 kg-substrate bare verbs. These are validated by the contract test.

## Consistency Notes

- `kkernel db migrate --dry-run` delegates to `cmd_db_check` rather than implementing
  a separate dry-run path. The `--check` flag makes the check exit nonzero if behind.
- The `cmd_vector_capabilities` function hard-codes baseline sqlite-vec values
  rather than instantiating a runtime — this is intentional for the v1 implementation
  but means the output does not reflect operator-configured backends.
- `coordinator/mod.rs` cannot be split into sub-files because `SubstrateCoordinator`
  has a `#[cfg(test)]` field (`fail_backend_id`) that tests access directly; making it
  `pub(crate)` would expose the failure-injection mechanism to integration tests.
- Edge weights are validated in the [0.0, 1.0] closed interval with finite-number
  checks. NaN and infinity are explicitly rejected with descriptive error messages.
