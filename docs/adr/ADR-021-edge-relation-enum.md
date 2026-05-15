# ADR-021: EdgeRelation Enum — Close the Substrate Taxonomy Loop

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

ADR-002 defines a closed set of edge relations (13 canonical names in 6 categories — 12
entity-to-entity relations plus `annotates` for cross-substrate). The natural Rust representation
stores `Edge.relation` as `String` and validates against the canonical set at write time. This is
the same shape that entity kinds and note kinds had before ADR-001 and ADR-019:

- No compile-time guarantees.
- Validation happens at storage boundaries but the type is unconstrained downstream.
- Agents and MCP tools have to re-validate everywhere relations cross the API.
- One forgotten validation site, and the closed set silently leaks.

ADR-019 closed this loop for `NoteKind`. ADR-001 already closed it for `EntityKind`. EdgeRelation is
the last free-string field in the core substrate.

### Relation count

| Category       | Relations                                              | Count  |
| -------------- | ------------------------------------------------------ | ------ |
| Structure      | `contains`, `part_of`, `instance_of`                   | 3      |
| Derivation     | `extends`, `variant_of`, `introduced_by`, `supersedes` | 4      |
| Dependency     | `depends_on`, `enables`                                | 2      |
| Implementation | `implements`                                           | 1      |
| Lateral        | `competes_with`, `composed_with`                       | 2      |
| Annotation     | `annotates`                                            | 1      |
| **Total**      |                                                        | **13** |

## Decision

**Define `EdgeRelation` as a closed enum in `khive-types`. 13 variants. No `Default` impl** — every
edge requires an explicit relation, there's no sensible fallback.

```rust
// crates/khive-types/src/edge.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRelation {
    // Structure
    Contains,
    PartOf,
    InstanceOf,
    // Derivation
    Extends,
    VariantOf,
    IntroducedBy,
    Supersedes,
    // Dependency
    DependsOn,
    Enables,
    // Implementation
    Implements,
    // Lateral
    CompetesWith,
    ComposedWith,
    // Annotation (cross-substrate: note → anything)
    Annotates,
}

impl EdgeRelation {
    pub const ALL: [Self; 13] = [
        Self::Contains, Self::PartOf, Self::InstanceOf,
        Self::Extends, Self::VariantOf, Self::IntroducedBy, Self::Supersedes,
        Self::DependsOn, Self::Enables,
        Self::Implements,
        Self::CompetesWith, Self::ComposedWith,
        Self::Annotates,
    ];

    pub const fn category(&self) -> EdgeCategory {
        match self {
            Self::Contains | Self::PartOf | Self::InstanceOf => EdgeCategory::Structure,
            Self::Extends | Self::VariantOf | Self::IntroducedBy | Self::Supersedes => EdgeCategory::Derivation,
            Self::DependsOn | Self::Enables => EdgeCategory::Dependency,
            Self::Implements => EdgeCategory::Implementation,
            Self::CompetesWith | Self::ComposedWith => EdgeCategory::Lateral,
            Self::Annotates => EdgeCategory::Annotation,
        }
    }
}

pub enum EdgeCategory { Structure, Derivation, Dependency, Implementation, Lateral, Annotation }

impl std::fmt::Display for EdgeRelation { /* snake_case strings, matching ADR-002 */ }
impl std::str::FromStr for EdgeRelation { /* case-insensitive parsing; reject unknown */ }
```

The `Edge.relation` field type changes from `String` to `EdgeRelation`. The MCP layer continues to
accept strings on the wire (for agent ergonomics) but parses through `EdgeRelation::from_str` at the
boundary — invalid strings return `invalid_params` with the full list of valid relations.

### Why `category()` accessor?

Agents working with graph patterns frequently ask "what kind of relation is this?" — structural vs
derivational vs lateral. Exposing the 6-category grouping as a method enables future queries like
"show me all derivational edges from X" without re-deriving the grouping in every consumer.

## Wire format

JSON wire format uses snake_case strings, matching ADR-002:

```json
{ "relation": "extends" }
```

Aliases accepted by `FromStr` (case-insensitive, hyphen-tolerant):

- `extends` / `Extends` / `EXTENDS` — all parse to `EdgeRelation::Extends`
- `part_of` / `part-of` / `partof` — all parse to `EdgeRelation::PartOf`
- `introduced_by` / `introduced-by` — `EdgeRelation::IntroducedBy`

This is the same parsing pattern as `NoteKind` (ADR-019) and `EmbeddingModel` (in lattice-embed):
permissive read, canonical write.

## Rationale

### Why a closed enum instead of validated string?

Three reasons that compound:

1. **Compile-time discipline**. Rust's exhaustive match forces every consumer that switches on
   relation to handle all 13 variants. Adding a 14th would be impossible without updating every
   match — which is the point: adding a relation should be an ADR-level decision, not a silent
   string addition.

2. **Single validation site**. With `String`, every API surface needs to remember to validate. With
   enum, validation happens exactly once — at `FromStr` parsing. Downstream code can assume
   validity.

3. **Consistency**. ADR-001 (entity kinds), ADR-019 (note kinds), and now ADR-021 (edge relations)
   all follow the same pattern. Three out of three substrate kinds use closed enums; the
   inconsistency was the third one.

### Why no `Default` for `EdgeRelation`?

`NoteKind` defaults to `Observation` because `create(kind="note", content="...")` is a valid call
without specifying `note_kind` — the system has a sensible "I noticed X" semantic. `Edge` has no
analogous default: every edge expresses a _specific_ relationship, and "I don't know what relation"
isn't a useful state. Forcing the caller to specify a relation matches reality.

### Why expose `EdgeCategory`?

Two consumers benefit:

- **Query planners** (future, ADR-008 Cypher phase): "find all derivational paths" → filter by
  `EdgeCategory::Derivation`.
- **UI rendering**: different visual styles per category (containment trees vs. derivation chains
  vs. lateral relations).

The category is intrinsic to the relation — making it a `const fn` accessor costs nothing and avoids
re-encoding the grouping in every consumer.

## Alternatives Considered

| Alternative                                               | Pros                       | Cons                                                                              | Why rejected                                               |
| --------------------------------------------------------- | -------------------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| Keep as validated `String`                                | No type change ripple      | Same drift risk, validation-everywhere overhead, inconsistent with ADRs 001 + 019 | Inconsistency is the cost                                  |
| Add `Default::Extends`                                    | More ergonomic for callers | Hides errors — caller forgets to specify and gets a quietly-wrong edge            | Explicit > implicit here                                   |
| Newtype `EdgeRelation(String)` with validated constructor | Lighter type change        | Doesn't get exhaustive-match benefit                                              | Enum wins                                                  |
| Add an `Other(String)` escape hatch variant               | Allows custom relations    | Defeats the entire closed-set discipline                                          | Hard no — same reason ADR-002 closed it in the first place |

## Consequences

### Positive

- The substrate taxonomy is now fully typed: `EntityKind` (6), `EdgeRelation` (13), `NoteKind` (5).
  No free-string drift left.
- Adding a relation becomes an ADR amendment to ADR-002 + ADR-021, not a silent code addition.
- Pattern-match exhaustiveness catches missed handling at compile time.
- `category()` accessor unlocks future filter patterns and UI grouping.

### Negative

- Changes the type of `Edge.relation` across `khive-types`, `khive-storage`, `khive-db`,
  `khive-runtime`, `khive-mcp`. Ripple is mechanical (search-replace + add `.parse()`/`.to_string()`
  at boundaries).
- Existing serialized data with non-canonical relation strings would fail to deserialize. Pre-alpha
  — no production data — so this is acceptable. Document in the migration notes when v0.2 ships.

### Neutral

- Sibling ADRs already follow this pattern; this is the consistency move, not a novel decision.

## Implementation Plan

1. **Add `EdgeRelation` + `EdgeCategory` enums** in `crates/khive-types/src/edge.rs` (new file or
   extend existing). ~80 LOC + tests for `FromStr` + `Display` + `category()` + 13-variant constant.

2. **Migrate `khive-storage::Edge.relation`**: change from `String` to `EdgeRelation`. SQL column
   stays TEXT — serialize via `Display`, deserialize via `FromStr` at the read boundary.

3. **Migrate `khive-storage::types::EdgeFilter.relations`**: from `Vec<String>` to
   `Vec<EdgeRelation>`.

4. **Migrate `khive-runtime`**:
   - `link(..., relation: EdgeRelation, ...)` instead of `&str`.
   - `update_edge(..., relation: Option<EdgeRelation>, ...)`.
   - `list_edges` filter uses `Vec<EdgeRelation>`.
   - Drop the runtime-level `validate_relation` helper — the type system does it now.

5. **Migrate `khive-mcp`**:
   - Edge `relation` stays a `String` on the wire (for agent ergonomics); handlers call
     `EdgeRelation::from_str` before dispatching and return `invalid_params` with the full canonical
     list on parse failure.
   - Update tool descriptions to clarify the 13 valid values.

6. **Sweep documentation**: confirm every reference to the relation count says "13 canonical
   relations" and every reference to category count says "6 categories" (including `Annotation`).

7. **Tests**:
   - `EdgeRelation::from_str` roundtrips for all 13.
   - `EdgeRelation::from_str("Extends")` and `EdgeRelation::from_str("extends")` both succeed
     (case-insensitive).
   - `EdgeRelation::from_str("related_to")` fails with a clear error listing valid options.
   - `category()` returns the right `EdgeCategory` for each (including `Annotates` → `Annotation`).
   - Wire roundtrip: serialize an `Edge`, deserialize, equal.

## Open Questions

1. **Should `EdgeRelation::ALL` be a public constant or a method?** v0.1: constant. Const arrays
   cost nothing and let callers iterate without invoking a function.
2. **Should the inverse-relation table (`extends` ↔ `extended_by`) be encoded here?** Not in v0.1.
   Inverse relations are a _view_ concern, not a substrate concern. If we add inverse-relation
   queries later, encode the mapping then.
3. **What about future extensions?** ADR amendment process: bump ADR-002 + ADR-021 together; add the
   variant; update all consumers via the compiler errors. Discipline by design.

## References

- ADR-001: Entity Kind Taxonomy (same pattern — closed enum, 6 variants)
- ADR-002: Closed Edge Ontology (defines the 13 canonical relations — this ADR types them)
- ADR-019: Note Kind Taxonomy (closed enum, 5 variants — companion in the substrate-typing trilogy)
- ADR-024: Note Search + Cross-Substrate Navigation (uses the `annotates` relation to make notes
  first-class graph nodes)
