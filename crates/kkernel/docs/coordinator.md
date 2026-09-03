# SubstrateCoordinator Design

**ADRs**: ADR-003 (system architecture), ADR-029 (coordinator layer)
**Last reviewed**: 2026-06-06

## Overview

The coordinator owns all cross-backend operations inside `kkernel`. Pack crates do not depend on
it — they receive a single-backend `KhiveRuntime`. The coordinator routes across backends above
the pack layer.

## Architecture

```text
kkernel::coordinator
  mod.rs  — SubstrateCoordinator + BackendRegistry + LocatorCache
```

Sub-modules (`edges`, `traversal`, `curation`, `health`) are reserved per ADR-029 for D5/D6
work that is not yet implemented.

## Implementation Phases

### D1 — BackendRegistry (shipped)

`BackendRegistry` stores backends in a `BTreeMap<String, BackendEntry>` for deterministic
iteration order. The first registered backend is the primary. Each entry may
carry a closed, nonempty `served_kinds` declaration. Omitted metadata preserves
conservative eligibility for every substrate; an explicit empty declaration is
rejected at registration.

### D2 — LocatorCache (shipped)

`LocatorCache` maps substrate UUIDs to the backend that owns them. Entries expire after 5 minutes
(configurable via `with_locator_ttl`). Eviction is lazy on read. `purge_expired` is available for
maintenance tasks.

`locate(id, namespace)` checks the cache first; on a miss it concurrently probes all backends and
populates the cache on first hit.

### D3 — Fan-out search (shipped)

`fan_out_search(&ValidatedSearchRequest, namespace)` broadcasts entity or note search to eligible
registered backends in parallel. Eligibility is declaration-based: a backend is skipped only when
its explicit `served_kinds` metadata excludes the request substrate, and filtering occurs before
any backend task is spawned. A backend with no declaration remains eligible. The request is constructed by the KG pack's canonical validator
and carries the resolved substrate plus `entity_kind`/`entity_type`,
`note_kind`/`include_superseded`, `properties`, `tags`, `limit`, and `min_score`. Applicable storage
filters reach every backend; compatibility kind spellings are reconciled before this boundary, so
the coordinator cannot accidentally accept a public filter that its signature has nowhere to put.

Results are merged with Reciprocal Rank Fusion (unweighted, k=60). Per-backend errors — including
spawned-task join failures — are captured in `BackendSearchResult::error`; a single failing backend
does NOT abort the fan-out or disappear from diagnostics. The MCP service derives the operation's
`partial`/`missing_backends` advisory and bounded backend-keyed `backend_errors` causes from these
typed per-backend results, and warns with each retained masked cause.

When `is_single_backend()` is true the fan-out degenerates to a single backend call.

### D4 — Cross-backend traversal (deferred)

BFS across backend boundaries following `contains`/`extends`/`depends_on` edges. The coordinator
intercepts `traverse()` results, checks each node's backend via `locate()`, and recursively fans
out to the owning backend. Entry point: `cross_backend_traverse(roots, max_depth, relations, ns)`.

### D5 — WAL cascade on hard-delete (deferred)

When a node is hard-deleted, cascade the delete to all incident cross-backend edges using a WAL
journal. On delete, look up WAL entries for the UUID and issue compensating `delete_edge` calls to
each referenced backend. Entry point: `cascade_delete(id, namespace)`.

### D6 — Backend health map (deferred)

Coordinator maintains a health score per backend derived from consecutive error counts and last
successful call timestamp. `fan_out_search` skips unhealthy backends (score below threshold).
Requires a background health-check loop and a `BackendHealthMap`. Entry point: `health_map()`.

## Single-backend behaviour

When only one backend is registered, every D1–D6 mechanism degenerates to its trivial identity:
no fan-out, no cross-backend routing, no health map misses. Multi-backend complexity is opt-in
via `khive.toml` (ADR-028).

## Invariants

- `BackendRegistry` is append-only after boot; no backend is removed at runtime.
- The primary backend is always the first registered.
- An explicit served-kind declaration is nonempty and uses the closed substrate vocabulary;
  omission is conservative and includes the backend.
- `LocatorCache` entries are immutable once inserted (backend affinity is stable per entity).
- `fan_out_search` never silently drops a backend error; ordinary errors and task-join failures are
  captured in the result and become bounded response diagnostics plus warning records.
- Every public search filter is validated once and either forwarded to its substrate runtime or
  rejected before fan-out.

## `kkernel main.rs` — `-e`/subcommand dispatch

`-e/--exec <OPS>` and a subcommand are the CLI's two mutually exclusive top-level
entry points. clap's derive `conflicts_with` cannot name a `#[command(subcommand)]`
field directly (confirmed via clap's own startup `debug_assert`, it is not a plain
`Arg`), so the conflict — and the "neither was given" case — are enforced in
`resolve_command_result` rather than declaratively on the field.

## `kkernel main.rs` — coordinator-attached boot path

`kkernel mcp` (the `Command::Mcp` branch) builds its multi-backend server through
`build_multi_backend_server_with_coordinator` in `src/main.rs` — the one place that
assembles the coordinator's `BackendRegistry`/`SubstrateCoordinator` inputs and hands
them to `khive_mcp::serve::build_server_from_multi_backend_registry`. It funnels
through the same `khive_mcp::serve::build_registry_for_multi_backend` choke point the
plain (coordinator-less) `build_server_multi_backend` path uses, so the db-anchor
consistency guard, the ADR-078 output-format resolution, and the ADR-091 checkpoint
pool are each implemented exactly once and apply identically to both boot paths. It
copies each declared backend's `served_kinds` metadata into the registry before
constructing the coordinator. It
also returns the resolved `"schedule"`-pack runtime (ADR-106) read out of the same
`multi.per_pack_runtimes` map used to build the `BackendRegistry`, so the daemon's
dynamic ADR-119 `schedule-tick` component drains the exact backend this boot resolved
rather than a re-derived config (PR #782).

Regression coverage for this path, in `main.rs`'s `#[cfg(test)]` module:

- `multi_backend_boot_paths_share_identical_non_default_output_format` (#613): the
  sibling parity tests never configure a non-default output format, so without this
  case, one boot path silently dropping `apply_env_output_format(...)` would still
  pass (both would land on the built-in `Json` default). This test sets
  `[runtime].default_output_format = Table` in the config both constructors consume
  and asserts the captured format equals that non-default value — the explicit
  expected-value check is what makes the assertion non-vacuous. `KHIVE_OUTPUT_FORMAT`
  is cleared/restored via an RAII guard (`#[serial]`) so an ambient env var can never
  mask a regression.
- `coordinator_boundary_rejects_diverging_db_path`: a `db_path` that diverges from the
  canonical anchor for the same `--db` input must be rejected at the coordinator
  choke point exactly like the plain path rejects it.
- `coordinator_boot_uses_anchor_captured_by_runtime_config` (#720): the
  coordinator-attached boot must retain the HOME-derived db anchor captured during
  runtime-config resolution even if `HOME` changes before registry construction —
  it must never re-derive the anchor from a (possibly now-different) `HOME`.
- `coordinator_link_annotates_resolves_edge_target_like_get` (#674): reproduces the
  production topology (two backends, `session` pack bound to `sessions`, `kg` falling
  back to `main`) that engages the `SubstrateCoordinator` for `kg` verbs. Before the
  fix, the coordinator's node locator only probed entity/note substrates, so
  `link(note, <edge_uuid>, annotates)` failed with "node not found on any backend"
  even though `get(<edge_uuid>)` resolved the same UUID.
