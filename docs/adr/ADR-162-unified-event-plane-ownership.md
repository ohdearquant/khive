# ADR-162: Unified Event-Plane Ownership

- **Status:** Proposed
- **Date:** 2026-08-16
- **Depends on:** ADR-004, ADR-005, ADR-018, ADR-022, ADR-041, ADR-046, ADR-088 (Amendment 1),
  ADR-094, ADR-103, ADR-129, ADR-142
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

**That authority is scoped to what the runtime observed, and it is a rule of scope rather
than of precedence.** This is the construction ADR-142 §5 already uses for process state:
for any process the runtime's agent table owns, that table is authoritative "full stop,"
while host-level orchestration tooling "remains free to spawn and track its own runs
outside this runtime's ownership until the cutover criterion" is met. The plane inherits
that shape rather than overriding it. Every operation that reaches this runtime produces
the plane's record of what happened, and for those facts no layer above may hold a
competing authority. Work a host performs that never reaches the runtime produces no events
here, and the host's own record of it is not thereby demoted, because there is nothing on
the plane for it to conflict with. A host-owned run that dispatches runtime verbs is split
by construction: the dispatches belong to the plane, the parts the runtime never saw remain
the host's, and ADR-142's cutover changes which runs fall in scope without changing this
rule.

The plane's write surface is runtime-internal. Layers above the runtime cause events by
performing operations; they do not append events directly, and no verb accepts a
caller-composed event for insertion into the authoritative log. What a caller can assert,
it can forge; the plane records what the runtime observed, in the runtime's own words.

**"Runtime-internal" is a claim about the trait boundary and not only about the verb
surface, and the two are not in the same state today.** The verb surface satisfies the rule
structurally, because no verb takes an event. The `EventStore` capability (ADR-005) is a
different matter: it is an in-process trait whose append accepts a fully constructed event,
including its namespace and actor fields, and any code linked into the runtime can call it.
In-process construction is therefore a position of trust, and the obligation that goes with
it is stated here — **an in-runtime call site may compose an event, but it may not choose
that event's attribution.** Namespace and actor are resolved from the same authenticated
context that governs the causing work (§2) and are stamped from it, never carried in from
values the composing code selected. Call sites that pass attribution through unvalidated
are conformance debt against this rule rather than an exception to it, and closing that gap
is required implementation work rather than an optional hardening — whether by construction
helpers that take the resolved identity instead of accepting the fields, or by validation at
the append boundary, is left to the implementing decision. Naming the gap is the point: a
rule stated only at the verb surface reads as satisfied while the surface that actually
persists the row is unguarded.

> **Implementation status and call-site inventory (2026-08-30):** The
> attribution construction gap is closed for the current in-process append
> surface. Ordinary runtime and pack code obtains a token-scoped
> `EventStore` from `KhiveRuntime::events`; its decorator overwrites namespace
> and actor from the sealed `NamespaceToken` on singleton, batch, preflight,
> and idempotent-batch append paths. Brain feedback, proposal-projection, and
> atomic KG-plan paths that must append inside a larger SQL transaction instead
> take an `EventAttribution` whose private fields can only be derived from that
> token, then stamp before the direct insert. Dispatch-audit constructors form
> the other request-caused
> group: they derive both fields from the resolved `GateRequest` for each
> dispatch, while `VerbRegistryBuilder::with_runtime_event_store` supplies
> the undecorated sink internally so one registry can retain per-request
> identity. The remaining direct writers are runtime-owned background work:
> channel lifecycle, checkpoint, and phase events use their fixed daemon
> principal, and pending-schedule failures use verified immutable creator
> provenance. Split/transport stores only forward already-stamped events.
> Direct storage-backend access and custom raw `EventStore` injection remain
> composition-root authority, not a caller-facing runtime capability.

This forbids caller-composed events, not caller-supplied content inside runtime-composed
ones. A verb whose arguments carry caller-supplied data — a feedback signal, a judgment, a
payload — may have that data recorded in the event the runtime composes for the operation:
the runtime stamps the event's kind, attribution, and context, and the caller's data
appears as what the caller supplied, never as what the runtime observed independently. The
distinction is authorship of the event, not presence of caller data within it.

That permission carries an obligation it does not discharge on its own. Event payloads are
durable and they are readable by every reader of the namespace they were written in —
ADR-022 §2 forces the caller's namespace into the query filter, so the exposure is
namespace-scoped rather than global, and namespace-scoped is not the same as private.
Nothing about a payload field is self-describing either: a recall query, a feedback comment,
and a search string are free text a caller may fill with anything, including a credential
pasted by mistake. **Every event class that records caller-supplied content must state what
it records and why, and must record the least that serves the class's purpose.** A class
whose consumer needs to count, correlate, or measure records a derived value — a length, a
hash, a bucket — in place of the raw text; a class that genuinely needs the literal content
says so, and in saying so accepts that it is writing durable namespace-readable data.
Telemetry that persists raw caller queries today predates this rule and is subject to it.
The rule's first job is to stop the next class from inheriting the permission without
inheriting the question.

### 2. Attribution is runtime-resolved

**The invariant is that attribution is resolved by the runtime, never asserted by the party
being attributed.** Where an operation the runtime dispatched caused the event, the
resolving mechanism is the dispatch seam: the event attributes to the actor that seam
resolved for the causing operation, the same per-request identity discipline ADR-142 fixes
for `owner_actor` — never a self-asserted label, never a value derived from content the
caller controls. Events caused by a tool call an agent-loop dispatcher issues on a process
record's behalf attribute to that record's `owner_actor`, exactly as ADR-142 §3 specifies
for audit attribution.

**Not every event on the plane has a dispatch seam, and the rule is stated so that those
events are covered rather than excluded.** Accepted decisions already emit two classes of
them. ADR-094 §2 fixes an emission contract that is "best-effort, in-process, direct
`append_event`, not a new verb," appending from inside the channel-poll and checkpoint loops
and explicitly not through `registry.dispatch()`. ADR-103 (c) adds phase-span events for
"background work that is not itself a verb dispatch," and its daemon-startup embedder warmups
run a path taking no namespace token, so "neither call executes inside `dispatch()` or under
the Gate"; those events attribute to the daemon principal. Both conform rather than excepting:
the runtime is attributing its own unsolicited work to its own principal, which is a
runtime-resolved attribution with no caller in the picture at all. The general form is that
**an event attributes to the principal the runtime resolved for the work that caused it — the
dispatched actor where a dispatch caused it, the daemon principal where the daemon's own
background work caused it — and a class with no resolvable principal is a class that may not
be added.** What the rule forbids is self-attribution by the party under audit; a daemon
recording work nobody asked it for is not that.

Process-lifecycle events additionally carry the process identity as event data, never as the
attributed principal, because process identity is a subject rather than an authenticated
actor (ADR-142 §1, "Actor provenance"). The `agent_id` field and the lineage context that
accompanies it are contracts owned by the process-model decisions rather than by this one:
this rule binds those events once ADR-161 fixes that schema, and is inert before then. A
reader who finds this requirement with no corresponding lineage contract in the tree should
read it as a forward obligation on the process-model lane, not as a claim that the contract
already exists here.

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
  gate program, a gate that cannot be evaluated refuses dispatch with a typed unavailability
  outcome that stays distinct from a policy denial: ADR-018's revised gate-error posture
  (Amendment 3 / ADR-129 Stage 1a) returns `RuntimeError::GateUnavailable` and records an
  error audit outcome rather than a `Deny`. That refusal must hold even when recording it
  fails. The audit append is therefore deliberately non-blocking with respect to the
  dispatch outcome: an append failure can never reopen a refused dispatch, and equally never
  converts an allowed dispatch into a failure. Coupling this class to its own persistence
  would let the audit trail's availability decide admission, inverting the dependency the
  fail-closed posture exists to protect. ADR-160 phase 0 is the decision that installs this
  posture in the runtime, and it is a prerequisite rather than an assumption: until it lands,
  what is stated here is the posture the class carries and not a description of what the
  gate-error path currently does.
- **A completed action's reported outcome can be coupled to its receipt while its effects
  are not.** ADR-088 Amendment 1 already carries this posture for one class: after a
  successful `git.digest` the runtime appends a receipt event, and where that append cannot
  be made durable the call "returns the stable error code `git_digest_receipt_persist_failed`
  and warns that writes may already have committed; it never returns an unqualified success,"
  while "ordinary dispatch audits remain best-effort." This is neither of the other two. It
  does not gate the action, which has already happened and whose effects survive the failure;
  it refuses to _report_ the action as cleanly completed without the durable record. It also
  states its own residue rather than implying none: a crash between the writes and the append
  leaves committed work with no receipt, so "absence of a receipt is therefore not proof that
  nothing committed."

This ADR promotes that difference to an explicit contract dimension: **every decision that
adds an event class must state the class's write posture** — what happens to the causing
operation when the append fails — rather than inheriting one silently. The three postures
above are the values in use today; the open sections below govern whether the third
generalizes past the single class that currently carries it.

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

The question is generalization rather than invention, and the difference matters for what
this section owes. A strict posture already exists on the plane for one class (§4's third
value), so the section does not have to justify that such a posture may exist; it has to
decide which further classes take one, and it inherits a worked example of the residue such
a posture leaves. It would also be deciding something strictly stronger than the existing
case: `git.digest`'s strictness couples only the _reported outcome_ of work that has already
happened, while a visibility contract couples what a model is permitted to _see_ next, which
puts the append on the critical path of the operation rather than at its end.

Owner: the named maintainer on the tracking issue opened for this section when this ADR
merges — an open section without an accountable assignee is treated as unowned and this ADR
as unimplemented on that point. Gate: a decision on this section — with the per-class
posture table filled in and the collision above resolved explicitly — before any surface
advertises a replayable session or transcript contract backed by this plane.

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

One consumer-side ruling is upstream of this section and does not belong to the plane's
maintainer: whether a merge-gating or admission-control consumer may block on plane outage
at all, and under what availability objective. That ruling sits with project ownership;
this section's contract must name it as an input rather than decide it.

Owner: the named maintainer on the tracking issue opened for this section when this ADR
merges, under the same unowned-means-unimplemented rule as section A. Gate: a decision on
this section before any merge-gating or admission-control consumer takes the plane as its
sole input.

## Non-goals

- **No new event kinds, verbs, or storage changes.** This ADR adds no code surface of its
  own; it states ownership and contract rules over surfaces that exist. It does not forbid
  other decisions from adding event kinds — §3 governs how they do so — and where a rule
  here names a schema owned elsewhere (the process-lifecycle identity of §2), the schema
  lands with the owning decision rather than being smuggled in here.
- **No replay or session-log mechanism.** Whether and how the plane backs a replayable
  transcript is Open section A's downstream, not this ADR.
- **No migration of existing mirrors.** Orchestration layers keep their views; this ADR
  fixes which record is authoritative, not which records may exist.

## Consequences

- Every future event-emitting decision inherits three obligations by reference: attribute
  from a runtime-resolved principal (§2), evolve additively (§3), and declare write posture
  (§4) — instead of each ADR re-deciding them locally.
- The agent process plane (ADR-142, ADR-161) has a stated home for its lifecycle and
  lineage events, closing the gap where process history would otherwise be readable only
  from the process table.
- Layers above the runtime can build rich views without those views accumulating authority:
  the conflict-resolution rule is fixed before the first conflict.
- Event classes carrying caller-supplied content acquire a stated content obligation (§1)
  instead of an unbounded permission, and the obligation attaches when the class is defined
  rather than after an incident.
- Attribution is stamped from sealed runtime context for every current request-caused
  in-process append path (§1); the explicit inventory above makes trusted background and
  composition-root paths auditable rather than treating every trait holder as equivalent.
- Typed gate unavailability held distinct from denial (§4, via ADR-160 phase 0) remains a
  conformance obligation; it is labelled as a gap rather than described as current behaviour,
  so a reader can separate what the plane guarantees today from what it is committed to.
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
