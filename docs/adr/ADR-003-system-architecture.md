# ADR-003: System Architecture

**Status**: accepted\
**Date**: 2026-05-22\
**Authors**: khive maintainers

## Context

khive is a research knowledge graph runtime. AI agents are the primary consumers.
The system must satisfy:

1. **Verb-stable interface**: agents call verbs (`create`, `link`, `search`, `query`). The
   verb vocabulary must be stable across transport evolution and binary evolution.
2. **Validation correctness**: entity_type normalization (ADR-001), edge endpoint validation
   (ADR-002), namespace enforcement must happen in exactly one place.
3. **Extension without fragmentation**: packs add vocabulary and verb handlers without
   forking the runtime or creating parallel validation paths.
4. **Trust boundary separation**: operator-context operations (sync, migrations, pack
   introspection) and MCP-serving operations (gated verb dispatch) have different trust
   requirements and must not share an exposure surface.
5. **Multi-backend composition**: packs with different storage profiles (hot KG data vs.
   cold corpus vs. archive) must coexist in one process without losing cross-pack
   composition.

## Decision

### Four invariants

```text
1. khive is verb-first, not transport-first.
2. Semantic validation lives in khive-runtime.
3. Packs extend runtime behavior; binaries and transports do not contain business logic.
4. The coordinator routes across backends; packs stay single-backend.
```

### Binary architecture

`kkernel` is the single Rust binary. Its subcommands enforce distinct operator and
caller-facing trust boundaries.

**Operator subcommands** own:

- Full pack capability surface (all registered packs, all handlers, all verbs)
- Sync: build SQLite DB from NDJSON sources (`kkernel sync`)
- Pack introspection (`kkernel pack list`, `kkernel pack handler <name>`)
- DB administration (`kkernel db migrate`, `kkernel db check`)
- SubstrateCoordinator (cross-backend dispatch)
- No gate enforcement: assumes operator context

**`kkernel mcp`** is the caller-facing MCP mode. The `khive-mcp` library owns:

- JSON-RPC stdio surface (single `request` tool)
- Gate enforcement: decides which kernel capabilities are agent-safe
- Curated subset of kernel verbs, filtered by policy

The binary links `khive-mcp`, `khive-runtime`, and the loaded pack crates. The mode
determines exposure and gate policy, while business logic remains shared.

The shipped subcommand topology is:

- `kkernel mcp`: long-lived stdio MCP server
- `kkernel sync`: one-shot NDJSON → SQLite build
- `kkernel pack list`: one-shot pack introspection
- `kkernel db migrate`: one-shot schema migration

### Crate dependency chain

```text
khive-types          (domain types, no_std, zero deps)
  ├── khive-score    (deterministic scoring)
  ├── khive-storage  (trait-only, zero implementations)
  │     ├── khive-db (SQLite storage + FTS5 TextSearch + sqlite-vec VectorStore compatibility)
  │     └── retrieval crates (khive-retrieval, khive-fusion, khive-bm25, khive-hnsw, khive-vamana)
  ├── khive-query    (GQL/SPARQL → SQL compiler)
  ├── khive-request  (verb-dispatch DSL parser)
  └── khive-runtime  (VerbRegistry, validation, operations)
          ▲
          │ depends on runtime
          └── khive-pack-kg     (KG vocabulary and verb handlers)

kkernel  (single binary: operator subcommands and MCP-serving mode)
  ├── kkernel::coordinator   (SubstrateCoordinator)
  ├── khive-mcp           (MCP server library + gate enforcement)
  ├── khive-runtime
  ├── khive-request
  └── khive-pack-*
```

**Critical rules**:

- `khive-runtime` MUST NOT depend on any pack crate. Packs depend on runtime.
- Pack crates MUST NOT depend on the coordinator or know about backend topology.
- The binary composes runtime + packs + coordinator at startup.

### Dispatch model

```text
External caller (Claude Code, Python, HTTP client)
  │
  ▼
Binary (`kkernel mcp`)
  │
  ├── [MCP-serving mode only] Gate enforcement
  │
  ▼
Optional DSL parsing (khive-request, if textual input)
  │
  ▼
SubstrateCoordinator
  │
  ├── Node locator (UUID → backend resolution)
  ├── Federated search fan-out (multi-backend)
  ├── Cross-backend edge routing
  │
  ▼
VerbRegistry dispatch → Pack handler
  │
  ▼
Per-pack KhiveRuntime (validation, orchestration)
  │
  ▼
StorageBackend (per-pack SQLite instance)
```

The `VerbRegistry` is the stable internal abstraction. MCP is the current caller-facing
transport. The SubstrateCoordinator is kernel-internal dispatch above the per-pack
runtimes. All user-visible capabilities enter through registered verbs.

### Storage architecture

Each pack declares a `StorageProfile`: a set of placement roles (`PlacementRole`)
describing the storage characteristics it needs. The boot process constructs one
`Arc<StorageBackend>` per declared backend, resolves each pack's storage profile
against available backends, and constructs the pack with a `KhiveRuntime` wrapping
the assigned backend.

```rust
pub struct StorageProfile {
    pub roles: Vec<PlacementRole>,
    pub default_backend: &'static str,
}

pub enum PlacementRole {
    Hot,
    Cold,
    Archive,
    ReadOnly,
}
```

Packs declare profiles through `PackStoragePolicy`:

```rust
pub trait PackStoragePolicy {
    fn storage_profile(&self) -> StorageProfile;
}
```

```text
khive.toml (illustrative: backend count and names are deployment decisions):
  [[backends]]  main → ./data/main.db
  [[backends]]  corpus → ./data/corpus.db

  [packs.kg]      backend = "main"
```

`backend = "main"` in TOML is sugar for single-placement. The StorageProfile model
allows lifecycle-driven placement changes (hot → cold) without cross-pack migration -
placement is a coordinator concern, not a pack concern.

- Pack → backend assignment is resolved at boot from profile + TOML config.
- Packs sharing a backend compose directly via shared substrate tables.
- Packs on different backends compose via the SubstrateCoordinator.
- Default (no config file): one `main` backend, all packs on `main`. Behavior is
  identical to a single-backend deployment.

## Crate Responsibilities

| Crate            | Owns                                                                                                        | Must NOT own                                                           |
| ---------------- | ----------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `khive-types`    | Domain structs, enums, `EntityTypeDef` contract, serialization types                                        | Runtime registries, DB access, semantic validation                     |
| `khive-score`    | `DeterministicScore`, fusion primitives                                                                     | Graph mutation, storage, transport                                     |
| `khive-storage`  | Storage traits, shared storage-facing types                                                                 | SQLite implementation, business validation                             |
| `khive-db`       | SQLite storage implementation: SQL substrates, FTS5 TextSearch, sqlite-vec VectorStore compatibility        | Entity ontology decisions, endpoint policy, retrieval-engine policy    |
| Retrieval crates | BM25/HNSW/Vamana indexes, hybrid retrieval primitives, and fusion strategies                                | Storage ownership, pack-specific scoring policy                        |
| `khive-query`    | GQL/SPARQL parsing, AST validation, SQL compilation                                                         | Write-time semantic validation                                         |
| `khive-request`  | Verb-dispatch DSL parsing into typed request structures                                                     | entity_type validation, endpoint validation                            |
| `khive-runtime`  | `VerbRegistry`, `EntityTypeRegistry`, endpoint validation, namespace enforcement, transaction orchestration | Transport parsing, pack-specific verb semantics, backend topology      |
| Pack crates      | Vocabulary registration, verb handlers, pack-specific endpoint rules                                        | Raw transport handling, DB-specific persistence, cross-backend routing |
| `kkernel`        | Binary composition, SubstrateCoordinator, sync, pack introspection, admin ops                               | Semantic validation (runtime's job), verb handler logic (pack's job)   |
| `khive-mcp`      | Library implementation of MCP stdio transport, gate enforcement, and request/response adaptation            | A standalone binary, business logic, entity CRUD, pack semantics       |

### Types vs Runtime boundary

`EntityTypeDef` (the static declaration) lives in `khive-types` (no_std):

```rust
pub struct EntityTypeDef {
    pub token: &'static str,
    pub base_kind: EntityKind,
    pub entity_type: &'static str,
    pub aliases: &'static [&'static str],
}
```

`EntityTypeRegistry` (the dynamic runtime registry with collision detection, alias
resolution, and write-time validation) lives in `khive-runtime`.

## Validation Boundary

Semantic validation lives in `khive-runtime`. This is the single validation boundary
for the entire system, regardless of how many backends or binaries are in play.

| Validation concern           | Owner                         | NOT here                                |
| ---------------------------- | ----------------------------- | --------------------------------------- |
| `entity_type` normalization  | `khive-runtime`               | parser, storage, MCP shell, coordinator |
| Edge endpoint legality       | `khive-runtime`               | storage, query compiler, coordinator    |
| `dependency_kind` validation | `khive-runtime`               | storage, parser                         |
| Namespace enforcement        | `khive-runtime`               | storage (dumb persistence)              |
| Note kind validation         | `khive-runtime`               | storage                                 |
| Cross-backend routing        | `SubstrateCoordinator`        | pack handlers, storage, runtime         |
| Gate enforcement             | `kkernel mcp` via `khive-mcp` | operator subcommands                    |
| Persistence-level integrity  | `khive-db`                    | (non-null ID, FK cascades)              |

```text
DB enforces non-null ID.
Runtime enforces entity_type validity.
Runtime enforces edge endpoint legality.
Runtime enforces namespace isolation.
Coordinator routes across backends.
Gate layer filters verbs for agent safety.
```

## Pack Extension Model

Packs are runtime extensions. They are composed by the binary at startup, not called
by the runtime as dependencies.

**A pack may**:

- Register verbs with `VerbRegistry`
- Register entity type definitions (`EntityTypeDef`)
- Register note kinds (with `NoteKindSpec` when available)
- Register additive edge endpoint rules (`EDGE_RULES`)
- Declare a schema plan for its backend
- Provide verb handlers that call runtime operations

**A pack may NOT**:

- Create new `EntityKind` variants (ADR-001)
- Create new `EdgeRelation` variants (ADR-002)
- Bypass runtime validation
- Implement transport-specific behavior
- Require `khive-runtime` to depend on the pack crate
- Know about backend topology or cross-backend routing
- Depend on the SubstrateCoordinator

## Trust Boundary

The `kkernel` modes embody different trust contexts:

**Operator context (`kkernel`)**:

- Full verb surface, no gating
- Direct backend access for admin ops (sync, migrate, check)
- Pack introspection for tooling
- Assumes the caller is a trusted operator

**Caller-facing context (`kkernel mcp`)**:

- Gated verb surface: policy decides which verbs are agent-safe
- No direct backend access
- No admin operations
- Assumes the caller is an untrusted AI agent

This separation is architectural, not just a configuration flag. The gate enforcement
layer exists only in MCP-serving mode. Operator subcommands run without that gate because
they already require trusted operator access.

**Deployment constraint**: Network adapters may expose only the MCP-serving mode. Operator
subcommands remain local or infrastructure-tier interfaces and must not be exposed through
the network adapter.

## Transport Boundary

All business operations enter through registered verbs. No transport owns khive semantics.

**A transport adapter may**:

- Spawn `kkernel mcp` as a subprocess
- Call MCP tools
- Expose HTTP endpoints that forward to registered verbs
- Adapt request/response formats

**A transport adapter may NOT**:

- Write directly to khive SQLite tables
- Implement create/link/search semantics independently
- Normalize `entity_type` independently
- Validate endpoint rules as a source of truth
- Bypass the SubstrateCoordinator for cross-backend operations

## Future Layer Contract

Future transports (HTTP gateway, web dashboard, CLI) must obey the same verb-first
boundary. They dispatch to `VerbRegistry` via one of the binaries, not to storage or
runtime operations directly.

| Layer              | Status  | Role                                      | Not allowed         |
| ------------------ | ------- | ----------------------------------------- | ------------------- |
| `kkernel mcp`      | current | stdio MCP transport backed by `khive-mcp` | business logic      |
| `khive-web`        | planned | HTTP adapter to registered verbs          | entity CRUD         |
| `khive-dashboard`  | planned | visual client over verb-backed endpoints  | ontology validation |
| Python smoke tests | current | subprocess caller over MCP stdio          | semantic SDK        |

Implementation details of future layers get their own ADRs when they ship.

## Anti-patterns

### 1. Verb logic in the binary

**Wrong**: `kkernel` or `khive-mcp` directly creates entities, links edges, or updates notes.
**Right**: Binaries adapt requests to `VerbRegistry` calls.

### 2. Business logic in `khive-request`

**Wrong**: DSL parser validates `entity_type`, edge endpoint legality, or namespace policy.
**Right**: `khive-request` parses syntax into typed requests. `khive-runtime` validates
semantics.

### 3. Bypassing `VerbRegistry` from the binary

**Wrong**: Binary calls runtime `create_entity` / `link_edge` directly for user-facing tools.
**Right**: User-facing capabilities enter through registered verbs.
**Exception**: Health, diagnostics, sync, and admin commands that do not mutate or query
KG semantics via the verb path may bypass `VerbRegistry`.

### 4. Entity CRUD in non-Rust layers

**Wrong**: Python/FastAPI or TypeScript reimplements create/search/link.
**Right**: External layers call a `kkernel` mode (subprocess) or a transport adapter
that dispatches into `VerbRegistry`.

### 5. Semantic validation in storage

**Wrong**: `khive-db` decides whether `snapshot` is a valid `entity_type` or whether
`Artifact → Dataset` is a legal `derived_from` edge.
**Right**: `khive-runtime` validates before persistence. `khive-db` enforces
persistence-level integrity only.

### 6. New verbs without packs

**Wrong**: Add a new top-level verb directly in runtime or the binary without a pack owner.
**Right**: Every verb belongs to a pack. Core KG verbs belong to `khive-pack-kg`.

### 7. Cross-backend routing in pack handlers

**Wrong**: Pack handler checks which backend a UUID lives on before operating.
**Right**: Pack code is single-backend. The SubstrateCoordinator resolves backend routing
before dispatching to the pack.

### 8. Gate enforcement in operator subcommands

**Wrong**: `kkernel` admin commands check agent safety policy before executing.
**Right**: `kkernel` is operator context: full trust, no gates. Gate enforcement is
exclusively the responsibility of `kkernel mcp`.

### 9. Pack code depending on coordinator

**Wrong**: Pack crate imports `kkernel::coordinator` or `SubstrateCoordinator`.
**Right**: Packs depend on `khive-runtime` only. The coordinator is kernel-internal
plumbing that packs never see.

## Rationale

### Why distinct subcommand modes?

The MCP server and admin CLI have different trust requirements. The MCP-serving mode must
gate which verbs are available, while operator commands must not be constrained by caller
safety policy. Separate subcommands make the trust boundary structural: `kkernel mcp`
installs the gate stack and operator subcommands do not.

### Why verb-first (not MCP-first)?

MCP is a transport protocol. The verb taxonomy is the stable abstraction. When HTTP or
web layers arrive, they dispatch into the same `VerbRegistry`. If the architecture is
"MCP-first," adding HTTP creates a second first-class surface with its own business logic.
If the architecture is "verb-first," HTTP is just another transport adapter.

### Why multi-backend (not single DB)?

Different data profiles need different storage characteristics. A hot KG database and a
300K-atom cold corpus have different VACUUM cadences, cache sizes, backup strategies,
and read-write patterns. Namespace isolation handles tenancy; backend isolation handles
storage profiles. These are orthogonal dimensions.

Single-backend remains the default. Multi-backend is opt-in via TOML configuration.
The SubstrateCoordinator ensures cross-backend composition works when needed.

### Why validation in runtime (not storage)?

Storage is a dumb persistence layer. It stores what it's told. Ontology rules (which
`entity_type` values are valid for which `EntityKind`, which edge endpoint triples are
legal) change when packs are loaded. Storage has no pack awareness. The runtime has the
full picture: loaded packs, registered vocabularies, endpoint rules, namespace context.

### Why packs as extension layer (not part of runtime)?

If pack logic lives in `khive-runtime`, every new pack requires changing the runtime crate.
If packs are separate crates that depend on runtime, adding a pack is adding a crate -
no runtime changes needed. The binary composes runtime + packs at startup.

### Why coordinator in kkernel (not a separate crate)?

The SubstrateCoordinator is kernel-internal dispatch plumbing. It is not a public library.
Packs do not need it; no external consumer needs it. Placing it inside `kkernel` keeps the
boundary tight and avoids adding a public surface for internal plumbing.

## Alternatives Considered

| Alternative                               | Why rejected                                                                                                                              |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Undifferentiated single execution mode    | Conflates operator and caller-facing trust boundaries. Separate subcommands install the correct gate policy.                              |
| Four-layer (Frontend/Deno/MCP/Crates)     | Three layers don't exist. Framing misleads.                                                                                               |
| MCP-first (transport is the architecture) | Breaks when HTTP arrives. Transport != architecture.                                                                                      |
| Validation in storage                     | Storage has no pack context. Wrong layer for ontology rules.                                                                              |
| Validation in parser                      | Parser is syntax-only. Semantic validation needs runtime state.                                                                           |
| Monolithic runtime (no packs)             | Every new verb requires changing runtime. Doesn't scale.                                                                                  |
| Single backend (namespace-only isolation) | Can't isolate VACUUM, can't read-only one slice, can't backup independently. Namespace handles tenancy; backend handles storage profiles. |
| Per-pack coordinator                      | Substrate-kind operations need a coordinator that sees ALL backends. Can't live inside one pack.                                          |
| Out-of-process coordinator                | Dramatic complexity for what is in-process plumbing.                                                                                      |

## Consequences

### Positive

- Engineers know where code belongs: verb logic → packs, validation → runtime,
  persistence → db, routing → coordinator, admin → operator subcommands,
  caller gating → `kkernel mcp` through the `khive-mcp` library.
- Adding a transport requires zero changes to packs, runtime, or coordinator.
- Adding a pack requires zero changes to runtime, coordinator, or transport.
- Adding a backend is TOML configuration, not code change.
- Agents see a stable verb vocabulary regardless of binary or transport evolution.
- Operator admin commands are structurally separated from agent-facing operations.

### Negative

- Multiple subcommand modes require explicit trust-boundary tests.
  Mitigated: mode construction installs a fixed gate policy rather than a runtime toggle.
- Multi-backend adds the SubstrateCoordinator layer.
  Mitigated: single-backend deployments see zero coordinator overhead.
- TOML configuration is a new operational surface.
  Mitigated: built-in defaults produce single-backend single-engine current-behavior shape.
- Cross-backend operations are non-atomic (SQLite WAL is per-backend).
  Mitigated: documented per-operation; operators keep dependent data on the same backend.

### Neutral

- Contributor documentation owns build commands and repository navigation. ADR-003 owns
  architectural invariants such as component boundaries and the dispatch model.
- MCP wire protocol is unchanged. Agents see the same `request` tool.
- The `request` DSL syntax is unchanged.

## Implementation status

The single-binary topology is shipped. `kkernel` declares the executable, and its `mcp`
subcommand delegates transport and gate enforcement to the `khive-mcp` library. Operator
subcommands and MCP-serving mode share runtime and pack implementations while retaining
separate construction paths for their trust policies.
