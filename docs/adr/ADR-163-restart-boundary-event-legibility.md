# ADR-163: Restart-Boundary Event Legibility

- **Status:** Proposed
- **Date:** 2026-08-16
- **Succeeds:** ADR-162 (open question raised, not closed, by the ownership skeleton)
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

The event's attribution follows ADR-162 §2 unchanged: it attributes to the terminated
record's own `owner_actor`, because the event is a fact about that record's work, not about
the operator or the boot process. Lineage context accompanies it per ADR-161 §5, so a reader
reconstructing a terminated tree from events alone sees the whole forest terminate rather
than a set of unrelated records.

### 2. The boot boundary is itself an event

The scan emits one boot-boundary event before the per-record events, carrying a boot
identifier, the scan's start time, and the count of non-terminal records the scan found.
Two properties follow, and both are the point:

- **A reader can bound the generation.** Every event before the boundary belongs to a
  previous generation of the process; every event after it belongs to this one. Without the
  boundary, a reader comparing timestamps must infer the restart from a gap, and a gap is
  not evidence — a quiet system produces gaps too.
- **A reader can detect an incomplete accounting.** The declared count is comparable against
  the terminal events that follow it. When they disagree, the plane says so rather than
  reading as a complete accounting of a smaller set. This is the same complete-versus-
  incomplete discriminant the substrate applies to retrieval, applied to a boot generation.

### 3. Write posture: decoupled from the transition, never silent

Per ADR-162 §4, this event class declares its posture explicitly.

The record transition is the authority; the event is legibility. A failed event append
therefore does **not** fail or roll back the ADR-142 transition — the record is terminated
regardless, and coupling the transition to the plane's write availability would make a
logging outage able to leave live-looking records behind, which is the failure this ADR
exists to prevent, inverted.

But a failed append is not permitted to be silent, because silence is precisely the encoding
this ADR removes. When per-record emission fails, the runtime records the failure in the
boot-boundary event's own accounting — the boundary states how many records the scan
terminated and how many terminal events it successfully emitted — so a reader comparing the
two sees an incomplete generation rather than a complete-looking one. When the boundary event
itself cannot be written, the runtime emits a degraded marker at the next successful plane
write and logs the failure at the host; a generation whose boundary is unknown must read as
unknown.

### 4. Scope

This ADR governs the runtime-owned plane and the records the ADR-142 boot scan owns. Work
records maintained by layers above the runtime — an orchestration engine's run journal, a
review tool's per-round artifact — are outside it: ADR-162 §1 already classes those as
convenience mirrors, and the legibility of a mirror is its owner's contract, not the
kernel's. That boundary is deliberate and is the reason this ADR does not attempt a general
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
- The cost is bounded and proportional: one event per terminated record plus one per boot,
  emitted on a path that already walks every one of those records.

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

### Emit only the boundary event, without per-record events

Rejected. The boundary alone tells a reader that a restart happened but not which records
died in it, so a reader following one record still sees unexplained silence. The per-record
events are what make the individual history legible; the boundary is what makes the set
countable.
