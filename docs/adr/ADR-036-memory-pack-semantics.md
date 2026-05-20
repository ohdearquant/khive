# ADR-036: Memory Pack Semantics

**Status**: accepted\
**Date**: 2026-05-19\
**Authors**: khive maintainers

## Context

PR #58 (`feat(pack-memory)`, branch `feat/memory-pack`, HEAD `2192f75`) ships
`khive-pack-memory`. As initially drafted, the pack registers two note kinds (`episodic`,
`semantic`) and two verbs (`remember`, `recall`), following the same composition pattern as
`khive-pack-gtd` (ADR-026). This ADR proposes a structural revision (one `memory` kind with
`memory_type` as an attribute) that aligns with the reference implementation and
eliminates a class of recall-filter bugs.

A codex review of PR #58 (public at `https://github.com/ohdearquant/khive/pull/58#issuecomment-4491382647`)
found two contract conflicts:

1. **Verb conflict** — ADR-023 §"remember and recall are removed entirely" (lines 223–227) states
   the agent surface has no `remember` or `recall`; ADR-019 §"What about Note vs Memory?"
   (lines 141–145) says memory uses `create`/`search` on notes, not a `remember`/`recall` pair.
   PR #58's registered verbs violate this text.

2. **Filter leak** — `recall(query=...)` without an explicit `kind` passes `None` to
   `runtime.search_notes(...)` (`handlers.rs` lines 125–152). The runtime only applies a
   note-kind filter when `note_kind` is `Some` (`operations.rs` lines 581–584), so the default
   path searches every note kind in the namespace. In a `kg,gtd,memory` registry this can return
   `observation`, `insight`, `task`, or other pack-registered kinds — violating the
   memory-only contract documented in `lib.rs` lines 10–11.

Both conflicts stem from the same root cause: ADR-019 and ADR-023 were written before
ADR-025 (Pack Standard) generalised vocabulary and verb ownership to packs. The "no
remember/recall" text in ADR-023 is anachronistic — it reflects a design where memory was
not a separate pack concern. ADR-025 and ADR-026 have since established that packs own their
verb surface; this ADR brings the earlier text into alignment.

This ADR resolves both conflicts at the contract level. Code fixes belong in a revision of PR #58.

## Decision

### 1. Pack-owned verbs are legitimate; amend the conflicting ADR text

Memory verbs (`remember`, `recall`) are legitimate pack-owned verbs. The strongest precedent
is ADR-026 (GTD pack), which introduces `assign`, `next`, `complete`, `tasks`, and `transition`
as pack-owned verbs over the notes substrate without controversy. ADR-025 §Pack trait establishes
that every pack declares `const VERBS: &'static [VerbDef]` as its canonical verb surface.

The "no remember/recall" passages in ADR-023 (lines 223–227) and ADR-019 (lines 141–145) are
**amended** by this ADR. Both paragraphs are superseded by the following:

> The agent surface for the base KG pack has no `remember` or `recall`. These verbs, along with
> the `memory` note kind, are owned by the memory pack (`khive-pack-memory`, ADR-036). KG-only
> deployments have no `memory` kind and no memory verbs — the vocabulary is pack-owned and only
> registered when the memory pack is loaded (`KHIVE_PACKS=kg,memory`). When loaded, `remember`
> and `recall` are the idiomatic surface; generic `create`/`search` may also target
> `kind="memory"` explicitly.

The amendment is additive: it does not change how KG-only deployments behave.

### 2. One memory note kind: `memory`

The memory pack registers a **single** note kind: `memory`. Both episodic and semantic memories
are stored under this kind. The episodic/semantic distinction is carried as a `memory_type`
attribute on the note, not as a separate kind value:

| Attribute     | Values                       | Default      | Storage           |
| ------------- | ---------------------------- | ------------ | ----------------- |
| `memory_type` | `"episodic"` \| `"semantic"` | `"episodic"` | `note.properties` |

| memory_type | Shape                       | Examples                                                 |
| ----------- | --------------------------- | -------------------------------------------------------- |
| `episodic`  | Time-anchored, event-shaped | "On 2026-05-19 Ocean said prefer `uv run` over `python`" |
| `semantic`  | Abstracted, fact-shaped     | "Ocean prefers `uv run` over `python`"                   |

The distinction is **advisory, not enforced**: nothing structurally validates that `episodic`
memories carry timestamps. Agents choose `memory_type` based on whether the content is primarily
event-oriented or persistent-fact-oriented; misclassification is tolerated.

This matches the canonical pattern: one row per memory in the notes substrate, all under one
kind, with the episodic/semantic distinction as a queryable attribute. Per-`memory_type`
retrieval strategies (e.g. time-decay for episodic, plain RRF for semantic) are reserved as
forward-compatible future work; v0.1 applies one unified pipeline regardless of `memory_type`.

### 3. `importance` is the user-facing name for the `salience` column

The notes substrate already provides two columns relevant to memory: `salience` (the importance
signal that participates in `(0.5 + 0.5 * salience)` rerank per ADR-024) and `decay_factor` (the
per-note exponential decay rate). The memory pack does not introduce new columns or duplicate
storage:

- The `remember` verb accepts `importance` as a parameter name. The handler writes it directly
  to the existing `salience` column. `importance` is a user-facing alias; the storage is
  unchanged.
- The `remember` verb accepts `decay_factor` as a parameter name and writes it to the existing
  `decay_factor` column.
- Memory notes carry no redundant copy in `properties` for either field.

Default `importance` is `0.5`. Default `decay_factor` is `0.01` (mild decay: importance halves
in ~69 days). The notes-substrate-wide default for `decay_factor` is `0.0` (no decay); the
memory pack handler defaults to `0.01` so that memory-kind notes participate in time-decay by
default while leaving other note kinds unaffected.

### 4. `source` is conveyed as an `annotates` edge, not a stored field

A memory's source — who or what produced it — is represented as an `annotates` edge from the
memory note to the source entity or note. Per ADR-002, `annotates` is the universal note → any
substrate relation, and per ADR-031 endpoint additions are permissive (annotates accepts any
target).

The `remember` verb accepts an optional `source_id` argument (a UUID). When present, the
handler creates the memory note and then links it to the source via `annotates` in the same
verb invocation. When absent, no edge is created and the memory's provenance is unattributed.

This replaces an earlier draft proposal to store `source` as a free string in
`note.properties`. Edges are the right substrate for "this memory came from X" relationships
because they participate in graph traversal (e.g. `neighbors(memory_id, relation="annotates")`
recovers all sources) and avoid coupling the memory pack to a future actor-identity ADR for
the source-string format.

For the common case "Ocean said X", the source is a `person` entity (or whatever entity kind
represents the actor). For "agent X produced this", the source is whichever entity represents
the agent (when actor entities are formalised in a future ADR; until then, this remains
unattributed by default).

### 5. `remember` is thin syntactic sugar over `create` + optional `link`

`remember(content, memory_type?, importance?, decay_factor?, source_id?, namespace?, tags?)`
reduces to:

```
1. note_id = create(
     kind = "memory",
     content = content,
     salience = <importance or 0.5>,
     decay_factor = <decay_factor or 0.01>,
     properties = { memory_type: <memory_type or "episodic"> },
     tags = <tags or []>,
     namespace = namespace,
   )

2. if source_id is provided:
     link(source_id = note_id, target_id = source_id, relation = "annotates")
```

The handler validates: (a) `content` is non-empty, (b) `memory_type ∈ {episodic, semantic}` if
provided, (c) `importance ∈ [0, 1]`, (d) `decay_factor >= 0`, (e) `source_id` is a valid UUID
that exists in the namespace.

This means agents that prefer explicit CRUD are not blocked:
`create(kind="memory", salience=0.7, decay_factor=0.01, properties={"memory_type":"semantic"}, ...)`
followed by an optional `link(annotates)` works identically.

### 6. `recall` filters to `kind="memory"`; decay-weighted retrieval

`recall(query, limit?, memory_type?, namespace?, min_score?)` is a memory-scoped variant of
`search(kind="note", ...)`. Its contract:

- The handler passes `Some("memory")` as the note-kind filter, eliminating codex Major #1 —
  non-memory notes (`observation`, `insight`, `task`, etc.) cannot leak into recall results
  regardless of which other packs are loaded.
- **Candidate scoping, not just output filtering.** The current `search_notes` applies
  `note_kind` as a post-filter after candidate selection (`limit * 4` bound). In a mixed
  `kg,gtd,memory` namespace, high-ranking non-memory notes can fill the candidate pool before
  any memory note is considered. PR #58 must push the `note_kind="memory"` predicate into
  candidate retrieval (FTS5 `WHERE note_kind = ?` clause + vector-search post-retrieval filter
  before the bound), or implement a bounded over-fetch/scan that continues fetching candidate
  pages until `limit` memory-kind candidates are collected (ceiling: `limit * 20` raw
  candidates to prevent unbounded iteration). A mixed-namespace regression test with more than
  `limit * 4` non-memory notes ahead of matching memory notes is required.
- `memory_type` (optional): post-filter results to only `episodic` or only `semantic`. Default
  is no filter (return both). The filter operates on `note.properties.memory_type` after
  memory-scoped candidate retrieval.

Retrieval pipeline (one unified formula for v0.1; per-`memory_type` overrides are future work):

```
1. Hybrid retrieve top-K candidates with kind="memory" (FTS5 + vector via RRF — ADR-024 step 1-3).

2. For each candidate:
     age_days            = (now - created_at) / seconds_per_day
     effective_importance = salience * exp(-decay_factor * age_days)

3. Score fusion:
     score = rrf_score * 0.70 + effective_importance * 0.20 + temporal * 0.10

4. Apply min_score filter, then truncate to limit.
```

The `exp(-decay_factor * age_days)` decay model and the `0.70 / 0.20 / 0.10` fusion weights
are chosen as the initial memory-pack policy, backed by the existing `salience` and `decay_factor`
columns in the notes substrate. Future research-driven
recalibration (Beta-Bernoulli posterior over recall hits, adaptive decay) is forward-compatible:
it operates on `salience` and `decay_factor` columns that already exist; no further schema change
required.

### 7. `forget` is not a verb; deletion uses the substrate

The memory pack registers no `forget` verb. Memory deletion is `delete(id=...)` — the UUID-only
substrate verb (ADR-023) resolves the note by UUID and soft-deletes it. This is consistent with
the prohibition codified in the verb contract. The PR's existing test asserting no `forget`
registration must be preserved.

### 8. Configuration

The pack loads via `KHIVE_PACKS=kg,memory`. The default remains `kg`
only; existing consumers are unaffected. Standalone `KHIVE_PACKS=memory` is not supported:
memory pack verbs (`remember`, `recall`, `delete`) delegate CRUD operations to KG-pack–registered
note kinds, so the KG pack must be present. Once ADR-037 lands, this is enforced at load time via
`MemoryPack::REQUIRES = &["kg"]`.

## Rationale

### Why amend ADR-019 and ADR-023 rather than removing the verbs

The alternative is to strip `remember`/`recall` from the pack and force agents to call
`create(kind="memory", ...)` and `search(kind="note", note_kind="memory", ...)`. That produces
a memory pack that contributes a note kind but no verbs — a vocabulary pack, not a domain pack.

It conflicts with the precedent ADR-026 established. The GTD pack introduces `assign` instead
of `create(kind="task")` precisely because domain-specific verbs are more legible, enforce
preconditions that generic CRUD does not, and reflect the pack's semantic ownership of its
lifecycle. The same logic applies here: `remember` validates `memory_type`, normalises
`importance` and `decay_factor` defaults, and optionally creates the `annotates` edge to the
source in a single call. `recall` enforces the `kind="memory"` filter and applies the
decay-weighted fusion. Neither is merely cosmetic.

A meta note on agent ergonomics: the MCP surface exposes a single `request` tool (ADR-027);
agents do not call `remember` or `recall` as MCP tools, they call `request(ops="remember(...)")`.
Verb names are part of the _pack contract_ and are documented through skills/plugins, not
through separate MCP tool registrations. The naming choice here is about contract clarity, not
MCP-level discoverability.

### Why one `memory` kind, not two

An earlier draft used two separate note kinds (`episodic` and `semantic`). The reference
implementation uses one kind (`memory`) with `memory_type` as an attribute. One kind has
two advantages:

1. **Single filter at recall time**: `recall` always passes `Some("memory")` to the
   `search_notes` runtime helper. There is no two-search merge, no kind-set juggling, and the
   recall-leak bug in PR #58 (codex Major #1) is fixed structurally rather than by handler
   gymnastics.
2. **Forward-compat for per-`memory_type` retrieval strategies**: future revisions may apply
   different retrieval pipelines to episodic vs semantic content (e.g. heavier time-decay for
   episodic, plain RRF for semantic). With `memory_type` as an attribute, that becomes a
   handler-level branch on a property; with separate kinds, it would require coordinating two
   runtime queries with different weightings.

The cost is that callers querying memory through generic `search(kind="note", note_kind="memory")`
get both episodic and semantic results mixed; filtering on `memory_type` requires a post-filter
on `properties.memory_type` or use of the `recall` verb's `memory_type` argument. This is
acceptable for v0.1.

### Why decay is wired in v0.1

Decay is not a v0.2 feature; the notes substrate already carries `decay_factor` as a column
and the formula is well-defined with no additional schema changes required. The memory pack handler
simply defaults `decay_factor` to `0.01` instead of the substrate-wide `0.0`, so memory notes
participate in time-decay by default while other note kinds remain unaffected. The
`effective_importance = salience * exp(-decay_factor * age_days)` formula and the
`0.70 / 0.20 / 0.10` fusion weights match the decay formula and scoring columns already present
in the substrate.

Future research-driven recalibration (e.g. Beta-Bernoulli posterior updates from recall hits,
adaptive `decay_factor` adjustment) is forward-compatible: those mechanisms operate on the
existing `salience` and `decay_factor` columns and require no schema change. They land in a
separate ADR when the research informing them is in place.

### Why `importance` aliases `salience` rather than introducing a new column

The `salience` column already exists on the notes substrate (defined in V1 migration) and
already participates in the ADR-024 rerank pipeline. Introducing a separate `importance`
column — whether typed or JSON — would either duplicate storage (two columns holding the same
value) or split storage (importance for memory, salience for everything else) with no
behavioural benefit. The user-facing parameter name on `remember` is `importance` because
that is the term the memory domain uses; the column is `salience` because that is the
substrate-level concept and other packs use it too. The pack handler is the translation point.

### Why `source` is an edge, not a stored field

Provenance — who or what produced a memory — is structurally a relationship between two
substrate records, not a property of one. Two reasons it belongs on an edge:

1. **Graph traversal recovers provenance**. `neighbors(memory_id, relation="annotates")`
   returns all sources without any new query path. A `source` string field would require a
   separate index and a separate lookup verb.
2. **No coupling to a future actor-identity ADR**. A free-string `source` field would have to
   eventually pick a canonical encoding (`"agent:kg-digest"` vs `{kind, id}` vs typed actor
   reference). An edge is type-agnostic — the target is whatever entity or note represents
   the source, and the encoding stays under entity-substrate control.

For the common case "Ocean said X", the source is a Person entity. For "agent X produced this",
the source is whichever entity represents the agent in the future actor-formalisation ADR. For
"this came from paper Y", the source is the Document entity. All three resolve through the
same `annotates` edge with no special-casing.

## Alternatives Considered

| Alternative                                                             | Pros                                        | Cons                                                                                                                            | Why rejected                                                                                      |
| ----------------------------------------------------------------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| Remove `remember`/`recall`; pack provides kind only, not verbs          | Keeps ADR-023 text unchanged; simpler pack  | Loses domain-specific preconditions; contradicts GTD precedent; leaves PR #58 semantically hollow                               | ADR-026 establishes pack-owned verbs as the right pattern; amendment is cheaper                   |
| Two memory kinds (`episodic`/`semantic`) instead of one + `memory_type` | Sharper kind discriminator at the substrate | Forces two-search merge in `recall`; complicates per-`memory_type` retrieval policy; diverges from the reference implementation | One kind + attribute is the canonical pattern and removes a class of recall-filter bugs           |
| Defer decay to v0.2                                                     | Smaller v0.1 surface                        | Decay column already exists; canonical pipeline has decay baked in; agents reading "memory" without decay get stale results     | Decay is structural for a memory model, not an optimisation; wire it now                          |
| Introduce a separate `importance` column instead of aliasing `salience` | Domain-specific column name in storage      | Duplicates the rerank signal; forces every reader/writer to know which column to consult                                        | The user-facing name lives on the verb argument, not the column                                   |
| Store `source` as a free string in `note.properties`                    | Simpler handler; no edge creation step      | Couples memory to a future actor-identity string format; not traversable via `neighbors` / `traverse`                           | Edges are the right substrate for "this came from X" relationships                                |
| Enforce `episodic`/`semantic` distinction via structural validation     | `memory_type` carries stronger invariant    | Arbitrary; agents disagree on what "time-anchored" means; validation complexity outweighs gain                                  | Advisory distinction is sufficient; per-`memory_type` retrieval can still branch on the attribute |

## Consequences

### Positive

- PR #58 has a clear path to merge: collapse to one `memory` kind with `memory_type` attribute,
  wire decay defaults, add the `source_id` → `annotates` edge step in `remember`, drop the
  two-search merge in favour of `Some("memory")` filter, point to this ADR as the contract.
- ADR-019 and ADR-023 are no longer in contradiction with pack-standard practice.
- The verb surface grows coherently: `remember`/`recall` join `assign`/`next`/`complete` as
  domain-specific pack verbs over the notes substrate.
- The decay model is on from day one; agents using `recall` get age-weighted results
  matching the canonical pipeline.
- Provenance is queryable via graph traversal (`neighbors`, `traverse`) without any new verb.
- Per-`memory_type` retrieval policy is unblocked future work — no schema change required to
  branch the handler on `properties.memory_type`.

### Negative

- The memory pack handler is slightly heavier than a thin syntactic-sugar layer because it
  does (a) note creation, (b) optional edge creation, and (c) decay-aware fusion at recall
  time. The complexity is bounded to the pack and is the price of a substantive memory model.
- `memory_type` lives in `properties` JSON, which is not directly indexable. If
  `memory_type`-filtered recall becomes a hot path with very large memory namespaces, a v0.2
  migration can promote it to a typed column.

### Neutral

- No schema migration. No DDL change. No new edge relation. No new entity kind. (`annotates`
  already accepts note → any-substrate per ADR-002.)
- `khive-pack-kg` is unaffected; its `search`/`create` paths continue to work as specified
  in ADR-023. The amended text is additive.

## Implementation

### Changes needed in PR #58 to align with this ADR

1. **`lib.rs` — collapse to one note kind**: register `NOTE_KINDS = &["memory"]` instead of
   `&["episodic", "semantic"]`.

2. **`handlers.rs` — `handle_remember`** rewrite:
   - Accept args: `content` (required), `memory_type` (optional, default `"episodic"`),
     `importance` (optional, default `0.5`), `decay_factor` (optional, default `0.01`),
     `source_id` (optional UUID), `tags` (optional), `namespace`.
   - Validate `memory_type ∈ {episodic, semantic}` if provided.
   - Build the note via the storage builder: `Note::new(...).with_salience(importance).with_decay(decay_factor)`.
     `Note::with_decay` is already present in `crates/khive-storage/src/note.rs:60`; the handler
     constructs the fully-initialised `Note` value and passes it to the runtime rather than relying
     on `runtime.create_note` to accept `decay_factor` as a parameter. Alternatively, extend
     `KhiveRuntime::create_note` (in `crates/khive-runtime/src/operations.rs`) to accept an
     optional `decay_factor: Option<f64>` parameter that calls `Note::with_decay` before
     persistence — either approach is acceptable; the important constraint is that
     `decay_factor` must reach the `Note` value before it is written to storage.
   - If `source_id` is provided: call `runtime.create_edge(source_id=note_id,
     target_id=source_id, relation="annotates")`. Validate `source_id` exists in the namespace
     first.

3. **`handlers.rs` — `handle_recall`** rewrite:
   - Args: `query` (required), `limit?`, `memory_type?`, `namespace?`, `min_score?`.
   - Always pass `Some("memory")` as the kind filter. Push this predicate into candidate
     retrieval (not just post-filter) — either extend `search_notes` to accept a `note_kind`
     FTS/vector candidate filter, or implement an over-fetch loop in `handle_recall` that
     fetches candidate pages until `limit` memory-kind hits are collected (ceiling: `limit * 20`
     raw candidates). Fixes codex Major #1 structurally.
   - For each hit, compute `effective_importance = salience * exp(-decay_factor * age_days)`.
   - Compute final score: `rrf * 0.70 + effective_importance * 0.20 + temporal * 0.10`.
     (`rrf` is the existing fusion output from `search_notes`; `temporal` is a fresh-first
     decay on `created_at` if not already part of the substrate fusion — match the canonical
     pipeline's exact formulation.)
   - Apply `min_score` filter.
   - If `memory_type` is specified: post-filter on `properties.memory_type`.
   - Truncate to `limit`.

4. **`tests/integration.rs`** updates:
   - Update the existing `episodic`/`semantic` registration assertions to expect `"memory"`.
   - Add: register `KgPack` + `MemoryPack`, write observation + memory notes, call `recall`,
     assert only memory notes returned. (Closes codex Major #1 test gap.)
   - Add: mixed-namespace regression test — create more than `limit * 4` non-memory notes
     (observations, insights) plus a smaller number of memory notes, call `recall(limit=5)`,
     assert all 5 results are memory-kind. This verifies candidate scoping, not just output
     filtering.
   - Add: write memory with `source_id` pointing at a Person entity, verify the `annotates`
     edge exists via `neighbors(memory_id, relation="annotates")`.
   - Add: write memory with explicit `decay_factor=0.0`, write another with default `0.01`,
     advance simulated time, recall both, assert the high-decay memory ranks lower.

### Amendments to existing ADRs (documentation only, no code)

- **ADR-023** lines 223–227: append a note linking to this ADR. The "no remember/recall on
  the agent surface" clause applies to the base KG pack only; when `khive-pack-memory` is
  loaded, those verbs are the idiomatic path for `kind="memory"` notes.
- **ADR-019** lines 141–145: append a similar note. Memory is represented as notes of
  `kind="memory"`; `remember`/`recall` are the pack-owned verbs for that kind.

Both amendments are prose-only; they do not change any Rust type, migration, or test.

## References

- [ADR-019](ADR-019-note-kind-taxonomy.md): Note Kind Taxonomy — §"What about Note vs Memory?"
  (lines 141–145) amended by this ADR
- [ADR-023](ADR-023-verb-consolidated-mcp-surface.md): Verb-Consolidated MCP Surface —
  §"remember and recall are removed entirely" (lines 223–227) amended by this ADR
- [ADR-024](ADR-024-note-search-and-cross-substrate.md): Note Search Pipeline — salience rerank
  (§"Salience-weighted rerank") and decay model (§"Decay v0.2") apply to memory notes
- [ADR-025](ADR-025-pack-standard.md): Pack Standard — the `Pack` + `PackRuntime` composition
  mechanism this pack uses; §Verb routing
- [ADR-026](ADR-026-gtd-pack.md): GTD Pack — precedent for pack-owned verbs distinct from KG CRUD
- PR #58: `feat(pack-memory)`, `feat/memory-pack`, HEAD `2192f75`
- Codex review: `https://github.com/ohdearquant/khive/pull/58#issuecomment-4491382647`
