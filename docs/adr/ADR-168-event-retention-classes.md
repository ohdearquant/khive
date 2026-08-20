# ADR-168: Event Retention Classes and Sealed Archival

- **Status:** Proposed
- **Date:** 2026-08-20
- **Extends:** ADR-162 (adds a per-class contract dimension in the sense of its §4), ADR-022
  (two additive surface behaviours, §5 below)
- **Depends on:** ADR-004, ADR-005, ADR-007, ADR-022, ADR-041, ADR-046, ADR-094, ADR-103,
  ADR-106, ADR-161, ADR-162, ADR-164

## Context

The event plane is append-only by construction, and nothing bounds its lifetime. The
`EventStore` capability (ADR-005) exposes append, get, query, and count; it has no delete or
update surface, and the backing tables (`events`, `event_observations`) have no non-test
deletion path anywhere in the workspace. That is the correct write posture for an
authoritative record of what happened (ADR-004, ADR-162 §1) — and it means growth is
unbounded by design, with no accepted decision stating a lifetime for any event class.

The pressure is no longer hypothetical. One long-running deployment has accumulated over
4.2 million event rows, and the plane's own exhaustive counting surface refuses windows
matching more than 2 million events, so consumers of wide windows already operate against
population limits rather than against the full record. A store whose fact plane grows
without bound eventually taxes every query, checkpoint, and backup that shares the file.

Retention cannot be a single age cutoff, because the plane is not only an audit trail.
Accepted decisions make it load-bearing in four distinct ways:

1. **Event-sourced state.** Proposal lifecycles are reconstructed from events (ADR-046),
   and a scheduled row's replay authority is reconstructed from its creator-provenance
   events, never from mutable metadata (ADR-106). Removing those rows while their referent
   is live does not trim history; it deletes state.
2. **Provenance projection input.** ADR-041 projects provenance edges from the log; the log
   is the source the projection can be re-derived from.
3. **Accounting basis.** ADR-103 builds windowed resource attribution over the plane, and a
   windowed lifecycle comparison is only sound while both ends of the window are readable.
   ADR-018's `RateLimit` obligation is validated but explicitly unenforced in v0; when an
   admission or quota decision ships, it will need an accounting basis that does not reset,
   and a retention rule fixed after that decision would constrain it. Fixed now, retention
   can state the carry-forward rule such a decision inherits.
4. **Audit.** Dispatch audits, gate outcomes, and telemetry — the classes an age horizon
   serves well.

A uniform cutoff that is safe for the fourth silently breaks the first three. The unit at
which safety can be stated is the event class, and the plane already has the pattern for
per-class contract dimensions: attribution (ADR-162 §2), additive evolution (§3), write
posture (§4), and sink (ADR-164 §1). This decision adds the fifth dimension — lifetime —
in the same form: a closed vocabulary, declared per class, with a fail-safe rule for
classes that have not declared.

## Decision

### 1. Retention class is a per-class contract dimension, from a closed three-value set

Every event class carries a retention class, joining attribution, additivity, write
posture, and sink as a per-class contract dimension. The vocabulary is closed:

- **`pinned_while_referenced`** — the class declares a referent and a terminal condition.
  Rows are ineligible for archival while their referent is live, and become age-eligible
  only after the referent reaches its terminal state. Examples of the shape: creator-
  provenance events for a schedulable row (referent: the row; terminal: the row is
  deleted or permanently deactivated per ADR-106's contract), proposal lifecycle events
  (referent: the proposal; terminal: a terminal lifecycle state per ADR-046), process
  lineage events (referent: the process record; terminal: a terminal process state per
  ADR-161).
- **`aggregate_then_archivable`** — the class declares a durable aggregate that must exist,
  and be verified, before rows in a window become eligible. The aggregate is itself a
  durable record with a stated home, written before eligibility and never reset by the
  archival that follows it. Any class serving quota or admission accounting takes this
  value: an aggregate that survives archival is what makes a non-resetting quota
  implementable over a bounded live store. The verification contract for an aggregate —
  what is checked, against what, and recorded where — is defined at the first class
  assignment that takes this value, not inherited silently from this decision.
- **`age_archivable`** — rows are eligible on horizon age alone (§3). The value for pure
  audit and telemetry classes with no referent and no accounting consumer.

**A class with no declared retention class is retained indefinitely.** That is the absence
state, not a fourth vocabulary value, and it fails in the safe direction: nothing already
recorded becomes eligible for archival by default. Every class existing when this ADR is
accepted is in that state until a follow-up assignment declares otherwise (§7). A newly
introduced class must declare one of the three values; omission is a rejection of the class
definition, exactly as ADR-164 §2 treats a missing sink declaration.

A class may additionally declare a **correlation key** — a payload field grouping rows into
units whose counts are compared against each other (ADR-163's restart-scan events pair an
opening count with a closing count keyed by boot identity). Eligibility and segment
assignment are evaluated at correlation-unit granularity: a unit's rows archive together or
not at all, so a paired-count comparison never spans the horizon.

### 2. Archival is copy-then-seal-then-verify; the plane as a whole stays append-only

Archival moves eligible rows into **sealed segments** outside the live store:

1. **Copy.** Rows eligible under §1 and older than the horizon (§3) are copied, with their
   `event_observations` rows, into a new segment — an append-only artifact outside the live
   database file. Segment format and placement are operator-owned configuration; the
   contract is on the manifest and the verification, not the container.
2. **Seal.** The segment closes with a manifest recording: segment identity, the classes it
   contains, row count, the covered `created_at` range, and a content digest computed over
   the segment's rows.
3. **Verify.** Before any pruning, the sealed segment is independently re-read: row count
   and content digest are recomputed from the segment and compared against the manifest.
   A mismatch voids the segment; nothing is pruned against it.

Only after verification may the live store be pruned, and pruning is bounded to exactly the
rows proven present in the verified segment — the deletion's row set is derived from the
segment readback, not from re-evaluating the eligibility predicate, so a row that became
eligible after the copy cannot be deleted without having been archived.

This is the one conscious amendment to the plane's append-only posture, and it is scoped:
the **live table** loses rows only to a verified sealed copy, and the **plane as a whole**
— live store plus sealed segments — remains append-only. No information is destroyed;
this decision provides no path that destroys a segment.

### 3. The horizon is operator configuration, and archival is off by default

Eligibility requires `created_at` older than a configured horizon in addition to the
class's own precondition. The horizon's value is operator-owned configuration; this
decision fixes the mechanism, not the number. **Unset means archival does not run.** A
deployment that never configures a horizon keeps today's behaviour exactly, and no
migration or backfill occurs on upgrade.

### 4. Archival actions are themselves events, recorded ahead of the destructive step

The retention machinery emits its own event classes, declared here with all five
dimensions:

- **`archive_segment_sealed`** — one per sealed segment, carrying the manifest fields.
  Attribution: the daemon principal (runtime background work, the ADR-162 §2 form). Sink:
  `caller_event_store`. Write posture: **precondition** — the event is appended and
  durable before any prune against that segment may run; if the append fails, the prune
  does not run. Retention class: `pinned_while_referenced`, referent the segment
  itself; terminal condition: destruction of the segment, a path this decision does not
  provide. Manifests are therefore effectively permanent, which is intended — the
  manifest must outlive everything it vouches for.
- **`archive_rows_pruned`** — one per prune, recording the segment pruned against, the
  row count removed, and the recomputed verification values beside the manifest values it
  matched. Same attribution and sink. Write posture: precondition — appended before the
  delete executes. Retention class: `pinned_while_referenced`, referent the segment;
  terminal condition: destruction of the segment, a path this decision does not provide.

Recording the verification pair (manifest value and recomputed value) in the prune event
is deliberate: a reader auditing retention can check the comparison from the plane alone,
without access to the segment media.

### 5. The query surface states what retention did — two additive behaviours on ADR-022

Retention must not silently change what a query means. Two additive contract behaviours:

- **Horizon disclosure on `list`.** A `list(kind="event")` whose effective time window
  extends past the oldest live row's horizon reports that the window is horizon-clipped —
  a completeness fact on the result envelope, present only when clipping occurred.
  Consumers that never query past the horizon see no change. This discriminant is scoped
  to retention state only; the general availability discriminant (whether the plane
  answered fully in the presence of write or read failure) belongs to ADR-162 Open
  section B, which should take this behaviour as an input rather than find it decided.
- **Archived-stub resolution on `get`.** Pruning writes a stub — event id and segment
  identity — into a live index, and a by-ID `get` of a pruned event resolves the stub to
  a typed archived answer naming the segment, rather than the absence answer an unknown
  id gets. Absence of any record and archival of a real record are different facts, and a
  surface that returns the same answer for both converts retention into silent data loss
  for every holder of an event id. The stub index grows with archived rows but carries
  two identifiers per row, which is the price of keeping by-ID resolution honest. The
  stub is data, not authorization: ADR-007's namespace-agnostic by-ID posture is
  unchanged, and stubs for `operator_audit`-sinked classes do not exist because those
  rows were never on this surface (ADR-164 §4).

### 6. Population, stated exactly

This decision governs the runtime event plane: the `events` and `event_observations`
tables, with observations always archiving alongside their event in the same segment.
The archived-stub index (§5) is a table this decision creates and is in population: its
growth is bounded to two identifiers per archived row, and stubs are never themselves
archived.

Explicitly out of scope:

- **The operator audit sink.** ADR-164 names its retention a separate operational
  decision; nothing here reaches it.
- **Pack-owned logs.** `brain_event_log` is a pack-owned append-only table on a different
  surface; a pack owning a private log owes its own retention decision, for which this
  ADR's class pattern is available but not imposed.
- **Non-event tables.** Notes, entities, edges, and their curation lifecycle are governed
  by ADR-014; nothing here adds a deletion path to any other substrate.

### 7. Class assignments land as follow-up, not here

This ADR fixes the dimension, the vocabulary, the fail-safe default, and the mechanism. It
deliberately assigns retention classes only to the two classes it introduces (§4). The
assignment of existing classes — which dispatch-audit, gate, telemetry, lifecycle, and
provenance classes take which value, with each `pinned_while_referenced` class's referent
and terminal condition named — is a follow-up decision per class family, made with the
owning ADR's contract in hand. Until a class is assigned, the §1 default holds and it is
retained indefinitely, so the follow-up can be wrong only in the direction of keeping too
much.

## Non-goals

- **No durability or availability decision.** ADR-162's Open sections A and B remain open;
  §5's horizon disclosure is scoped input to B, not a resolution of it.
- **No coverage change.** Retention governs recorded events. What gets recorded — including
  known gaps where mutations produce no event-plane observation — is the emission
  contract's problem, not retention's, and archival neither widens nor narrows it.
- **No segment transport or remote storage policy.** Where segments live beyond "outside
  the live database file" is operational.
- **No automatic scheduling policy.** When the archival pass runs is operational; the
  contract governs what it may touch and what it must record.
- **No change to event vocabulary.** Kinds remain additive per ADR-162 §3; retention
  removes rows, never kinds, and a class whose rows are all archived remains a defined
  class.

## Consequences

- The plane gains a bounded live store without giving up its append-only character: the
  amendment is scoped to the live table, conditioned on a verified sealed copy, and
  recorded on the plane itself before it happens.
- Every future event-emitting decision now declares five dimensions instead of four, and
  the fifth has a fail-safe absence state, so the cost of not deciding is growth rather
  than loss.
- A future admission or quota decision inherits `aggregate_then_archivable` as the stated
  carry-forward rule, instead of discovering after the fact that its accounting basis was
  age-pruned.
- By-ID holders of archived event ids keep an honest answer (§5), at the cost of a stub
  index that grows with archived volume.
- The follow-up assignment work (§7) is real and visible: until it lands, no existing
  class is eligible and the live store keeps growing. That is the intended failure
  direction, and it makes this decision safe to accept independently of the assignments.
- Two new event classes exist with their dimensions declared, exercising the full
  five-dimension declaration this family of decisions now requires.

## Alternatives considered

### A single age horizon for all classes

Rejected. Safe for audit classes, wrong for event-sourced state (ADR-046, ADR-106),
provenance sources (ADR-041), and accounting bases (ADR-103, future quota work) — and
wrong silently, since deletion of load-bearing rows presents as data loss long after the
prune.

### Delete-only retention, no archive

Rejected. It destroys information the plane exists to keep, forecloses re-projection
(ADR-041) and late audit, and makes the append-only claim of ADR-004/162 false rather
than consciously amended. The marginal cost of copy-then-verify over delete is the
archive media itself, which is the cheapest component in the system.

### Export without pruning

Considered, and preserved as the default state: with no horizon configured, this decision
changes nothing. Rejected as the end state because it does not bound the live store,
which is the operational pressure motivating retention.

### Transparent federation of live and archived rows on the query surface

Rejected. Making `list` read sealed segments transparently reintroduces the unbounded
query population retention exists to bound, couples caller-facing availability to archive
media, and hides the horizon instead of disclosing it. An explicit discriminant and a
typed stub answer keep the surface honest at bounded cost; an operator-side tool reading
segments directly needs no runtime surface at all.

### Per-namespace retention policy

Deferred. Namespace is attribution, not an authorization or lifecycle boundary (ADR-007);
a per-namespace horizon would make retention a per-tenant contract before any decision
establishes tenancy semantics for the plane. The per-class dimension composes with such a
decision later without being blocked on it.
