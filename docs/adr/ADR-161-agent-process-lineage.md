# ADR-161: Agent Process Lineage

- **Status:** Proposed
- **Date:** 2026-08-16
- **Extends:** ADR-142

## Context

ADR-142 defines the runtime-owned agent process record: identity (`agent_id`), lifecycle
state, provider binding, actor provenance (`owner_actor`, `owner_peer_class`, the mapping
snapshots), checkpoint linkage, and a spawn fingerprint. [ADR-142 §1, "Persistent process
record"] The record carries no reference to the process that spawned it, and the observation
surface reads exactly one record by id: `agent.observe(id)` returns that record's fields, and
no enumeration verb exists. [ADR-142 §1, "Verb surface"; §1, "Observation surface"]

Two consequences follow when a spawner is itself an agent, and both are already documented
failure classes rather than hypotheticals.

First, a coordinator's kill cannot reach the workers it started. Host-level orchestration
tooling — the plane ADR-142 §5 names as the thing this runtime subsumes — has documented
exactly this defect in its own amendment history: a coordinating run records no link to the
worker sessions it starts, so stopping the coordinator strands the workers, and a stale-sweep
that walks records skips every coordinator because the link it would traverse does not exist.
A process model that reproduces that omission reproduces that defect. ADR-142's own agent
table is currently that reproduction: a record spawned by another agent's tool call is
indistinguishable, in the table, from a record spawned by a direct caller.

Second, aggregate accounting has no spine. ADR-142's parity row 16 accumulates usage per run
from the terminal-outcome path, but a tree of processes — a coordinator and the workers it
spawned — has no durable structure to accumulate over, so per-tree cost and per-tree outcome
questions ("what did this delegation actually spend, across everything it started?") are
answerable only by external bookkeeping, which is precisely the convenience-mirror role
ADR-142 §1 forbids treating as truth.

A comparable existing agent-execution engine treats lineage as a first-class durable seam:
processes record their parent and their depth, and children are enumerable without waking
any process. This ADR adopts that contract for the ADR-142 agent table.

The process-record field set is a versioned, additive-only contract [ADR-142 §1,
"Observation surface"], so lineage can be added without breaking any `agent.observe`
consumer. The agent pack's verbs enter through the standard pack surface [ADR-142 §1;
ADR-003, "New verbs without packs"], so enumeration verbs are additive registrations under
the same discovery, collision, and gate rules as the existing five.

## Decision

### 1. Parentage on the process record

The persistent process record gains two fields, both immutable after `agent.spawn`, both
additive to the ADR-142 field set:

- `parent_agent_id` (nullable): the `agent_id` of the process record on whose behalf the
  spawning dispatch was issued, or null for a spawn submitted directly by a caller that is
  not an agent process.
- `lineage_depth`: 0 when `parent_agent_id` is null, otherwise the parent record's
  `lineage_depth` plus one, computed by the runtime at spawn admission.

**Parentage is derived, never asserted.** `agent.spawn` accepts no parent parameter. When the
agent-loop dispatcher issues `agent.spawn` on a process record's behalf, the runtime binds
`parent_agent_id` from the dispatch context alone. A spawn request arriving with any
caller-supplied parent claim is a validation error.

**What in that context identifies the parent is the executing record's own `agent_id`, and
nothing weaker will do.** ADR-142's dispatch context carries `owner_actor` and
`owner_peer_class` [ADR-142 §1, "Actor provenance"; §3], and those are the wrong grain for this
purpose: they identify an actor and a peer class, not a record. One actor routinely owns
several concurrent process records — that is the ordinary case for a coordinator and its
workers, all of which bind the same `owner_actor` — so the pair is identical across all of
them and cannot say which one issued a spawn. Binding parentage from it would attach children
to an arbitrary member of that set, silently and with no error to observe.

This ADR therefore requires that the agent-loop dispatcher's context carry the `agent_id` of
the record on whose behalf it is dispatching, and `parent_agent_id` binds from that field. It
is runtime-resolved on exactly the terms `owner_actor` already is: set by the dispatcher from
its own execution state, never read from the request, never influenced by the process. A
dispatch that cannot produce a record-identifying `agent_id` is not an agent-issued spawn, and
the resulting record is a root with `parent_agent_id` null — the runtime never falls back to
guessing a parent from actor or class. Attaching a child to a _possible_ parent is worse than
recording no parent at all, because a null is legible as absent while a wrong parent reads as
fact. A record's parentage therefore cannot be forged, transferred, or repointed.

Because a parent must be an existing record at the moment its child is admitted, and every
child is a new record, the lineage relation is acyclic by construction: the table is a
forest, with direct-caller spawns as roots.

**Resolved parentage enters replay identity.** ADR-142's replay identity is the pair
(resolved actor, idempotency key), with argument identity judged by `spawn_fingerprint` over
exactly `{provider, task, provider_session_id, checkpoint_session_id}` [ADR-142 §1,
lifecycle table, spawn row; "Persistent process record"]. Parent context appears in neither,
so without amendment the following arm is silently wrong: one owner reuses one key string
with identical arguments from two spawn sites — first agent-issued under a parent, then
directly — and the second admission replay-matches, returning the original record with the
first site's `parent_agent_id`; the second caller receives a lineage it never had. This ADR
therefore amends the fingerprint's compared content: the canonical serialization gains the
runtime-resolved `parent_agent_id` as one additional field, included when a parent was
resolved and omitted entirely when the spawn is direct, digested with the rest at first
acceptance and never recomputed. A repeat whose pair matches and whose arguments are
identical but whose resolved parent context differs now fails the fingerprint comparison and
is a validation error — the same outcome ADR-142 already assigns to a matching pair with
different arguments — while a repeat matching in pair, arguments, and resolved parent
returns the original record with its original, correct lineage. The added field is
runtime-resolved context, never a caller argument, so the no-supplied-parent rule above is
unaffected. The two-site arm described here is an acceptance fixture for any implementation
of this ADR.

Because ADR-142 fixes `spawn_fingerprint` as an _order-sensitive_ canonical serialization, an
added field is not fully specified by naming it, and two further points are normative here.
**Position:** `parent_agent_id` serializes last, after `checkpoint_session_id`, so the existing
four fields keep their order and their relative encoding untouched. **Compatibility with
already-digested records:** every fingerprint stored before this ADR was computed over the
four-field form, and recomputing them is not possible — the digest is taken at first acceptance
and never recomputed, by ADR-142's own rule. A stored four-field digest is therefore compared
as a four-field digest: the runtime records which form each fingerprint was computed under, and
a repeat against a pre-existing record compares under that record's own form, omitting the
parent field entirely rather than comparing a five-field digest against a four-field one. Such
a comparison can only ever mismatch, which would turn every legitimate replay of an existing
record into a validation error at the moment this ADR ships.

**The recorded form is a version, not an inference from the serialized content, and the two must
not be conflated.** The agent table gains `spawn_fingerprint_version`, an immutable small integer
written at first acceptance beside the digest: `1` for the four-field form, `2` for the form
defined here. Comparison reads the stored version first and serializes the candidate under that
version, so the version is what selects the encoding — never a guess from the digest, which is
opaque, and never a guess from whether a parent was resolved, which is a property of the repeat
rather than of the record.

This distinction is load-bearing for direct spawns, and without it this section contradicts
itself. A direct spawn under version 2 omits `parent_agent_id` entirely, so its serialized bytes
are identical to a version-1 serialization of the same arguments. It is nonetheless a version-2
record and is stamped `2`. What is closed to new records is the version-1 **stamp**, not the
byte pattern: a record admitted after the runtime implements this ADR is always version 2,
whether or not a parent was resolved. Reading "the four-field form is closed" as a statement
about content would leave a new direct spawn with no admissible form at all, which is not the
rule. Existing digests keep their `1` and no migration of them is required or permitted; a
record with no stored version predates this ADR and reads as `1`.

This is an amendment to two ADR-142 contracts and is labelled as one rather than described as
purely additive. The first is `spawn_fingerprint`'s compared content and its versioning. The
second is the agent table's column list [ADR-142 §1], which gains `spawn_fingerprint_version`
here and `parent_agent_id` and `lineage_depth` in §1 above; all three are additive columns that
change no existing column's meaning, nullability, or immutability. Naming the table amendment
explicitly matters because a version discriminant that lived only in prose would be exactly the
unpersisted form this section rejects. Every other ADR-142 contract is untouched.

**Depth is bounded.** The runtime enforces a configured maximum `lineage_depth` at spawn
admission; a spawn that would exceed it is a per-operation validation error naming the limit
and the parent's depth. A runaway recursive spawner is thereby a bounded failure rather than
an unbounded table write. The limit's value is an operator configuration parameter with a
published default; the existence of the bound, not its value, is normative here.

### 2. Children survive their parent

Parentage is history, not a lifetime coupling. A parent record reaching `terminal` — by any
reason, including `host_restart` — changes nothing on its children's records: they keep
running, keep their `parent_agent_id` (which now names a terminal record), and terminate by
their own lifecycle rules. There is no implicit cascade, no orphan reparenting, and no
dangling reference: process records are durable, so a child's parent pointer always resolves
to a record, live or terminal.

**This rule is about causation, and it must not be misread as a survival guarantee.** It says
that a parent's termination does not _cause_ its children's; it does not say children outlive
every event that terminates a parent. The `host_restart` case is exactly where the difference
shows and is worth stating rather than leaving to inference: the ADR-142 boot scan transitions
_every_ non-terminal record to `terminal`, so after a host restart the children are terminal
too — not because their parent died, but because the same scan reached each of them directly
[ADR-142, boot-scan rule]. A whole tree therefore ends up terminal at a restart, and the
absence of a cascade here changes nothing about that outcome. Nor is the explicit cascade of §4
an exception to this section: `descendants=true` terminates children by the caller's request,
which is a separate operation from the parent's own termination, and this section constrains
only the latter. Read together the three rules are consistent and each is narrow: nothing
implicit follows a parent down, the boot scan reaches every record on its own authority, and a
cascade kills only what a caller asked it to.

This is deliberate, and it is the half of the documented defect that a naive fix inverts.
The defect is that a coordinator's stop _cannot reach_ its workers; the fix is that the
reaching is now _possible and explicit_ (§3, §4), not that it becomes automatic. An
automatic kill cascade would make a parent's `abandoned` transition — a clean terminal for a
disconnected attachment [ADR-142 §1, lifecycle table] — silently destroy healthy children,
turning a transport hiccup into a subtree massacre.

Across a host restart, ADR-142's boot scan terminates every non-terminal record —
whole trees included — and continuation is a fresh `agent.spawn` with a new record and a
new lifecycle [ADR-142 §1, restart-boundary row]. A continuation spawn therefore starts a
new tree (or joins the live tree of whatever process issued it); lineage is never inherited
across the restart boundary, for the same reason authority is not.

### 3. Enumeration verbs

The agent pack registers two additional read-only verbs, under the same registry, discovery,
and gate rules as the existing five [ADR-142 §1; ADR-023]:

| Verb                | Required parameters | Optional parameters                              | Success value                                                                             |
| ------------------- | ------------------- | ------------------------------------------------ | ----------------------------------------------------------------------------------------- |
| `agent.list`        | —                   | `parent_id`, `state`, `owner`, `limit`, `cursor` | `{ agents: [record...], count, complete, next_cursor? }`                                  |
| `agent.descendants` | `id`                | `max_depth`, `limit`, `cursor`                   | `{ root: agent_id, agents: [record + relative_depth...], count, complete, next_cursor? }` |

- `agent.list` enumerates process records matching every supplied filter. `parent_id`
  selects direct children only; `parent_id` omitted with no other filter enumerates the
  caller's whole visible table. `state` accepts a lifecycle state or `non_terminal`.
- `agent.descendants` walks the lineage forest from `id` transitively, breadth-first,
  returning each reachable record with its depth relative to the root. `max_depth` bounds
  the walk; the record named by `id` is not included in its own descendants.
- Both verbs read the agent table only. **Enumeration never wakes a process**: no provider
  is invoked, no state changes, no activity timestamp updates — the same read-only contract
  as `agent.observe`, over a set instead of one record.
- Both results carry an explicit `complete` boolean: false whenever `limit` truncated the
  result or `max_depth` cut the walk before exhaustion, so a caller can never mistake a
  truncated enumeration for the whole population. An enumeration that cannot read the table is
  a per-operation error, never an empty success.
- **Continuation is a cursor, and the ordering it rides on is part of the contract.** A
  numeric offset is not enough here: the agent table is written concurrently with the reads,
  so an offset into a set that gains and loses rows between calls silently skips records and
  repeats others, and a caller paging to exhaustion has no way to notice either. Both verbs
  therefore enumerate in a total, stable order — ascending `(spawned_at, agent_id)`, with
  `agent_id` breaking ties so the order is total even for records admitted in the same
  instant — and `agent.descendants` applies that order within each depth level, preserving
  breadth-first traversal across pages. `spawned_at` is named deliberately: it is the agent
  table's own admission timestamp [ADR-142 §1, agent-table column list], and the table carries
  no `created_at`. A cursor contract naming a column the table does not have is unimplementable,
  so the sort key is stated here as a column reference rather than as a generic convention.
  A truncated result carries `next_cursor`, an opaque
  token encoding the position in that order; passing it back as `cursor` resumes immediately
  after the last returned record. `next_cursor` is present exactly when `complete` is false,
  and absent exactly when it is true, so the two fields cannot disagree.
- Records admitted after a page was served sort after that page's cursor position and are
  returned by a later page; records that reach `terminal` mid-enumeration are still returned,
  since state is a field rather than a filter on existence. Neither case can shift a record
  the caller has already seen into a position it would be served from again. The cursor is
  opaque deliberately — a caller that constructs or arithmetically manipulates one is relying
  on an encoding this ADR does not fix.
- Returned records carry the same field set as `agent.observe`, under the same additive-only
  versioning [ADR-142 §1, "Observation surface"]; `parent_agent_id` and `lineage_depth`
  appear in both surfaces.

**Authorization is per record and identical to observation.** The ADR-142 lifecycle-record
authorizer already defines who may observe a record: the record's `owner_actor`, or a caller
whose current mapped peer class is in the operator's delegated-lifecycle class set [ADR-142
§1, "Actor provenance"]. Enumeration applies exactly that predicate per candidate record and
returns the records that pass, silently omitting the rest: a caller's enumeration result is
precisely the set of records it could have `agent.observe`d individually, so the two
surfaces can never disagree about visibility, and enumeration discloses nothing about
records outside the caller's authority — including their existence. `count` and `complete`
describe the visible set, not the table.

### 4. Reaching a subtree: kill with descendants

`agent.kill` gains one optional parameter, `descendants` (default false). The default
preserves ADR-142's single-record kill semantics byte for byte.

With `descendants=true`, the runtime resolves the target's descendant set at kill admission
and kills the parent first, then each descendant in breadth-first order.

**The cascade walk is structural; its output is visibility-scoped.** These are two different
questions and §3's enumeration answers only the second, so the cascade cannot reuse that walk
wholesale. `agent.descendants` is visibility-filtered by construction: a record the caller may
not observe is omitted, and its existence is not disclosed. A cascade built on that walk would
inherit the omission into its _accounting_ — an unobservable live descendant would be absent
from the resolved set, absent from every re-resolution, and the final re-resolution would
therefore find no non-terminal descendant and report `subtree_terminal: true` while that record
runs. The property this section exists to provide would be false exactly where it matters most.

So the traversal that computes liveness walks the lineage structurally, over every descendant
of the target regardless of the caller's authority over it, and `subtree_terminal` is computed
against that structural set. What the caller is told is then filtered:

- Records the caller is authorized to observe are named in the per-record outcomes, each with
  its own outcome — killed, already terminal, denied-kill, or error.
- Records the caller is not authorized to observe are never named, never counted, and never
  described. No id, no depth, no parentage, no cardinality.
- When the structural walk finds surviving records that the caller may not observe, the result
  carries `subtree_terminal: false` together with an `undisclosed_survivors: true` discriminant.
  The caller learns that the subtree is not dead without learning anything about who is in it.

The asymmetry is deliberate and runs one way only: authorization can subtract from what a caller
is _told_, never from what `subtree_terminal` is _computed over_. A caller can always trust
`subtree_terminal: true` to mean the subtree is dead.

**`undisclosed_survivors` is a deliberate one-bit disclosure, and stating it as zero disclosure
would be false.** The field tells a caller that at least one record it may not observe survived,
which is by definition information about a record it could not have observed directly. The
disclosure is bounded to exactly that bit: no id, no depth, no parentage, and no cardinality, so
a caller learns that its cascade did not fully clean up and learns nothing that distinguishes one
hidden subtree from another. It is disclosed rather than withheld because the alternative is
worse in the direction that matters — a caller told only `subtree_terminal: false` with an empty
outcome list cannot tell a fully-cleaned subtree from one with survivors it may not see, and
would reasonably read the empty list as success. An implementation may not widen this bit into a
count, and may not omit it to claim a stronger property than the operation provides.

An implementation that filters the traversal rather
than the output satisfies §3's disclosure rule and breaks this one, which is why the two walks
are stated separately here rather than shared. Parent-first is deliberate: a coordinator that is still
running can spawn replacements for workers killed under it, so the spawner stops before its
subtree does. A spawn admitted on a record's behalf after that record reached `terminal` is
an illegal-transition error on the spawning dispatch [ADR-142 §1], so a killed parent cannot
refill its subtree while the walk proceeds.

The admission-time set is not the whole story, and the cascade must not pretend it is: a
descendant that is still live during the walk — resolved into the set but not yet reached —
can itself spawn between set resolution and its own kill, and that child is outside the
resolved set. The cascade therefore repeats resolution-and-kill: after the walk completes,
the runtime re-resolves the target's descendants, kills any non-terminal record the
re-resolution finds (under the same per-record authorization), and repeats until a
re-resolution finds no non-terminal descendant or a bounded pass count is reached. The
operation's result carries `subtree_terminal`: true only when the final re-resolution found
no non-terminal descendant, false otherwise. Naming of survivors follows the visibility rule
above without exception: every surviving record **the caller may observe** is named, and a
survivor the caller may not observe contributes to `subtree_terminal: false` and to
`undisclosed_survivors` only. Per-record outcomes enumerate every record every pass reached
**that the caller may observe**, attributed to its pass. A cascade can therefore never report
clean while a record spawned during the cascade survives — `subtree_terminal` is computed over
the structural set, so concurrent spawns cannot hide in the authorization gap — while a caller
reading `subtree_terminal=false` learns which of the survivors it is entitled to see, not
necessarily all of them. The concurrent-spawn arm — a mid-walk descendant spawning a child that the
admission-time set does not contain — is an acceptance fixture for any implementation of
this ADR.

**Every one of these operations is bounded in total work, output, and audit volume, and the
bounds are refusals rather than truncations.** A lineage tree is written by the processes being
enumerated, so its size is not under the reader's control, and an unbounded traversal is a
resource-exhaustion surface reachable by any caller who can spawn. Four bounds are normative.
Depth is capped at spawn admission, so no tree exceeds the configured maximum `lineage_depth`.
Output per call is capped by `limit`, whose own maximum is an operator configuration parameter
with a published default. Total records visited by a single enumeration or by one cascade pass
is capped independently of `limit` by `lineage_visit_limit`, because a walk can visit far more
records than it returns once filters and authorization are applied; exceeding it is a
per-operation error naming the bound, never a silently short answer — a truncated-looking
success here would be indistinguishable from a small tree. Cascade passes are capped by
`cascade_pass_limit`, as §4 already requires, and a cascade that exhausts it returns
`subtree_terminal: false` with the survivors it knows about rather than looping.

Both are operator configuration parameters with published defaults, on the same footing as
`lineage_depth` and the `limit` maximum: the existence of each bound is normative here, its
value is not. They are named because an unnamed bound cannot be configured, audited, or cited
in the error that reports it, and all four bounds are stated together so that an implementation
can be checked against a closed list rather than against prose.

**`agent.kill` with `descendants=true` needs its output bound stated explicitly, because it
takes no `limit`.** Adding one would be wrong: a cascade that killed records and then truncated
its own report would leave the caller unable to name what it just terminated, which is the
failure §4 exists to prevent. Its output is instead bounded by construction — per-record
outcomes are emitted only for records the walk actually reached, and the walk is capped by
`lineage_visit_limit` per pass and `cascade_pass_limit` passes. A tree large enough to threaten
the result size therefore fails as a per-operation error at the visit bound before any kill is
issued, rather than returning a partial report. This is the one operation in this section whose
output bound is derived rather than parameterized, and it is derived deliberately.

Audit volume follows from these and needs no separate cap: one event per pass plus one per
record reached, both already bounded above. What does need saying is that the bounds are
enforced per operation and not per caller — this ADR does not define a rate limit, and a caller
issuing many bounded enumerations in a loop remains a matter for the gate rather than for these
verbs.

The subtree kill is per-record, not transactional: each record's kill succeeds or fails by
ADR-142's own rules (an already-`terminal` descendant is a no-op, exactly as in the
single-record case), and the operation's result enumerates per-record outcomes — killed,
already terminal, or error — rather than collapsing the subtree into one aggregate outcome. A caller
therefore sees exactly which records the cascade reached, and a partially failed cascade is
visible as itself, never as a clean kill.

**Authorization for a cascading kill is evaluated per record with the same authorizer as a
direct `agent.kill` of that record.** A descendant the caller could not kill directly is not
killed by the cascade and does not abort the rest of the walk. Whether it is _named_ turns on
observation, not on kill: killing and observing are separate authorizations, and a caller
frequently holds the second without the first. A descendant the caller may observe but may not
kill is reported in the per-record outcomes as denied — the caller could have read that record
through `agent.observe` anyway, so naming it discloses nothing new. A descendant the caller may
not observe is not reported at all; it contributes only to `subtree_terminal` and, if it
survives, to `undisclosed_survivors`. Reporting is therefore bounded by what the caller could
already have seen, which is the same bound §3 places on enumeration. In the common case — one owner spawning its own tree — every record
shares `owner_actor` (a child's owner binds from the spawning dispatch's resolved actor,
which is the parent record's owner [ADR-142 §1, "Actor provenance"]), so the whole subtree
is reachable; the per-record rule matters at the delegation boundary, where a
delegated-class caller's authority is class-defined per record rather than inherited down
the tree.

`agent.suspend` and `agent.resume` take no descendants parameter. Suspension is legal only
at a record's own message-yield boundary [ADR-142 §1, lifecycle table], and a subtree has no
shared yield boundary to suspend at; resume re-derives authority per record by design
[ADR-142 §1]. A caller that wants a subtree quiesced enumerates it and acts per record,
with each operation's own admission rules intact.

### 5. Audit

The spawn audit event gains the resolved `parent_agent_id` (or its absence) alongside the
attribution it already carries, so the audit trail records the same forest the table does. A
`descendants=true` kill emits one audit event per resolution pass — naming the root, the
pass, and that pass's resolved descendant set — plus the per-record kill events the
individual transitions already produce, and a closing event carrying the final
`subtree_terminal` value with any surviving records named.

**The audit plane carries the structural set, not the caller-visible one, and this is not a
disclosure exception.** §4's visibility scoping bounds what a _caller_ is told in the operation
result; audit events are written for the audit reader, whose authority is the operator's rather
than the requesting caller's, and an audit trail that recorded only what the caller was allowed
to see would be unable to answer the question it exists to answer — which records the runtime
actually killed. The two planes therefore carry different sets by design, and an implementation
must not "fix" the difference by narrowing the audit event to the caller's view or by widening
the operation result to the audit's.

**That difference is only safe behind an operator-only audience, and the event plane does not
provide one today. This is a prerequisite of this section, not an assumption it may make.** The
caller-facing events query surface authorizes by namespace and by nothing else: the handler
forces the caller's namespace into the filter and match-all resolves to `WHERE namespace = ?`
[ADR-022 §2, "Namespace isolation"]. It applies no per-record predicate to event payload
content. So an event whose payload carries the structural descendant set is readable in full by
any caller sharing its namespace — and the delegation boundary described above is exactly the
case where callers share a namespace while holding per-record authority over different subsets
of it. Emitting the structural set onto that plane as it stands would route around §4's
visibility rule by a second path — the visibility rule would hold on the operation result and
be defeated on the event, which is the same disclosure with an extra hop.

Two consequences are normative. First, the structural-set audit events defined here may not be
emitted onto the caller-facing events surface until that surface can express an operator-only
audience; until then an implementation emits them to the operator audit sink only, and a
runtime that cannot distinguish the two sinks does not implement this section. Second, the
audience requirement belongs to the event plane rather than to this ADR: ADR-162 governs event
classes, payload discipline, and audience, and this section is a consumer of that contract. The
requirement is recorded here so that the prerequisite is visible at the point of use, and this
ADR does not create a path from an operation result to an audit record.

Lineage in the audit plane is thereby reconstructable from events alone, without reading the
table — subject to the audit sink actually retaining them, which is a property of the sink and
not something this ADR grants.

## Non-goals

- **No lifetime coupling.** This ADR adds no supervision, restart, or dependency semantics
  between parent and child. A parent observing and reacting to its children's states is
  application logic over the enumeration surface, not a runtime behavior.
- **No authority inheritance.** Lineage confers nothing: authorization remains exactly
  ADR-142's per-record rules. `parent_agent_id` is never consulted by the gate, the
  lifecycle authorizer, or the data-scope derivation.
- **No cross-restart lineage.** Continuation after `host_restart` is a fresh spawn in a
  fresh tree, per ADR-142's restart boundary.
- **No reparenting or deletion.** Parentage is immutable; this ADR defines no record
  deletion or retention policy and inherits ADR-142's durable-record posture.

## Consequences

- The documented coordinator-kill defect class becomes structurally impossible for
  runtime-owned agents: every spawner-spawnee edge exists in the table at admission, so a
  stop or a sweep that intends to reach a subtree has a durable path to it, and
  `descendants=true` makes the reach a single audited operation.
- Enumeration-without-waking gives operators and coordinators a truthful population view:
  `agent.list(state="non_terminal")` is the live process table, `complete` says whether the
  view is whole, and no process is disturbed by being counted.
- Per-tree accounting becomes derivable from first-class state: usage accumulated per run
  (ADR-142 parity row 16) can be aggregated over `agent.descendants` without external
  bookkeeping, keeping the agent table the single source of truth for structure as well as
  state.
- The process record grows by two immutable fields and the verb surface by two read-only
  verbs plus one optional parameter. Every existing caller is unchanged by construction
  (additive fields, default-false parameter, authorizer reused rather than extended), and
  every ADR-142 contract is unchanged **except** `spawn_fingerprint`, whose compared content
  and versioning this ADR amends in §1. That exception is the one place an implementation of
  this ADR touches an accepted contract, and it is stated where it is made rather than left
  to be discovered.
- The agent-loop dispatcher must carry the executing record's `agent_id` in its dispatch
  context. Where it carries only `owner_actor` and `owner_peer_class` today, that is a real
  addition and not a reinterpretation of existing fields; until it lands, agent-issued spawns
  correctly record no parent rather than a guessed one.

## Alternatives considered

### Automatic kill cascade (parent terminal implies subtree terminal)

Rejected. It conflates history with supervision: `abandoned` is a clean terminal for a
disconnected attachment, and an automatic cascade would let a transport disconnect destroy
healthy children. The defect being fixed is unreachability, not insufficient automation;
§4 makes the cascade explicit, authorized, and audited instead of implicit.

### Reparent orphans to a synthetic root

Rejected. Reparenting rewrites history and destroys the accounting spine — a subtree's costs
would migrate to a record that never spawned it. Children keeping a terminal parent pointer
is truthful and resolves every query this ADR adds.

### Caller-supplied parent parameter on `agent.spawn`

Rejected. A suppliable parent is a forgeable lineage: any caller could attach its record to
another owner's tree, corrupting enumeration, cascade scope, and accounting at once. Derived
binding from the dispatch context follows the field-source discipline ADR-142 already fixes
for `owner_actor`, and costs callers nothing — the runtime always knows the spawning record.

### A separate lineage table outside the process record

Rejected. Two tables describing one process create the second source of truth ADR-142's
observation surface forbids; a lineage row that outlives or predates its process record is a
new consistency obligation with no capability the two record fields do not provide.

### Enumeration through a general query surface instead of pack verbs

Rejected for this ADR. Every top-level operation needs a pack owner and registry dispatch
path [ADR-003; ADR-142 §1], and the two verbs here are shaped by the lifecycle authorizer's
per-record visibility rule, which a general query surface does not know. A broader query
capability over runtime tables, if ever wanted, is its own decision and does not block this
one.
