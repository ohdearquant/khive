# kkernel Design

**Last reviewed**: 2026-06-06

## ADR Compliance

### System Architecture (kernel/MCP split) (ADR-003)

- `kkernel` is the admin/management binary; `khive-mcp` is the MCP stdio server.
- They share the `khive-runtime` crate but have separate entry points.
- Pack crates do not depend on `kkernel` or its coordinator module — they receive
  a single-backend `KhiveRuntime` from the MCP layer.
- The anti-pattern of packs depending on the coordinator is explicitly guarded by the
  module boundary: `coordinator` is `kkernel`-internal.

### Multi-backend configuration (ADR-009, ADR-028)

- `BackendRegistry` holds registered backends. Constructed at boot from `khive.toml`.
- `kkernel backend list` and `kkernel backend info` expose the registry to operators.
- Current v1 implementation exposes a single default backend. Full `khive.toml`-driven
  multi-backend enumeration is deferred to a follow-up milestone.

### VCS and sync (ADR-010, ADR-020)

- `kkernel sync` delegates to `khive_vcs::sync::run_sync` — the NDJSON-to-SQLite
  rebuild logic is owned by `khive-vcs`, not by `kkernel`.
- `kkernel kg fetch` (alias: `kkernel kg sync`) fetches a remote KG archive and
  populates the local remote cache under `.khive/kg/remotes/<remote>/`.

### ADR-015: Schema migrations

- `KhiveRuntime::new()` runs `run_migrations()` internally — constructing the runtime
  is sufficient to apply all pending migrations.
- `kkernel db migrate` wraps this; `kkernel db check` uses a read-only runtime to
  report schema state without writing.

### Pack standard (vocabulary + handlers) (ADR-017)

- `pack_introspect` module builds an in-memory `VerbRegistry` from all `inventory!`-
  registered packs and exposes `list_packs()` and `pack_handler(name)`.
- Handler `visibility` distinguishes MCP-exposed `Verb` entries from internal
  `Subhandler` entries.

### Verb namespace contract (ADR-023)

- The kg substrate pack owns 18 bare verb names (no dot prefix): `create`, `get`,
  `list`, `stats`, `update`, `delete`, `search`, `link`, `neighbors`, `traverse`,
  `query`, `merge`, `propose`, `review`, `withdraw`, `resolve`, `verbs`, `context`
  (ADR-089).
- A commercially licensed extension pack, when installed, must prefix its verbs with
  `<pack>.` (e.g. `<pack>.verb`); sub-variants use underscore, not nested dots
  (`<pack>.verb_variant`, not `<pack>.verb.variant`).
- Enforced by the integration test in `tests/verb_namespace_contract.rs`.

### Dynamic pack loading (self-registration via inventory!) (ADR-027)

- Pack crates self-register using `inventory::submit!`. The linker drops crates whose
  symbols aren't referenced, so `kkernel/lib.rs` and the contract test binary both
  include explicit `use PackName as _` anchors to prevent dead-stripping.
- `PackRegistry::discovered_names()` returns all self-registered pack names at runtime.

### SubstrateCoordinator (ADR-029)

- `coordinator/mod.rs` implements D1 (BackendRegistry), D2 (LocatorCache), and D3
  (fan-out search with RRF).
- D4 (cross-backend traversal), D5 (WAL cascade delete), and D6 (health map) are
  deferred; sub-modules (`edges`, `traversal`, `curation`, `health`) are reserved.
- See `docs/coordinator.md` for implementation phase detail.

### KG validation and init (ADR-034, ADR-035)

- `kkernel kg validate` runs three built-in structural checks (duplicate UUIDs, sort
  order, referential integrity) plus configurable rules from `rules.toml`.
- `kkernel kg init` creates `.khive/kg/` and writes `khive.toml` with defaults.
- See `docs/kg-rules.md` for the rule TOML format.

### KG status and fetch/sync alias (ADR-036, ADR-037)

- `kkernel kg status` computes a content hash of the DB state and the NDJSON files and
  reports whether they match.
- `kkernel kg fetch` has a `visible_alias = "sync"` so `kkernel kg sync --repin <remote>`
  reaches the same handler.

### Embedding model lifecycle (ADR-043)

- `kkernel engine list/status` expose `_embedding_models` table data.
- `kkernel engine migrate` and `kkernel engine drift-check` are deferred to follow-up
  #380 (EmbedMigrationWorker and lattice_transport integration).
- No MCP verbs are exposed for engine management — these are operator-only commands.

### Vector store capabilities and orphan sweep (ADR-044)

- `kkernel vector capabilities` emits the sqlite-vec baseline capability flags.
  Values match `SqliteVecStore::capabilities()` in `khive-db`.
- `kkernel vector sweep` is deferred to follow-up #381; `SqliteVecStore` returns
  `Unsupported` for the orphan-sweep operation.

### Proposal lifecycle (ADR-046)

- The kg pack exposes `propose`, `review`, and `withdraw` verbs as part of the
  18 kg-substrate bare verbs. These are validated by the contract test.

### Atomic `exec --ops-file --atomic` execution path (ADR-099 Slice B3)

`atomic_apply.rs` is the CLI-boundary orchestrator for `kkernel exec --ops-file --atomic`.
This distribution ships the `kg` pack only, so `--atomic` admits KG-substrate verbs.
`atomic_apply.rs` runs, in order:

1. Parse-time admissibility (`khive_request::atomic::check_atomic_admissible`, B1) plus the
   op-count guard — both run BEFORE building any runtime or touching the database.
2. The async prepare pass over KG-substrate verbs via `khive_runtime::atomic_prepare::prepare_op`.
3. The synchronous commit pass (`khive_runtime::atomic_runner::run_atomic_unit`, B2).
4. The async post-commit reindex pass (`khive_runtime::atomic_prepare::apply_post_commit_effects`).

**Verbs without a prepare implementation.** `propose`/`review`/`withdraw` are listed in
`khive_types::pack::ATOMIC_ADMISSIBLE_VERBS` (ADR-099 D3 intends them to eventually gain a
seam) but have none yet; the B3 fix rejects them at the same pre-runtime
`check_atomic_admissible` guard, as `AtomicRejectionReason::KnownUnimplemented`, before they
ever reach `KhiveRuntime::new`/`prepare_one`. `prepare_op`'s own
`prepare_governance_unimplemented` fallback is unreachable through this CLI path and remains
only as defense-in-depth for other `prepare_op` callers.

`merge` joined this deferred bucket in the B3 fix too: a full-parity atomic merge prepare was
drafted and unit-tested against `atomic_prepare` directly, but its edge-conflict resolution
cannot be expressed in ADR-099's static predicate/guard plan shape, so it is rejected here
rather than shipped partially-scoped (`merge is not yet supported under --atomic; use the
non-atomic merge verb`).

The returned envelope is additive-only and lives entirely outside `dispatch_request_local`'s
response shape — non-atomic `--ops-file` runs (and every other exec path) are untouched.

**`validate_atomic_args`** closes an ADR-099 B3 parity gap: the canonical (non-atomic)
handlers deserialize their args through a `#[serde(deny_unknown_fields)]` param struct, so a
typo like `conten` (for `content`) is rejected rather than silently ignored. The pre-fix
`--atomic` path had no equivalent gate — each `prepare_*` fn only read the keys it knew
about, so a typo'd key was dropped on the floor and the op reported `ok:true` with every
OTHER field reset to its current value, silently losing the caller's intended change. The
fix reuses (rather than reimplements) the canonical param structs: `kkernel` already depends
on `khive-pack-kg` directly, and its param structs (`UpdateParams`/`DeleteParams`/
`LinkParams`) are re-exported `pub` specifically for this seam. Deserializing an op's args
through the same struct the canonical handler uses reproduces its `deny_unknown_fields`
rejection and exact error message for free, with no duplicated key list to drift out of
sync — the deserialized value itself is discarded; `prepare_*` still reads the raw `Value`
map. `merge`, `create`, and the read/governance verbs are out of scope: they are already
rejected earlier at `check_atomic_admissible`, or are not part of the v1 admissible set at
all.

**`delete_expected_kind`/`update_expected_kind`** resolve a caller-supplied `kind=...`
string into the `AtomicDeleteKind`/`AtomicUpdateKind` enum `prepare_delete`/`prepare_update`
enforce, via the SAME canonical `resolve_kind_spec` the non-atomic `handle_delete`/
`handle_update` call. The two functions are exact mirrors of each other (same reasoning,
same shape, differing only in which `AtomicOpPlan`-adjacent enum they target). `kind` absent
resolves to `Ok(None)` (no check, parity with the canonical handlers' own optional
discriminator); `Event`/`Proposal` are a fail-loud rejection before `prepare_delete`/
`prepare_update` ever runs — those substrates are not v1-admissible for atomic
delete/update at all.

### `exec` local-dispatch fallback server (ADR-067, ADR-028 §8)

`build_local_fallback_server` (`src/exec.rs`) is the server constructor for both of
`kkernel exec`'s non-daemon dispatch paths: the daemon-unreachable/mismatch fallback inside
`run_exec_inline_with_forward`, and the `--ops-file` bulk-apply path (which deliberately
never attempts the daemon fast path at all — bulk apply needs cross-op atomicity the daemon
doesn't provide). `KhiveMcpServer::new` alone only ever builds a single-backend runtime, with
no visibility into a `khive.toml` `[[backends]]` declaration; before this fix, both
local-dispatch paths always used that single-backend constructor, so a config declaring a
separate backend for e.g. the `session` pack was invisible to them — the in-process fallback
silently wrote that pack's data into the `main` backend instead of its declared one. The fix
makes both paths agree with the daemon's own boot logic (`khive_mcp::serve::build_server`):
an empty `khive_cfg.backends` still takes the plain single-backend constructor (byte-identical
`config_id`, since `compute_config_id` skips the topology fold for an empty list); otherwise
both delegate to `build_server_multi_backend_with_db_anchor`, the same captured-anchor
constructor the production MCP boot path uses. `cli_db_override` is the raw, pre-resolution
`--db`/`KHIVE_DB` value, required for `--db :memory:` multi-backend override handling; passing
the wrong value would silently ignore an operator's in-memory isolation request. `db_anchor` is
the canonical anchor captured alongside `cfg`, threaded through so fallback construction never
re-reads a changed `HOME`.

### `exec.rs` regression test notes

- `strict_mode_rejects_before_daemon_forward_when_comm_and_no_actor`: `run_exec_inline` must
  enforce the strict-actor gate (`KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`) BEFORE forwarding to the
  daemon. Prior to this fix, `enforce_strict_actor_mode` was only called in the in-process
  fallback path (after the daemon fast-path returned), so an attacker or misconfigured
  operator could start a no-actor daemon, then run strict-mode `kkernel exec`, which would
  forward through it and exit 0 — bypassing the gate. The fix moves the check before the
  daemon block.
- `atomic_update_null_and_type_semantics_match_canonical_no_op_behavior`: atomic `update`
  null/type semantics must match canonical's actually-reachable behavior. Empirically
  verified against live `handle_update` that `name=null`/`description=null` are canonical
  no-ops, not rejections — canonical's field type is `Option<Value>`, and serde's derived
  `Deserialize` for `Option<T>` intercepts a literal JSON `null` at the outer `Option`
  boundary and maps it straight to `None` regardless of the inner type, so canonical's own
  "reject null" arms in `string_value`/`optional_string_patch` are unreachable through normal
  struct deserialization. This test deliberately does NOT implement the naive expectation
  ("`update(name=null)` REJECTED") since that doesn't match the live system. What canonical
  DOES still reject is a non-null, non-string `name` (e.g. `name: 123`) — pre-fix, atomic
  silently treated that as absent too, reporting success for an invalid update.

### `reindex` (`src/reindex.rs`)

`kkernel reindex` rebuilds embedding vectors and FTS documents for entities and notes in
this distribution's `kg` substrate.

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
