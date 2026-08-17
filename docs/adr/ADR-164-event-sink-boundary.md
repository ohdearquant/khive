# ADR-164: Event Sink Boundary

- **Status:** Proposed
- **Date:** 2026-08-17
- **Extends:** ADR-162 (adds a per-class contract dimension in the sense of its §4; ADR-162 is not replaced)
- **Depends on:** ADR-005, ADR-007, ADR-018, ADR-022, ADR-161, ADR-162

## Context

ADR-161 §5 needs a destination the caller-facing events surface cannot reach. Its audit records
carry the _structural_ descendant set, deliberately different from the set a caller is told about
in the operation result, and it states plainly that emitting them onto the caller surface "would
route around §4's visibility rule by a second path." It also records that no event-plane decision
provides such a destination: ADR-162 governs classes by attribution, additivity, and write
posture, and does not speak to who may read a class.

ADR-161 pays for that gap where it stands, with a prohibition and a two-sink requirement stated at
the point of use. That is the right way to record a prerequisite and the wrong place to keep it,
because the next class with the same need must restate it.

The apparent answer is a read-authorization dimension on the plane: let a class declare who may
read it, and have the query surface enforce the declaration. **That answer does not work, and the
reason is a property of the existing surface rather than a design preference.**

ADR-022 §1 exposes _two_ read paths. `list(kind="event")` takes an `EventFilter` and is namespace
scoped. `get(id=<uuid>)` is the same by-ID resolution used for entities, notes, and edges, extended
by ADR-022 to fall through to `EventStore::get_event` when the UUID matches no other substrate. The
second path carries no filter. Accepted ADR-007 is explicit that this is intentional and not an
oversight: by-ID operations are namespace-agnostic, "no by-ID namespace check is added anywhere in
the shared substrate layer," because storage is deliberately dumb and the Gate is the single
enforcement seam.

So a predicate added to `EventFilter` governs `list` and leaves `get` exactly as it was. A design
that filters list rows and reports the payload protected is protected against the path it filtered
and no other, while presenting to a reviewer as a solved problem. Anyone holding the event UUID
reads the record in full.

A second consideration points the same way. ADR-162 already identifies the hazard in accepting
security-relevant values on a fully constructed event: `EventStore` append takes the event as
given, including fields the composing code selected. An authorization-bearing audience field would
enlarge that same trusted-input surface, and would do so for a value whose whole purpose is to be
trusted.

Both observations converge on the same move. **Select the destination before persistence instead of
authorizing the read after it.**

## Decision

### 1. An event class declares its sink, from a closed two-value set

Every event class carries a sink declaration, joining attribution, additivity (ADR-162 §3), and
write posture (ADR-162 §4) as a per-class contract dimension. The vocabulary is closed:

- `caller_event_store` — the class is written to the event store ADR-022 queries. This is what
  every class does today.
- `operator_audit` — the class is written to the operator audit sink and is not written to the
  store ADR-022 queries.

A third destination is a further decision, not a new label. The set is closed for the same reason
the entity, note, and edge vocabularies are closed: a value that can be invented at a call site is
not a contract.

### 2. Existing classes are grandfathered; omission rejects only new classes

Every class that exists when this ADR is accepted has sink `caller_event_store` without
redeclaration, and its readers are unaffected. No existing read narrows as a consequence of this
decision.

A newly introduced class must declare its sink explicitly. Omission is a rejection of the class
definition, not a default.

There is no global default, and the asymmetry is deliberate. A default of `operator_audit` would
silently remove existing events from a surface that ADR-022 already specifies as returning them. A
default of `caller_event_store` would silently expose the next sensitive class, which is the
failure this ADR exists to prevent. Neither direction is safe as a default, so the only safe rule
is to grandfather what exists and require a decision for what is new.

### 3. ADR-161's structural-set classes bind to the operator sink

The audit records described in ADR-161 §5 declare `operator_audit`. That section's prohibition and
its "a runtime that cannot distinguish the two sinks does not implement this section" requirement
are thereby discharged by a plane-level contract instead of restated per consumer.

### 4. What the operator sink is not reachable by, stated plainly

An `operator_audit` record is not reachable through ADR-022 **by either path**. It is absent from
`list(kind="event")` because it is not in the store that verb reads, and absent from
`get(id=<uuid>)` for the same reason. This is the property that makes the decision sound, and it is
worth stating in these terms rather than as "the events are protected": the by-ID path is not
_handled_, it is _irrelevant_, because there is no record on that surface to resolve.

An implementation that writes an `operator_audit` record into the caller-facing store and relies on
a filter to hide it does not implement this ADR, and would reintroduce the exact defect described
in Context.

### 5. Authorization stays where ADR-007 and ADR-018 put it

This ADR introduces no authorization check inside the ADR-022 handler and no second enforcement
seam. The sink declaration is a routing fact evaluated at write time, not a policy evaluated at
read time. Access to the operator audit sink is a separately gated operator surface, governed by
the Gate like any other surface.

Nothing here makes namespace an authorization boundary. Namespace remains attribution per ADR-007.

### 6. Deferred, explicitly

A general audience or capability mechanism is **deferred**, not rejected. Specifically out of scope
and left to a future decision: per-record grants, an event class whose rows legitimately carry
different audiences, dynamic sharing between actors, and any cross-namespace projection of events.

The evidence that would justify reopening it is narrow and nameable: a reviewed case where one
stable event class must carry two different audiences per row and cannot be split into two classes;
or a Gate contract able to authorize a resolved event subject across both `list` and `get` without
page-count or timing leaks. Absent either, a general mechanism buys expressiveness nobody has asked
for at the cost of a trusted field on every row.

## Non-goals

- **No new query surface.** Caller-facing reads are unchanged, and this ADR adds no verb.
- **No change to ADR-022's namespace scoping or its by-ID resolution.** Both are accepted contracts
  and this decision is built on them rather than amending them.
- **No retention, aggregation, or transport policy for the operator sink.** Where operator audit
  records live and how long they survive is a separate operational decision.
- **No restart-visibility projection.** ADR-163 §4 declares its cross-namespace scope correct and
  requires deliberate projection for any caller surface that wants it. That is a different problem
  with a different answer, and routing it through a sink declaration would be a category error.
- **No self-containment ahead of its dependency.** This ADR extends ADR-162 and discharges a
  prerequisite stated in ADR-161; it must not be accepted before both.

## Consequences

- ADR-161 §5 can cite a contract instead of mandating a runtime behaviour at its point of use, and
  the next class needing an operator-only destination declares one value rather than restating an
  argument.
- The unsafe design is now unavailable rather than merely discouraged: there is no audience field
  to set incorrectly, and no filter whose omission silently exposes a class.
- Operator-only records are not queryable through ADR-022. This is the intended property and it is
  also a real cost — tooling that wants to read them needs the operator surface, and if the named
  operator sink is not itself durable and queryable, that tooling needs an aggregator.
- **A tension with ADR-162 §1 is created and is not resolved here.** ADR-162 makes the runtime-owned
  event substrate the single authoritative record of what happened; a class routed away from it is a
  scoped exception to that claim. The exception is narrow and declared, but a reader should see it
  as an exception rather than discover it. If the operator sink later becomes a queryable plane in
  its own right, the two-plane split is the thing to revisit first.
- Every routed class is one more place a future emitter can choose the wrong destination. The closed
  vocabulary and the reject-on-omission rule bound that risk; they do not remove it.

## Alternatives considered

### An audience dimension joining ADR-162 §4's per-class obligations

This decision was initially scoped as exactly that: a class would declare who may read it, and the
caller-facing query surface would enforce the declaration. It is recorded here rather than dropped,
because it was the working shape of this ADR until ADR-022's surface was read in full, and because
what set it aside was evidence rather than preference.

ADR-022 §1 exposes both `list(kind="event")` and `get(id=<uuid>)`, and ADR-007 makes by-ID reads
namespace-agnostic by design. A declaration enforced through `EventFilter` therefore governs one
of the two paths. The decision above supersedes that shape on those two facts.

It is worth being precise about the relationship: the sink boundary is not a narrower version of
the audience dimension. It answers the same requirement by a different mechanism — choosing a
destination before persistence rather than authorizing a read after it — and that difference is
what makes the by-ID path irrelevant instead of unhandled. A narrower audience dimension would have
inherited the same gap.

### A per-row audience field filtered at read time on one store

Keeps a single authoritative plane and would make future per-record grants cheap. Rejected for
three reasons that compound: the audience becomes a security-sensitive value accepted on a fully
constructed event, which is the defect ADR-162 already names for attribution; the Gate's request
contains an identifier rather than a resolved row, so it cannot evaluate a row-derived predicate
without a further lookup and a contract change; and `get` does not consult `EventFilter`, so the
filter would have to be reimplemented on a second path to mean anything.

### An open read-time policy hook with no stored field

Maximally expressive and needs no migration. Rejected because it must parse payload schemas as
authorization input, it makes pagination and pushdown semantics policy-dependent, and it
establishes the second enforcement seam ADR-007 exists to prevent.

### Two authoritative event stores

Gives hard physical separation and simple reads. Rejected because it duplicates retention, query,
and ordering responsibilities, and contests ADR-162 §1 directly rather than taking a declared
exception to it.

### Doing nothing

Defensible at the contract level: ADR-161 §5 already prohibits the unsafe emission, and ADR-163 §4
already declares its own scope correct, so no shipped defect is established by this decision.
Rejected narrowly, because "the operator audit sink" is named at the bite site and defined nowhere,
and a prerequisite that each consumer restates in its own words is a contract that will eventually
be restated inconsistently.
