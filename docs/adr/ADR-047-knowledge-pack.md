# ADR-047: Knowledge Pack

**Status**: accepted (amended 2026-06-07)
**Date**: 2026-05-25
**Authors**: Ocean, lambda:khive

## Amendment (2026-06-07): content-only atoms; normalized response envelope

The schema-consolidation work supersedes two parts of the original contract below.
Where the body still reads otherwise, this amendment governs:

- **Atoms have no separate `description` column — the `content` column carries it.**
  `content` holds the atom's _description_ (the `description` field from the atom
  markdown front matter — a short summary, ≥ 20 words). The atom's full **body** is
  its typed **sections** (`knowledge_sections`), not the `content` column.
  `knowledge.upsert_atoms` accepts `content` only — there is no `description` input
  alias. The `knowledge_atoms` table and `fts_knowledge` index carry no `description`
  column, and atom scoring ranks across name, tags, and content.
  (`knowledge_domains` keep their own `description` — this change is atoms-only.) See
  [ADR-048](ADR-048-knowledge-section-profiles.md) §"Atom and section content constraints".
- **`search`, `topic`, and `list` return `{results, total, ...}`**, not
  `{items, total}` — part of the response-envelope normalization. Inline
  `{items: ...}` references below are stale.

## Context

khive's `kg` pack ([ADR-017](ADR-017-pack-standard.md)) exposes a complete CRUD surface
over the eight entity kinds and fifteen edge relations. Registering a research concept
requires at minimum three steps: `create(kind="concept", ...)`, optionally
`link(relation="introduced_by", ...)`, and `search(kind="concept", ...)` for retrieval.
These three steps recur in every research-agent workflow.

Agents that work exclusively with research concepts encounter two friction points:

1. **Domain promotion is manual.** `create` accepts a `tags` list; callers must
   remember to add the domain string both to `properties.domain` (for structured
   access) and to `tags` (for FTS discoverability). Omitting either silently degrades
   retrieval quality.
2. **Parameter shape for citations is inverted relative to how researchers think.** The
   underlying `link` verb names its parameters `source_id` (the graph-source entity) and
   `target_id` (the graph-target entity). For `introduced_by` edges, the graph-source is
   the concept and the graph-target is the paper — but researchers naturally say "cite
   _this concept_ to _this paper_", which maps to `concept_id` / `source_id` in
   domain vocabulary.

Other packs ([ADR-019](ADR-019-gtd-pack.md) for tasks, [ADR-021](ADR-021-memory-pack.md)
for memory) demonstrate the pattern: wrap kg primitives with an opinionated verb surface
that encodes domain conventions, leaving the underlying substrate unchanged.

## Decision

### 1. Two tiers: corpus verbs and concept verbs

The knowledge pack has two tiers of functionality:

**Corpus tier** (9 verbs) — a standalone knowledge-atom store with its own tables,
FTS5 index, TF-IDF search, and budget-constrained selection. Atoms are slug-keyed
content units; domains are named groupings of atoms. This tier ports the retrieval
capabilities of the lore service into the pack system.

**Concept tier** (3 verbs) — sugar over the kg pack's entity/edge substrate for
research-concept workflows. These verbs use existing entity kinds and edge relations;
they do not introduce new ones.

| Verb                       | Tier    | Category   | Description                                                                  |
| -------------------------- | ------- | ---------- | ---------------------------------------------------------------------------- |
| `knowledge.upsert_atoms`   | Corpus  | Commissive | Bulk insert/update slug-keyed knowledge atoms                                |
| `knowledge.upsert_domains` | Corpus  | Commissive | Bulk insert/update domain groupings of atoms                                 |
| `knowledge.get`            | Corpus  | Assertive  | Fetch one atom or domain by ID or slug                                       |
| `knowledge.list`           | Corpus  | Assertive  | Paginated listing of atoms or domains                                        |
| `knowledge.delete_atoms`   | Corpus  | Commissive | Soft-delete atoms by slug                                                    |
| `knowledge.stats`          | Corpus  | Assertive  | Corpus statistics (counts, coverage)                                         |
| `knowledge.index`          | Corpus  | Commissive | Backfill embeddings + FTS for atoms                                          |
| `knowledge.fold`           | Corpus  | Assertive  | Budget-constrained knapsack selection (token budgeting)                      |
| `knowledge.search`         | Corpus  | Assertive  | TF-IDF + embedding rerank (default when embedder configured) over the corpus |
| `knowledge.learn`          | Concept | Commissive | Register a concept entity with domain promotion                              |
| `knowledge.cite`           | Concept | Commissive | Link a concept to its source paper via `introduced_by`                       |
| `knowledge.topic`          | Concept | Assertive  | List/search concepts, optionally filtered by domain                          |

### 1a. Corpus tier schema (V19 migration)

The corpus tier introduces two tables via versioned migration V19
(`knowledge_atoms_and_domains`):

- `knowledge_atoms` — slug-keyed content units with name, content (the atom's
  description/summary from front matter; no separate `description` column — the
  full body lives in the typed `knowledge_sections`), tags (JSON array),
  properties (JSON object), and finalized flag.
- `knowledge_domains` — named groupings with slug, name, description, tags, and
  members (JSON array of atom slugs).

An FTS5 external-content virtual table (`fts_knowledge`) indexes slug, name,
and content from `knowledge_atoms` via triggers that sync on
insert/update/delete. The trigram tokenizer enables substring matching.

Soft-deleted atoms (non-null `deleted_at`) are excluded from the FTS index via
a `WHEN new.deleted_at IS NULL` guard on the insert trigger.

### 1b. Concept tier: three verbs, no new kinds

The concept tier registers three verbs over the existing `concept` entity kind. It
does **not** introduce new note kinds, entity kinds, or edge relations:

| Verb    | Underlying operation                           | Value-add                                                       |
| ------- | ---------------------------------------------- | --------------------------------------------------------------- |
| `learn` | `create(kind="concept")`                       | Auto-promotes `domain` to both `properties.domain` and `tags`   |
| `cite`  | `link(relation="introduced_by")`               | Domain-oriented parameter names; weight clamped to `[0.0, 1.0]` |
| `topic` | `search(kind="concept")` + optional tag filter | Domain-filter parameter; consistent `limit` cap of 100          |

### 2. Corpus tier verbs

#### `knowledge.upsert_atoms` — bulk atom insert/update

```
upsert_atoms(atoms: [{slug, name, content, tags?, properties?, finalized?}, ...], chunk_size?) → {upserted: N}
```

Inserts or updates atoms by `(namespace, slug)` key. On conflict, updates name,
content, tags, properties, finalized, and `updated_at`. Empty `atoms`
array is rejected. Tags are stored as a JSON array string; properties as a JSON
object string.

#### `knowledge.upsert_domains` — bulk domain insert/update

```
upsert_domains(domains: [{slug, name, description?, tags?, members?}, ...]) → {upserted: N}
```

Inserts or updates domains by `(namespace, slug)` key. Members is a JSON array of
atom slugs.

#### `knowledge.get` — fetch by ID or slug

```
get(id: <uuid|slug>) → {type: "atom"|"domain", ...fields}
```

Resolves by UUID first, then by slug against both `knowledge_atoms` and
`knowledge_domains`. Returns 404 if not found.

#### `knowledge.list` — paginated listing

```
list(type?: "atom"|"domain", limit?: 20, offset?: 0) → {results: [...], total: N, limit, offset}
```

Default type is `atom`. Limit capped at 500.

#### `knowledge.delete_atoms` — soft delete

```
delete_atoms(ids: [<slug|uuid>, ...], cascade?: true) → {deleted: N}
```

Sets `deleted_at` timestamp. FTS trigger automatically removes from search index.

#### `knowledge.stats` — corpus statistics

```
stats() → {atoms: N, domains: N, ...}
```

#### `knowledge.index` — backfill embeddings + FTS

```
index(ids?: [<slug|uuid>], batch_size?: 500, insert_only?: false) → {indexed: N}
```

Backfills embedding vectors and FTS content. When `ids` is omitted, indexes the
entire corpus in batches. `insert_only` skips the delete-then-reinsert cycle for
fresh corpus backfill.

#### `knowledge.fold` — budget-constrained selection

```
fold(candidates: [{id, score, size, content?, category?}, ...], budget: N, min_score?: 0.0, category_weights?: {}) → {selected: [...], total_size: N}
```

Greedy knapsack: sorts candidates by score-density (score/size), applies category
weight multipliers, filters by `min_score`, then packs greedily until budget is
exhausted. Pure computation — no database access.

#### `knowledge.search` — TF-IDF ranked search

```
search(query, type?, role?, limit?: 10, min_score?: 0.0, weights?: {}, decompose?: false, decompose_threshold?: 4, intersection_bonus?: 0.25, rerank?: true, rerank_alpha?: 0.7) → {results: [...], total: N}
```

FTS5 recall → in-memory TF-IDF scoring across name, tags, and content
fields with configurable weights. Features:

- **Query decomposition**: splits long queries into sub-queries, scores each
  independently, and bonuses items that appear across multiple sub-queries. Opt in with `decompose=true`.
- **Embedding rerank** (default when embedder configured): blends TF-IDF scores with cosine
  similarity against the query embedding. `rerank_alpha` controls the blend (0.7 = TF-IDF dominant).
  Disable with `rerank=false`. No-op if no embedder is configured.
- **Role weighting**: prepends the agent role to the query for contextual scoring.

### 3. `learn` — concept registration with domain promotion

```
learn(name, description?, domain?, tags?) → {id, full_id, kind, name, domain, tags, ...}
```

- `name` is required and must be non-empty after trimming.
- `domain`, if provided, is stored in `properties.domain` **and** appended to `tags`
  unless already present. This ensures the domain is reachable via both structured queries
  and FTS.
- `tags` accepts an explicit list; the domain tag is merged in, not replaced.
- `learn` is **not idempotent**. Calling `learn(name="LoRA")` twice creates two entities.
  Callers that need idempotent registration should use `topic(query="LoRA", limit=1)` first
  and fall back to `learn` only when no result is found. This is documented in the SKILL.md
  anti-patterns section; the verb intentionally does not add the round-trip overhead by
  default.

### 4. `cite` — provenance citation

```
cite(concept_id, source_id, weight?) → {id, full_id, relation, concept_id, source_id, weight}
```

- `concept_id` is the concept being introduced (graph-source in `introduced_by` terms).
- `source_id` is the paper, document, or person that introduced it (graph-target).
- Both accept full UUID or 8-char hex prefix (via `resolve_prefix`).
- `weight` defaults to `1.0` (definitional). Values outside `[0.0, 1.0]` are **silently
  clamped**. This is consistent with how other handlers treat weight: the substrate does
  not admit out-of-range weights; clamping is preferable to an error for an optional
  quality annotation. The effective weight is reflected in the response.
- The underlying edge relation is `EdgeRelation::IntroducedBy` (ADR-002). The pack does
  not bypass the closed edge ontology.

### 5. `topic` — concept browsing

```
topic(domain?, query?, limit?) → {results: [...], total: N}
```

- Without `query`: lists all concepts in the namespace up to `limit`.
- With `query`: runs hybrid FTS+vector search scoped to `kind="concept"`, then optionally
  post-filters by `domain` tag.
- `limit` defaults to 20 and is capped at 100. The cap is applied silently; the response
  reflects the capped limit via `items` and `total`.
- The domain filter is case-insensitive tag match (`eq_ignore_ascii_case`).

### 6. Pack dependency declaration

The pack declares `REQUIRES: &["kg"]`. The runtime enforces this at boot: loading
`knowledge` without `kg` fails with a dependency error. The concept tier delegates
entity CRUD to the `kg` pack; the corpus tier operates on its own tables
(`knowledge_atoms`, `knowledge_domains`) via direct SQL through the runtime's
`SqlAccess` trait.

### 7. Binary wiring

`crates/khive-mcp/Cargo.toml` declares `khive-pack-knowledge` as a direct dependency.
`crates/khive-mcp/src/pack.rs` re-exports `KnowledgePack` under a `#[doc(hidden)]` alias
to force-link the crate so `inventory::submit!` constructors run. This is the standard
pattern for all first-party packs in this binary.

`scripts/publish.sh` includes `khive-pack-knowledge` after `khive-pack-schedule` and
before `khive-pack-template`, reflecting the dependency ordering.

## Consequences

### Accepted trade-offs

- `learn` creates duplicates on repeated calls. The idempotency round-trip is the caller's
  responsibility. This is consistent with how `create` works; the pack is sugar, not a
  new semantic contract.
- `cite` silently clamps weight. An invalid weight is a caller error on an optional
  annotation; clamping over rejecting avoids breaking batch ingestion pipelines.
- `topic` has a hard cap of 100. Callers who need more than 100 concepts should page via
  `list(kind="concept", ...)` from the kg pack directly.

### What this ADR does NOT cover

- Idempotent variant (`learn_or_get`) — deferred; no current demand from agent workflows.
- `weight_requested` surfacing in `cite` response — deferred; low-priority annotation.
- Pagination for `topic` — callers who need full pagination should use the kg pack's
  `list(kind="concept")` which has explicit `offset` support.
- ADR amendment for ADR-002 or ADR-001 — not needed; the knowledge pack uses existing
  kinds and relations only.

## Alternatives considered

### Extend kg handlers with domain-aware variants

Rejected. Adding `domain` auto-promotion to `create` would impose the research-agent
convention on callers who use `create` for non-research purposes. The pack model exists
precisely to keep the kg substrate neutral and compose opinionated layers above it.

### Single `knowledge` verb dispatched by sub-command

Rejected. A single entry-point with a `kind` discriminant (`knowledge(action="learn",
...)`) violates the verb-flat interface (ADR-015). The three verbs are distinct
speech acts (two Commissive, one Assertive per ADR-025); flattening them degrades
discoverability.

### Introduce a `concept` note kind alongside the entity kind

Rejected. Research concepts are entities (named, structured, graph-connected). Notes are
for context and observations _about_ entities, not for the entities themselves. The
existing `concept` entity kind in ADR-001 is the correct substrate; no new kind is needed.
