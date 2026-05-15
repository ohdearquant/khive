# ADR-024: Note Search Pipeline + Cross-Substrate Navigation

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

Two related gaps in the current v0.1 substrate:

1. **Note search is structured-only**. Note retrieval today does namespace + kind filtering and
   time-ordered pagination. There is no FTS5 indexing on notes and no vector embedding. An agent
   searching for "what did I learn about FlashAttention?" gets every note in the namespace, not
   relevant ones.

2. **No cross-substrate navigation**. Notes can record observations about entities, but there's no
   indexed pointer back. The MCP surface offers no way to ask "what notes talk about entity X?" or
   "what entities does note Y reference?" Cross-substrate is the whole point of having a graph — a
   research agent learns by traversing between observations (notes), facts (entities), and
   relationships (edges).

The insight that simplifies the design: **notes are nodes in the same graph**, not a separate
substrate that needs a special "links to" table. When a note annotates an entity, that's a real edge
with a real canonical relation (`annotates`). The graph machinery already does cross-substrate
navigation; we just need to land the edge type.

This ADR fixes both with a single coherent design.

## Decision

### Part 1 — Note retrieval pipeline

Auto-index notes on `create(kind="note", ...)` into both FTS5 and the vector store, then make
`search(kind="note", query, limit)` (per ADR-023) run a hybrid retrieval pipeline identical in shape
to entity hybrid_search.

**Indexing on note creation**:

When `create(kind="note", note_kind, content, salience, properties)` writes a note:

1. Always: upsert a `TextDocument` into FTS5 at `notes_<sanitized_namespace>`. Body = `note.content`
   (later: include selected `properties` fields if useful).
2. If `embedding_model` is configured: embed `content` via `self.embed()`, insert into the per-model
   vector store with `kind="note"` discriminator. (See "Vector store schema change" below.)

**Hybrid retrieval pipeline** for notes mirrors the entity pipeline:

1. FTS5 query against `notes_<ns>` → ranked text hits.
2. If a query vector is computed (`embed(query)` succeeds): vector search against the same
   `vec_<model>` table filtered to `kind="note"` and `namespace=?` → ranked vector hits.
3. **Reciprocal Rank Fusion** with `k=60` (per ADR-012 default) → fused scores.
4. **Salience-weighted rerank**: multiply fused score by `(0.5 + 0.5 * note.salience)`. A note with
   salience 1.0 gets the full fused score; salience 0.0 gets halved. This prevents the most
   important notes from being buried by trivial high-frequency observations without dominating the
   ranking entirely.
5. **Filter superseded notes** (per ADR-019): drop any note that is the target of a `supersedes`
   edge. The filter is a `NOT EXISTS` subquery against `graph_edges` indexed on
   `(relation, target_id)`.
6. **Filter soft-deleted** (per the soft-delete filter work): drop notes where
   `deleted_at IS NOT NULL`.
7. Truncate to `limit`.

This becomes the `search(kind="note", ...)` implementation.

**Decay** (v0.2): salience can decay with time (`note.decay_factor` already exists on the Note
struct). Multiply effective salience by `exp(-decay_factor * age_days)` when computing the rerank
weight. Defer to v0.2 unless agents start complaining about stale notes drowning fresh ones.

### Part 2 — Vector store schema change

The current `vec_<model_key>` table has columns `entity_id`, `namespace`, `embedding`. The name
`entity_id` is now a misnomer — we want to store notes too. Two clean options:

**Option A** (chosen): rename the column logically to `subject_id` and add a `kind` column.

```sql
CREATE VIRTUAL TABLE vec_<model_key> USING vec0(
    subject_id TEXT PRIMARY KEY,
    namespace TEXT NOT NULL,
    kind TEXT NOT NULL,             -- "entity" | "note"
    embedding float[<dim>] distance_metric=cosine
);
```

Migration: V2 (per ADR-022) — drop and recreate the vec table; backfill from existing entities.
Pre-alpha, no real data, this is safe.

**Why not separate tables per kind** (`vec_<model>_entities`, `vec_<model>_notes`): cross-substrate
similarity ("find notes and entities semantically close to this query") would need UNION ALL across
two virtual tables — extra cost for the common case where agents legitimately want to see both.
Single table with filter is simpler and faster.

The `VectorStore` trait stays the same shape; `insert` and `search` gain an internal `kind`
parameter routed via the `khive-runtime` layer (the trait could expose `kind` explicitly or thread
it through metadata — implementer choice).

### Part 3 — Cross-substrate navigation: notes are nodes, annotation is an edge

A note that "annotates" an entity, edge, event, or another note is fundamentally a graph
relationship — the note is a _node_, and the annotation is an _edge_. We don't need a junction table
or a foreign-key field. We use the graph we already have.

**Wiring**: this ADR uses the canonical `annotates` relation (Category 6 of ADR-002;
`EdgeRelation::Annotates` per ADR-021). The source of an `annotates` edge is always a note; the
target is any substrate UUID — entity, edge, event, or another note.

**Workflow**: when an agent calls
`create(kind="note", content="...", annotates=[<uuid1>, <uuid2>])`, the runtime:

1. Creates the note (existing path: persist + FTS5 + vector index per Part 1).
2. Creates one `annotates` edge per target UUID:
   `link(source=note.id, target=<uuid>, relation=annotates, weight=1.0)`.

No new table. No new field on the Note struct. Edges reference UUIDs that may resolve to any
observable substrate — the existing edge schema already supports this.

**Discovery via the existing graph machinery**:

| Query                                                     | Existing op                                                                                                             |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| "notes that annotate entity X"                            | `neighbors(node_id=X, direction="in", relations=["annotates"])` — returns annotation-source UUIDs that resolve to notes |
| "things this note annotates"                              | `neighbors(node_id=note_id, direction="out", relations=["annotates"])` — returns the targets                            |
| "all edges touching entity X with their annotating notes" | `traverse(roots=[X], direction="both", relations=["annotates", ...])` — combined navigation                             |

**One runtime helper** worth adding — UUID resolution across substrates (since edges return raw
UUIDs, callers need to know "is this UUID a note or an entity?"):

```rust
/// Resolve a UUID to its substrate kind and load the record.
pub async fn resolve(
    &self,
    namespace: Option<&str>,
    id: Uuid,
) -> RuntimeResult<Option<Resolved>>;

pub enum Resolved {
    Entity(Entity),
    Note(Note),
    Event(Event),
}
```

Implementation: try the entity store first (fastest, indexed by id), then notes, then events. Cost:
at most 3 lookups per UUID for negative cases. Cheap enough for v0.1.

**MCP exposure** — uses what's already there:

| Verb (existing)                                                | Cross-substrate query                                |
| -------------------------------------------------------------- | ---------------------------------------------------- |
| `neighbors(node_id, relations=["annotates"], direction="in")`  | "notes annotating this"                              |
| `neighbors(node_id, relations=["annotates"], direction="out")` | "things this annotates"                              |
| `traverse(roots, relations=["annotates", ...])`                | mixed expansion                                      |
| `get(kind="???", id)`                                          | resolve via the new `resolve` helper if kind unknown |

**New verb `resolve(id)`** added to MCP for the "I have a UUID, what is it?" use case — small but
useful. Returns `{ kind, data }` discriminated.

**Surface impact**: +1 new MCP verb (`resolve`). Cross-substrate questions like "notes about this
entity" use `neighbors` with `relations=["annotates"]` — no new traversal verb needed. Total MCP
surface grows from 13 (ADR-023) to 14.

### Part 4 — `create(kind="note", ...)` ergonomics

The `create(kind="note", ...)` verb (per ADR-023) takes an optional `annotates` parameter listing
UUIDs the note is about. The handler creates the note, then issues `annotates` edges for each
target:

```rust
pub struct CreateParams {
    // ... existing fields ...
    pub annotates: Option<Vec<String>>,   // note only — UUIDs of substrate items this note observes/comments on
}
```

If `annotates` is omitted, the note is unattached — still indexed in FTS5 + vectors, still findable
by semantic search, but no graph edges out of it.

The annotation edges are real edges. They appear in `list(kind="edge", ...)` results and
participate in graph traversal. This means agents can ask questions like "show me all annotated
entities in the project" (`list(kind="edge", relations=["annotates"])` then resolve the targets)
without needing any cross-substrate-specific machinery.

## Rationale

### Why mirror the entity hybrid_search pipeline?

Two reasons:

1. The shape works — RRF over text + vector is a well-validated combination.
2. **Code reuse**: extending `hybrid_search` to take a `kind` parameter and route to the right
   FTS5 + vector store is cheaper than building a parallel pipeline. Per AEP, FindExisting + Modify
   > Create.

### Why salience-weighted rerank instead of pure RRF?

Notes have an explicit importance signal (salience 0.0-1.0). Ignoring it produces results that
ignore the agent's own valuation of what mattered. The chosen weight `(0.5 + 0.5 * salience)` means
even salience-0 notes contribute (just at half weight). Avoids hard-zeroing salience-0 notes since
they may still be discriminatively relevant for a query.

### Why `annotates` edges instead of a junction table?

A junction table would duplicate machinery the graph layer already has. Notes are nodes; an
annotation is just an edge with relation `annotates`. The inverse-lookup case ("notes for this
entity") becomes a standard `neighbors(node_id, direction="in", relations=["annotates"])` call —
already indexed, already fast. One mechanism instead of two.

### Why the new `resolve` verb?

`neighbors` and `traverse` return edges whose target is a raw UUID. The caller often needs to know
"is this UUID a note or an entity or an event?" before fetching it. `resolve(id)` is the
substrate-typed lookup that closes this gap. It's the only new MCP verb this ADR adds.

## Alternatives Considered

| Alternative                                                             | Pros                          | Cons                                                                                       | Why rejected                                          |
| ----------------------------------------------------------------------- | ----------------------------- | ------------------------------------------------------------------------------------------ | ----------------------------------------------------- |
| Skip note semantic search in v0.1; just structured filter               | Less code                     | Agents have to scan every note for any topic query — search quality is the whole point     | Wrong baseline for a research KG                      |
| Embed notes into a separate `vec_notes_<model>` table                   | Tight isolation between kinds | Cross-kind similarity needs UNION ALL; harder to extend                                    | One vec table per model with `kind` filter is simpler |
| Implicit entity refs via NER on note content                            | Less burden on the agent      | Error-prone; needs an NLP pipeline; v0.1 is laptop-runnable                                | Defer to v0.2 with an explicit research-extractor MCP |
| Store cross-substrate refs as a JSON array on notes row                 | Simpler than edges?           | Inverse lookup ("notes about entity X") needs full scan; duplicates graph machinery        | `annotates` edges reuse the indexed graph layer       |
| Junction table `note_entity_refs`                                       | Indexed inverse lookup        | New table, parallel mechanism to edges, doesn't support note→edge or note→event annotation | `annotates` edges generalize it for free              |
| Use the existing edge system (notes become entities, refs become edges) | Uniform model                 | Conflates the substrate distinction in ADR-004; loses the "this is a note" semantics       | Notes stay distinct                                   |

## Consequences

### Positive

- Note search becomes semantic — the agent gets relevant notes, not the time-ordered firehose.
- Cross-substrate navigation closes a real gap: agents can ask "what notes talk about
  FlashAttention?" or "what entities does this observation touch?"
- One unified retrieval pipeline (`search`) covers both entities and notes via `kind=`.
- The vector store schema change (V2 migration) is a one-time cost; future kinds (events?) extend
  the same table.

### Negative

- V2 migration changes the `vec_<model>` table — pre-alpha but anyone running an existing DB needs
  to re-embed. Mitigated: pre-alpha, no production data.
- The `annotates` edges add to the graph's edge count, which slightly enlarges traversal results.
  Mitigated: queries can filter by `relations=["annotates"]` to include/exclude them as needed.
- Salience weighting is a magic-number choice (`0.5 + 0.5 * salience`). May need tuning once we have
  real data. Worst case: callers complain, we re-pick the formula or expose it as config.

### Neutral

- Cross-substrate navigation reuses the existing graph machinery — no new traversal code, no new
  storage table. The `annotates` relation is one of the 13 canonical relations in ADR-002.

## Implementation Plan

After ADR-019 (NoteKind), ADR-021 (EdgeRelation including `Annotates`), and the soft-delete filter
land:

1. **V2 migration** (per ADR-022): rename `entity_id → subject_id` and add a `kind` column on the
   `vec_<model>` tables (drop and recreate; backfill entities). No `notes`-table changes; no
   junction table — both annotation and supersession go through the existing `graph_edges` table
   (relations `annotates` and `supersedes` respectively, per ADR-002).

2. **Storage layer**:
   - `VectorStore::insert(subject_id, kind, embedding)` — extend signature.
   - `VectorStore::search(query_embedding, kind: Option<&str>, top_k)` — extend signature to filter
     by kind.
   - `Note` struct is unchanged structurally — supersession and annotation are both edges in
     `graph_edges`, not fields on `Note`.

3. **Runtime layer**:
   - Auto-index notes on `create(kind="note", ...)` (FTS5 + vector store with `kind="note"`).
   - `search(kind="note", query, limit)` implementation: FTS5 + vector hybrid + salience weight +
     supersede/delete filters.
   - `resolve(id)` — substrate-typed UUID lookup.
   - `create_note(..., annotates: Vec<Uuid>)` creates the note and one
     `link(source=note.id, target=t, relation=annotates, weight=1.0)` per target.

4. **MCP layer** (built on ADR-023 verb surface):
   - `create(kind="note", ..., annotates=[...])` — the handler creates the note plus one `annotates`
     edge per target UUID.
   - `search(kind="note", query, limit)` — hybrid retrieval with salience weighting.
   - New `resolve(id)` verb — returns `{kind, data}` discriminated record.
   - Cross-substrate navigation reuses existing
     `neighbors(node_id, relations=["annotates"], direction=...)` and
     `traverse(roots, relations=["annotates", ...])`.

5. **Tests**:
   - Index a note, search for keyword → returns it.
   - Index a note + entity sharing a keyword, search with `kind="note"` → returns only the note.
   - Salience-weighted: two notes with same content, different salience → higher salience ranks
     first.
   - Superseded note excluded by default.
   - `create(kind="note", annotates=[entity_id])` creates the note and the `annotates` edge
     atomically.
   - `neighbors(entity_id, direction="in", relations=["annotates"])` returns the annotating note's
     UUID.
   - `neighbors(note_id, direction="out", relations=["annotates"])` returns the targets.
   - `resolve(note_id)` → `{kind: "note", data: {...}}`. `resolve(entity_id)` →
     `{kind: "entity", data: {...}}`.

## Open Questions

1. **Should there be an alias for `search(kind="note")` to ease adoption?** No. Per ADR-023, agents
   call `search(kind="note", query=...)` directly — no shorthand wrapper.
2. **Should the salience weight be configurable per call?** v0.1: hardcoded `0.5 + 0.5 * salience`.
   If feedback says "I want pure RRF without salience," expose a `weight_by_salience: bool = true`
   flag.
3. **Cross-substrate ranking** — when an agent walks
   `neighbors(node_id=entity_id, direction="in", relations=["annotates"])` and resolves the source
   UUIDs to notes, should the resulting list be ranked by note salience? Yes, by the same formula.
   Document.
4. **Note content extraction for implicit refs** — when an agent writes "FlashAttention reduces
   memory by 4x" without specifying `annotates`, can we auto-detect the entity reference by name
   match? Defer to v0.2 with an opt-in flag; risk of false positives is real.

## References

- ADR-004: Substrate Observables (Note is one of three kinds)
- ADR-012: Retrieval Architecture (defines the hybrid_search shape this extends)
- ADR-014: KG Curation Operations (the soft-delete state we filter on)
- ADR-019: Note Kind Taxonomy (defines the `supersede` semantics this search filters out by default)
- ADR-022: Schema Migrations (the V2 migration mechanism)
- ADR-023: Verb-Consolidated MCP Surface (defines `search`, `list`, `create(kind="note", ...)`,
  `neighbors`, `traverse`; this ADR adds the `resolve` verb and the `annotates` edge wiring)
