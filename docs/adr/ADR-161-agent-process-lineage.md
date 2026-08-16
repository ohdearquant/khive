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

**Parentage is derived, never asserted.** `agent.spawn` accepts no parent parameter. When
the agent-loop dispatcher issues `agent.spawn` on a process record's behalf, the dispatch
context that already carries `owner_actor` and `owner_peer_class` for that record [ADR-142
§1, "Actor provenance"; §3] identifies the spawning record, and the runtime binds
`parent_agent_id` from that context alone. A spawn request arriving with any caller-supplied
parent claim is a validation error. This is the same discipline ADR-142 fixes for
`owner_actor`: the field's source is the runtime's own resolved context, never a value the
process or the caller can influence. A record's parentage therefore cannot be forged,
transferred, or repointed.

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

| Verb                | Required parameters | Optional parameters                              | Success value                                                               |
| ------------------- | ------------------- | ------------------------------------------------ | --------------------------------------------------------------------------- |
| `agent.list`        | —                   | `parent_id`, `state`, `owner`, `limit`, `offset` | `{ agents: [record...], count, complete }`                                  |
| `agent.descendants` | `id`                | `max_depth`, `limit`                             | `{ root: agent_id, agents: [record + relative_depth...], count, complete }` |

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
  truncated enumeration for the whole population. A truncated result names the continuation
  offset. An enumeration that cannot read the table is a per-operation error, never an empty
  success.
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

With `descendants=true`, the runtime resolves the target's descendant set — through the same
walk as `agent.descendants`, at kill admission — and kills the parent first, then each
descendant in breadth-first order. Parent-first is deliberate: a coordinator that is still
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
no non-terminal descendant, false otherwise, with every surviving record named. Per-record
outcomes enumerate every record every pass reached, attributed to its pass. A cascade can
therefore never report clean while a record spawned during the cascade survives — a caller
that reads `subtree_terminal=false` knows the subtree is not dead and exactly which records
remain. The concurrent-spawn arm — a mid-walk descendant spawning a child that the
admission-time set does not contain — is an acceptance fixture for any implementation of
this ADR.

The subtree kill is per-record, not transactional: each record's kill succeeds or fails by
ADR-142's own rules (an already-`terminal` descendant is a no-op, exactly as in the
single-record case), and the operation's result enumerates per-record outcomes — killed,
already terminal, or error — rather than collapsing the subtree into one aggregate outcome. A caller
therefore sees exactly which records the cascade reached, and a partially failed cascade is
visible as itself, never as a clean kill.

**Authorization for a cascading kill is evaluated per record with the same authorizer as a
direct `agent.kill` of that record.** A descendant the caller could not kill directly is not
killed by the cascade, is reported in the per-record outcomes as denied, and does not abort
the rest of the walk. In the common case — one owner spawning its own tree — every record
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
`subtree_terminal` value with any surviving records named. Lineage in the audit plane is thereby reconstructable from
events alone, without reading the table.

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
  verbs plus one optional parameter; every existing caller and every ADR-142 contract is
  unchanged by construction (additive fields, default-false parameter, authorizer reused
  rather than extended).

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
