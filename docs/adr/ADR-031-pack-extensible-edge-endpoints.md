# ADR-031: Pack-Extensible Edge Endpoints

**Status**: accepted\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

[ADR-002](ADR-002-edge-ontology.md) and [ADR-021](ADR-021-edge-relation-enum.md) close the
relation taxonomy at 13 members and define a three-case endpoint contract:

- `annotates`: note source → any substrate target.
- `supersedes`: same-substrate (note→note or entity→entity).
- All other 11 relations: entity→entity.

The contract was deliberately conservative — for KG semantics, "concept A depends_on concept B"
is the canonical shape, and a research-style note like an observation shouldn't `depends_on`
another observation.

[ADR-025](ADR-025-pack-standard.md) made note kinds and entity kinds pack-extensible: the GTD
pack adds the `task` note kind without touching `khive-types`. But the endpoint contract stayed
hardcoded in the runtime. That worked until [ADR-026](ADR-026-gtd-pack.md) introduced task
dependencies — a task note semantically can and should depend on another task note, but the
runtime would reject the edge as a violation of "non-`annotates` relations must be
entity→entity".

The choices were:

1. **Drop the edge claim.** Store `depends_on` only in `properties`. Loses graph traversal for
   blockers ("what blocks task X?" can't be a one-hop query).
2. **Open the closed relation enum.** Lets packs invent new relation names. Breaks the
   closed-taxonomy invariant that makes traversal predictable across packs.
3. **Keep the relations closed; make the per-relation endpoint rules pack-extensible.**

Option 3 is what this ADR formalises. The taxonomy stays semantically stable; vocabulary growth
happens at the endpoint level where it belongs.

## Decision

### `Pack` gets an `EDGE_RULES` const

```rust
// crates/khive-types/src/pack.rs

pub enum EndpointKind {
    NoteOfKind(&'static str),    // matches notes whose `kind` field is the literal
    EntityOfKind(&'static str),  // matches entities whose `kind` field is the literal
}

pub struct EdgeEndpointRule {
    pub relation: EdgeRelation,
    pub source: EndpointKind,
    pub target: EndpointKind,
}

pub trait Pack {
    // ... existing
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[];
}
```

`PackRuntime` (the object-safe mirror) gains an `edge_rules()` method that defaults to empty,
so existing packs that don't extend the contract require no changes.

### `VerbRegistry::all_edge_rules()` aggregates them

The runtime registry collects `EDGE_RULES` from every registered pack into a single slice. Order
follows pack registration; duplicates are harmless (validation is a membership check).

### Runtime validation consults pack rules after base rules

`KhiveRuntime` carries an installed copy of the aggregated rules (`Arc<RwLock<Vec<…>>>`) shared
across runtime clones. The transport layer (`khive-mcp`) calls
`runtime.install_edge_rules(registry.all_edge_rules())` immediately after the registry is built.

`validate_edge_relation_endpoints` then resolves both endpoints, and:

1. If the ADR-002 base contract accepts the triple → OK.
2. Else if any installed pack rule's `(relation, source, target)` matches the resolved triple → OK.
3. Else → `InvalidInput` with the same error message as the pre-ADR-031 surface.

Rules are **additive only**. A pack cannot tighten the base contract; it can only broaden it.
The closed `EdgeRelation` enum is unchanged.

### GTD pack declares one rule

```rust
const EDGE_RULES: &[EdgeEndpointRule] = &[
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::NoteOfKind("task"),
        target: EndpointKind::NoteOfKind("task"),
    },
];
```

This makes `link(task_a, task_b, depends_on)` legal — and only that shape. A task → entity, or
task → observation, still fails (no rule covers it).

## Rationale

### Why closed relations + open endpoints

The 13 relations are semantic primitives. Their meaning is stable across packs — `depends_on`
always means "X cannot complete without Y" regardless of substrate. Opening the relation set
would mean a pack could introduce a `blocked_by` or `precedes` synonym, leading to vocabulary
fragmentation that no traversal can rationalise.

Endpoint rules, by contrast, are pack-specific: only GTD knows what a "task" is. Pushing the
contract into pack metadata lets each pack declare its own semantic surface without touching
the closed taxonomy.

### Why additive only

Packs adding endpoint pairs is local: GTD's `task→task depends_on` doesn't affect any other
pack. Packs _removing_ base-contract pairs would be a global change masquerading as local
metadata, and would create order-of-registration dependencies. Strict additivity is the
property that keeps the system composable.

### Why per-instance Arc<RwLock<…>> on KhiveRuntime

The runtime is created before packs are registered. Packs receive cloned runtime handles. To
share the installed rules across clones without re-architecting the runtime construction
sequence, the rules live in an `Arc<RwLock<Vec<EdgeEndpointRule>>>` that all clones see.
`install_edge_rules` is the single mutation point, called once during transport startup. After
installation the rules are read-only for the lifetime of the binary.

### Why not extend `annotates` or `supersedes`

In principle the same mechanism could broaden `annotates` or `supersedes`. `annotates` is
already maximally permissive (note → any), so extension is mostly meaningless. `supersedes` is
structural (same-substrate by definition); pack-allowed cross-substrate supersession would
violate the structural meaning, so packs should not declare such rules. No restriction is
enforced in code, but the convention is to extend only the 11 entity-default relations.

## Alternatives Considered

| Alternative                                                        | Pros                           | Cons                                                                   | Why rejected                              |
| ------------------------------------------------------------------ | ------------------------------ | ---------------------------------------------------------------------- | ----------------------------------------- |
| Drop the edge claim from ADR-026; properties-only `depends_on`     | No taxonomy churn              | Loses graph traversal for blockers ("what blocks X?" can't be one-hop) | Removes a semantically useful query path  |
| Open the closed `EdgeRelation` enum to pack-declared relations     | Maximum extensibility          | Breaks the universal vocabulary; fragments traversal across packs      | Closed taxonomies are a core design goal  |
| Wildcard endpoints: pack declares `task→any` rather than specifics | Less ceremony for pack authors | Same vocabulary-drift risk as opening the relation enum                | Specificity is what makes rules auditable |
| Allow packs to _remove_ base-rule pairs                            | More flexible                  | Order-dependent surface; one pack can invalidate another               | Strict additivity preserves composability |

## Consequences

### Positive

- Task dependencies are graph-traversable, not just property-encoded.
- The closed relation enum stays closed.
- Future packs (workflow, calendar, project tracking) can extend endpoints without touching
  `khive-types` core.
- Validation is centralised: one runtime function consults both base and pack rules.

### Negative

- Adds one trait const and one runtime method to the pack ABI surface.
- Pack authors must reason about what endpoint pairs make semantic sense — there is no
  compile-time check that a rule's `(relation, source, target)` is meaningful, only that it
  unblocks a previously-rejected combination.

### Neutral

- Pack rules cannot break the base contract — clients that only use kg semantics see no change.

## Implementation Status

| Step                                                             | Where                                             | Status |
| ---------------------------------------------------------------- | ------------------------------------------------- | ------ |
| 1. `EdgeEndpointRule` + `EndpointKind` types                     | `crates/khive-types/src/pack.rs`                  | done   |
| 2. `Pack::EDGE_RULES` const + `PackRuntime::edge_rules()` method | `crates/khive-types/src/pack.rs`, runtime mirror  | done   |
| 3. `VerbRegistry::all_edge_rules()` aggregator                   | `crates/khive-runtime/src/pack.rs`                | done   |
| 4. `KhiveRuntime::install_edge_rules` + storage                  | `crates/khive-runtime/src/runtime.rs`             | done   |
| 5. Validator consults pack rules after base rules                | `crates/khive-runtime/src/operations.rs`          | done   |
| 6. Transport installs rules after registry build                 | `crates/khive-mcp/src/server.rs`                  | done   |
| 7. GTD pack declares `task→task depends_on` rule                 | `crates/khive-pack-gtd/src/lib.rs`                | done   |
| 8. Integration tests prove edges land + non-matching shapes fail | `crates/khive-pack-gtd/tests/integration.rs`, MCP | done   |

## References

- [ADR-002](ADR-002-edge-ontology.md): Closed Edge Ontology (the base endpoint contract)
- [ADR-021](ADR-021-edge-relation-enum.md): EdgeRelation enum (the closed taxonomy this ADR
  preserves)
- [ADR-025](ADR-025-pack-standard.md): Pack Standard (the composition mechanism this ADR extends)
- [ADR-026](ADR-026-gtd-pack.md): GTD pack (the first consumer of pack-extensible endpoints)
