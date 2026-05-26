# ADR-047: Knowledge Pack

**Status**: accepted
**Date**: 2026-05-25
**Authors**: Ocean, lambda:khive

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

### 1. Three verbs, no new kinds

The knowledge pack registers three verbs over the existing `concept` entity kind. It
does **not** introduce new note kinds, entity kinds, or edge relations:

| Verb    | Underlying operation                           | Value-add                                                       |
| ------- | ---------------------------------------------- | --------------------------------------------------------------- |
| `learn` | `create(kind="concept")`                       | Auto-promotes `domain` to both `properties.domain` and `tags`   |
| `cite`  | `link(relation="introduced_by")`               | Domain-oriented parameter names; weight clamped to `[0.0, 1.0]` |
| `topic` | `search(kind="concept")` + optional tag filter | Domain-filter parameter; consistent `limit` cap of 100          |

No columns are added to the database. No new endpoints are required in the schema.

### 2. `learn` — concept registration with domain promotion

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

### 3. `cite` — provenance citation

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

### 4. `topic` — concept browsing

```
topic(domain?, query?, limit?) → {items: [...], total: N}
```

- Without `query`: lists all concepts in the namespace up to `limit`.
- With `query`: runs hybrid FTS+vector search scoped to `kind="concept"`, then optionally
  post-filters by `domain` tag.
- `limit` defaults to 20 and is capped at 100. The cap is applied silently; the response
  reflects the capped limit via `items` and `total`.
- The domain filter is case-insensitive tag match (`eq_ignore_ascii_case`).

### 5. Pack dependency declaration

The pack declares `REQUIRES: &["kg"]`. The runtime enforces this at boot: loading
`knowledge` without `kg` fails with a dependency error. Because `knowledge` adds no new
kinds or relations, `kg` must be active to handle any entity CRUD that `learn` or `topic`
invokes.

### 6. Binary wiring

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
