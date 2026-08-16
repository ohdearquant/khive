# ADR-162: Unified Event-Plane Ownership

- **Status:** Proposed
- **Date:** 2026-08-16
- **Depends on:** ADR-004, ADR-005, ADR-018, ADR-022, ADR-041, ADR-046, ADR-094, ADR-103,
  ADR-142
- **Related:** ADR-160 (phase 0, gate-outage audit), ADR-161 (lineage audit events)

## Context

The event substrate already exists, piecewise, across accepted decisions. ADR-004 declares
Event the third substrate — append-only, "what happened." ADR-005 defines the `EventStore`
capability trait. ADR-018 wires dispatch-time emission through the gate audit path. ADR-022
gives events a query surface with ordering and cursors. ADR-041 projects provenance edges
from the log. ADR-046 makes proposal lifecycles event-sourced. ADR-094 adds
sequencing-assertable lifecycle telemetry. ADR-103 builds resource attribution over the same
plane. The schedule surface appends immutable creator-provenance events before activating a
scheduled row, and replay authority is reconstructed from those events, never from mutable
note metadata (ADR-106 and its amendments).

What no accepted decision states is the ownership rule: that this one plane is the
authoritative record of "what happened" for **every** execution layer above the runtime —
including host-level orchestration and harness tooling of the kind ADR-142 §5 names — and
that no layer above the runtime owns a second authoritative log.

The gap is not hypothetical. ADR-142 already had to legislate against a second source of
truth for process state (its Observation surface names host-level views "convenience
mirrors, never a second source of truth"), precisely because orchestration layers grow
their own bookkeeping when the substrate does not claim the role. The same drift applies to
events: a harness that records its own authoritative run history forks provenance — two
logs, written by different code under different failure modes, answering the same question
differently — and every consumer must then decide which log to believe, which is the
decision this substrate exists to make unnecessary.

A second motivating pressure comes from the agent process model. ADR-142's lifecycle
transitions, ADR-161's lineage and cascade decisions, and the audit attributions both
require are event-shaped facts about processes rather than about verb dispatches; they
belong on the same plane, under the same attribution discipline, or the process table
becomes readable only through itself.

This ADR fixes the ownership and attribution skeleton now, and deliberately does **not**
decide two contract questions that are under active design review — durability and
availability — because deciding them prematurely would either weaken an existing
load-bearing guarantee or overcommit the write path. Both are stated as explicitly open
sections with owners and gates, in the manner of ADR-142 §4's Defer rows.

## Decision

### 1. One authoritative plane

The runtime-owned event substrate (ADR-004/005, queried per ADR-022) is the single
authoritative event plane for every execution layer that runs above this runtime: verb
dispatch, gate decisions and denials, agent process lifecycle and lineage, scheduled-event
provenance, proposal lifecycles, and resource attribution all record to it. A log kept by
any layer above the runtime — an orchestration engine's run history, a harness's session
journal, a client's transcript — is a convenience mirror: it may exist, it may be richer in
presentation, and it is never consulted to resolve a conflict with the plane. This extends
ADR-142's no-second-truth rule from process state to event history, with the same
rationale: mirrors that can win arguments become the actual system.

The plane's write surface is runtime-internal. Layers above the runtime cause events by
performing operations; they do not append events directly, and no verb accepts a
caller-composed event for insertion into the authoritative log. What a caller can assert,
it can forge; the plane records what the runtime observed, in the runtime's own words.

### 2. Attribution rides the dispatch seam

Every event on the plane attributes to the actor the runtime's own dispatch seam resolved
for the operation that caused it — the same per-request identity discipline ADR-142 fixes
for `owner_actor`: never a self-asserted label, never a value derived from content the
caller controls. Events caused by a tool call an agent-loop dispatcher issues on a process
record's behalf attribute to that record's `owner_actor`, exactly as ADR-142 §3 specifies
for audit attribution. Process-lifecycle events additionally carry the process identity
(`agent_id`, and lineage context per ADR-161 §5) as event data, never as the attributed
principal — process identity is a subject, not an authenticated actor (ADR-142 §1, "Actor
provenance").

### 3. The vocabulary is versioned and additive

Event kinds, their payload schemas, and their attribution fields form a versioned contract
that evolves additively: a kind is never removed or repurposed, a payload field is never
redefined, and a consumer built against today's plane does not need to track a breaking
change tomorrow. This is the same evolution rule ADR-142 fixes for the process record and
ADR-022 assumes for its query surface; stating it at the plane level makes it binding on
every future event-emitting decision rather than re-derived per ADR.

### 4. Write posture is a per-class contract dimension

Different event classes already carry different write postures, and the difference is
load-bearing rather than accidental:

- **Provenance-gating events fail closed ahead of the action.** A scheduled row's
  creator-provenance event is appended before the row is activated, and a row without that
  proof is refused at fire time — the event is a precondition, and its absence blocks the
  dependent action.
- **Dispatch-audit events are decoupled from the dispatch decision.** Under the fail-closed
  gate program (ADR-160 phase 0), a gate that cannot be evaluated denies dispatch — and
  that denial must hold even when recording the denial fails. The audit append is therefore
  deliberately non-blocking with respect to the dispatch outcome: an append failure can
  never reopen a denied dispatch, and equally never converts an allowed dispatch into a
  failure. Coupling this class to its own persistence would let the audit trail's
  availability decide admission, inverting the dependency the fail-closed posture exists to
  protect.

This ADR promotes that difference to an explicit contract dimension: **every decision that
adds an event class must state the class's write posture** — what happens to the causing
operation when the append fails — rather than inheriting one silently. The two postures
above are the currently used values; the open sections below govern whether a third,
stronger posture is added.

## Open section A — durability (not decided here)

The question: for which event classes, if any, does the plane promise that what a model or
caller observed is logged — an equivalence between visibility and the log, rather than
best-effort recording?

The tension is concrete. A session-transcript contract of the form "model-visible if and
only if logged" makes the log a precondition of proceeding, which is the fail-closed
posture — but applied to the dispatch-audit class it would collide with the decoupling in
§4, which is itself a deliberate, tested guarantee of the fail-closed gate program. Any
resolution must therefore be per-class: it may introduce a synchronous
append-or-fail-the-operation posture for transcript-class events while leaving the
dispatch-audit class decoupled, and it must state the availability cost it accepts (a
synchronous posture makes event-store write availability part of the operation's
availability).

Owner: the runtime's event-plane maintainer. Gate: a decision on this section — with the
per-class posture table filled in and the collision above resolved explicitly — before any
surface advertises a replayable session or transcript contract backed by this plane.

## Open section B — availability (not decided here)

The question: what does the plane promise when it cannot write or cannot serve?

A fact plane that other layers consult for gating decisions must state its own degraded
mode on the result surface, or consumers will fall back to their mirrors and the mirror
becomes the actual system — the exact failure §1 exists to prevent. The known shape of the
answer is an explicit completeness discriminant on query results (a reader can always
distinguish "the plane answered in full" from "the plane answered partially or not at
all"), consistent with the degraded-state reporting this substrate already practices
elsewhere; the open work is fixing the discriminant's contract on the ADR-022 surface and
the write-side refusal semantics.

Owner: the runtime's event-plane maintainer. Gate: a decision on this section before any
merge-gating or admission-control consumer takes the plane as its sole input.

## Non-goals

- **No new event kinds, verbs, or storage changes.** This ADR adds no code surface; it
  states ownership and contract rules over surfaces that exist.
- **No replay or session-log mechanism.** Whether and how the plane backs a replayable
  transcript is Open section A's downstream, not this ADR.
- **No migration of existing mirrors.** Orchestration layers keep their views; this ADR
  fixes which record is authoritative, not which records may exist.

## Consequences

- Every future event-emitting decision inherits three obligations by reference: attribute
  from the dispatch seam (§2), evolve additively (§3), and declare write posture (§4) —
  instead of each ADR re-deciding them locally.
- The agent process plane (ADR-142, ADR-161) has a stated home for its lifecycle and
  lineage events, closing the gap where process history would otherwise be readable only
  from the process table.
- Layers above the runtime can build rich views without those views accumulating authority:
  the conflict-resolution rule is fixed before the first conflict.
- Two contract questions are visibly open with owners and gates, rather than implicitly
  decided by whatever the first implementation happens to do.

## Alternatives considered

### Let the execution layer that produces a run own its log

Rejected. Runs cross layers — a harness-initiated run dispatches runtime verbs, whose gate
decisions and effects the harness cannot observe first-hand — so a harness-owned log is
structurally partial, and two partial logs with independent failure modes answer the same
question differently. The layer with the complete view of dispatch is the runtime; the
plane sits there.

### Decide durability now, uniformly

Rejected. A uniform "logged if and only if visible" posture would couple the dispatch-audit
class to its own persistence, weakening the fail-closed gate program's tested guarantee
(§4); a uniform best-effort posture would foreclose transcript-class contracts this plane
should be able to back. The posture is per-class, and the per-class decision deserves its
own review (Open section A) rather than a default smuggled in with the ownership rule.

### Caller-appendable events

Rejected. An event a caller composes is an assertion, not an observation; admitting it
gives every layer above the runtime write access to the record that adjudicates disputes
with those layers. Layers cause events by acting; the runtime records what it resolved and
observed.
