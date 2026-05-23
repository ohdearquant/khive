# ADR-010: KG Versioning — "GitHub for Knowledge Graphs"

**Status**: planned (strategic direction; implementation deferred to v0.4+)\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

Knowledge graphs today are mostly single-user or rely on database-level access control (per-tenant
isolation). There is no widely-adopted analog of git/GitHub for KGs:

- No clone / branch / commit / push / pull
- No diff between graph states
- No merge with conflict resolution
- No shared remotes for collaboration
- No social layer (forks, issues, PRs on graph changes)

Yet collaborative knowledge work is exactly where research KGs would shine:

- Multiple researchers contributing to the same domain graph
- Forking a domain ontology to specialize for a sub-field
- Pulling community-curated taxonomies into your private workspace
- Reviewing proposed entity additions before they merge

**Strategic insight**: khive's positioning is **GitHub for knowledge graphs**. The crates we're
building (substrate types, capability traits, deterministic scoring) are infrastructure. The
differentiation is collaboration + versioning + merge semantics on graph state.

## Decision

**Adopt "GitHub for knowledge graphs" as the strategic product direction.**

This ADR commits to building (in order of priority):

1. **KG diff** — compute changes between two graph states (entities added/removed/modified, edges,
   properties).
2. **KG merge** — combine two graph states with explicit conflict resolution.
3. **KG snapshots** — content-addressed snapshots of graph state at time T (the "commit" primitive).
4. **KG branches** — named pointers to snapshots; cheap to create, isolated for write.
5. **KG remotes** — push/pull snapshots between khive instances.
6. **Social layer** — forks, PRs, review on proposed changes (the "GitHub" layer above git).

### What this is NOT

- Not a re-implementation of git internals on top of graphs.
- Not a CRDT-style automatic merge (graph semantics make automatic merge unsafe).
- Not realtime collaboration (that's a different problem — operational transform, presence).
- Not in v0.1 / v0.2. The current crates are foundation; the versioning layer comes later.

## Rationale

### Why this positioning works

1. **The analogy is concrete**: Every developer understands git. "khive is to KGs what git is to
   code" is one sentence away from explaining the entire product.

2. **The gap is real**: Research collaboration on KGs is bottlenecked by _coordination_, not
   _technology_. Two researchers working on the same domain graph today have no way to merge their
   work safely.

3. **The infrastructure aligns**:
   - Substrate observables (Note/Entity/Event) are inherently versionable — they have
     created_at, updated_at, and an `Event` log already captures the operation history.
   - The closed edge ontology (ADR-002) makes diff/merge tractable — you can't have arbitrary
     conflicts because the relation vocabulary is bounded.
   - Namespace hierarchy (ADR-007) maps to git's "branch" concept — `local/main` vs
     `local/experimental` is a natural branching model.

4. **The competitive moat is the merge semantics**, not the storage. Anyone can store graph data.
   Almost nobody has thought hard about _what merging two graphs means_.

### Why graph merge is non-trivial

Unlike code (text), graphs have:

- **Identity conflicts**: Two contributors may create the same entity (e.g., both add
  "FlashAttention" as a Concept) with slightly different IDs and properties. Auto-deduplication
  requires identity reconciliation.
- **Edge consistency**: Removing an entity in branch A while adding edges to it in branch B creates
  a dangling-reference conflict. Resolution requires knowing user intent.
- **Property merge**: Branch A sets `properties.year = 2021`, branch B sets
  `properties.year = "2021"`. Type conflict. Resolution requires schema awareness.
- **Semantic merge**: Branch A links `LoRA -[extends]-> Adam`. Branch B links
  `LoRA -[extends]-> SGD`. Both are wrong — LoRA extends neither. Merging both produces a
  contradictory graph.

A useful merge tool must:

1. Detect conflicts at the _semantic_ level, not just the byte level.
2. Surface conflicts to the user with enough context to decide.
3. Support both auto-merge for non-conflicting changes and human review for semantic conflicts.

## Primitives We'll Need

### Snapshot

A content-addressed identifier (hash of all entities + edges + properties at time T). Snapshots are
immutable and shareable.

```rust
pub struct Snapshot {
    pub id: SnapshotId,            // hash
    pub parent: Option<SnapshotId>, // previous snapshot
    pub created_at: i64,
    pub namespace: Namespace,
    pub message: String,            // commit message
    pub author: Option<String>,     // who made the change
}
```

### Branch

A named pointer to a snapshot. Branches are mutable; snapshots are not.

```rust
pub struct Branch {
    pub name: String,        // "main", "experimental"
    pub namespace: Namespace, // the parent namespace
    pub head: SnapshotId,
}
```

### Diff

The minimal change set between two snapshots.

```rust
pub struct GraphDiff {
    pub added_entities: Vec<Entity>,
    pub removed_entities: Vec<Id128>,
    pub modified_entities: Vec<EntityChange>,  // before/after pairs
    pub added_edges: Vec<Edge>,
    pub removed_edges: Vec<LinkId>,
    pub modified_edges: Vec<EdgeChange>,
}
```

### Merge

Three-way merge: base + ours + theirs → result OR conflicts.

```rust
pub enum MergeResult {
    Clean(Snapshot),
    Conflicts(Vec<Conflict>),
}

pub enum Conflict {
    IdentityCollision { left: Entity, right: Entity },
    PropertyMismatch { entity_id: Id128, key: String, left: PropertyValue, right: PropertyValue },
    DanglingEdge { edge: Edge, missing_endpoint: Id128 },
    SemanticContradiction { explanation: String, edges: Vec<Edge> },
}
```

### Remote

A connection to another khive instance for push/pull.

```rust
pub struct Remote {
    pub name: String,        // "origin"
    pub url: String,         // grpc/http endpoint
    pub auth: Option<RemoteAuth>,
}
```

## Implementation Phasing

| Version      | Scope                                                             |
| ------------ | ----------------------------------------------------------------- |
| v0.1 (today) | Foundation crates ship. ADR documented.                           |
| v0.2         | `khive-query` crate. SPARQL/GQL on local graph.                   |
| v0.3         | Postgres backend.                                                 |
| **v0.4**     | **`khive-diff` crate: compute diffs between snapshots.**          |
| **v0.5**     | **`khive-merge` crate: three-way merge with conflict detection.** |
| **v0.6**     | **Snapshot + Branch primitives. Local commit/checkout.**          |
| **v0.7**     | **Remote push/pull. Multi-instance collaboration.**               |
| **v1.0**     | **Social layer: forks, PRs, review UI.**                          |

The phasing reflects "ship the foundation first, build the versioning layer on top." Each phase is
independently shippable and useful — diff alone is valuable for change review even without merge.

## Alternatives Considered

| Alternative                                     | Pros                           | Cons                                                                             | Why rejected               |
| ----------------------------------------------- | ------------------------------ | -------------------------------------------------------------------------------- | -------------------------- |
| Build versioning into v0.1                      | Strategic clarity from day one | Delays foundation; merge semantics need to be earned, not designed upfront       | Too much risk              |
| Skip versioning, focus on AI research pipelines | Faster to "useful demo"        | No differentiation from any other KG tool; commodity positioning                 | Strategically weak         |
| Wrap an existing VCS (git LFS, dolt)            | Reuse mature tooling           | None of them have _graph semantics_; merge would still be byte-level             | Wrong abstraction          |
| CRDT-based automatic merge                      | No conflicts                   | Graph semantics make CRDTs unsafe (see "semantic contradiction"); silently wrong | Worse than failing visibly |

## Consequences

### Positive

- Clear strategic positioning: "GitHub for knowledge graphs."
- Foundation crates have a long-term purpose, not just demo support.
- Merge semantics become a competitive moat.
- Open source community can contribute curated graphs (community taxonomies, paper databases).

### Negative

- Major investment (v0.4+ is significant work). Mitigated: phased, each phase is independently
  shippable.
- Merge UX is hard. Mitigated: start with conflict surfacing, not auto-resolution.
- "git for X" pitches have a graveyard (dolt, terminusdb, etc.). Mitigated: focus on the specific KG
  semantics that those tools miss.

### Neutral

- The product direction may attract a different user base than "research KG with AI agents." That's
  fine — they're complementary.

## Open Questions

1. **Storage of snapshots**: Content-addressed (like git objects) or branch-based (like database
   snapshots)? Probably content-addressed for sharing efficiency.

2. **Snapshot granularity**: Per-namespace, per-project, or fine-grained sub-graph? Likely
   per-namespace as the default unit.

3. **Conflict resolution UX**: CLI-driven (like git), web UI, or hybrid? Web UI is essential for the
   "GitHub" layer; CLI for power users.

4. **Federation protocol**: SPARQL endpoints? GraphQL? Custom Bolt-like protocol? TBD when v0.7 is
   in scope.

5. **Auth + permissions**: Per-snapshot ACLs? Per-namespace? Per-entity? Defer to v1.0 (social
   layer).

## References

- ADR-002: Closed Edge Ontology (makes merge tractable)
- ADR-004: Substrate Observables (Event log is the version history substrate)
- ADR-007: Namespace as Open String (branch model maps to namespace hierarchy)
- ADR-008: Query Layer Separation (will need diff/merge query primitives)
- Inspirations (and warnings): git, GitHub, dolt, terminusdb, atomicdata, hyperdrive
