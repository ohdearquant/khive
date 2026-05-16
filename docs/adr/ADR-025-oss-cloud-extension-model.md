# ADR-025: OSS Foundation + Cloud Extension Model

**Status**: accepted\
**Date**: 2026-05-16\
**Authors**: Ocean, lambda:khive

## Context

khive-oss ships as a standalone Apache-2.0 library: 7 crates (`khive-types`, `khive-score`,
`khive-storage`, `khive-db`, `khive-query`, `khive-runtime`, `khive-mcp`), 11 MCP verbs, local
SQLite storage. The proprietary cloud service (khive.ai) adds multi-tenancy, billing, and a richer
verb surface.

Two structural questions need a decision before the cloud layer lands:

1. **Who owns the core abstractions?** During initial development the cloud repo was the parent;
   khive-oss was extracted from it. Going forward, which direction do fixes and improvements flow?
2. **How does the cloud layer add functionality?** Does it reimplement the KG/storage/query logic,
   fork it, or extend it as a Rust crate dependency?

Without an explicit decision, fixes applied to khive-oss will diverge from the cloud copy, and
feature work in the cloud will silently reinvent primitives already tested in the OSS layer.

ADR-003 (Four-Layer Architecture) defines the layer boundaries for the OSS deployment: frontend →
Deno → khive-mcp (stdio) → crates. This ADR refines that boundary specifically for cloud
deployment, where `khive-mcp` is consumed as a crate dependency rather than a spawned process and a
new `khive-server` crate replaces the Deno HTTP gateway.

## Decision

**khive-oss is the source of truth. The cloud service is a thin extension layer that depends on
`khive-runtime` and `khive-mcp` as crate dependencies.**

### Dependency inversion

The dependency arrow is permanently reversed. khive-oss is the source of truth; the cloud service
depends on it. Improvements flow OSS → cloud via version bump, not from cloud back into OSS via
extraction.

```
khive-oss (Apache-2.0, crates.io)
  khive-types / khive-score / khive-storage / khive-db
  khive-query / khive-runtime / khive-mcp

      ↓ crate dep
      ↓

khive-cloud (proprietary)
  khive-auth    — OAuth 2.0 + API key provisioning
  khive-billing — Stripe metering + subscription management
  khive-pg      — Postgres/Turso backend (implements the ADR-005 capability traits: SqlAccess / GraphStore / VectorStore / TextSearch)
  khive-verbs   — ~15 proprietary verbs (memory, communication, GTD, lore, waves)
  khive-server  — HTTP gateway + multi-tenant session management
```

The cloud layer **does not reimplement** KG primitives, storage traits, hybrid search, or the GQL
compiler. Those live in khive-oss and are consumed as-is.

### Storage trait boundary

`khive-storage` (ADR-005) defines the capability traits (`SqlAccess`, `GraphStore`,
`VectorStore`, `TextSearch`) with zero implementations. khive-db provides the SQLite backend.
khive-pg provides the cloud Postgres/Turso backend. Both satisfy the same traits; the runtime is
backend-agnostic. Cloud Postgres schema changes flow through the same versioned migration system
defined in ADR-022 (adapted for Postgres DDL).

### Single MCP session — unified verb surface

In a cloud session, one MCP server presents the agent with the full verb set:

- **Base 11 verbs** — composed from `KhiveMcpServer` (mechanism in Open Questions).
- **~15 proprietary verbs** — served by khive-cloud alongside.

Agents see approximately 26 verbs in a single session and are unaware of the boundary. The 11
OSS verbs and the proprietary verbs coexist in the same tool list without naming conflicts because
the verb sets are disjoint (OSS: `create`, `get`, `list`, `update`, `delete`, `merge`, `search`,
`link`, `traverse`, `neighbors`, `query`; cloud: memory, communication, GTD, lore, waves).

### Namespace → API-key scoping

khive-oss namespace isolation (ADR-007) maps directly to per-tenant API-key scoping in the cloud.
A namespace string in OSS becomes a tenant identifier derived from the API key at the cloud
boundary. The OSS runtime enforces namespace isolation; the cloud layer only needs to route the
correct namespace string into the session.

### Transport

| Deployment | Transport | Verb contract |
| ---------- | --------- | ------------- |
| Local OSS  | stdio MCP | Identical     |
| Cloud      | HTTP MCP  | Identical     |

The verb contract is transport-agnostic. Switching transport does not change agent behavior.

## Rationale

### Why not keep two parallel implementations

Running a separate cloud copy of the KG/storage/query layer creates a maintenance burden that
compounds with time: every schema migration, every retrieval improvement, every bug fix needs to
be applied twice. The OSS layer is under active development (hybrid search, GQL/SPARQL, schema
migrations in ADR-022). Divergence is not hypothetical — it is the default outcome if there is no
explicit boundary.

Crate dependency eliminates divergence structurally: the cloud simply receives updates on the
next version bump.

### Why not fully separate cloud reimplementation

A reimplementation starting from the same storage concepts would converge on the same abstractions
(typed entities, closed edge ontology, namespace isolation) with worse test coverage and no
community benefit. The OSS layer already has the design work done.

### Why not fork

A fork loses the ability to pull upstream improvements. khive-oss is small enough (7 crates) that
maintaining a patch fork is more expensive than tracking the upstream dependency directly.

### Why compose base verbs instead of reimplementing them

Reimplementing the 11 OSS verbs in the cloud layer would require maintaining parity with the OSS
implementations indefinitely — a weaker form of the "two parallel implementations" problem.
Composing `KhiveMcpServer` from khive-oss into the cloud handler ensures the base verb behavior is
always identical, and updates to those verbs (e.g., a fix to `traverse` depth limits) automatically
appear in the cloud. The exact composition mechanism is unvalidated and tracked in Open Questions.

## Alternatives Considered

| Alternative                         | Pros                        | Cons                                                                            | Why rejected                              |
| ----------------------------------- | --------------------------- | ------------------------------------------------------------------------------- | ----------------------------------------- |
| Two parallel implementations        | Decoupled release cycles    | Maintenance doubles; fixes applied twice; divergence is inevitable              | Structural divergence over time           |
| Full cloud reimplementation         | No OSS stability constraint | Converges on same design, worse test coverage, no community benefit             | Wasteful duplication of design work       |
| Fork khive-oss as cloud base        | Full control over both      | Loses upstream improvements; patch fork is more expensive than tracking the dep | Upstream tracking is cheaper              |
| Cloud reimplements the 11 OSS verbs | No in-process delegation    | Parity maintenance; bugs fixed in OSS do not appear in cloud automatically      | Weaker form of two-implementation problem |

## Consequences

### Positive

- **Single source of truth**: KG semantics, storage traits, hybrid search, and query compilation
  live in one place. Fixes ship to both OSS and cloud on the same version bump.
- **Faster iteration**: cloud feature work focuses on auth, billing, and proprietary verbs without
  re-solving storage problems.
- **Identical verb contract**: agents targeting OSS and agents targeting the cloud use the same 11
  base verbs. Migrations between deployments require no agent-side changes.
- **Community benefit**: improvements developed for the cloud (retrieval tuning, schema
  migrations) are upstreamed to the OSS layer and vice versa.

### Negative

- **OSS API stability is a hard constraint**: the cloud service is pinned to a specific
  khive-oss version. Breaking changes to the 11 OSS verbs or the storage traits require a
  coordinated cloud upgrade.
- **Version coupling**: slow OSS release cadence would delay cloud features that depend on
  upstream changes. Mitigated by khive-oss being a small, focused crate set under active
  development by the same team.
- **Repo inversion cost**: the current parent→child relationship between the cloud repo and
  khive-oss must be reversed — extracting shared code, re-vendoring, and wiring khive-oss as a
  crate dependency carries a one-time migration effort.

### Neutral

- **Licensing boundary = the crate-dependency edge**: everything above the edge (khive-cloud
  crates) is proprietary. Everything at or below the edge (khive-oss crates) is Apache-2.0.
  The Rust crate boundary is the first-party license boundary. Transitive OSS dependencies
  (rmcp, rusqlite, sqlite-vec, etc.) carry their own licenses that flow into the proprietary
  binary; a license audit of the full transitive closure is required before commercial
  distribution.
- **The OSS crate set does not grow for cloud needs**: proprietary verbs, Postgres backend, auth,
  and billing are cloud crates. The 7 OSS crates stay scoped to local KG functionality.

## Relationship to ADR-011

ADR-011 makes Deno the only user-facing server and `khive-mcp` the only Rust binary for v0.1 OSS
local deployment. That decision is **unchanged for OSS local use**: local users continue to run
Deno + stdio MCP exactly as ADR-011 specifies.

This ADR **amends ADR-011 for cloud deployment only**: the cloud adds `khive-server`, a Rust HTTP
gateway crate, as the entry point for multi-tenant HTTP MCP sessions. The Deno gateway is not
deployed in the cloud path because Deno's edge-deployability advantage is irrelevant there and the
cloud benefits from having a single-language (Rust) trust boundary. This is a **scoped amendment**
to ADR-011's "exactly one Rust binary / Deno-only server" rule; it does not affect OSS.

## Open Questions

1. **Unified tool surface composition (UNVALIDATED — primary technical risk)**: the cloud handler
   must present ~26 verbs in a single MCP session. `KhiveMcpServer` in khive-oss uses
   `#[tool_router]` on its `impl` block, which generates a private default router bound to that
   type; `#[tool_handler]` then wires that single router. rmcp 1.7 has no documented public API to
   merge two independent `ToolRouter` instances into one `ServerHandler`. Two candidate mechanisms
   exist, neither validated:
   - **(a) Custom ToolRouter merge**: reflect on or re-export rmcp internals to combine the
     OSS router and the cloud router into a single dispatch table at startup.
   - **(b) In-process MCP proxy**: the cloud handler embeds a `KhiveMcpServer` instance and
     forwards the 11 base verbs to it in-process, acting as an MCP-to-function bridge without
     exposing two separate routers to rmcp.
     This must be prototyped and validated before the cloud MCP session design is considered final.

2. **Down migrations for Postgres (cloud-only)**: ADR-022 does not implement down migrations.
   Rollback strategy for cloud Postgres schema changes requires a separate decision.

## References

- ADR-003: Four-Layer Architecture (the layer boundary this ADR refines for cloud deployment)
- ADR-005: Storage Capability Traits (the ADR-005 capability traits the Postgres backend implements)
- ADR-007: Namespace as Open String (namespace isolation maps to API-key scoping)
- ADR-011: Deno Server + MCP-Only Integration (amended by this ADR for cloud deployment)
- ADR-022: Schema Migrations (migration system extended for cloud Postgres backend)
- ADR-023: Verb-Consolidated MCP Surface (the 11-verb contract composed into the cloud session)
- khive-ai issue #23: cloud extension model tracking issue
