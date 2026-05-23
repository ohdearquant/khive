# ADR-080: SubstrateCoordinator — Cross-Backend Operations Inside kkernel

**Status**: proposed (umbrella — concrete decisions split into ADR-086/087/088/089 on
2026-05-22)\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-001 (Entity Kind Taxonomy), ADR-002 (Edge Ontology), ADR-019 (Note Kind
Taxonomy), ADR-022 (Schema Migrations), ADR-025 (Pack Standard), ADR-076 (Kernel/MCP Split),
ADR-079 (Pack-Scoped Backends)\
**Split into**: ADR-086 (edge representation + locator + link mechanics), ADR-087 (substrate-
kind federated search), ADR-088 (cross-backend traversal + curation), ADR-089 (partition
tolerance)

## Context

ADR-079 introduces pack-scoped backends: each pack declares `backend = "main"` (or `"lore"`,
`"archive"`, etc.) in `khive.toml`, and the boot process constructs one `Arc<StorageBackend>`
per declared backend plus one `KhiveRuntime` per pack. ADR-078 introduces multi-engine
embedding: a shared `Arc<EmbedderRegistry>` across packs, with per-pack filtered views.

This combination raises three problems neither ADR-078 nor ADR-079 addresses on its own:

**P1 — Substrate-kind queries must federate across backends.** When a caller invokes
`search(kind=note, query="X")`, the ADR-024 contract is "search all notes." If `memory` is on
`main.db` and `lore` is on `lore.db`, the search must touch both backends and merge results.
The current `KhiveRuntime::hybrid_search` is single-backend; there is no coordinator above it
that knows about both runtimes.

**P2 — Edges may need to cross backends.** ADR-079's earlier draft said "cross-backend linking
impossible" and pushed the use case to client-side batching. That answer was a deferral, not a
design choice. A `kg` entity on `main.db` should be able to `annotates`-link to a `lore` atom
on `lore.db`. Hard-disallowing this defeats the unified-graph model.

**P3 — Some operations live above any single runtime.** `link(source_id, target_id, relation)`
takes two UUIDs the caller does not know how to route. `traverse(roots, depth=3)` may need to
follow edges across backend boundaries. `update_entity(uuid, patch)` must locate the entity's
backend before patching. None of these fit cleanly inside a per-pack `KhiveRuntime`.

The architectural shape needed is the **coordinator pattern** — a layer above the
pack→runtime mapping that owns the cross-backend dispatch, the node-location cache, and the
cross-backend metrics. RuVector's `ShardCoordinator` is the obvious reference
(`Arc<DashMap<ShardId, Arc<GraphShard>>>` with `target_shards` fan-out); oxigraph's
`Storage { kind: StorageKind }` enum is the reference for backend-kind future-proofing (already
adopted in ADR-079 D5). Both references inform this design without being copied wholesale.

### What this ADR umbrella is and isn't

This ADR specifies the **umbrella scope and motivation** for the SubstrateCoordinator — a
kernel-internal component in the `kkernel` crate (ADR-076). It is the parent of four atomic
ADRs that each lock a single decision:

| Sub-ADR                                                    | Decision      | Scope                                                                                                                                                       |
| ---------------------------------------------------------- | ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [ADR-086](ADR-086-cross-backend-edge-representation.md)    | D2 + D3 + D10 | Cross-backend edge representation: `target_backend` column, in-memory lazy node locator, coordinator-driven `link()`                                        |
| [ADR-087](ADR-087-substrate-kind-federated-search.md)      | D4            | Substrate-kind verb fan-out across backends with unweighted RRF fusion                                                                                      |
| [ADR-088](ADR-088-cross-backend-traversal-and-curation.md) | D9 + D11      | Cross-backend traversal (BFS across boundaries), curation semantics (update across backends, merge errors, hard-delete cascade), normative operation matrix |
| [ADR-089](ADR-089-coordinator-partition-tolerance.md)      | D12           | Degraded reads + hard writes when a backend is unreachable; backend health model                                                                            |

This umbrella ADR is **not** a place for:

- Detailed Rust type signatures (implementation scaffolding belongs in implementation PRs)
- Step-by-step boot sequences (live in the implementation plan
  `plan_20260522_runtime_restoration.md`)
- Implementation step-counts (the sub-ADRs reference plan phases B4/B5)

It is the place for:

- Why a coordinator exists (P1/P2/P3 above)
- D1 (where the coordinator lives in the crate graph)
- Alternatives that argue against having a coordinator at all (A, D, E, F below)
- Consequences spanning all four sub-ADRs

What this ADR umbrella does **not** introduce (rejected scope):

- A distributed/network query layer (federation across processes). RuVector's `Federation` /
  `ClusterRegistry` is rejected; khive is in-process. A future ADR may add network federation.
- Transparent re-partitioning. RuVector's `EdgeCutMinimizer` (METIS) is rejected; khive
  backends are user-intentional, not auto-derived.
- Cross-backend atomic transactions. SQLite WAL is per-backend; cross-backend writes are
  non-atomic per ADR-088 D11.
- A new query language. The coordinator's surface is verb-aligned, not query-language-shaped.

## Decision — umbrella-level

### D1 — Coordinator lives inside the `kkernel` crate

Per ADR-076's planned end-state, `kkernel` is the management binary and `khive-mcp` becomes a
thin shim. The coordinator is dispatch-layer code, not a public library — placing it inside
`kkernel` keeps the boundary tight. Module path: `kkernel::coordinator`. Pack crates do not
depend on it.

**Interim placement**: until `kkernel` exists (ADR-076 currently `proposed`), the coordinator
module may live inside `khive-mcp` as `khive_mcp::coordinator`. Implementation order: ADR-076's
kkernel crate is created first; then the coordinator module lands inside it. Both binaries (the
current `khive-mcp` and the eventual `kkernel mcp` subcommand) link the same coordinator
module via the same crate.

**Why not a separate `khive-coordinator` crate**: a separate crate adds compile units and a
public surface for what is fundamentally internal kernel plumbing. Packs do not need it; no
external consumer needs it. RuVector keeps its `Coordinator` inside
`ruvector-graph::distributed` for the same reason. D1 may eventually merge into ADR-076 as a
scope amendment if the kkernel ADR evolves; for now it stays as the umbrella's one direct
decision.

### Where the other decisions live

| Decision | ADR                                                        | One-line scope                                           |
| -------- | ---------------------------------------------------------- | -------------------------------------------------------- |
| D2       | [ADR-086](ADR-086-cross-backend-edge-representation.md)    | Edges on source's backend with `target_backend` column   |
| D3       | [ADR-086](ADR-086-cross-backend-edge-representation.md)    | In-memory lazy `DashMap<Uuid, BackendName>` locator      |
| D4       | [ADR-087](ADR-087-substrate-kind-federated-search.md)      | Unweighted RRF across backends for substrate-kind search |
| D9       | [ADR-088](ADR-088-cross-backend-traversal-and-curation.md) | Traversal works across backends transparently            |
| D10      | [ADR-086](ADR-086-cross-backend-edge-representation.md)    | `link()` is coordinator-driven                           |
| D11      | [ADR-088](ADR-088-cross-backend-traversal-and-curation.md) | Update OK; merge errors; hard-delete cascades incoming   |
| D12      | [ADR-089](ADR-089-coordinator-partition-tolerance.md)      | Partition-tolerant degraded reads; hard-fail writes      |

## Alternatives considered (umbrella-level)

These alternatives argue against the coordinator concept itself. Alternatives specific to a
sub-decision live in the relevant sub-ADR (e.g., RuVector's pure-locator approach is rejected
in ADR-086 §A; oxigraph's named-graph-only approach is rejected in ADR-086 §B).

### A. No coordinator — pack handlers do their own cross-backend work

Pack handlers receive `HashMap<BackendName, Arc<KhiveRuntime>>` and orchestrate fan-out
themselves.

Pros: no new kernel component; coordinator complexity becomes per-pack code. Cons: every pack
re-implements substrate-kind dispatch, locator, cross-backend cascade. The shape becomes a
de-facto coordinator pattern copy-pasted across packs. Routing decisions become pack code,
making operational tuning (which backends host which kinds) impossible without recompiling.

Rejected. Centralizing in `kkernel` keeps pack code single-backend — packs are about
semantics, not topology.

### D. Per-pack coordinator inside each pack crate

Each pack ships its own coordinator that knows about its backend plus the substrate-kind
backends.

Pros: pack autonomy. Cons: substrate-kind dispatch needs a coordinator that sees ALL backends —
that can't live inside one pack. Every pack would need a stub pointing to the "real"
coordinator, defeating the locality argument.

Rejected. Coordinator is naturally kernel-scoped because substrate-kind operations are
kernel-scoped.

### E. Out-of-process coordinator (microservice)

Coordinator is its own process; packs and `kkernel mcp` talk to it over IPC.

Pros: clean isolation; future-proofs distributed deployments. Cons: dramatic complexity
explosion; per-call IPC overhead on every verb; defeats the "single-binary in-process MCP
daemon" model; kkernel/khive-mcp split (ADR-076) is the right level of separation, not
per-component.

Rejected. RuVector's network federation is a different concept (cross-cluster, not
cross-backend); khive stays in-process.

### F. Defer cross-backend operations entirely (no coordinator)

Keep multi-backend (ADR-079) but disallow cross-backend operations. `link(a_on_main, b_on_lore)`
returns `CrossBackendLinkUnsupported` — the original ADR-079 §6 stance.

Pros: simplest implementation; no coordinator needed. Cons: defeats the unified-graph model.
Operators with main+lore must do cross-backend reasoning client-side. Substrate-kind search
becomes wrong (`kind=note` returns only one backend's notes, not all).

Rejected. ADR-079's original deferral became its own technical debt within hours of being
written. The coordinator pattern unblocks it.

## Consequences (umbrella-level)

Detailed positive/negative/neutral consequences live in each sub-ADR. The umbrella-level
consequences are:

### Positive

- Cross-backend operations work uniformly across `link`, `neighbors`, `traverse`, `update`,
  `search(kind=substrate)` — see sub-ADRs for specifics.
- Pack code stays single-backend; kernel handles the boundary crossings.
- Single-backend deployments observe **zero behavioral change** — the coordinator is no-op
  overhead on the common shape. Each sub-ADR documents this in its "neutral consequences"
  section.
- ADR-024's substrate-kind verb contract holds for multi-backend deployments.

### Negative

- **kkernel crate must exist (ADR-076)** before coordinator implementation can land in its
  target home. Interim placement in `khive-mcp` is acceptable.
- Non-atomic cross-backend operations (see ADR-086 link, ADR-088 cascade, ADR-089 partition).
  SQLite WAL is per-backend; this is documented per-operation in the sub-ADRs.
- More test surface — substrate-kind tests, cross-backend link tests, partition tests. The
  single-backend default is the regression-test anchor.

### Neutral

- ADR-002's 13-relation edge ontology is unchanged.
- ADR-024's cross-substrate search contract is preserved.
- MCP wire format unchanged.
- ADR-079 §6 "Cross-pack composition" is superseded by these sub-ADRs (ADR-079 already
  cross-references them).

## Migration plan

Implementation phases are documented in
`.khive/notes/plans/plan_20260522_runtime_restoration.md` — Phase B4 (coordinator + substrate
router) and Phase B5 (cross-backend edges). Each phase corresponds to one or more sub-ADRs
landing.

Each phase ships independently; the build stays green between commits. The single-backend
regression-test anchor runs after every commit; any divergence is a P0 fix.

## Open Questions (umbrella-level)

Sub-ADRs own their own open questions. Umbrella-level open questions:

1. **Should D1 (coordinator placement) eventually move to ADR-076 as an amendment?** Currently
   stated here for proximity to the coordinator design. Could merge into ADR-076 if the kernel
   architecture stabilizes.
2. **Operation matrix ownership.** ADR-088 owns the normative matrix today. If the matrix
   grows large enough to warrant its own reference appendix, factor it out.

## References

### khive-internal (predecessor architecture)

- `apps/cli/src/server/unified.rs:556-664` — `backend_for`, `lore_backend_for`,
  `lore_storage_backend`, per-verb-family dispatch picking backends
- `apps/cli/src/server/unified.rs:730-848` — `dispatch_lore` vs `dispatch_memory`

### RuVector (distributed graph reference)

- `crates/ruvector-graph/src/distributed/coordinator.rs` — `ShardCoordinator` shape that
  inspired this design
- `crates/ruvector-graph/src/distributed/federation.rs` — cross-cluster federation
  (REJECTED for in-process khive; pattern of partial-coverage flags adopted by ADR-089)
- `crates/ruvector-graph/ARCHITECTURE.md` — design rationale

### oxigraph (named-graph + backend-kind reference)

- `lib/oxigraph/src/storage/mod.rs:50-90` — `StorageKind { RocksDb, Memory }` pattern
  adopted by ADR-079 D5

### khive open-core (current state to modify)

- `crates/khive-runtime/src/runtime.rs:78-87` — single-backend regression site (this ADR's
  raison d'être)
- `crates/khive-runtime/src/graph_traversal.rs` — single-backend traversal (ADR-088 D9
  replaces the entry point)
- `crates/khive-storage/src/graph.rs` — current `GraphStore` trait (ADR-086 extends)
- `crates/khive-db/src/stores/graph.rs:298-329` — `upsert_edge` SQL (ADR-086 migration target)
- `crates/khive-db/src/migrations.rs` — schema migration registry (ADR-086 appends V_NEXT)

### Cross-references

- ADR-001, ADR-002, ADR-019 — substrate kind taxonomies that this ADR federates
- ADR-014 — single-backend curation; ADR-088 D11 extends to multi-backend
- ADR-022 — schema migration mechanism used by ADR-086
- ADR-024 — cross-substrate search contract that ADR-087 fulfills
- ADR-025 — pack standard; packs see single backends, coordinator above
- ADR-031 — pack-extensible edge endpoints; ADR-086 D10 consults the same rules
- ADR-076 — kernel/MCP split; coordinator's home crate
- ADR-078 — multi-engine embedding; coordinator does not generate embeddings
- ADR-079 — pack-scoped backends; coordinator sits above
