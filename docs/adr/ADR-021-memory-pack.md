# ADR-021: Memory Pack

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Amended**: 2026-06-19, added §10 (`memory_type` to namespace routing) recording the
[ADR-007](ADR-007-namespace.md) Rev 6 carve-out: episodic memory writes stamp the caller's actor
id by default, semantic memory writes stamp the shared pool, and an explicit `namespace=`
overrides both.
**Amended**: 2026-08-30, schema V23 makes the `memory` candidate filter an
indexed FTS classifier rather than a post-MATCH row filter (#1907).
**Amended**: 2026-09-18, [new-memory creation admission](#amendment-new-memory-creation-admission)
replaces the generic-create equivalence in §4 and qualifies §10 and the neutral
consequences (#2724 proposed it; accepted 2026-09-18).
**Amended**: 2026-09-22, [memory admission at the proposal-note route withdraws the AddNote
exception](#amendment-memory-admission-at-the-proposal-note-route-withdraws-the-addnote-exception-2026-09-22)
(#2057, #2967).
**Amended**: 2026-11-01, [attributed semantic writes and caller properties](#amendment-attributed-semantic-writes-and-caller-properties-2026-11-01) routes a semantic write with no
namespace to the caller's actor namespace, replacing the shared-pool default in §10 that the 2026-06-19 entry
records, and adds `properties` to `memory.remember` (#6356; accepted 2026-11-02).
**Amended**: 2026-11-02, [batch apply for memory rewrites](#amendment-batch-apply-for-memory-rewrites-2026-11-02)
adds `memory.apply_batch`, and its
[Delta 1](#delta-1-old-rows-in-namespaces-the-caller-owns-2026-11-02) admits old rows in namespaces the caller
declares as owned (#6358, #6362, #6369; accepted 2026-11-02, #6374).
**Amended**: 2026-11-03, [Delta 2](#delta-2-supersedes-reads-cover-the-namespaces-the-reader-can-see-2026-11-03) of
the batch-apply amendment: recall reads `supersedes` edges in every namespace the reader can see, and
`memory.remember` reserves the `supersedes` property (#6386; proposed).
**Amended**: 2026-11-04, [Delta 3](#delta-3-an-edge-between-memory-rows-lives-in-its-source-rows-namespace-2026-11-04) of
the batch-apply amendment: an edge between two memory rows is written in the source row's namespace by every verb,
and edge reads report how many such edges they left out (#6380; proposed).

## Context

[ADR-013](ADR-013-note-kind-taxonomy.md) registers `memory` as a pack-extensible note kind
owned by `khive-pack-memory`. [ADR-019](ADR-019-gtd-pack.md) is the canonical
_lifecycle-shape_ pack example, demonstrating notes-as-tasks with a state machine. The
memory pack is the canonical _decay-shape_ pack example, demonstrating notes-as-memories
with salience-weighted time decay.

Memory is structurally distinct from tasks. Tasks have a deterministic state machine
(inbox → next → active → done) and pre-condition validation on transitions. Memory has no
state machine — it is created, retrieved with decay-weighted scoring, and eventually
deleted. The interesting machinery is in _retrieval_, not in _lifecycle_. Memory verbs
(`remember`, `recall`) compose differently from GTD verbs but follow the same pack
standard from [ADR-017](ADR-017-pack-standard.md).

A memory pack also has direct utility for the agent workflow: persistent salience-weighted
context that decays over time matches how agents accumulate working knowledge across
sessions. The first-class memory pack closes a gap that ad-hoc note storage cannot fill —
generic notes lack the decay column and the source-attribution edge.

### Scope

This ADR specifies the memory pack's vocabulary, verb surface, semantic contract, and
composition pattern. It does NOT prescribe specific retrieval-weight tuning beyond the
v1 defaults — recall scoring is a research surface that will iterate, and the weights
specified here are starting values, not invariants.

## Decision

### 1. One `memory` kind, `memory_type` as attribute

The pack registers a single note kind: `memory`. Both episodic and semantic memories are
stored under this kind, distinguished by a `memory_type` attribute on the note:

| Attribute     | Values                       | Default      | Storage           |
| ------------- | ---------------------------- | ------------ | ----------------- |
| `memory_type` | `"episodic"` \| `"semantic"` | `"episodic"` | `note.properties` |

| memory_type | Shape                       | Examples                                                        |
| ----------- | --------------------------- | --------------------------------------------------------------- |
| `episodic`  | Time-anchored, event-shaped | "On 2026-05-19 a maintainer said prefer `uv run` over `python`" |
| `semantic`  | Abstracted, fact-shaped     | "Maintainers prefer `uv run` over `python`"                     |

The distinction is **advisory, not enforced**. Nothing structurally validates that
episodic memories carry timestamps. Agents choose `memory_type` based on whether the
content is primarily event-oriented or persistent-fact-oriented; misclassification is
tolerated.

Two kinds (`episodic` and `semantic`) would force a two-search merge in `recall`,
complicate per-type retrieval branches, and gain nothing over a queryable property.

### 2. `salience` and `decay_factor`; `decay_factor` defaults to mild decay

The notes substrate carries `salience` (the primary signal used by retrieval rerank,
[ADR-012](ADR-012-retrieval-composition.md)) and `decay_factor` (per-note exponential
decay rate). The memory pack does NOT introduce new columns; it uses these substrate
fields directly:

| Wire parameter | Storage column | Default (episodic)         | Default (semantic)           |
| -------------- | -------------- | -------------------------- | ---------------------------- |
| `salience`     | `salience`     | `0.3`                      | `0.5`                        |
| `decay_factor` | `decay_factor` | `0.02` (~35-day half-life) | `0.005` (~139-day half-life) |

There is no aliasing — the wire parameter and storage column are both `salience`.

The substrate-wide `decay_factor` default is `0.0` (no decay). The memory pack handler
overrides this default using the resolved `memory_type`: episodic notes receive
`0.02` (~35-day half-life) and semantic notes receive `0.005` (~139-day half-life), so
memory participates in time decay by default while other note kinds remain unaffected.

### 3. `source` is an `annotates` edge, not a stored field

A memory's provenance — who or what produced it — is represented as an `annotates` edge
from the memory note to the source entity or note. Per [ADR-002](ADR-002-edge-ontology.md),
`annotates` is the universal note → any-substrate relation; pack-extensible endpoint
rules ([ADR-017](ADR-017-pack-standard.md)) accept any target.

The `remember` verb accepts an optional `source_id` argument. When present, the handler
creates the memory note and links it to the source via `annotates` in the same invocation.
When absent, no edge is created and the memory's provenance is unattributed.

For the common case "a maintainer said X" the source is a `person` entity. For "agent X
produced this" the source is whichever entity represents the agent. For "this came from
paper Y" the source is the `document` entity. All three resolve through the same
`annotates` edge with no special-casing — provenance is queryable via
`neighbors(memory_id, relation="annotates")`.

Storing source as a free string in `properties` would couple the memory pack to a future
actor-identity ADR for the string format, and would not participate in graph traversal.
Edges are the right substrate for "this came from X" relationships.

### 4. `memory.remember` — sugar over `create` + optional `link`

The generic-create equivalence in this section is superseded by the
[creation-admission amendment](#amendment-new-memory-creation-admission). It is kept here
because the equivalence still describes what `memory.remember` does; what changed is that a
caller may no longer reach it through generic create.

```
memory.remember(content, memory_type?, salience?, decay_factor?, source_id?, tags?, namespace?)
```

Semantically equivalent to:

```
1. id = create(
     kind = "memory",
     content = content,
     salience = <salience | (episodic: 0.3, semantic: 0.5)>,
     decay_factor = <decay_factor | (episodic: 0.02, semantic: 0.005)>,
     properties = { memory_type: <memory_type | "episodic"> },
     tags = <tags | []>,
     namespace = namespace,
   )

2. if source_id is provided:
     link(source_id = id, target_id = source_id, relation = "annotates")
```

The handler validates: (a) `content` non-empty, (b) `memory_type ∈ {episodic, semantic}`
if provided, (c) `salience ∈ [0, 1]`, (d) `decay_factor >= 0`, (e) `source_id` is a
valid UUID present in the namespace.

The write `namespace` is resolved per `memory_type` (ADR-007 Rev 6, full contract in §10): when
no explicit `namespace=` argument is supplied, a semantic write stamps `token.namespace()` (the
shared pool, `'local'` by default) and an episodic write stamps `token.actor().id` (the caller's
actor id). An explicit `namespace=` argument overrides this for both types.

Explicit CRUD is no longer an equivalent route. The
[creation-admission amendment](#amendment-new-memory-creation-admission) refuses
`create(kind="memory", ...)` even when the caller supplies `salience`, `decay_factor` and
`memory_type` itself, because such a request still carries neither the episodic actor routing nor
the keyed-replay contract `memory.remember` provides. The equivalence above describes what that
verb does; it is not a second way in.

### 5. `memory.recall` — memory-scoped retrieval with decay weighting

```
memory.recall(query, limit?, memory_type?, namespace?, min_score?)
```

A memory-scoped variant of `search(kind="note", ...)` with three behaviours that
distinguish it from generic note search:

1. **Candidate scoping.** The handler passes `kind="memory"` as the candidate filter,
   pushed into FTS5 and vector-search retrieval (not as a post-filter). In a mixed
   `kg,gtd,memory` namespace with thousands of non-memory notes, this prevents
   high-ranking non-memory notes from filling the candidate pool before any memory note
   is considered. The SQLite backend stores granular note kind in the indexed
   `record_kind` FTS column and intersects `record_kind : "memory"` with the lexical
   query inside MATCH; an exact row predicate remains the equality backstop for the
   trigram tokenizer. If the underlying `search_notes` operation applies kind only as a
   post-filter, the handler implements bounded over-fetch (ceiling `limit * 20` raw
   candidates) until `limit` memory hits are collected.
2. **Decay-weighted scoring.** Each candidate's `salience` is decayed by age:
   `effective_salience = salience * exp(-decay_factor * age_days)`, where `age_days`
   is `(now - created_at) / seconds_per_day`. Decay is computed per candidate using the
   note's own `decay_factor` (allowing per-memory decay rates).
3. **Score fusion.** Final score combines the substrate's RRF hybrid score (FTS5 + vector)
   with the decayed salience and a temporal fresh-first signal:

   ```
   score = rrf_score * 0.70 + effective_salience * 0.20 + temporal * 0.10
   ```

   These weights are the v1 defaults. Per-`memory_type` overrides (heavier decay for
   episodic, plain RRF for semantic) are forward-compatible: the handler can branch on
   `properties.memory_type` without any schema change.

`memory_type` (optional) filters to `episodic` or `semantic`. Notes with no stored
`memory_type` property resolve to `episodic` (the default) for both filtering and scoring.
Default (no filter) returns both types.

Each recall hit carries **resolved (read-model) values**: `memory_type` is always a
non-null string (`"episodic"` or `"semantic"`), and `salience`/`decay_factor` reflect the
effective values used for ranking — the stored values when present, or the type-appropriate
defaults (`episodic: 0.3/0.02`, `semantic: 0.5/0.005`) when the stored columns are NULL.
This contrasts with `get`, which returns raw stored fields for curation purposes.

`min_score` truncates low-scoring matches before returning.

### 6. No `forget` verb

The pack registers no `forget`. Memory deletion uses the substrate verb:
`delete(id=<memory_uuid>)`. This is consistent with the soft-delete contract from
[ADR-014](ADR-014-curation-operations.md) — `delete` resolves the UUID to a note,
soft-deletes it, and excludes it from subsequent `recall` candidate sets.

A pack-owned `forget` would either duplicate `delete` (verb pollution) or carry different
semantics (e.g., permanent purge) that have no demonstrated need. Deferred until a
concrete use case requires it.

### 7. Pack composition

The memory pack is a thin pack over the notes substrate. It declares:

- **Vocabulary**: one note kind (`memory`); no entity kinds; no new edge endpoint rules
  (the existing `annotates` rule from the kg pack accepts any note→any-substrate target,
  which covers `memory→entity` and `memory→note` provenance edges)
- **Verbs**: `remember`, `recall`, `feedback`, `prune`, `vacuum`
- **Storage profile**: hot tier (same as kg/gtd packs); `default_backend="main"`
- **Requires**: `kg` (memory pack delegates CRUD to kg-pack note handlers)

The `requires = ["kg"]` field uses the inter-pack dependency mechanism from
[ADR-017](ADR-017-pack-standard.md) — boot-time check ensures `khive-pack-kg` is loaded
before `khive-pack-memory` registers.

The pack does NOT register any `KindHook` specialization. Memory notes route through the
kg pack's standard note CRUD handlers; the memory pack's verbs are convenience
constructions over those handlers.

### 8. Brain integration (forward reference)

When `khive-pack-brain` ([ADR-032](ADR-032-brain-profile-orchestration.md)) is loaded
alongside this pack, the `recall` handler consults brain's resolved profile before
running the candidate-fusion stage:

```text
memory.recall(query, …):
  1. P = brain.resolve(actor, namespace, consumer_kind="recall") on miss → defaults
  2. weights = P.config_overrides (RRF / salience / temporal weights — §5)
  3. candidates = recall_embed → recall_candidates (multi-engine if ADR-031 loaded)
  4. fused = recall_fuse(candidates, weights)
  5. stamp each result and RecallExecuted with the serve-attribution state
```

The serve-attribution state is wire-visible and has three values:

| `serve_attribution` | `served_by_profile_id` | Meaning                                                                                             |
| ------------------- | ---------------------- | --------------------------------------------------------------------------------------------------- |
| `"profile"`         | profile id             | the profile record was readable and its state served the request                                    |
| `"unattributed"`    | absent / null          | profile resolution selected a profile, but its record could not be read; configured defaults served |
| `"unspecified"`     | absent / null          | no profile was supplied or resolved                                                                 |

The distinction between `unattributed` and `unspecified` is load-bearing. Downstream
feedback may retain the historical binding/default fallback only for `unspecified` (and
legacy calls that omit the marker). `unattributed` is explicit negative knowledge and
must never be re-resolved to a profile. A readable profile record whose state snapshot is
null remains `profile`: the configured-default bootstrap state is still a successful
profile serve.

The §5 scoring formula's `0.70 / 0.20 / 0.10` weights are the **defaults**; brain's
resolved profile may override them per `(actor, namespace, consumer_kind)` binding
context (ADR-032 §10). Profiles whose `state_class` is `Bayesian` (the v1 state class
— ADR-032 §5a) supply scalar weight overrides; future LoRA-class profiles (ADR-032
§5b, gated on ADR-042) supply an adapter hook to the rerank step.

If brain is not loaded, the recall handler runs with the §5 defaults — the pack
remains fully usable in single-pack deployments.

See ADR-033 §8 for the recall pipeline's brain integration details and ADR-032
§6 for the data-flow contract.

### 9. Multiple memory pack instances

Operators may deploy multiple memory pack instances (e.g., `memory-hot` for
recent/active recall, `memory-cold` for archive). Both instances declare `kind=memory`.

Per [ADR-017](ADR-017-pack-standard.md)'s KindRoute model:

- **`memory.recall(kind="memory")`** fans out across all enabled instances; results fuse via
  backend-level RRF.
- **`memory.remember(kind="memory")`** writes to the operator-declared `primary_write_instance`
  (config). Explicit `instance="memory-cold"` overrides this (subject to auth).
- Operators MUST declare `primary_write_instance` when more than one memory instance is
  enabled — registration order is not a valid tiebreaker.

Sample config:

```yaml
packs:
  memory-hot:
    kind: memory
    db: ./hot.db
  memory-cold:
    kind: memory
    db: ./cold.db
runtime:
  kind_routing:
    memory:
      primary_write_instance: memory-hot
```

### 10. `memory_type` → namespace routing (ADR-007 Rev 6)

The write namespace of a memory note is resolved from its `memory_type`, per the Rev 6 carve-out
in [ADR-007](ADR-007-namespace.md) Rule 0. This is the single place in the system where the write
namespace depends on the kind of record being written; every other write path pins `'local'` by
default.

| `memory_type` | Default write namespace (no `namespace=` argument)      | With explicit `namespace=X` |
| ------------- | ------------------------------------------------------- | --------------------------- |
| `semantic`    | `token.namespace()` (shared pool, `'local'` by default) | `X`                         |
| `episodic`    | `token.actor().id` (the caller's actor id)              | `X`                         |

Resolution rules:

- The explicit `namespace=` argument on `memory.remember` (and on the equivalent
  `create(kind="memory", ...)`) overrides the default for BOTH memory types. It is the precise
  escape: `memory.remember(content="...", memory_type="episodic", namespace="local")` writes the
  episodic memory into the shared pool, and `memory.remember(content="...",
  memory_type="semantic", namespace="ns-x")` writes the semantic memory into `ns-x`.
  The [creation-admission amendment](#amendment-new-memory-creation-admission) removes the
  generic-create equivalence; the `memory.remember` routing rules themselves are unchanged.
- The episodic default uses `token.actor().id`, the actor identity threaded onto the request token
  (ADR-053). An anonymous caller has actor id `'local'` (`ActorRef::anonymous`), so when no
  `[actor]` is configured the episodic default resolves to `'local'`, identical to the semantic
  default and to pre-Rev-6 behavior. The per-actor episodic namespace takes effect only once a
  real actor is configured.
- The default `memory_type` is `episodic` (§1). A `memory.remember` call that supplies neither
  `memory_type` nor `namespace=` therefore writes under the caller's actor id, which is `'local'`
  for an anonymous caller and the configured actor id otherwise.

Why episodic is per-actor and semantic is shared: an episodic memory is a specific actor's lived
experience, so it is attributed to that actor and private to that actor's default recall view. A
semantic memory is distilled, shareable knowledge, so it belongs in the common pool. This is the
per-actor-episodic plus shared-semantic model. The prior contract stamped both memory types into
`'local'`, which conflated one actor's experience with the shared knowledge base.

This is attribution plus view-layer visibility, not storage isolation:

- The store performs no `record.namespace == caller_namespace` check (ADR-007 Rule 2). By-ID ops
  on a memory note (`get`, `update`, `delete`, `merge` by UUID) are namespace-agnostic regardless
  of which actor wrote the memory. `delete(id=<memory_uuid>)` (§6) works across actors.
- Recall reads operate over the caller's read visible-set,
  `{local} ∪ {actor.id} ∪ {actor.visible_namespaces}` (ADR-007 Rule 3b). An actor's own episodic
  memories are visible to its default recall because the set always includes `{actor.id}`. Another
  actor sees them only if its configured `visible_namespaces` includes the writer's id. This is
  the intended privacy boundary, realized as a read-scope (view) decision, not a storage
  partition.
- `memory.recall` requires no change for the Rev 6 routing. The visible-set fanout already
  returns actor-id-stamped memories on both the FTS candidate path and the ANN post-filter path;
  episodic memories written under the caller's actor id are in scope by construction.

The `memory_type` post-filter on recall (§5) is orthogonal to namespace routing: it filters the
candidate set by the stored `properties.memory_type` after candidate scoping, independent of
which namespace a candidate was written to.

## Rationale

### Why pack-owned `remember` / `recall` instead of generic CRUD

The alternative is to strip `remember`/`recall` and force agents to use
`create(kind="memory", ...)` and `search(kind="note", note_kind="memory", ...)`. This
produces a memory pack that contributes a kind but no verbs — a vocabulary pack, not a
domain pack.

It conflicts with the precedent [ADR-019](ADR-019-gtd-pack.md) establishes. The GTD pack
introduces `assign` instead of `create(kind="task")` precisely because domain-specific
verbs are more legible, enforce preconditions that generic CRUD does not, and reflect the
pack's semantic ownership of its lifecycle.

The same logic applies here:

- `remember` validates `memory_type`, normalizes `salience` and `decay_factor`
  defaults, and creates the `annotates` edge in a single call. Generic `create` + `link`
  requires the agent to know two verbs and the precise edge relation.
- `recall` enforces the memory-only candidate scoping (preventing leak of `task` or
  `observation` notes into recall results) and applies decay weighting. Generic `search`
  has neither built in.

Neither is merely cosmetic; both encode pack-specific semantics that would otherwise be
re-implemented by every memory-using agent.

### Why one `memory` kind (not two)

An earlier draft considered separate `episodic` and `semantic` note kinds. One kind +
attribute has two advantages:

1. **Single filter at recall.** `recall` always passes `kind="memory"` as the candidate
   filter. No two-search merge. No kind-set juggling. The recall-leak bug class is fixed
   structurally.
2. **Forward-compatible per-type retrieval policy.** Future revisions may apply different
   pipelines to episodic vs. semantic (e.g., heavier time decay for episodic, plain RRF
   for semantic). With `memory_type` as an attribute, that becomes a handler-level branch
   on a property. With separate kinds, it would require coordinating two runtime queries.

The cost: callers querying through generic `search(kind="note", note_kind="memory")`
get both types mixed; filtering on `memory_type` requires a `properties.memory_type`
post-filter or use of the `recall` verb's `memory_type` argument. Acceptable.

### Why decay defaults are type-differentiated (not a flat `0.01`)

The substrate-wide default of `0.0` (no decay) is correct for note kinds where age is
not a relevance signal (e.g., decisions, references). For memory, age IS a relevance
signal — a memory from yesterday is more salient than one from a year ago, all else
equal.

Episodic memories (session events) are inherently transient. A flat `0.01` default
was revised to a type-differentiated scheme: episodic uses `0.02` (~35-day half-life)
so session context ages out faster, while semantic memories use `0.005` (~139-day
half-life) because durable facts should remain retrievable much longer. Salience
defaults follow the same logic: episodic `0.3` vs. semantic `0.5`.

Agents that want different decay characteristics override per-memory:
`memory.remember(content="...", decay_factor=0.05)` for fast-fading episodic content,
`memory.remember(content="...", decay_factor=0.0)` for permanent semantic facts.

### Why `salience` (not a separate `importance` column)

The `salience` column already exists on the notes substrate and participates in the
[ADR-012](ADR-012-retrieval-composition.md) rerank pipeline. Using `salience` directly
as both the wire parameter and storage column keeps the interface consistent across all
packs — the memory pack uses the same vocabulary as the substrate and other packs, with
no aliasing or translation layer.

An earlier draft aliased `importance` → `salience`. That alias was eliminated (2026-05-25)
to enforce a single consistent term throughout the codebase. The storage column
`notes.salience` was always canonical; the wire-level alias was the anomaly.

### Why scoring weights `0.70 / 0.20 / 0.10`

These are v1 starting values, not architectural invariants. The weights say: the
hybrid retrieval score (FTS5 + vector via RRF) is the primary signal, decayed salience
is a secondary signal, and freshness is a tertiary signal. Future research-driven
recalibration — Beta-Bernoulli posterior over recall hits, adaptive decay adjustment,
per-`memory_type` weight tables — lands in a separate ADR when the research informing
them is in place.

The weights are configurable per deployment via the pack's storage profile config
parameters (mechanism from [ADR-017](ADR-017-pack-standard.md)), not hard-coded in
handler logic.

## Alternatives Considered

| Alternative                                                          | Why rejected                                                                                                                                                           |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Strip `remember`/`recall`; pack provides kind only                   | Loses domain-specific preconditions; contradicts GTD precedent; memory pack becomes a vocabulary pack with no semantic ownership.                                      |
| Two memory kinds (`episodic`/`semantic`)                             | Forces two-search merge in `recall`; complicates per-type retrieval policy; mixed-namespace recall correctness is harder.                                              |
| Separate `importance` column (aliasing `salience`)                   | Duplicate terms cause confusion; the alias was eliminated 2026-05-25 in favour of `salience` throughout.                                                               |
| Store `source` as a string in `properties`                           | Couples memory to a future actor-identity string format; not traversable via `neighbors`/`traverse`; loses the universal `annotates` edge.                             |
| Enforce `episodic`/`semantic` distinction with structural validation | Arbitrary; agents disagree on what "time-anchored" means; validation complexity outweighs the gain.                                                                    |
| `forget` as a pack-owned verb                                        | Duplicates `delete`; no demonstrated use case for distinct semantics; verb pollution.                                                                                  |
| Defer decay to v2                                                    | `decay_factor` column already exists; recall pipeline already supports decay-weighted scoring; deferring would mean memory recall is identical to generic note search. |
| Hard-code decay/scoring weights in handler                           | Loses tunability across deployments; future research recalibration would require code changes.                                                                         |

## Consequences

### Positive

- `remember`/`recall` join `assign`/`next`/`complete` as domain-specific pack verbs over
  the notes substrate. The verb surface grows coherently with each pack.
- Decay is wired from day one — agents using `recall` get age-weighted results matching
  intuitions about memory salience.
- Provenance is queryable via graph traversal (`neighbors`, `traverse`) without any new
  verb or storage.
- Per-`memory_type` retrieval policy is unblocked future work — no schema change required
  to branch the handler on `properties.memory_type`.
- The pack composes cleanly with the kg pack (delegates note CRUD), the gtd pack
  (coexists without interaction), and future packs that may use `annotates` edges as
  pack-extensible provenance.

### Negative

- The `recall` handler is slightly heavier than thin syntactic sugar — it does (a) note
  creation, (b) optional edge creation, and (c) decay-aware score fusion. The complexity
  is bounded to the pack and is the price of a substantive memory model.
- `memory_type` lives in `properties` JSON, which is not directly indexable. If
  `memory_type`-filtered recall becomes a hot path with very large memory namespaces, a
  future migration can promote it to a typed column.
- Scoring weights (`0.70 / 0.20 / 0.10`) are v1 starting values, not validated against
  any specific benchmark. Future tuning is expected.

### Neutral

- No schema migration. No DDL change. No new edge relation. No new entity kind.
  `annotates` already accepts note → any-substrate per [ADR-002](ADR-002-edge-ontology.md).
- `khive-pack-kg` is unaffected. Its `search`/`create` paths continue to work as
  specified; the memory pack uses them via the runtime's pack-extensible verb dispatch.
  The [creation-admission amendment](#amendment-new-memory-creation-admission) qualifies this
  statement for generic creation, new `stream.batch` memory writes, and standalone `stream.append`
  memory writes: those paths refuse through the memory pack's own create hook, and every other kg
  path is unaffected.
- The pack composes with [ADR-020](ADR-020-git-native-kg-implementation.md) git-native
  KG: memory notes are part of the notes substrate, which is excluded from v1 KG
  snapshots ([ADR-010](ADR-010-kg-versioning.md) §SnapshotCoverage). Memory persists in
  `working.db` and the main database; cross-instance memory portability is v2 work.

## Amendment: new-memory creation admission

**Status**: accepted 2026-09-18, scope clarification dated 2026-09-14. This revision corrects the
earlier amendment's description of public stream append and explicitly preserves
proposal-based note creation. The clarified contract was accepted before its
implementation merged. It partially supersedes §4's
generic-create equivalence, the equivalent-create clause in §10, and the
corresponding neutral consequence. Other decisions remain accepted and unchanged.

### Problem and decision

Generic memory creation currently permits rows with absent `memory_type`,
`salience`, or `decay_factor`. Recall may report effective defaults that were never
stored, so recall and raw or generic reads can describe different values for the
same record. Generic creation also does not implement the specialized
writer's complete decay and actor-routing contract. Treating these entry points
as interchangeable promises behavior shared create does not provide.

When the memory pack is loaded, `memory.remember` is the supported replacement for
generic create, new `stream.batch` memory writes, and standalone `stream.append` memory writes.
Shared generic creation of
the memory kind refuses before creating a note or provenance edge, with an error
naming `memory.remember` as the supported writer. This includes
`create(kind="memory", ...)` and
`create(kind="note", note_kind="memory", ...)`, including keyed requests,
explicit namespaces and requests supplying defaults themselves. Existing
validation may reject malformed arguments before reaching this refusal.

The memory pack owns admission through the existing
[ADR-017](ADR-017-pack-standard.md) `KindHook::prepare_create` interface. KG does
not hard-code memory, redirect requests, or enforce a general prohibition on kinds
with specialized verbs. Generic task creation retains its existing behavior.

### Stream consequence and preserved operations

The shared creation hook also governs a new `stream.batch` write whose kind is
memory and a standalone `stream.append` whose `note_kind` is memory. Those operations refuse
with the same directing error. A batch refusal is a preparation failure: no preceding sibling
write in that batch commits. This consequence is part of the decision, not a new per-member
conflict result. Public `stream.batch` append members continue to create observations.

Approved `propose`/`review` changesets containing `AddNote` retain their existing registered-kind
admission because they are the one creation route outside the shared create hook. This amendment
neither redirects that route to `memory.remember` nor gives it the stored-default, namespace-routing
or keyed-replay contract.

`memory.remember` uses the memory writer directly and remains admitted, including
existing keyed replay. It retains type-specific stored defaults, explicit zero
values, source annotations, embedding selection and the namespace rules in §10.
This amendment does not change its key equality, replay result or conflict
contract.

The create-only hook does not deny generic reads, searches, updates, deletion,
restoration or other already-supported operations on existing memories. An
expected-version stream write to an existing keyed memory remains governed by
existing update rules. Internal note constructors and historical raw rows remain
available to their existing callers; this is not a universal storage invariant.
Existing missing fields are neither backfilled nor repaired on read, and recall's
legacy effective defaults remain unchanged. No supported memory snapshot-import
roundtrip is introduced or claimed.

### Compatibility, alternative and validation

Callers of generic create and new `stream.batch` memory writes must move to the
actual `memory.remember` parameter and result contract. There is no silent
redirect or compatibility alias with different namespace or key semantics.
[ADR-007](ADR-007-namespace.md#qualification-memory-creation-admission)
reciprocally qualifies its generic-create equivalence; specialized memory routing
and namespace-as-attribution decisions remain unchanged.

The alternative is full shared normalization, including stored type, salience,
decay and authenticated actor/explicit-namespace routing. Inserting only default
properties into generic create does not satisfy that alternative. The directing
refusal reuses the complete specialized writer without extending shared creation
arguments or the hook interface.

Acceptance tests exercise real KG and memory packs: both generic spellings refuse
without changing notes, key holders or provenance edges; new `stream.batch`
memory writes refuse before sibling mutations; specialized default/explicit writes
are visible in raw storage, generic get and a nonempty matching recall; identical
keyed replay preserves the original record; existing-memory updates and other kinds remain
usable. Error audit records are outside the no-domain-mutation assertion. Tests
retain legacy-row read behavior and show that reads do not fill absent fields.
No native execution or performance result is asserted by this proposal.

No schema migration, dependency, new verb or graph relation is required. Reverting
the hook restores prior input admission; it does not migrate existing data.

## Amendment: memory admission at the proposal-note route withdraws the AddNote exception (2026-09-22)

**Status**: accepted 2026-09-22. This amendment withdraws the preceding amendment's explicit
preservation of `AddNote` for the memory kind: "Approved `propose`/`review` changesets containing
`AddNote` retain their existing registered-kind admission because they are the one creation route
outside the shared create hook" no longer holds for `kind="memory"`. Every other kind's `AddNote`
admission is unaffected.

Originating issue(s): #2057, #2967

### Decision

The memory pack registers the kind-owned, validation-only proposal-note hook
[ADR-017](ADR-017-pack-standard.md)'s 2026-09-22 amendment defines, for `kind="memory"`. This
pure-validation check runs at two points against the same `AddNote` changeset:

- **Proposal creation.** A new `propose` call whose changeset contains an `AddNote(kind="memory",
  ...)` entry is refused before the proposal is created.
- **Apply.** An approved proposal whose changeset contains such an entry is refused at apply,
  including a proposal approved before this amendment landed.

Both refusals use the canonical invalid-input error shape with `details.reason =
"kind_admission_refused"`, `details.kind = "memory"`, `details.route = "proposal_add_note"`, and a
message directing the caller to `memory.remember`. The refusal fires even when the draft already
supplies `memory_type`, `salience`, `decay_factor`, or an otherwise apparently complete set of
defaults: those inputs still do not supply `memory.remember`'s typed stored columns, authenticated
actor/namespace routing, or keyed replay contract, so the shared creation hook's rationale for
refusing `create(kind="memory", ...)` applies identically to the proposal route.

### Pre-upgrade proposals

Approval of an `AddNote(kind="memory")` proposal authorized an immutable changeset subject to
apply-time revalidation; it was never proof that the write would commit. For a proposal approved
before this amendment, apply now fails visibly with the shape above, and:

- writes no new memory note, edge, or provenance record;
- leaves the approved draft and its full review history unchanged and intact;
- MUST NOT silently rewrite the proposal, report it applied, redirect the write to
  `memory.remember` on the caller's behalf, or otherwise grandfather the creation.

No automatic migration or backfill of pre-upgrade proposals or their history is performed.

### Unaffected

`memory.remember` is unaffected: it remains the supported writer, with its existing type-specific
defaults, explicit numeric values, embedding selection, and namespace and keyed-replay contract.
Reads, updates, deletion, restoration, and `AddNote` proposals targeting any kind other than
memory are unaffected by this amendment.

### Acceptance

A fresh `AddNote(kind="memory")` proposal is refused at creation with the shape above, and no
proposal row is created. A proposal of the same shape approved under the pre-amendment contract is
refused at apply with the same shape, with its draft and review history read back unchanged.
`memory.remember` and `AddNote` proposals of every other kind are unaffected by either check.

## Implementation

### Pack manifest

```rust
// crates/khive-pack-memory/src/lib.rs
pub struct MemoryPack;

impl Pack for MemoryPack {
    const NAME: &'static str = "memory";
    const NOTE_KINDS: &'static [&'static str] = &["memory"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef { name: "memory.remember",          description: "...", visibility: Visibility::Verb },
        HandlerDef { name: "memory.recall",            description: "...", visibility: Visibility::Verb },
        HandlerDef { name: "memory.recall_embed",      description: "...", visibility: Visibility::Subhandler },
        HandlerDef { name: "memory.recall_candidates", description: "...", visibility: Visibility::Subhandler },
        HandlerDef { name: "memory.recall_fuse",       description: "...", visibility: Visibility::Subhandler },
        HandlerDef { name: "memory.recall_score",      description: "...", visibility: Visibility::Subhandler },
    ];
    // ADR-023 §4: pack-prefixed verb names — `memory.remember` / `memory.recall` (Verb)
    //             `memory.recall_embed` / `memory.recall_candidates` / `memory.recall_fuse` / `memory.recall_score` (Subhandler)
    const REQUIRES: &'static [&'static str] = &["kg"];
}
```

The pack's `StorageProfile` (from [ADR-003](ADR-003-system-architecture.md) /
[ADR-017](ADR-017-pack-standard.md)) is `hot` with `default_backend="main"`.

### Handler responsibilities

`remember`:

- Validate inputs (content non-empty, memory_type ∈ {episodic, semantic}, salience ∈ [0,1])
- Construct `Note` via the storage builder with `salience`, `decay_factor`,
  `properties.memory_type`
- Persist via `runtime.create_note(...)`
- If `source_id`: validate it exists in the namespace; create `annotates` edge

`recall`:

- Build candidate request with `kind="memory"` as candidate filter (push into FTS5/vector
  candidate retrieval, not just post-filter)
- For each candidate: compute `effective_salience = salience * exp(-decay_factor * age_days)`,
  apply score fusion formula
- Apply optional `memory_type` post-filter
- Apply `min_score` truncation, then `limit`

### Tests

| Scenario                                                        | Assert                                                                      |
| --------------------------------------------------------------- | --------------------------------------------------------------------------- |
| `memory.remember(content="x")` defaults                         | `memory_type="episodic"`, `salience=0.3`, `decay_factor=0.02`               |
| `memory.remember(content="x", memory_type="semantic")` defaults | `salience=0.5`, `decay_factor=0.005`                                        |
| Explicit `salience=0.5` with episodic type                      | Stored value is `0.5`, not overridden by type default                       |
| Explicit `decay_factor=0.01` with episodic type                 | Stored value is `0.01`, not overridden by type default                      |
| `memory.remember(... source_id=P)`                              | `annotates` edge from memory_id to P exists                                 |
| `memory.recall(query="x")` excludes non-memory notes            | Mixed namespace with observations + tasks; no leak                          |
| `recall` with mixed namespace > `limit * 4` non-memory          | Candidate scoping pushes filter into retrieval; correct limit hits returned |
| Decay-weighted ranking                                          | High-decay old memory ranks below low-decay equivalent                      |
| `memory_type` post-filter                                       | Returns only specified type                                                 |
| `delete(memory_id)` works without forget verb                   | Subsequent recall excludes the deleted memory                               |

## References

- [ADR-007](ADR-007-namespace.md): Namespace as attribution-only. Rev 6 episodic-memory
  write carve-out (Rule 0 amendment) and the read visible-set fanout (Rule 3b) that makes
  per-actor episodic memory visible to its own default recall. §10 here is the consumer-side
  statement of that carve-out.
- [ADR-002](ADR-002-edge-ontology.md): Edge ontology — `annotates` as the universal note →
  any-substrate relation
- [ADR-012](ADR-012-retrieval-composition.md): Retrieval composition — RRF hybrid score
  feeds the recall fusion formula
- [ADR-013](ADR-013-note-kind-taxonomy.md): Note kind taxonomy — `memory` registered as
  pack-extensible kind
- [ADR-014](ADR-014-curation-operations.md): Curation operations — `delete` is the path
  for memory removal
- [ADR-017](ADR-017-pack-standard.md): Pack standard — composition mechanism this pack uses
- [ADR-019](ADR-019-gtd-pack.md): GTD pack — the lifecycle-shape pack contrasting with
  the decay-shape pack defined here
- `crates/khive-pack-memory/`: implementation

## Amendment: declared recall-event correlation (2026-10-06)

**Status**: Proposed\
**Companion**: [ADR-081 Amendment 1](ADR-081-recall-retune-driver.md#amendment-1-2026-10-06-declared-recall-event-correlation)
defines the feedback, validation, authority and acceptance contract. This section has
its own Proposed status; it does not change the Accepted status of the preceding
record or implement the new field.

For each successful nonempty `memory.recall`, add one fresh canonical lowercase dashed
UUIDv4 `recall_event_id` to every actual returned hit object. The value is shared within
that invocation and differs between otherwise identical recalls. Preserve result
order, profile stamps, bare arrays and existing verbose/degraded/truncated `results`
envelopes. Diagnostic candidate lists are not returned hits. Human-oriented
presentations may omit metadata; correlation consumers forward structured results.
Empty responses retain their current shape with no top-level ID, and failed calls
return no correlation identifier.

Allocate the ID before response projection/background handoff. When the existing
best-effort `RecallExecuted` append succeeds, its physical `Event.id` is that exact
UUID. Existing zero-result telemetry may generate an unreturned ID. Task admission,
timeouts and telemetry failures remain off the response path: the returned ID declares
an intended event identity, not a committed receipt. Feedback may arrive before the
event or refer to an event that never persists; a missing row is unknown. Duplicate
physical event IDs refuse append, including identical replays, without overwriting
the event or its projections or retrying with a replacement ID.

Automatic feedback forwards only the uniquely selected result's optional correlation.
The shared feedback handler also accepts it directly under the ADR-081 validation
rules. Missing/null remains compatible; no-signal abstention does not validate
correlation. The value is unverified caller context, never authority to read an event,
change a namespace/profile, or bypass scorer/dedup/clamp rules. No recall lookup,
synchronous durability, backfill or new storage schema is introduced.

Both Proposed sections require formal acceptance before dependent implementation
merges. Implementation and its acceptance use actual `main` after #4277 and #4279
land, with their changes preserved. ADR-081's real-path acceptance includes physical
event-ID equality across repeated recalls, selected-result forwarding, legacy and
malformed inputs, namespace isolation, task/append failures and duplicate-ID refusal.
No implementation or executed acceptance is claimed by this documentation change.

## Amendment: attributed semantic writes and caller properties (2026-11-01)

**Status**: Accepted 2026-11-02 (Proposed 2026-11-01). Amends §4 (the `memory.remember` arguments), §5 (the recall hit
shape) and §10 (the semantic row of the routing table).

### Problem

Two gaps make it impossible to tell from a memory row who wrote it and where its fact came from.

1. §10 routes a semantic write that names no namespace to the token namespace, which is `local` for every caller. The
   fields a memory row stores carry no writer other than its namespace, so a store shared by several actors
   accumulates semantic rows whose writer is lost for good.
2. `memory.remember` accepts `tags` but no structured metadata. A caller that needs to record provenance (who, when,
   where, what kind of fact, which records it came from, which row it replaces) writes it into the content. That text
   is embedded and indexed with the fact, so provenance competes with meaning in vector and lexical ranking, and none
   of it can be read back as a field.

### Decision

1. **Default write namespace.** With no `namespace=` argument, both memory types write to the caller's actor namespace
   (`token.actor().id`). The §10 table's semantic row becomes `token.actor().id`. An explicit `namespace=X` still wins
   for both types, `local` included: shared knowledge is written to the shared pool by choice, not by default.
2. **Anonymous callers.** An anonymous caller's actor id is `local` (`ActorRef::anonymous`), so its writes of either
   type land in `local` as before, and they are flagged: the runtime stores `properties.attribution = "unresolved"`
   on the row and returns `"attribution": "unresolved"` in the response. An attributed write carries no
   `attribution` key. Refusing anonymous writes was considered and rejected as the default, because a fresh
   single-user install with no actor configured makes every write anonymous.
3. **`properties` on `memory.remember`.** An optional JSON object, stored in `note.properties` beside `memory_type` and
   `tags`. Each rule below refuses with an invalid-input error that names the key and the rule; nothing is truncated.
   - At most 32 keys. A key is 1 to 64 characters: an ASCII lowercase letter, then lowercase letters, digits or `_`.
   - `memory_type` and `tags` have their own parameters, `attribution` is written by the runtime, and `supersedes` is written by `memory.apply_batch`; use `link(relation="supersedes")` to add a supersedes edge.
   - A value is a string, a finite number, a boolean, or an array of strings. Null, nested objects and arrays holding
     anything but strings are refused.
   - The serialized object is at most 8192 bytes; the refusal reports the measured size and the limit.

   Conventional keys for provenance, not enforced: `who`, `when`, `where`, `kind`, `source_ids`, `supersedes`.
   Changing a memory's properties later uses the generic `update` verb, whose `properties` argument already
   shallow-merges into the note.
4. **Recall hit shape.** Every returned hit adds `namespace`, `tags` (`[]` when none) and `properties` (the stored
   object, `{}` when none). Existing fields keep their names and meaning.

### Consequences

- **Read visibility.** A semantic memory written by actor A is in A's default recall view. Actor B reads it when B's
  `visible_namespaces` lists A ([ADR-007](ADR-007-namespace.md) Rule 3b), as episodic memories are read today. A
  deployment that relied on every actor seeing semantic rows through `local` either lists the writers in
  `visible_namespaces` or writes shared facts with `namespace="local"`.
- **Existing rows stay where they are.** Rows already in `local` keep their namespace; re-attributing them is a data
  migration outside this amendment.
- **Prune.** `memory.prune` keeps its default namespace filter, `local`; pruning an actor's semantic memories passes
  `namespace=<actor id>`.
- **Trust.** Properties are caller-asserted context, never verified attribution; the write namespace remains the
  runtime's attribution.
- **Keyed replay.** A replay under an idempotency key compares content only, as it does for tags: a replay with the
  same content and different properties returns the stored memory unchanged with `replayed=true`.
- **Recall budget.** `token_budget_chars` keeps counting memory content only. A hit can carry up to 8192 bytes of
  properties that the budget does not count; the limit is the bound.

### Alternatives considered

| Alternative                                                            | Disposition                                                                                                                          |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| Keep semantic writes in `local` and stamp a writer key into properties | Rejected: two attribution channels for one fact, and `local` stays the default home of rows nobody owns.                             |
| Refuse anonymous memory writes                                         | Rejected as the default: it breaks a single-user install with no actor configured.                                                   |
| Nested, free-form properties                                           | Rejected: flat scalar and string-list values are what filters and recall output need; nesting invites storing documents in metadata. |

### Validation

The implementing change carries, for each rule in Decision 3, a refusing test and a passing boundary test (32 keys
pass and 33 refuse; a 64-character key passes and 65 refuses; 8192 bytes pass and 8193 refuse); a remember with
properties read back through `get` and through a recall hit; an attributed semantic write with no namespace landing in
the actor namespace; an explicit `namespace="local"` semantic write landing in `local`; anonymous semantic and
episodic writes landing in `local` with `attribution: "unresolved"` in the row and the response; anonymous semantic and
episodic writes naming `namespace="local"` flagged the same way; and an attributed write with no `attribution` key.

## Amendment: batch apply for memory rewrites (2026-11-02)

**Status**: Accepted 2026-11-02 (Proposed 2026-11-02). Adds the `memory.apply_batch` verb. Builds on the amendment of
2026-11-01 (caller properties and the actor-namespace default).

### Problem

This amendment serves the project owner's instruction of 2026-11-01, rendered: "No, that won't do: every memory has to
be updated, without exception. This can go a little at a time; as far as possible, each agent reads its own rows
itself." A few minutes later the same instruction set out the program, rendered: "All memory entries must be updated
by the agents that wrote them ... as they go through, they keep records, a jsonl file for batch operations ... execute
all these updates ... then, at rest, reindex all memory." This verb keeps and executes those records; Decision 7 names
the reindex.

Rewriting a store's memories means writing one new row per fact, linking it to the row it replaces, and keeping the
old row. Done as separate `memory.remember` and `link` calls, a rewrite of thousands of rows has no dry run, no single
place that checks each row against the rules before anything is written, and no safe way to resume after a failure:
an unkeyed re-run writes duplicates. The kg `apply_changeset` verb is atomic but has no dry run, refuses a `tags` key in
properties, refuses a replayed create as an error, and does not go through the memory write path.

### Decision

1. **The verb.** `memory.apply_batch(batch, rows, dry_run = true)`. `batch` is a name of at most 128 characters; `rows`
   holds 1 to 200 rows; `dry_run` is true unless the caller passes `false`. An anonymous caller is refused before any
   row is read, since every write would land in the shared namespace with no writer.
2. **Row shape.** `{id?, part = 1, action, content?, memory_type?, salience?, properties?, tags?}`; unknown fields are
   refused. `action` is one of:
   - `supersede`: `id` names the old row. Writes a new memory and a `supersedes` edge from it to the old row. The old
     row is not changed.
   - `create`: no `id`. Writes a new memory with no edge, for a fact found in records that no memory holds.
   - `skip`: `id` names the old row; nothing is written.
   - `delete-candidate`: `id` names the old row; nothing is written, and the row is listed for a person to decide.
3. **Writes.** A `supersede` or `create` row is written through the `memory.remember` path (embedding, text index,
   decay) in the caller's own namespace; no row can name another namespace. The verb adds three properties, which a
   row may not supply itself: `batch` (the batch name), `supersedes` (the old row's full id, `supersede` only) and
   `batch_row` (the SHA-256 of the row's canonical JSON: action, id, part, content, memory_type, salience, properties,
   tags). The remember idempotency key is `<batch>:<old id>:<part>` for `supersede` and `<batch>:create:<part>` for
   `create`; two rows of one call with the same key are both refused.
4. **Re-runs.** When the key is already taken in the caller's namespace and the stored `batch_row` equals the row's,
   the row is `deduped` and its `supersedes` edge is asserted again (linking is idempotent), so a run that stopped
   between the write and the edge completes. When the stored `batch_row` differs, the row is refused as
   `dedupe-name-taken`; a corrected row needs a new `part` or a new batch name. Nothing is overwritten.
5. **Checks, per row, identical in a dry run and a real run.** A failing row is refused with a reason and never stops
   the others:
   - `id` missing for `supersede`, `skip` or `delete-candidate`, or present for `create`;
   - the old row does not resolve, is not a memory, or sits in a namespace that is neither the caller's nor the shared
     one (the reason names the namespace found);
   - `content` empty, or any check `memory.remember` applies to content, memory_type, tags or properties fails (the
     reason carries remember's message);
   - the salience the row would be stored with (the supplied `salience`, or the memory type's default when the row
     gives none: 0.5 for semantic, 0.3 for episodic) outside [0, 1], or above 0.4 while `properties.verified` is
     absent, `false` or empty; `verified` is `true` or a non-empty string naming what was checked;
   - `properties.who` absent or empty on a `supersede` or `create` row;
   - `properties.source_ids` absent or empty on a `supersede` row whose old row sits in the shared namespace;
   - a supplied `batch`, `supersedes` or `batch_row` property;
   - credential-shaped text in the idempotency key, the content, the properties or the tags, found by the same
     scans the write path runs (the reason names the detector and the field, never the matched text).
6. **Result.** One entry per row, in input order: `{id, part, action, outcome, old_namespace, new_id, reason}`, with
   `outcome` one of `planned`, `written`, `deduped`, `skipped`, `listed`, `refused`, `failed`. `old_namespace` is the
   old row's namespace as read; `new_id` comes from the write itself. `failed` means the store refused a write that
   passed every check; the row is re-runnable. Counts per outcome follow the entries. A dry run returns `planned` or
   `deduped` where a real run would write or dedupe.
7. **Reindex.** Rows written by the verb are embedded and indexed at write and are recallable at once. Superseded rows
   stay stored and recall's supersedes suppression (on by default) drops any candidate with an incoming `supersedes`
   edge the reader can see. A full re-embed after a store-wide rewrite is `kkernel reindex --namespace <ns>`, which
   re-embeds entities, notes (memories included) and knowledge with each kind's embedding policy and fills missing
   vectors; `--id <uuid>` repairs one record.

### Consequences

- A rewrite is resumable chunk by chunk: re-running a chunk writes only what is missing and refuses a changed row
  instead of silently keeping the first version.
- The old row is never modified or deleted by this verb; deletion stays a separate, human decision.
- A chunk is not atomic: a storage failure leaves the rows before it written. That is the price of resumability, and the
  result names every row's outcome.

### Alternatives considered

| Alternative                               | Disposition                                                                                                 |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| kg `apply_changeset`                      | Rejected: no dry run, `tags` refused in properties, replayed create is an error, not the memory write path. |
| Caller loops `memory.remember` and `link` | Rejected: no dry run, no per-row check before writing, and an unkeyed re-run writes duplicates.             |
| Atomic chunk                              | Rejected: a single bad row would void 199 good ones, and a re-run would still need the dedupe rule above.   |
| Overwrite on a changed re-run             | Rejected: a corrected row would silently replace a row other readers may already have recalled and judged.  |

### Validation

Tests for: dry run writing nothing and reporting the same refusals as a real run; each refusal reason above, including
a semantic row with no salience and no `verified`; a `supersede` writing one row in the caller's namespace with the
three added properties and one `supersedes` edge, the old row unchanged; a `create` writing no edge; a re-run of the
same chunk returning `deduped` for every written row and writing nothing; a re-run with one row's properties changed
refusing that row as `dedupe-name-taken`; a deduped row whose edge was missing getting its edge; an anonymous caller
refused; 201 rows refused; the new row recalled at once and the old row suppressed by the edge.

### Follow-ups

- The dedupe name is held only by a live row: the unique index on `(namespace, kind, key)` excludes deleted rows
  (migration 028). After the holder is pruned, a re-run of its chunk writes one new live row with the same `batch_row`.
  That is the intended direction: the fact is written again, never lost and never duplicated among live rows.
- #6361: compare `batch_row` when the remember write replays an existing row.
- #6359: version the `batch_row` digest before the row shape changes.

### Delta 1: old rows in namespaces the caller owns (2026-11-02)

**Status**: Accepted 2026-11-02 (Proposed 2026-11-02). Changes Decision 5's namespace check only.

**Problem.** An actor that runs helper agents often gives them their own namespace, so their memories stay apart
from the actor's own until they are checked. Under a store-wide rewrite those rows belong to the actor that ran the
helpers, yet Decision 5 refuses them as `old-namespace`: their namespace is neither the caller's nor the shared one.
Measured on one deployment's export: 369 rows sit in four such namespaces, one per actor.

**Decision.**

1. **Declaration.** The `[actor]` table takes `owned_namespaces`, a list of namespaces, empty by default. Loading the
   configuration refuses an empty entry, an entry that does not parse as a namespace, the shared namespace `local`,
   the actor's own id, a duplicate, and a non-empty list with no `[actor] id`. Ownership is declared by name and never inferred:
   `visible_namespaces` grants read access, and configurations list other actors there for reading, so it says
   nothing about ownership; a namespace that shares the actor's id as a prefix is not owned unless it is listed.
2. **Carriage.** The list travels with the caller's identity, the same way as `actor_id` and `visible_namespaces`:
   from the configuration in process, and on the daemon request frame as `owned_namespaces`, omitted when empty. A
   daemon that predates the field ignores it, so an owned row is refused there as `old-namespace`: a missing
   declaration produces a refusal, never an admission. When the caller pins an actor other than the configured
   one, the client's configuration resolver drops the list, because it was declared for the configured actor. The
   daemon takes the list on the frame as sent and does not check it against the frame's `actor_id`.
3. **The check.** Decision 5's namespace rule becomes: the old row's namespace is the caller's own, the shared one,
   or one listed in the caller's `owned_namespaces`; anything else is refused as `old-namespace`, and the reason
   names the namespace found. An owned row is checked like a row in the caller's own namespace, so the shared-row
   `source_ids` rule does not apply to it. The new row is written in the caller's namespace as before, and the old
   row is not changed.
4. **Scope.** The declaration is read by this check alone. It grants no write into the owned namespace, widens no
   read, and no other verb consults it.

**Consequences.** A caller's declaration is its own claim, carried at the same trust as its `actor_id`. On one
machine, any client running as the same user can therefore declare another actor's namespace as owned, and a
supersede there hides that actor's old rows from recall while keeping them in the store. The hiding reaches only a
reader whose recall covers both that namespace and the caller's: the new row and its `supersedes` edge are written in
the caller's namespace, and recall reads `supersedes` edges in the namespaces the reader can see
([Delta 2](#delta-2-supersedes-reads-cover-the-namespaces-the-reader-can-see-2026-11-03)), so a reader scoped to the
owning actor's namespace alone still recalls the old rows. The check is a rule of this
verb, not an access control: by-ID operations stay namespace-agnostic ([ADR-007](ADR-007-namespace.md) Rule 2) and
isolation is a storage-layer property (Rule 9), so the declaration only bounds what `memory.apply_batch` agrees to
supersede: more than with no declaration, which refuses every row outside the caller's namespace and the shared `local` one, and less than a
by-ID operation reaches. It protects no row from a caller that holds its id. An owned namespace has to be listed by every configuration that rewrites its rows. A row in an undeclared namespace keeps the
refusal it has today.

**Alternatives considered.** Reading ownership from `visible_namespaces` (rejected: read access, and it lists other
actors). An `<actor id>:` prefix rule (rejected: an actor's sub-actors under its prefix can be separate writers that
rewrite their own rows, and some helper namespaces do not share the prefix). A per-call argument naming the owned
namespaces (rejected: a choice made per call rather than once in configuration, and invisible in the configuration a
reviewer reads).

**Validation.** Tests for: each load-time refusal above and an accepted list; a dry run admitting an old row in an
owned namespace with `old_namespace` naming it; the same row refused as `old-namespace` when the list is empty; a
namespace that shares the actor's prefix but is not listed refused; a daemon request frame without the field
decoding to an empty list and a frame with an empty list omitting it; a pinned actor other than the configured one
carrying no owned namespaces.

### Delta 2: supersedes reads cover the namespaces the reader can see (2026-11-03)

**Status**: Proposed 2026-11-03. Changes how recall's supersedes suppression (Decision 7) reads, and the reserved
`properties` keys of the 2026-11-01 amendment.

**Problem.** Decision 7 says recall drops any candidate with an incoming `supersedes` edge the reader can see, but the
edge lookup read only the request's primary namespace. A request with no `namespace` argument has `local` as its
primary namespace and sees more than `local`, so an edge that `memory.apply_batch` wrote in the caller's namespace was
not read, and the old row was returned unless the new row was also among the candidates (#6386). Recall also drops a
candidate named by another candidate's `properties.supersedes`. `memory.remember` accepted that key from any caller,
while `memory.apply_batch` writes it itself after its namespace check and refuses it from a caller; and a value that
did not parse as a UUID was used as an id prefix, so an empty or one-character value hid a candidate whose id the
writer never named.

**Decision.**

1. **Edge reads.** Recall reads incoming `supersedes` edges in every namespace of the request's visible set and drops a
   candidate that has one in any of them. A request with an explicit `namespace` sees that namespace, so it reads there
   as before.
2. **Reserved key.** `supersedes` joins `memory_type`, `tags` and `attribution` as a key `memory.remember` refuses in
   `properties`; the refusal names `memory.apply_batch` and `link(relation="supersedes")`. Rows already stored keep
   the property.
3. **Property values.** The property path honors a full UUID, or an id prefix of at least 8 characters, each a
   lowercase hex digit or a hyphen. Any other value hides nothing.

**Consequences.** A supersede written by `memory.apply_batch` hides the old row from every recall that sees the
caller's namespace, the default request included, which is what Decision 7 already said. The Delta 1 bound holds: a
reader scoped to the owning actor's namespace alone does not see the caller's namespace and still recalls the old rows.
A recall reads one batched edge query per visible namespace instead of one. A row that relied on a short or upper-case
id prefix in `properties.supersedes` no longer hides its target; a `supersedes` edge does.

**Validation.** Tests for: a default-path recall dropping a `local` memory whose `supersedes` edge sits in another
visible namespace, with the new row outside the candidates; an explicit-namespace recall returning what it returned
before; `memory.remember` refusing `properties.supersedes` with the refusal above; stored rows whose
`properties.supersedes` is empty or one hex character hiding nothing, and a full UUID and an 8-character prefix still
hiding their targets.

### Delta 3: an edge between memory rows lives in its source row's namespace (2026-11-04)

**Status**: Accepted 2026-11-05 (Proposed 2026-11-04). Changes where Decision 7's `supersedes` edges, and every other edge between two
memory rows, are written, and what an edge read reports when it leaves edges out. Rule 1's refusal text amended
2026-11-05 (proposed): a refusal names the source row's namespace only when the caller can read it.

**Problem.** Two writers put the same kind of edge in different namespaces (#6380). `memory.apply_batch` writes the
`supersedes` edge from a new row to the row it replaces in the caller's namespace. `link` writes in `local`, because the
kg pack dispatches it with the registry write identity, which ADR-007 Rule 0 pins to `local`. ADR-007 Rev 6 Rule 0's scope
note says every non-memory write, edges included, keeps the `local` default, and neither it nor the 2026-11-01
amendment for semantic writes treats an edge between two memory rows differently. This delta narrows that sentence for
edges whose source and target are both memory rows, and ADR-007 Rule 9's qualifications and Rule 8 constraints 1
and 4 now name the placement rule below beside `restore`'s ownership comparison. A second reader of one measured row sees five of its six outgoing `supersedes` edges, and nothing in the
`neighbors` response says that one is missing.

**Decision.**

1. **Namespace rule.** An edge whose source and target are both memory rows is written in the source row's namespace,
   whichever verb writes it. Three verbs write such an edge: `memory.apply_batch`, `link`, and a link op in
   `apply_changeset`, and `restore` revives one. Each of the three writers reads the source row and compares its stored namespace with the request's write namespace,
   the one the gate checked: the caller's own, or the explicit `namespace` argument, or for `apply_changeset` the op's
   own `namespace` field. The two must be equal. A mismatch is refused before anything is written. When the source
   row's namespace is one of the namespaces the request can read, the refusal names it as the namespace to retry with.
   When it is not, the refusal says that the source row is in a namespace the caller cannot read and that the edge
   cannot be written under the request's namespace, and it names no namespace. A refused `restore` and a refused
   `apply_changeset` link op follow the same split. The edge is never redirected and an op's namespace is never
   overridden.
   `memory.apply_batch` writes the edge from the row it has just written in the caller's namespace, so its two
   namespaces always agree. `restore` of a soft-deleted edge between memory rows compares the edge's stored namespace
   with its source row's and refuses a mismatch, so a tombstone the move leaves in place cannot come back live in the
   wrong namespace. This is a placement rule and grants nothing: who may write a namespace stays the gate's decision on
   the request (ADR-007 Rev 8 Rule 9). It refuses no read; the only by-id effect it changes is where an edge between
   memory rows may be written or revived, recorded as an ADR-007 Rule 9 qualification.
2. **Reads report filtering.** `neighbors` and recall's supersedes read return every edge in the namespaces the reader
   can see. For the row whose edges the read lists (recall: each returned row; `neighbors`: the anchor row), and for each
   relation, the response also carries the number of edges between memory rows that arrive at that row and that the
   read leaves out because they are stored in a namespace outside that set. Each such edge is one the reader would
   have been given if it could see that namespace, so an edge with a soft-deleted endpoint, which no read returns,
   is not counted. The field is present on every filtered read, with `0` when nothing was left out, so a missing field
   means the producer is broken, never that nothing was hidden. Under rule 1 an edge is stored in its source row's
   namespace, so after the move in rule 3 the counted edges are exactly those from rows outside the reader's
   namespaces, and a reader already sees every outgoing edge of a row it can see; the edges this count covers are
   the incoming ones. The count discloses three things: that such edges exist, how many there are, and their relation. It
   withholds three: which rows they come from, which namespace those rows are in, and what the rows contain.
   The field is `hidden_incoming_edges`, shaped `{"total": n, "by_relation": {"<relation>": n}}`; `total` is always
   present. Recall carries it on every returned row, for `supersedes`. A `neighbors` call anchored on a memory row always
   returns the object-shaped response and carries the field once, at the top; `by_relation` lists each relation of the
   request's filter, or with no filter each relation whose count is above zero. This is a breaking change for that
   anchor kind only: a call anchored on a memory row with no `limit` used to return a bare array. A `neighbors` call
   anchored on any other record keeps its current shape. The count comes from one storage query that is given the
   reader's visible namespaces and returns numbers only; no read is opened in a namespace the reader cannot see.
3. **Existing edges.** A one-time move rewrites each live edge between memory rows whose namespace differs from its
   source row's namespace, keeping the edge's id. Soft-deleted edges stay where they are: no read returns them, and
   edge listing does not enumerate them. Before its first write the move computes the destination of every edge it
   would move, as (source row's namespace, source, target, relation), over the whole set. If an edge already holds one
   of those destinations, live or soft-deleted, or two moving edges share one, the whole run is refused and nothing is
   written. The applying run is not one transaction: it replaces each edge only if the edge's stored `updated_at` and
   `deleted_at` still equal what the preflight read, so an edge changed by another writer stops the run instead of
   being overwritten. If another write takes an edge's destination after the preflight, that edge's update fails on the
   unique (namespace, source, target, relation) index and the run stops there in the same way. After it stops or
   finishes, it counts the misplaced edges again from the store rather than subtracting what it moved. A run that
   stops part way reports how many edges it moved and how many remain; every run, a re-run included, repeats the whole
   preflight before its first write and moves only the edges still misplaced. Other writers are stopped while it runs. The move has a dry run
   that prints per-namespace edge counts before and after and writes nothing; the applying run prints the same counts.

**Consequences.** A reader who can see a row sees all of that row's outgoing edges between memory rows, so a supersede
chain is either whole or reported as partial with a count. `link` gains one read of its source record when both
endpoints are memory rows. Edges from a memory row to a non-memory record, and edges between non-memory records, keep
their current rule.

**Known limit.** A reader who can see the target row but not the source row learns how many edges of each relation
arrive from rows it cannot see, never which rows, which namespace, or their content. Learning the identity of the
superseding row remains a follow-up, not solved here. A refused write names the source row's namespace only when the
caller can already read that namespace, so rule 1 discloses no namespace that rule 2 withholds; a caller holding the
id of a row in a namespace it cannot read learns only that the row is in such a namespace. One further disclosure
follows from the design: the count covers only edges whose endpoints are live, so it falls when a hidden source row is
soft-deleted, which shows a reader that a row it cannot see was deleted.

**Validation.** Tests for: `memory.apply_batch` and `link` writing a `supersedes` edge between the same two rows into
the same namespace (the source row's); a `link` and an `apply_changeset` link op whose write namespace differs from the source row's refused with no edge
written; the same refusal, for `link`, an `apply_changeset` link op and `restore`, naming the source row's namespace
when the caller can read it and carrying no namespace other than the request's when it cannot; `restore` of a soft-deleted edge between memory rows whose namespace differs from its source row's refused
with the edge left soft-deleted; a move whose set holds two edges with one destination refused whole before any write; `neighbors` and recall from a second reader returning the visible edges plus, per row and relation, a count equal to the incoming
edges it cannot see, with no hidden row's id or namespace in the response; the same reads with nothing hidden carrying the count as `0`, not omitting it; the bare-array `neighbors` response unchanged for a non-memory anchor, and a memory-row anchor with no `limit` carrying the count; the move's dry run on a fixture holding the issue's shape (one row with six outgoing `supersedes` edges,
one in the caller's namespace and five in `local`), printing counts and leaving every edge in place, then the applying
run leaving all six in the source row's namespace.
