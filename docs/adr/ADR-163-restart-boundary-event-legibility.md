# ADR-163: Restart-Boundary Event Legibility

- **Status:** Proposed
- **Date:** 2026-08-16
- **Extends:** ADR-162 (answers a question the ownership skeleton raises and does not close; ADR-162 is not replaced)
- **Depends on:** ADR-142, ADR-162, ADR-022

## Context

ADR-142 already makes a host restart legible **at the process record**: the boot scan
transitions every non-terminal record to `terminal` with reason `host_restart` before
serving new requests, and `terminal_reason` is a persisted field returned by
`agent.observe`. A post-restart reader of the agent table sees a terminated row, not a
dangling in-flight one. That contract is not in question here and is not amended.

ADR-162 makes the runtime-owned event substrate the single authoritative record of what
happened, fixes attribution at the dispatch seam, and requires every event class to declare
a write posture. It does not say what the plane emits when the boot scan terminates
records — and today it emits nothing.

The consequence is that the plane, unlike the table, cannot answer the question it exists to
answer. A reader following a run on the plane sees its in-flight events and then silence.
Silence there is ambiguous across at least three different histories: the work is still
running, the work died mid-round without a terminal transition, or the work was terminated
by a boot scan. Those demand different responses — wait, investigate, re-spawn — and the
plane offers no way to tell them apart. A reader who wants the answer must leave the plane
and consult the table, which is exactly the "one authoritative plane" property ADR-162
establishes being spent at the first hard question asked of it.

This is not a hypothetical reading failure. A restart-boundary artifact was read as evidence
of live work in this system's own operation: the reader saw a record of a round with no
terminal entry and concluded the round was in flight, when the process behind it had been
gone since the previous boot. The record's own timestamps, compared against boot time,
settled it — but only for a reader who already suspected the answer. The general reader has
no reason to suspect anything, which is what makes silence the wrong encoding for a state
that the runtime knows precisely.

The same argument the substrate already applies to retrieval applies here: an incomplete
answer must be distinguishable from a complete one, on the result, without inference. ADR-162
Open section B states that requirement for serving. This ADR states its temporal counterpart:
across a restart boundary, "went quiet" and "terminated at restart" must be distinguishable
on the plane itself.

## Decision

### 1. The boot scan emits a terminal event per terminated record

For every record the ADR-142 boot scan transitions to `terminal` with reason `host_restart`,
the runtime emits one terminal event on the plane, carrying the record's identity, the
terminal reason, and the boot-boundary identifier from §2. Emission happens as part of the
scan, before the runtime serves new requests, so a reader who observes any post-restart
activity necessarily observes a plane that already accounts for the previous generation's
in-flight records.

**Attribution: the runtime is the principal, the record is the subject.** ADR-162 §2 binds an
event's attributed principal to the principal the runtime resolved _for the work that caused
it_, and states that for process-lifecycle events the process identity is a subject rather
than an authenticated actor. The operation here is the boot scan. No owner dispatch caused
it, and `owner_actor` did nothing: stamping that actor as the principal would make a
per-actor view of the plane count operations the actor never performed.

These events therefore attribute to the runtime's own system actor. That is an instance of
§2's rule rather than an extension of it: §2 resolves a dispatched actor where a dispatch
caused the work and a runtime principal where the runtime's own work did, and the boot scan
is the second kind. What this ADR adds is the specific principal for this specific case:
**for a runtime-initiated operation — one the runtime performs on its own behalf rather than
in service of a caller's dispatch — the attributed principal is the runtime's system actor.**
The boot scan is the first restart-boundary operation to reach the plane; the rule is written
generally because it will not be the last.

The record's identity rides as subject data, never as the principal: `agent_id`,
`owner_actor` (as the terminated work's owner, so a per-owner reconstruction still finds it),
and the lineage context ADR-161 fixes, so a reader rebuilding a terminated tree from events
alone sees a forest terminate rather than a set of unrelated records.

### 2. The boot boundary is a pair of events, not one

The plane is append-only, so a single boundary event cannot both precede the per-record
events and report how many of them were written. The scan therefore frames its work with two
events sharing one boot identifier:

- an **opening** event, emitted before any per-record event, carrying the boot identifier,
  the scan's start time, and the count of non-terminal records the scan found;
- a **closing** event, emitted after the per-record events and before the runtime serves new
  requests, carrying the count terminated and the count of terminal events successfully
  emitted.

Three properties follow, and each is a reading the plane could not previously support:

- **A reader can bound the generation.** Every event before the opening belongs to a previous
  generation of the process; every event after it belongs to this one. Without the pair, a
  reader comparing timestamps must infer the restart from a gap, and a gap is not evidence —
  a quiet system produces identical gaps.
- **A reader can detect an incomplete accounting.** The opening's found-count is comparable
  against the closing's emitted-count. When they disagree, the plane says so rather than
  reading as a complete accounting of a smaller set. This is the complete-versus-incomplete
  discriminant the substrate applies to retrieval, applied to a boot generation.
- **A reader can detect an interrupted scan.** An opening with no closing means the scan
  itself did not finish — a state that, with a single boundary event in either position, is
  indistinguishable from a scan that found nothing or from one whose boundary was never
  written. The pair makes the scan's own liveness legible on the same terms it makes the
  records' liveness legible, which is the property this ADR is about.

### 3. Write posture: decoupled from the transition, never silent

Per ADR-162 §4, this event class declares its posture explicitly.

The record transition is the authority; the event is legibility. A failed event append
therefore does **not** fail or roll back the ADR-142 transition — the record is terminated
regardless, and coupling the transition to the plane's write availability would make a
logging outage able to leave live-looking records behind, which is the failure this ADR
exists to prevent, inverted.

But a failed append is not permitted to be silent, because silence is precisely the encoding
this ADR removes. When per-record emission fails, the failure surfaces in the closing event's
accounting: the opening declares what the scan found, the closing declares what it terminated
and what it managed to emit, and a reader comparing them sees an incomplete generation rather
than a complete-looking one. When a boundary event itself cannot be written, the runtime logs
the failure at the host and emits a degraded marker at the next successful plane write; a
generation missing either half of its pair reads as unknown, never as clean. Note the
asymmetry this produces and why it is the right one: a missing closing event degrades a
generation to unknown, while a missing opening leaves per-record terminal events that are
individually still legible — the pair fails toward less information, never toward false
confidence.

### 4. Scope

This ADR governs the runtime-owned plane and the records the ADR-142 boot scan owns. Work
records maintained by layers above the runtime — an orchestration engine's run journal, a
review tool's per-pass artifact — are outside it. ADR-162 §1 already settles their standing
two ways: a layer's view of work the runtime did see is a convenience mirror, and work that
never reached the runtime produces nothing on the plane to be authoritative about. Either
way the legibility of such a record is its owner's contract, not the kernel's. That boundary is deliberate and is the reason this ADR does not attempt a general
"every log must be restart-legible" rule it has no authority to enforce.

## Non-goals

- **No change to ADR-142's restart boundary.** Termination on restart, no silent resume, and
  continuation-as-fresh-spawn are all unchanged; this ADR only makes the termination visible
  on the plane as well as the table.
- **No resumption semantics.** Nothing here lets a reader restart, reattach, or inherit
  authority from a terminated record.
- **No new query surface.** The events are read through ADR-022's existing surface.
- **No retention policy.** How long boundary events live is the plane's retention question,
  not this ADR's.

## Consequences

- Silence on the plane stops being ambiguous across a restart: a reader distinguishes
  terminated-at-restart from still-running from died-without-terminal using events alone.
- The plane keeps the property ADR-162 gives it. A reader who must consult the table to
  interpret the plane has a plane that is authoritative in name only; this closes the first
  and most common case where that happened.
- Boot generations become bounded and countable, so an incomplete accounting is visible as
  incomplete — the temporal analogue of the retrieval discriminant.
- The cost is bounded and proportional: one event per terminated record plus two per boot
  (the opening and closing of §2), emitted on a path that already walks every one of those
  records.

## Alternatives considered

### Leave it to the table

Rejected. It is already true that the table answers this, and it is exactly why the gap is
easy to miss: the contract looks satisfied when read from the kernel's side. The reader who
gets it wrong is the one following the plane, and telling that reader to consult a different
surface concedes that the plane is not authoritative for "what happened."

### Infer the restart from a timestamp gap

Rejected. A gap is not evidence — an idle system produces identical gaps, and the inference
requires the reader to already know boot time and to suspect a restart. An encoding that only
works for a reader who has guessed the answer is not an encoding.

### Couple the transition to the event write

Rejected. It would let an event-store outage block or reverse termination, leaving records
that look live after their processes are gone — the exact defect inverted. §3 keeps the
transition authoritative and makes the plane's own gaps self-declaring instead.

### Emit only the boundary events, without per-record events

Rejected. The boundary pair alone tells a reader that a restart happened but not which records
died in it, so a reader following one record still sees unexplained silence. The per-record
events are what make the individual history legible; the boundary pair is what makes the set
countable.

### A single boundary event instead of an opening/closing pair

Rejected on append-only mechanics. A single event placed before the per-record emissions
cannot report how many of them succeeded, since they have not happened; placed after them, it
cannot bound the generation for a reader who arrives mid-scan, and an interrupted scan becomes
indistinguishable from one that found nothing. Either single-event placement forces the
implementer to silently drop one of this ADR's two required readings (§2). The pair costs one
additional event per boot.
