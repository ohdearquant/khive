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
  khive-pg      — Postgres/Turso backend (implements khive-storage traits)
  khive-verbs   — ~15 proprietary verbs (memory, communication, GTD, lore, waves)
  khive-server  — HTTP gateway + multi-tenant session management
```

The cloud layer **does not reimplement** KG primitives, storage traits, hybrid search, or the GQL
compiler. Those live in khive-oss and are consumed as-is.

### Storage trait boundary

`khive-storage` (ADR-005) defines capability traits (`SqlAccess`, `GraphStore`, `VectorStore`,
`TextSearch`) with zero implementations. khive-db provides the SQLite backend. khive-pg provides
the cloud Postgres/Turso backend. Both satisfy the same traits; the runtime is backend-agnostic.

### Single MCP session — unified verb surface

In a cloud session, one MCP server presents the agent with the full verb set:

- **Base 11 verbs** — delegated to `KhiveMcpServer` from khive-oss unchanged.
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

### Why delegate base verbs instead of reimplementing them

Reimplementing the 11 OSS verbs in the cloud layer would require maintaining parity with the OSS
implementations indefinitely — a weaker form of the "two parallel implementations" problem. Calling
into `KhiveMcpServer` from khive-oss ensures the base verb behavior is always identical, and
updates to those verbs (e.g., a fix to `traverse` depth limits) automatically appear in the cloud.

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

### Neutral

- **Licensing boundary = the crate-dependency edge**: everything above the edge (khive-cloud
  crates) is proprietary. Everything at or below the edge (khive-oss crates) is Apache-2.0.
  The Rust crate boundary is the license boundary.
- **The OSS crate set does not grow for cloud needs**: proprietary verbs, Postgres backend, auth,
  and billing are cloud crates. The 7 OSS crates stay scoped to local KG functionality.

## References

- ADR-005: Storage Capability Traits (the trait surface the Postgres backend implements)
- ADR-007: Namespace as Open String (namespace isolation maps to API-key scoping)
- ADR-011: Deno Server + MCP-Only Integration (transport layer that cloud extends)
- ADR-023: Verb-Consolidated MCP Surface (the 11-verb contract the cloud delegates)
- khive-ai issue #23: cloud extension model tracking issue
