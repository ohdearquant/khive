# ADR-021: Memory Pack

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Amended**: 2026-06-19, added §10 (`memory_type` to namespace routing) recording the
[ADR-007](ADR-007-namespace.md) Rev 6 carve-out: episodic memory writes stamp the caller's actor
id by default, semantic memory writes stamp the shared pool, and an explicit `namespace=`
overrides both.

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

Agents that prefer explicit CRUD are not blocked:
`create(kind="memory", salience=0.7, decay_factor=0.01, properties={"memory_type":"semantic"}, ...)`
followed by an optional `link(annotates)` produces an equivalent result.

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
   is considered. If the underlying `search_notes` operation applies kind only as a
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
  5. emit RecallExecuted event with payload.served_by_profile_id = Some(P.id)
```

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
- The pack composes with [ADR-020](ADR-020-git-native-kg-implementation.md) git-native
  KG: memory notes are part of the notes substrate, which is excluded from v1 KG
  snapshots ([ADR-010](ADR-010-kg-versioning.md) §SnapshotCoverage). Memory persists in
  `working.db` and the main database; cross-instance memory portability is v2 work.

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
