# ADR-023: Verb-Consolidated MCP Surface

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

The MCP surface needs to expose CRUD-shaped operations across multiple observable kinds: entities,
edges, notes (and later — commits, branches, diffs once versioning ships in ADR-015).

A naming pattern with one tool per `<kind>_<op>` combination grows linearly: 3 kinds × 5 CRUD ops =
15 tools just for CRUD. Add commits/branches/events and the surface heads toward 25–30 tools. Agents
reading the tool list see a wall of similar-looking names and have to learn which kind goes with
which prefix.

The verb-consolidated alternative trades that for a `kind=` discriminant: one `create`, one `get`,
one `update`, one `delete`, one `list` — each dispatches internally on `kind`. The surface stays
compact even as new kinds land.

This ADR commits to the verb-consolidated shape.

## Decision

The MCP surface is built around **verbs**, not nouns. Operations that apply to multiple observable
kinds take a `kind=<observable>` discriminant.

### Final tool list (v0.1)

| Tool        | Kinds it covers    | Notes                                                                                                                                                                                                                  |
| ----------- | ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `create`    | entity, note       | Discriminated by `kind`. Entity needs `entity_kind` field for EntityKind (concept/document/...); note needs `note_kind` for NoteKind                                                                                   |
| `get`       | entity, edge, note | Discriminated by `kind`. Returns `null` if not found                                                                                                                                                                   |
| `list`      | entity, edge, note | Structured filter per kind                                                                                                                                                                                             |
| `update`    | entity, edge, note | Patch semantics per ADR-014                                                                                                                                                                                            |
| `delete`    | entity, edge, note | `hard` flag for entity/note; edges always hard                                                                                                                                                                         |
| `merge`     | entity, note       | Dedupe two records into one. Per ADR-014 for entities; for notes, content/properties merge by strategy, tags union, edges/references rewired                                                                           |
| `supersede` | entity, note       | Mark a record as obsoleted by a newer one. History-preserving (the old record stays). Creates a `supersedes` edge `new --supersedes--> old` for both kinds (notes and entities are both first-class nodes per ADR-024) |
| `link`      | (edges only)       | Kind-agnostic — making an edge is always one verb. Args: source_id, target_id, relation, weight                                                                                                                        |
| `traverse`  | (graph)            | Multi-hop traversal from roots                                                                                                                                                                                         |
| `neighbors` | (graph)            | Immediate neighbors of a node                                                                                                                                                                                          |
| `query`     | (graph query)      | GQL/SPARQL query string                                                                                                                                                                                                |
| `search`    | entity, note       | Semantic search (was `recall` for notes; extends to entity hybrid_search). `kind=` selects target.                                                                                                                     |
| `request`   | (batch dispatcher) | Per ADR-020 — composes any of the above                                                                                                                                                                                |

**13 tools.** Compact enough to scan in one screen, even when commits/branches ship. ADR-024 adds
one more (`resolve`) for cross-substrate UUID lookup, bringing the v0.1 total to 14.

### `merge` and `supersede` — semantic distinction

The two are **not interchangeable**:

- **`merge(kind, into, from)`** — "these two records describe the same thing; dedupe them." Used
  when the agent realizes "LoRA" and "Low-Rank Adaptation" are duplicate concept entities, or two
  `observation` notes about the same fact. Properties combine per strategy; tags union;
  edges/references rewire to the kept record; the `from` record is removed.

- **`supersede(kind, new_id, old_id)`** — "this newer record corrects or replaces an older one."
  Used when an agent's understanding evolves: an earlier `decision` is revised, an `observation` is
  refined. The old record stays in storage as history (filtered from default `search` results), with
  a `supersedes` edge from the new record to the old one. The new record is the active truth.

Merging destroys distinct-record history; supersession preserves it. Both are common agent
operations.

### Versioning tools (when ADR-015 ships)

`commit`, `branch`, `checkout`, `merge_branch`, `log`, `diff`, `apply_diff` — these are verb-shaped
already (no `kind=` needed, they're already specific verbs on the version-control domain). They live
alongside the CRUD verbs without conflicting.

If we later add commits-as-observables (e.g., `list(kind=commit)`), the verb surface absorbs them
without growing.

### `kind` parameter spec

Each multi-domain verb takes a `kind` parameter as a snake_case string discriminator:

- `kind="entity"` → routes to entity store. Additional required field `entity_kind` for `create`
  (concept|document|dataset|project|person|org per ADR-001).
- `kind="edge"` → routes to graph store. Used by `get`, `list`, `update`, `delete`.
- `kind="note"` → routes to note store. Additional required field `note_kind` for `create`
  (observation|insight|question|decision|reference per ADR-019; defaults to `observation`).

Unknown `kind` returns `invalid_params` with the valid options listed.

### Param-struct shape

Per-verb param structs in `crates/khive-mcp/src/tools/`:

```rust
pub struct CreateParams {
    pub kind: String,                // "entity" | "note"
    pub namespace: Option<String>,
    pub name: Option<String>,        // entity only
    pub entity_kind: Option<String>, // entity only — EntityKind value
    pub description: Option<String>, // entity only
    pub content: Option<String>,     // note only
    pub note_kind: Option<String>,   // note only — NoteKind value
    pub salience: Option<f64>,       // note only
    pub properties: Option<serde_json::Value>,
    pub tags: Option<Vec<String>>,
}

pub struct GetParams {
    pub kind: String,        // "entity" | "edge" | "note"
    pub namespace: Option<String>,
    pub id: String,          // UUID
}

pub struct UpdateParams {
    pub kind: String,                // "entity" | "edge" | "note"
    pub namespace: Option<String>,
    pub id: String,
    // Entity patch fields:
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub properties: Option<serde_json::Value>,
    pub tags: Option<Vec<String>>,
    // Edge patch fields:
    pub relation: Option<String>,
    pub weight: Option<f64>,
    // Note patch fields:
    pub content: Option<String>,
    pub note_kind: Option<String>,
    pub salience: Option<f64>,
}

pub struct DeleteParams {
    pub kind: String,
    pub namespace: Option<String>,
    pub id: String,
    pub hard: Option<bool>,  // entity/note; edges always hard
}

pub struct ListParams {
    pub kind: String,
    pub namespace: Option<String>,
    pub limit: Option<u32>,
    // Entity-specific filter:
    pub entity_kind: Option<String>,
    // Edge-specific filter:
    pub source_id: Option<String>,
    pub target_id: Option<String>,
    pub relations: Option<Vec<String>>,
    pub min_weight: Option<f64>,
    pub max_weight: Option<f64>,
    // Note-specific filter:
    pub note_kind: Option<String>,
}

pub struct SearchParams {
    pub kind: String,        // "entity" | "note"
    pub namespace: Option<String>,
    pub query: String,
    pub limit: Option<u32>,
}

pub struct MergeParams {
    pub kind: String,        // "entity" | "note"
    pub namespace: Option<String>,
    pub into_id: String,
    pub from_id: String,
    pub strategy: Option<String>,  // "prefer_into" | "prefer_from" | "union"
}

pub struct SupersedeParams {
    pub kind: String,        // "entity" | "note"
    pub namespace: Option<String>,
    pub new_id: String,      // the current/correct version
    pub old_id: String,      // the version being superseded
}
```

Per-kind irrelevant fields are simply ignored when present and omitted in the JSON-schema-friendly
way (all optional except `kind` and the kind-specific minimum).

### `remember` and `recall` are removed entirely

The agent surface has no `remember` or `recall`. Notes are created via
`create(kind="note", content="...", note_kind="observation", annotates=[...])`. Notes are searched
via `search(kind="note", query="...", limit=...)`.

Reason: `remember` and `recall` are loaded words for agents — they imply specific memory semantics
that may not match what's actually happening (the system stores a typed note with an explicit kind
and optional graph edges). Generic verbs (`create`, `search`) describe what the operation does
without overloading the agent's mental model. Agents that need "memory" semantics can wrap these
calls in their own application logic.

### What about `request` (ADR-020)?

`request` is a meta-tool: it batches the verbs above. Its DSL syntax stays exactly as ADR-020
specified — e.g.,
`[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B")]`.
The verb consolidation makes the DSL more uniform — every batched op is `verb(kind=..., args)`.

## Rationale

### Why verb-consolidation works here

A multi-domain surface with `<kind>_<op>` naming forces N × M tools (kinds × operations). Verb
consolidation gives N tools (one per operation, discriminated by `kind`). The crossover where
verb-consolidation wins happens around 2–3 kinds × 4–5 ops — we're past it:

- 3 observable kinds today (entity, edge, note).
- ADR-015 versioning adds commits, branches, diffs.
- Future kinds (events as observables, sub-namespaces, etc.) will compound the savings.

### Why keep `link`, `traverse`, `neighbors`, `query` as their own verbs?

`link` is the verb for "make an edge"; `create(kind="edge")` would work but
`link(source, target, relation)` reads naturally and is what graph people already say. Same for
`traverse` and `neighbors`. `query` is GQL/SPARQL — it's already a verb, not a CRUD op.

Verb consolidation is a tool to keep the surface compact; it's not a religion. Where a
domain-specific verb is more natural, keep it.

### Why `search` instead of folding into `list`?

`list` is structured filtering ("give me entities of kind X with tag Y"). `search` is
similarity-based ("give me entities semantically close to this query"). They use different machinery
(one runs SQL filters, the other runs hybrid retrieval). Conflating them under a single verb would
force a `mode=` flag that does most of the discrimination work — at which point the verbs are just
disguised.

Keep them separate. Both are short, both are intuitive, neither has a `kind=` ambiguity issue.

### Worked example — multi-create

Old (namespaced):

```
entity_create(kind="concept", name="LoRA")
entity_create(kind="concept", name="QLoRA")
remember(content="LoRA is a parameter-efficient fine-tuning method", kind="insight")
```

New (verb-consolidated):

```
create(kind="entity", entity_kind="concept", name="LoRA")
create(kind="entity", entity_kind="concept", name="QLoRA")
create(kind="note", note_kind="insight", content="LoRA is a parameter-efficient fine-tuning method")
```

Both are equally readable. The new form makes the dispatch explicit. Batched via `request`:

```
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"QLoRA\")]")
```

## Alternatives Considered

| Alternative                                                                        | Pros                                                    | Cons                                                                                | Why rejected                             |
| ---------------------------------------------------------------------------------- | ------------------------------------------------------- | ----------------------------------------------------------------------------------- | ---------------------------------------- |
| Namespaced names (`entity_*`, `edge_*`, `note_*`)                                  | Schema-precise per tool; matches some other MCP servers | Surface grows linearly with kinds; agents see a wall of similar names               | Surface management wins as kinds grow    |
| Adopt a fully flat verb set with `type=` everywhere (including `link`, `traverse`) | Maximum consistency                                     | Forces domain-specific verbs into generic shape — `link(type="edge", ...)` is silly | Verb consolidation is a tool, not a rule |
| Two surfaces — namespaced for power users, verbs for agents                        | Both available                                          | Maintenance cost doubles; documentation splits; agents have to learn the mapping    | One surface                              |
| Defer until versioning ships and revisit                                           | No churn now                                            | Compounding rework as more kinds land                                               | Refactor sooner is cheaper               |

## Consequences

### Positive

- 13 MCP tools in v0.1 (14 once ADR-024's `resolve` lands) instead of ~15 namespaced per-kind names
  — and growing more slowly as we add kinds.
- Every CRUD-style operation is one verb. New observable kinds (commits, events) just become new
  `kind=` values.
- Discriminated dispatch makes the implementation testable in isolation (per-kind routing logic is a
  small switch).
- `request` DSL becomes more uniform — every batched op is `verb(kind=..., args)`.

### Negative

- Wider param structs (all kinds' fields share one struct, most optional). The handler validates
  required-fields-by-kind. This is slightly less type-safe than per-kind structs.
- Param structs become wider (all kinds' fields in one struct, most optional). The handler validates
  required-fields-by-kind. This is mildly less type-safe than per-kind structs.
- Per-kind JSON schemas are harder to express; clients see "any of these fields, depending on kind".

### Neutral

- The runtime layer (`khive-runtime`) is untouched — ops like `create_entity`, `link`, `update_edge`
  stay as their concrete typed Rust signatures. The MCP layer is the dispatcher.
- The versioning model (commits/branches/merges) and the import/export design in ADR-015 use this
  verb-consolidated convention from the start. Versioning ships with verbs like `commit`, `branch`,
  `merge_branch` rather than `kg_commit`/`kg_branch`.

## Implementation Plan

1. **Build the MCP tool surface** in `crates/khive-mcp`:
   - Verb-shaped param structs (`CreateParams`, `GetParams`, etc.) — one per verb, with optional
     kind-specific fields.
   - One handler per verb that dispatches on `kind`.
   - Keep `link`, `traverse`, `neighbors`, `query`, `request` as their own verbs (domain-specific).
2. **Integration tests** in `crates/khive-mcp/tests/integration.rs` assert the verb count and
   per-kind dispatch.
3. **Tool descriptions** spell out verb semantics + valid `kind` values + per-kind required fields.
4. **`crates/khive-mcp/src/tools/`** module structure: one file per verb (`create.rs`, `get.rs`,
   `list.rs`, `update.rs`, `delete.rs`, `search.rs`, `merge.rs`, `link.rs`, `traverse.rs`,
   `neighbors.rs`, `query.rs`, `request.rs`, `supersede.rs`).
5. **Documentation sweep** (CLAUDE.md, AGENTS.md, README.md) reflects the verb surface.

## Open Questions

1. **Should `kind` accept aliases?** e.g., `kind="entities"` (plural) → entity. Consistent with the
   case-insensitive parsing in EntityKind/NoteKind/EdgeRelation. Probably yes — same pattern.
2. **Should the param structs be split internally (per-kind) for type safety?** v0.1 = wide struct,
   validate-by-kind in handler. v0.2 could introduce a tagged-enum param at the cost of more serde
   gymnastics. Defer.
3. **Should `search` accept a `mode=hybrid|semantic|keyword` flag?** v0.1 = always hybrid (FTS5 +
   optional vector). If users want pure keyword or pure semantic, they can use `query` (GQL/SPARQL
   filters) or pass a vector explicitly. Defer the flag until a user asks.

## References

- ADR-014: KG Curation Operations (the runtime operations this surface exposes)
- ADR-015: KG Versioning Model (versioning verbs land on this same surface)
- ADR-019: Note Kind Taxonomy (provides `NoteKind` for the `note_kind` field)
- ADR-020: Request DSL (composes the verb surface)
- ADR-021: EdgeRelation Enum (provides the relation values for edge operations)
- ADR-024: Note Search + Cross-Substrate Navigation (adds the `resolve` verb and the
  `annotates`-edge wiring for `create(kind="note", ...)`)
