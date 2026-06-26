# ADR-007 Rev 6: Namespace as Attribution-Only Open String — Dumb Storage, Single Gate, Operator-Configured Read Visibility, Per-Actor Episodic Memory

**Status**: Accepted/Ratified (2026-06-19)
**Date**: 2026-06-19
**Authors**: lambda:khive, alpha:architect
**Amends**: ADR-007-namespace.md (replaces v1 base text, both 2026-05 amendments, Rev 2, Rev 3 read-scope clause; Rev 6 amends the Rev 4 Rule 0 write-default for episodic memory)
**Supersedes (partial)**: ADR-050 §"Decision"; ADR-059 §"Decision" and §"Visibility tiers"
**Superseded-by-none**: ADR-053 (ActorStore, SessionStore, actor threading) survives in full
**ADR chain**: ADR-018 (Gate trait, single dispatch site) | ADR-014 (curation, merge semantics)
| ADR-002 (edge cascade, no dangling refs) | ADR-021 (memory pack, episodic vs semantic routing)

**Rev 6 summary (2026-06-19)**: Carves one exception out of the Rule 0 write default. Through
Rev 4, every write pinned `namespace = 'local'` by default (the actor identity contributed only
to the read visible-set, never to the write namespace). Rev 6 amends that default for a single
write path: an episodic memory write (`memory.remember` with `memory_type = episodic`) stamps
`namespace = the caller's actor id` (`token.actor().id`) by default, rather than the shared
`'local'` pool. Semantic memory writes are unchanged: they continue to stamp `token.namespace()`
(the shared pool, `'local'` by default). An explicit `namespace=` request parameter continues to
override the write namespace for both memory types, exactly as in Rule 3. This carve-out is
attribution plus view-layer visibility, not a storage boundary: the store performs no
`record.namespace == caller_namespace` check (the prior-v1 pattern PR-A1 removed, which must not
return), by-ID ops stay namespace-agnostic (Rule 2), and the actor-stamped episodic memory
becomes visible purely through the existing Rev-4 read visible-set fanout
(`{local} ∪ {actor.id} ∪ {actor.visible_namespaces}`). It is backward-compatible: an anonymous
actor has id `'local'` (`ActorRef::anonymous`), so with no `[actor]` configured the episodic
write namespace resolves to `'local'`, byte-identical to pre-Rev-6 behavior. The carve-out takes
effect only once a real actor is configured and threaded into `token.actor()`. Recall needs no
change: the Rev-4 fanout already returns actor.id-stamped memories on both the FTS path and the
ANN post-filter path. The Gate (ADR-018) remains the single trust seam. All other Rev 4 / Rev 3
rules are retained verbatim. See the Rev 6 amendment to Rule 0 below and Rule 3 (memory routing).

**Rev 4 summary (2026-06-17)**: Generalizes the Rev 3 default read scope. Rev 3 fixed the
default multi-record read scope at exactly `['local']`, with an explicit `namespace=` request
parameter as the only escape. Rev 4 widens the default read scope to `['local'] ∪
visible_namespaces`, where `visible_namespaces` is assembled at config load from two sources:
the operator-configured `[actor] visible_namespaces` list in `khive.toml`, and the configured
`[actor] id` when it is non-`'local'` (folded in, deduplicated). `'local'` is always included.
With neither configured (the default), behavior is identical to Rev 3 — fully
backward-compatible. Writes are unchanged (still pin `'local'` by default, explicit
`namespace=` escape unchanged), by-ID ops are unchanged (namespace-agnostic, Rule 2), and the
Gate remains the single enforcement seam. Rev 4 deliberately AMENDS the Rev 3
"attribution-only" reading of `[actor] id` (Rule 0): a non-`'local'` actor identity now
contributes to the DEFAULT READ visible-set. It still never routes writes and never sets
`default_namespace`. This is read-augmentation, not the actor-as-namespace isolation Rev 3
rejected: it only ever ADDS namespaces to what a default read sees — it never hides records,
never silos, and never breaks by-ID resolution (the failure modes of the rejected model, which
hid data behind per-actor partitions for both reads and writes). Rev 4 obviates the
Rev-3-pending PR-A2/PR-F relabel backfill: legacy non-`'local'` rows become visible by
configuring `[actor] id` or `visible_namespaces`, without mutating stored attribution. See
Rule 3b.

**Rev 3 summary (2026-06-17)**: Promoted from Proposed to Accepted/Ratified. Closes the two
Rev-2-deferred per-pack carry questions as NO-CARRY: comm = no-carry (reverses the Rev-2 "leans
yes"); memory = no-carry (resolves the Rev-2 "undecided"). All seven production packs store in
the single shared "local" namespace by default. Per-actor distinction is a view-layer tag, never
a namespace partition. Edge namespace is attribution-only, stamped "local" by default, never an
isolation mechanism; the ADR-059 three-column filter is rejected. ADR-059 remains withdrawn.
(Rev 4 retains all Rev 3 rules; it generalizes only the Rule 3 default read scope.)

---

## Context

khive accumulated four namespace documents in the v1 series that disagree on what namespace is
for. ADR-007 v1 base text treated namespace as a type-level authorization proof
(NamespaceToken, NamespaceView, by-ID post-fetch checks). The 2026-05-25 amendment introduced
AllowAllGate as the default gate. The 2026-05-27 Namespace-by-Layer amendment split packs into
two routing groups. ADR-050 proposed removing the KG-pack token rebinding introduced by that
split. ADR-059 drafted a three-tier visibility model.

Ocean's ruling (CLAUDE.md "Authorization — the gate is a seam, by design" and "Namespace and
authorization") resolves the divergence: namespace is attribution, not isolation. Storage is
dumb. The Gate is the one enforcement seam.

PR-A1 (commit 2607e263, merged 2026-06-16) shipped the by-ID half: all ensure_namespace /
ensure_namespace_visible post-fetch checks removed from get_entity, get_note, delete_entity,
delete_note, update_entity, update_note, update_edge, delete_edge. The multi-record half
(list, search, recall, neighbors, traverse, query) is also collapsed to the single shared
"local" set at dispatch: VerbRegistry::dispatch mints the storage token with an empty
extra-visible set and primary = "local" (pack.rs, verified). What remains is a one-time data
backfill (PR-A2/PR-F): relabeling the stranded non-"local" base rows to "local" and reindexing
FTS5/ANN, so the already-collapsed query scope actually returns them.

This ADR does not specify multi-actor isolation topology. That is behind the Gate trait and
addressed in ADR-068. This ADR specifies the namespace contract and the seam that operator
Gate implementations plug into.

---

## Decision

### Rule 0 — One shared brain, one namespace

khive's default deployment is a single shared brain: one SQLite file, one namespace
("local"), all lambdas and agents reading and writing together.

Actor identity (lambda:khive, lambda:leo, agent:*, user:ocean) is attribution only: stamped on
write records and gate-request context, available for logging, filtering, and operator policy
input. It never silently becomes the storage namespace and never gates by-ID access.

Config-layer realization: the `[actor] id` config key is attribution only and does not set
`default_namespace`. `runtime_config_from_khive_config` preserves whatever the caller resolved
into the base config (an explicit `--namespace` / `KHIVE_NAMESPACE`, else `local`), regardless
of which actor is configured. Threading actor identity onto write records is deferred to
ADR-053; until that lands, `[actor] id` is inert at the storage layer. This is the
distinction Rule 0 turns on: a caller may target a named namespace per request, but the actor a
deployment is configured as must not route storage on its own.

Source: CLAUDE.md "The local system is a single shared brain: one namespace (`local`), and
every lambda / agent / subagent reads and writes it."

**Rev 6 amendment to Rule 0 (episodic memory writes stamp the actor id by default).** Rule 0
continues to hold for every write path EXCEPT one: an episodic memory write
(`memory.remember` with `memory_type = episodic`, or the equivalent
`create(kind="memory", properties={"memory_type": "episodic"})`) stamps
`namespace = token.actor().id` by default, rather than the shared `'local'` pool. This is the
single carve-out from the prior "writes always pin `'local'`" default. It is bounded as follows:

- Scope is episodic memory only. Semantic memory writes (`memory_type = semantic`) are
  unchanged: they stamp `token.namespace()` (the shared pool, `'local'` by default). All
  non-memory writes (KG entities, edges, notes, tasks, comm, schedule, brain) are unchanged and
  continue to pin `'local'` by default per the unamended Rule 0.
- An explicit `namespace=` request parameter overrides the write namespace for both memory
  types, identical to the Rule 3 escape. `memory.remember(..., namespace="ns-x")` writes to
  exactly `ns-x` regardless of `memory_type`. The actor-id default applies only when no explicit
  `namespace=` is supplied.
- This is attribution plus view-layer visibility, not a storage boundary. The store performs no
  `record.namespace == caller_namespace` check at any layer (the prior-v1 pattern PR-A1 removed
  must not return). By-ID ops remain namespace-agnostic (Rule 2): an episodic memory written
  under `lambda:leo` is fetchable, updatable, and deletable by UUID with no namespace check. The
  actor-stamped episodic memory becomes _visible_ to a default recall purely through the existing
  Rev-4 read visible-set fanout (`{local} ∪ {actor.id} ∪ {actor.visible_namespaces}`, Rule 3b),
  not through any storage partition.
- Backward-compatible. An anonymous actor has id `'local'` (`ActorRef::anonymous` returns
  `kind = "anonymous"`, `id = "local"`). With no `[actor]` configured, the caller is the
  anonymous actor, so the episodic write namespace resolves to `'local'`, byte-identical to
  pre-Rev-6 behavior. The carve-out takes effect only once a real actor id is configured and
  threaded onto the request token via `token.actor()` (the actor-identity threading from
  ADR-053). Until a non-`'local'` actor is configured, this amendment is inert.

Rationale: an episodic memory is a specific actor's lived experience. Attributing it to that
actor, and keeping it private to that actor's default recall view, matches what episodic memory
is. A semantic memory is distilled, shareable knowledge and belongs in the common pool. Per-actor
episodic plus shared semantic is the intended model. The prior Rule 0 text ("writes always pin
`'local'`") predates this episodic/semantic distinction; it stamped both memory types into the
shared pool, which conflated one actor's experience with the common knowledge base. Rev 6 closes
that gap for the episodic path while leaving the semantic path, and every other write path,
unchanged.

This is not the actor-as-namespace isolation Rev 3 rejected. The rejected model derived the
storage namespace from actor identity for ALL writes and confined ALL reads to a per-actor
partition, coupling identity to storage, hiding records, and breaking the shared brain and by-ID
resolution. Rev 6 changes only the episodic-memory write default, leaves every other write path
on `'local'`, never confines a read to a partition, never hides records behind a namespace check,
and keeps by-ID ops global. Visibility is restored additively by the Rule 3b read fanout, which
only ever shows MORE, never less.

### Rule 1 — Storage is dumb

Stores (khive-db, khive-storage traits) are unscoped database connections. Namespace is a
column on every record, written as-is from the record struct. Stores execute what they are
told.

- Multi-record methods (list, search, FTS, vector search, neighbors, traverse, query) accept
  namespace as a caller-supplied parameter used in the SQL WHERE clause.
- By-ID methods (get, update, delete, upsert) use WHERE id = ? only. No namespace equality
  check in the store or in the runtime above it.

Source: v0 archive ADR-007-namespace-as-open-string.md (2026-05-15) rules 1-4 ("stores are
unscoped database connections"), re-affirmed for v1.

No inline namespace == checks in handlers or stores are permitted. No per-namespace storage
partitioning. These make the Gate redundant — the exact regression this seam exists to prevent.

### Rule 2 — By-ID ops are namespace-agnostic (SHIPPED, PR-A1)

get, update, delete by UUID resolve a globally-unique UUID with no namespace check at any
layer: not in the store SQL, not in the runtime post-fetch check, not in the pack handler.

Status: SHIPPED in commit 2607e263. Covered by regression tests added in that PR.

Affected functions confirmed clean post-PR-A1 (operations.rs): get_entity,
get_entity_including_deleted, get_note_including_deleted, delete_entity, delete_note,
update_entity, update_note, update_edge, delete_edge.

### Rule 3 — Multi-record ops scope to the single shared "local" set (routing SHIPPED; data backfill pending)

CHANGE FROM ADR-007 v1: The 2026-05-27 Namespace-by-Layer amendment routed memory, gtd, comm,
brain, and schedule multi-record ops by actor namespace ("WHERE namespace = <actor_namespace>"),
while routing KG and knowledge to "local". Gemini REFUTE Finding 2 correctly identified this
as a contradiction of Rule 0: framing per-pack actor routing as "explicit pack policy"
re-introduces the exact actor-as-namespace isolation coupling Ocean ordered removed. Finding 1
added that memory is live-audited as bulk "local" and that cross-lambda learning via
memory.recall over one pool depends on the shared store.

Ocean ruling (2026-06-17, Rev 3): ALL packs are NO-CARRY. There is no per-pack namespace
carry, now or planned. This closes the two questions that Rev 2 left deferred to Rev 3:

- Comm is NO-CARRY (reverses the Rev-2 "leans yes"). Messages are inherently actor-addressed,
  but actor addressing is a view-layer tag filter on `from_actor`/`to_actor` in message
  properties (ADR-057), not a namespace partition. Both copies of a comm.send stay in the
  caller's namespace (`"local"` by default); the actor label is attribution only.
- Memory is NO-CARRY (resolves the Rev-2 "undecided"). The memory pool is shared, and
  cross-lambda recall via memory.recall over one pool is the intended behavior. Per-actor
  distinction on memory reads is a view-layer filter on `actor_id` attribution (deferred to
  ADR-053); it does not become a namespace partition.

Under this ADR: list, search, recall, neighbors, traverse, and query for ALL packs pass
WHERE namespace = 'local' by default. The single exception is an explicit `namespace=` request
parameter (a caller may deliberately read a named set, e.g. `list(namespace="lambda:khive")`
or `create(namespace="ns-beta")`), which routes that one operation to the named set. There is
no per-pack actor routing, and `default_namespace` (the actor/config identity) does NOT route
storage: it reaches the gate as policy context, but the storage token stays "local" unless the
caller named a namespace explicitly.

The dispatch boundary (VerbRegistry::dispatch, pack.rs) mints the storage token with primary =
the explicit `namespace=` parameter when present, else `Namespace::local()`. The "local" pin is
the default; the explicit parameter is the only escape.

Per-actor distinctions for operational packs are view-layer tag filters, not namespace
partitions:

- GTD: filter by the `assignee` column (tag or field), not by namespace.
- Comm: actor addressing uses `from_actor`/`to_actor` in message `properties` (ADR-057,
  implemented). Both copies of a comm.send stay in the caller's namespace ("local"). Actor
  label is attribution only; namespace carry is NO.
- Memory: recall reads operate over the caller's read visible-set
  (`{local} ∪ {actor.id} ∪ {actor.visible_namespaces}`, Rule 3b); cross-lambda recall over the
  shared pool is the intended behavior for the namespaces in that set. Per-actor read distinction
  is also available as a view-layer filter on the `actor_id` attribution column when an
  owner-scoped view is needed. The `actor_id` attribution column is deferred to ADR-053; interim
  attribution is via `properties`. Write routing is per-`memory_type` (Rev 6): a semantic write
  stamps `'local'` (namespace carry is NO, unchanged), while an episodic write stamps the
  caller's actor id by default (the single Rev 6 carve-out from the Rule 0 write default). An
  explicit `namespace=` overrides both. See the Rev 6 amendment to Rule 0 and ADR-021 §10.
- Brain: profile resolution is its own scoping mechanism (brain.resolve, profile bindings),
  independent of namespace. Namespace carry is NO.
- Schedule: attribution columns carry the scheduling actor; namespace is "local". Namespace
  carry is NO.
- KG / knowledge: shared "local" store. Namespace carry was NO (confirmed in Rev 2; unchanged).

This dissolves the Rule 0 vs Rule 3 contradiction present in the prior amendment (gemini
Finding 2) and resolves the memory-pack scoping incoherence (gemini Finding 1). The Rev-2
deferred carry questions for comm and memory are now closed: both are NO-CARRY.

Status: implemented. VerbRegistry::dispatch mints the storage token with primary = the explicit
`namespace=` parameter when present, else `Namespace::local()`; `default_namespace` feeds only
the gate request, never the storage token. `runtime_config_from_khive_config` treats `[actor] id`
as attribution only. The earlier pin of `Namespace::local()` at the dispatch mint site (PR #159)
applied unconditionally — collapsing even an explicit parameter and so breaking namespace
isolation between caller-named sets — and is superseded by this explicit-parameter escape.

### Rule 3b — Default read scope MAY be widened by an operator-configured visible set (Rev 4)

CHANGE FROM Rev 3: Rev 3 fixed the default multi-record read scope at exactly `['local']`. Rev 4
generalizes it. The default read scope (the scope applied when no explicit `namespace=` request
parameter is supplied) is:

```
['local'] ∪ visible_namespaces
```

where `visible_namespaces` is assembled at config load from two sources: the
operator-configured `[actor] visible_namespaces` list in `khive.toml`, and the configured
`[actor] id` when it is non-`'local'` (folded in, deduplicated; an actor.id of `'local'` adds
nothing since `'local'` is always present). `'local'` is always a member, whether or not it
appears in either source. When neither source contributes a non-`'local'` namespace, the
default read scope is exactly `['local']`, identical to Rev 3. The widening is therefore
opt-in: a deployment that configures neither `[actor] id` nor `[actor] visible_namespaces`
keeps Rev 3 behavior verbatim.

The scope applies to all multi-record reads for all packs: list, search, recall, neighbors,
traverse, query. The runtime supplies the set to the store as a `WHERE namespace IN (...)`
filter. Storage stays dumb (Rule 1): the set is caller/config-supplied, not actor-derived
routing, and the store executes the filter it is told.

What Rev 4 does NOT change:

- Writes. The write namespace is `'local'` by default, or the explicit `namespace=` parameter
  when present. `visible_namespaces` does NOT widen the write namespace. Rule 0 holds: actor
  identity never becomes the storage namespace, and a non-`'local'` `[actor] id` does not route
  writes. (A non-`'local'` `[actor] id` does, however, contribute to the default READ
  visible-set — see the Rev 4 amendment to Rule 0 above and Rule 3b.)
- The explicit `namespace=X` escape (Rule 3). An explicit request parameter scopes that one
  operation to exactly `[X]` for both read and write. It is the precise-targeting escape and is
  NOT widened by `visible_namespaces`: `list(namespace="ns-beta")` returns only `ns-beta`,
  never `ns-beta ∪ local ∪ visible_namespaces`. This preserves the ability to read a single
  named set precisely.
- By-ID ops (Rule 2). get, update, delete, merge by UUID remain namespace-agnostic.
- The Gate (Rule 4). Authorization remains a single seam. `visible_namespaces` is a
  configuration convenience, not an authorization mechanism. A TenantGate continues to
  mint the read-visibility set from the authenticated identity, independent of this config
  field.

Why this is not the rejected actor-routing (Rev 3 Rule 0, ADR-059): the rejected model derived
the storage namespace from actor IDENTITY for BOTH writes and reads — writes followed `[actor]
id` into a per-actor partition and reads were confined to it, coupling identity to storage,
hiding records behind partitions, and breaking by-ID resolution and the shared brain. Rev 4
lets `[actor] id` contribute ONLY to the default READ visible-set, and only ever additively:
writes still pin `'local'` (never an actor partition), `'local'` is always included, by-ID ops
stay namespace-agnostic, and records keep their stored namespace (nothing is relabeled).
Read-augmentation cannot reproduce the rejected model's failures because it only ever shows
MORE, never hides, silos, or reroutes. The visible-set is a caller/config-supplied
generalization of the Rule 1 multi-record namespace parameter from a single value to a set,
applied to the default read path.

Relation to the Rev 3 data backfill: Rev 3 §Consequences flagged a pending PR-A2/PR-F relabel
of stranded non-`'local'` base rows to `'local'` (a live data mutation with no automatic
rollback) so the local-only scope would return them. Rev 4 makes that backfill unnecessary for
visibility: configuring `visible_namespaces` to include those namespaces makes the legacy rows
visible without rewriting their stored attribution. Per the "data vs view" principle, the
invisibility of legacy rows is a read-scope (view) problem; the correct fix is to widen the read
scope, not to mutate stored attribution. A deployment MAY still consolidate namespaces via
curation verbs as a deliberate data decision, but it is no longer required to restore
visibility.

Status: see implementation note in Rule 3 §Status. The visible-set machinery
(NamespaceToken.visible, operations.rs multi-namespace read fanout over
token.visible_namespaces(), VerbRegistryBuilder::with_visible_namespaces, server.rs config
plumbing) was already present from the shared-brain visible-set work; Rev 4 connects it by
minting the dispatch token with the configured visible set on the default (no-explicit-param)
read path instead of an empty extra-visible set.

### Rule 3a — Edge namespace is attribution-only, never isolation

Every edge record carries a `namespace` column. By default, that column is stamped "local".
Edge namespace is attribution only: it records who created the edge. It is not an
isolation mechanism and is not used for access control.

The ADR-059 "three-column edge-visibility filter" — filtering on edge.namespace AND
source_entity.namespace AND target_entity.namespace — is REJECTED and remains rejected. No
per-edge namespace-join appears in any query, in any deployment. The graph is
shared structure; no actor "owns" an edge via namespace.

Edge namespace is NOT derived from the endpoints. The B1 storage-schema fact from the ADR-059
gemini mirror (edge carries its own namespace column, not derived from endpoints) is retained
as a storage-schema description, not as an isolation mechanism.

Source: ADR-059 §"Context" — gemini REFUTE correction 1 confirmed scheme B1 (edge carries its
own namespace column). That storage fact is accurate. What ADR-059's decision built on top of
it (the three-column visibility filter) is withdrawn.

### Rule 4 — Authorization enforced at one seam: the Gate

VerbRegistry::dispatch (crates/khive-runtime/src/pack.rs) calls self.gate.check(&gate_req)
before every verb invocation. This is the single enforcement point.

- Default gate: AllowAllGate — every request passes. Zero embedded cost.
- Deployed with multi-actor isolation: a TenantGate implementation (a custom crate behind the
  Gate trait) validates the authenticated identity and enforces per-actor namespace access.
- No policy DSL ships in khive. khive-gate-rego is a dev-dep only; operator policy lives
  behind the Gate trait, outside this repository.

The gate call is live code, not dead code. It is the seam that allows a permissive default
and an isolation-enforcing Gate implementation to share the same binary without structural
change.

**Namespace clarification.** Namespace is attribution and a gate policy-input — never a
storage boundary. The invariant is absolute regardless of Gate implementation:
storage is never partitioned by namespace, and by-ID ops resolve a globally-unique UUID with
no namespace check. The only difference between a permissive and an isolating deployment is which Gate is installed. The gate receives
the acting actor, the request namespace, and the target records' attribution as policy input,
and returns allow/deny:

- AllowAllGate ignores all of it and returns allow.
- A TenantGate MAY key per-tenant isolation on the namespace string (or any attribution
  field). That is the gate reading namespace as policy input — not storage partitioning on it.

How an operator's gate maps authenticated identities to allow/deny is operator policy,
implemented behind the trait. This ADR specifies only the gate's input contract and the
storage invariants all deployments preserve. "Namespace never becomes the storage namespace"
is absolute. "Namespace may be a gate's policy key" is consistent with it: a policy input
is not a storage boundary.

Source: CLAUDE.md "The `Gate` is a replaceable trait. The runtime holds an `Arc<dyn Gate>` and
consults it on every verb dispatch — one authorization boundary."

### Rule 5 — Merge semantics: same-namespace guard survives as substrate check, not isolation

CHANGE FROM ADR-007 v1: The v1 base text and Namespace-by-Layer amendment defended the
merge_entity same-namespace guard on the grounds that it prevents merging a "local KG entity
with an actor-scoped operational note." Gemini Finding 4 correctly refuted this: entities and
notes are different substrates merged by different verbs (merge_entity vs merge_note), so the
cross-substrate scenario is structurally impossible regardless of namespace values. In an
all-"local" world the guard is circular ("local" == "local") and is dead code with respect to
isolation.

This ADR retains the guard as merge semantics (ADR-014 curation), specifically as a
same-substrate deduplication quality gate, not as an isolation mechanism. It does not prevent
anything in the current deployment. It is dead-but-harmless and may be cleaned up in a future
PR. It must not be defended as isolation.

Source: curation.rs line 2908-2910 ("a merge-semantic constraint, not tenant isolation"),
gemini Finding 4.

### Rule 6 — Namespace type: open string with validated factory

Namespace is a string-backed newtype. The validated factory Namespace::parse(s) is the
construction surface (non-empty, length <= 256, character set [a-zA-Z0-9\-_:.], no trailing
separator, no empty segments). Namespace::local() returns the "local" singleton.

Structural validation from ADR-007 v1 is retained. It is not isolation machinery; it prevents
accidental garbage in the namespace column.

Removed from scope vs ADR-007 v1:

- NamespaceToken as a proof-of-authorization for by-ID access (superseded by Rule 2).
- NamespaceView wrapper that gates coordinator access (superseded by Rule 2).
- Timing oracle mitigation (returning identical errors for "wrong namespace" vs "not found")
  because by-ID ops no longer do namespace checks.
- NamespaceGrants, AuthContext, PrincipalId types from the base text — authorization types
  behind the Gate trait, outside this repository.
- Read-by-ID namespace post-fetch verification. SHIPPED removed by PR-A1.

NamespaceToken may be retained as the attribution carrier passed into pack handlers (it carries
actor, namespace, visible-set metadata), but it is no longer a by-ID access guard.

### Rule 7 — Attribution stamping

Writes stamp namespace, actor_id, and actor_kind on records from the dispatch context.
Attribution is informational: queryable, filterable, loggable, and available to the gate as
policy input. It does not route storage.

---

## Supersession Map

| Document                                          | Status                      | Superseded clauses                                                                                                                                                                                                                            | Surviving clauses                                                                                                                                                                                        |
| ------------------------------------------------- | --------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ADR-007 v1 base text (this file, prior revision)  | Superseded (Rev 3 replaces) | NamespaceToken as by-ID guard; NamespaceView; Read-by-ID namespace check; Timing oracle mitigation; NamespaceGrants / AuthContext / PrincipalId                                                                                               | Namespace::parse structural validation; "No Default"; Namespace vs backend independent axes; Hierarchy helper naming utility                                                                             |
| ADR-007 2026-05-25 amendment                      | Superseded (Rev 3 replaces) | Two-model framing (collapsed: the default is one model; operator-supplied Gate adds isolation)                                                                                                                                                 | AllowAllGate as default gate; [actor] id as attribution config; 4-tier config search path; display_name advisory-only                                                                                    |
| ADR-007 2026-05-27 amendment (Namespace-by-Layer) | Superseded (Rev 3 replaces) | KG-pack namespace override via token.with_namespace(); per-pack actor namespace routing for memory/gtd/comm — this ADR replaces routing with view-layer tag filters                                                                           | None; the routing intent is now stated correctly in Rule 3                                                                                                                                               |
| ADR-007 Rev 2 (2026-06-16)                        | Superseded (Rev 3 replaces) | Status: Proposed; Rev-3-deferred comm carry question ("leans yes"); Rev-3-deferred memory carry question ("undecided")                                                                                                                        | All substantive rules 0-7 (retained and sharpened); gemini REFUTE findings; PR-A1 shipped status                                                                                                         |
| ADR-050                                           | Partially superseded        | Decision: removal of KG-pack override (this ADR absorbs and confirms)                                                                                                                                                                         | Context: documents the override as a historical bug; Rationale "Why not token rebinding"                                                                                                                 |
| ADR-053                                           | Survives in full            | No conflict                                                                                                                                                                                                                                   | All: ActorStore, SessionStore, DispatchRequest, actor threading. Attribution threading is orthogonal to namespace isolation.                                                                             |
| ADR-059 (withdrawn)                               | Withdrawn before acceptance | Decision: visibility tiers (shared + private + proposal-only); three-column edge-visibility filter; subagents submit proposals; legacy "local" maps to private namespace. The three-column filter (Rule 3a) is rejected and remains rejected. | Gemini-mirror B1 storage-schema fact: edge carries its own namespace column, not derived from endpoints. Retained as a storage fact in Rule 3a; the isolation mechanism built on top of it is withdrawn. |

Note on ADR-058: PR #143 proposes a new ADR-058 for the brain posterior read-path. That
number is orthogonal to the namespace work. The supersession map above does not reference
ADR-058 because this ADR does not touch the brain read-path. PR #143 may proceed without
collision.

---

## Alternatives Considered

| Alternative                                                  | Pros                                                           | Cons                                                                                                                 | Disposition                                                                                                                                                                                   |
| ------------------------------------------------------------ | -------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Keep ADR-007 v1 NamespaceToken as by-ID guard                | Type-safe isolation proof                                      | Incompatible with dumb-storage contract; removed by PR-A1                                                            | Rejected, SHIPPED removal                                                                                                                                                                     |
| Per-pack actor namespace routing (Namespace-by-Layer Rule 3) | Preserves operational separation for gtd/comm                  | Contradicts Rule 0; breaks cross-lambda memory.recall; live audit shows memory is already "local"                    | Rejected in full. Blanket form rejected in Rev 2; narrow comm-only carry and memory carry both closed as NO-CARRY by Ocean ruling 2026-06-17 (Rev 3). No per-pack carry exists or is planned. |
| ADR-059 three-tier visibility model                          | Supports cooperative multi-lambda with fine-grained boundaries | Extra complexity; per-edge namespace-join query; "local" becomes ambiguous; conflicts with "one shared brain" ruling | Withdrawn before acceptance (ADR-007 Rev 2). Withdrawn status reaffirmed by Rev 3.                                                                                                            |
| ADR-059 three-column edge visibility filter                  | Prevents edge-induced namespace leaks                          | Complexity; joins across three namespace columns on every edge query; incompatible with "edges are shared structure" | Rejected (Rule 3a). Edge namespace is attribution-only, stamped "local" by default. No per-edge namespace-join at any tier.                                                                   |
| New ADR number (ADR-060) instead of amending ADR-007         | Clean slate                                                    | Leaves conflicting ADR-007 as active on main                                                                         | Rejected; amend in place                                                                                                                                                                      |
| Namespace as pure write-stamp, no SQL filter anywhere        | Simplest storage                                               | Removes legitimate tag-based view filtering for gtd assignee, comm addressing                                        | Rejected; view-layer filtering is correct, namespace-routing is not                                                                                                                           |

---

## Consequences

### Positive

- By-ID ops are namespace-agnostic (SHIPPED). Agents can reach any record they know the UUID of.
- Storage is dumb: no authorization logic at the store or runtime-post-fetch layer.
- Gate is the single enforcement seam: tenant isolation is one Gate implementation swap.
- "local" as the universal namespace means cross-project KG edges and cross-lambda memory.recall
  work without gymnastics.
- Memory pool is shared: lambdas learn from each other's recalled experience.
- Removes the KG-pack token rebinding bug (ADR-050 context) that caused cross-tenant bleed.
- Removes the Rule 0/Rule 3 contradiction present in the prior amendment.

### Negative

- PR-A2/PR-F require a full reindex, not just a WHERE-clause change: FTS5 is physically
  per-namespace and ANN snapshots are per-namespace graph blobs, both regenerated from the
  relabeled base rows. Vector base data needs no movement (vec_* tables are per-model,
  namespace-agnostic — live-DB verified). Higher-cost than the prior amendment implied, but a
  single reindex pass is the mechanism.
- PR-F is a live-data mutation with no automatic rollback. Requires snapshot discipline.
- The dispatch routing shipped local-only (empty visible set). Rev 3 closed the resulting
  data-state gap (KG list/search/recall MISS stranded non-"local" base rows) by a pending
  PR-A2/PR-F relabel of those rows to "local". Rev 4 closes it differently: configuring
  `visible_namespaces` to include the stranded namespaces makes the rows visible without
  relabeling. The relabel backfill is therefore no longer required for visibility (Rule 3b).

### Rev 4 deltas

Positive:

- Legacy non-"local" rows (e.g. v1-era lambda:* writes) become visible by configuration, with
  no live data mutation and no FTS/ANN reindex. The Rev 3 PR-A2/PR-F relabel — a no-rollback
  live mutation — is no longer required to restore visibility.
- Stored attribution is preserved: a record written under lambda:leo keeps namespace=lambda:leo
  and is readable, rather than being flattened to "local". This honors the "data vs view"
  principle: read scope is a view decision, attribution is data.
- Cross-actor read sharing (a configured set of namespaces visible to one deployment) without
  granting write access to those namespaces — reads widen, writes stay pinned to "local".

Negative:

- A misconfigured `visible_namespaces` widens reads beyond intent. In single-actor deployments this is
  low-risk (one operator, one brain) but the field must be validated (non-empty entries,
  Namespace::parse) and is logged. It is not an authorization control; a TenantGate must
  not rely on it.
- Two read-scope mechanisms now coexist: the default widened set (Rule 3b) and the explicit
  `namespace=` precise escape (Rule 3). The distinction (set-union default vs single-namespace
  override) must be documented at the call site to avoid surprise.

### Rev 6 deltas

Positive:

- Episodic memory is attributed to the actor that lived it and is private to that actor's
  default recall view, while semantic memory stays in the shared pool. This realizes the
  per-actor-episodic plus shared-semantic model that the prior "all memory pins `'local'`"
  default prevented.
- The change is confined to one write default. By-ID ops, the read fanout, the Gate seam, and
  every non-episodic write path are untouched. No `record.namespace == caller_namespace` check is
  introduced; visibility is restored by the existing Rule 3b read fanout, not by a storage
  partition.
- Backward-compatible with zero behavior change for unconfigured deployments: the anonymous actor
  id is `'local'`, so episodic writes resolve to `'local'` until a real actor is configured.
- Recall requires no code change. The Rev-4 visible-set fanout already returns actor.id-stamped
  memories on both the FTS and ANN paths.

Negative:

- Episodic and semantic writes now resolve to different default namespaces. A caller that wants
  an episodic memory in the shared pool must pass `namespace="local"` explicitly. The
  `memory_type`-conditioned default must be documented at the `memory.remember` call site (done
  in ADR-021 §10) to avoid surprise.
- Once a real actor is configured, episodic memories written by one actor are not in another
  actor's default recall view unless that actor's `visible_namespaces` includes the writer's id
  (Rule 3b). This is the intended privacy boundary, but it is a behavior change for multi-actor
  deployments relative to the all-`'local'` default. It is a view-scope effect, not a storage
  partition: the records remain reachable by UUID and by an explicit `namespace=` read.

### Neutral

- Namespace::parse structural validation survives.
- merge_entity / merge_note same-namespace guard survives as dead-but-harmless merge semantics.
- ADR-053 (ActorStore, DispatchRequest) survives in full.
- Wire format unchanged: namespace is a string in JSON/MCP.

---

## References

- CLAUDE.md "Authorization — the gate is a seam, by design" — Ocean's ratified design.
- CLAUDE.md "Namespace and authorization" — coding standards.
- v0 archive: khive-old/docs/_archive/adr_v0/ADR-007-namespace-as-open-string.md — original
  dumb-storage rules 1-4.
- Commit 2607e263 — PR-A1 implementation, by-ID contract SHIPPED.
- gemini_refute_adr007.md (resume 44078a77) — REFUTE findings 1-6, authoritative corrections.
- ADR-014 — curation operations, merge semantics.
- ADR-018 — Gate trait, single dispatch site, AllowAllGate.
- ADR-002 — edge cascade, no dangling refs.
- ADR-053 — ActorStore, SessionStore, actor threading (survives).
- ADR-021 §10, memory pack `memory_type` to namespace routing and the `namespace=` override
  (the consumer-side statement of the Rev 6 carve-out).
- `ActorRef::anonymous`, the anonymous caller (`kind = "anonymous"`, `id = "local"`) that makes
  the Rev 6 carve-out a no-op for unconfigured deployments.
