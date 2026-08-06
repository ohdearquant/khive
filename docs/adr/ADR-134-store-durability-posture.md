# ADR-134: Store durability posture, and the obligation it carries for accounting records

- **Status:** Proposed
- **Date:** 2026-07-29
- **Relates to:** ADR-103 (resource attribution — routes accounting through the per-dispatch audit
  row), ADR-133 (write-path handling of that row), ADR-091 (WAL/checkpoint behaviour)

## Context

### What was found

The store runs `synchronous=NORMAL` under WAL. It is set in the pool
(`crates/khive-db/src/pool.rs:762`, `:787`, `:1102`) and again per store
(`crates/khive-db/src/stores/note.rs:211`, `entity.rs:183`, `vectors.rs:250`). Every setting found
was `NORMAL`; no `FULL` was found, which is a search result and not a proof of absence.

Under WAL, `synchronous=NORMAL` means a commit returns **without** fsyncing the WAL. The two
failure classes differ and must not be collapsed:

- A **process crash** loses nothing. The data is in the OS page cache and survives.
- An **OS crash or power loss** can lose recent _committed_ transactions.

That is a legitimate and common trade. This record does not exist because the value is wrong. It
exists because the value was never decided, and because of what now sits on top of it.

### Why it matters now

ADR-103 establishes that accounting rides the per-dispatch audit row, and that the usage object
lands under that row's `resource` payload, read by four consumers including accounting and quota.

So the records that determine accounted usage outcomes are held in a store that can lose recent
commits on power loss. Nothing in either record connected those two facts: a reader of ADR-103 would
not learn the durability posture of the store it depends on, and a reader of the pool configuration
would not learn that accounting rides on it.

### How it became visible

Store instances can legitimately run at different levels — one at `NORMAL`, a peer at `FULL`.
The divergence between two postures is what turns this setting from scenery into a choice.
A value nobody compares is invisible.

### The distinction this record must not blur

**Write-path handling and store durability are separate properties, and satisfying one says nothing
about the other.**

ADR-133 governs how the write path treats a record: whether it can be dropped, deferred past its
operation's return, or reported as successful without committing. This record governs whether the
store, having committed, can still lose it. A record handled perfectly by the write path, on a store
configured to lose recent commits, is still exposed.

Both have to hold. Neither closes the other.

## Decision

### D1 — `synchronous=NORMAL` is the recorded posture, with its loss window stated

`NORMAL` is adopted as an explicit decision rather than inherited as a default. Its loss window,
stated so that no downstream reader has to derive it:

> A plain process crash loses nothing. An OS crash or power loss can lose transactions committed
> since the last WAL sync, including audit rows and therefore including the accounting payloads
> they carry.

The **size** of that window is set by checkpoint cadence, which ADR-091 owns. A reader converting
"commits since the last WAL sync" into an interval should take the cadence and its counters from
there rather than deriving them here, so the two records cannot drift into stating different
windows. This record fixes the posture; ADR-091 fixes how long the exposure lasts.

This costs no throughput and closes the part of the exposure that was genuinely alarming, which was
that nobody had decided it.

### D2 — The target posture is a forced durable sync on the accounting path specifically

The intended end state is not store-wide `FULL`. It is `NORMAL` generally, with the accounting path
forced to a durable sync through the same primitive the store already uses to select a posture:
`PRAGMA synchronous=FULL`, scoped to the connection that commits the accounting-bearing row rather
than applied to the pool as a whole. `synchronous` is a per-connection setting — SQLite does not
require every connection open on a database to share one value — so a connection dedicated to
accounting-bearing commits can pay the fsync on every commit it makes while every other connection
(note, entity, vector, and non-accounting audit writes) keeps paying nothing beyond `NORMAL`.

That is the trade-off this target buys against store-wide `FULL`: the fsync cost lands only on the
commits that need the guarantee, not on the store's full write volume. It is still a real,
per-commit cost on that one connection, and it is that cost — not the store-wide one — that D3's
second number prices.

This is a target, not an implementation, and D3 gates it.

### D3 — The posture change is gated on measurement, not on argument

Two numbers are required before any posture change:

1. Throughput delta between `NORMAL` and `FULL` on a **file-backed** store at a **stated
   concurrency level**.
2. The cost of a forced durable sync on the accounting path alone.

Both must be measured on a file-backed store. An in-memory backend cannot exercise this at all, and
a single-writer number does not transfer, because contention is the whole question.

**"`FULL` is too slow" is currently an assumption and not a number.** It is recorded here as an
assumption so that it cannot later be cited as a finding.

### D4 — Accounting durability is gated on exposure, not on a date

Before the system produces an accounting record any consumer depends on for a resource-usage
outcome, the accounting path is durably synced — by store-wide `FULL` or by D2's targeted sync,
whichever the D3 numbers favour.

The condition is tied to the arrival of accounted usage rather than to a release or a calendar date,
because the exposure begins when a record starts determining a resource-usage outcome and not before.

**Prerequisite.** This decision is conditioned on ADR-133 INV-1 holding in the implementation, not
merely in the record. A durable sync protects a committed row against loss after commit; it says
nothing about whether that row was produced exactly once before the sync ran. A sync applied to a
row ADR-133's write path could still duplicate makes the duplicate exactly as durable as the
original. D4 is not satisfied by adding the sync alone — ADR-133 INV-1 must hold first, or the sync
is protecting the wrong property.

### D5 — The loss direction is recorded, because it is why the interim window is tolerable

The exposure is asymmetric, and the asymmetry is load-bearing for D1:

- Audit-row loss produces a usage **undercount**, so the direction of error is under-accounting —
  the safer direction for whoever the record concerns.
- Quota reads over a partial history fail toward **over-serving**. That is abuse-relevant, not
  safety-relevant, while no consumer yet depends on the record.

Neither observation makes loss acceptable once usage is accounted, and D4 is where that changes. They
are recorded because a durability decision without a stated loss direction invites a later reader to
assume the worst case in both directions, or the best.

**The favourable direction is conditional, and the condition is not this record's to keep.** "Errs
toward under-accounting" holds only while the sole failure mode is loss. A write path that retries
after an ambiguous commit outcome can persist the same accounting payload twice, which errs
_against_ the party the record concerns and, unlike an undercount, is not detectable from the
accounting records afterwards — a duplicate is indistinguishable from a second genuine dispatch
unless record identity was established when the row was produced.

So D5's reasoning depends on an exactly-once guarantee held elsewhere (ADR-133 INV-1). If that
guarantee weakens, D5's justification for tolerating the interim window weakens with it, and D1
should be revisited rather than left standing on a premise that moved. A record whose tolerance
argument rests on another record's invariant must name that dependency, or the argument outlives the
thing supporting it.

## Invariants

- **INV-1.** The durability posture of any store holding accounting-, authorization-, or
  security-audit-bearing records is a recorded decision carrying its loss window, never an inherited
  default. A posture nobody chose is a defect regardless of which value it holds.
- **INV-2.** Store durability and write-path handling are asserted separately. No record, test, or
  review may treat a guarantee about one as evidence about the other.
- **INV-3.** Once any consumer depends on an accounting record for a resource-usage outcome, that
  record is durably synced at commit.

## Consequences

**Intended.** The posture becomes legible: a reader of ADR-103 can now find out what happens to an
accounting row under power loss, and a reader of the pool configuration can find out what depends on
it. The interim window is bounded by an explicit condition rather than by nobody having noticed.

**Accepted, for the interim.** Between this record and D4's condition, an OS crash or power loss can
lose recent accounting rows. Under D5 the direction favors the party the record concerns, which is why the interim
is tolerable and not why it is fine.

**Cost of the eventual change.** An fsync per commit on the accounting path at minimum. D3 exists so
that cost is priced before it is chosen, and so that the cheaper targeted option is compared against
the blunt one rather than assumed better.

**Not addressed here.** Cross-process write coordination. SQLite has no cross-process commit
coordinator, so any future record introducing a single writer process must carry its own durability
decision explicitly: the acknowledgement crosses a process boundary, and ack-after-fsync becomes a
property that writer implements and its clients cannot verify. That decision must be stated in that
record rather than inherited from this one.

## Acceptance

1. The posture is recorded with its loss window expressed in terms of **which failure class loses
   what** — process crash and OS crash/power loss stated separately, never merged into "a crash".
2. A measured throughput comparison between the current setting and `FULL`, on a file-backed store,
   at a stated concurrency level, so D2 is chosen against numbers.
3. A measured cost for the targeted accounting-path sync, so the two options are compared rather
   than one being assumed cheaper.
4. If the accounting path is treated differently from the general path, a test asserts the
   accounting write is durable under the configured posture — with a fixture that configures it
   wrongly on purpose and must make the test fail. A durability test that passes against a
   misconfigured store is measuring nothing.
5. A check that fails when a store holding INV-1 records has an unrecorded posture, so this class is
   caught by construction rather than by an ad-hoc configuration comparison.

## Alternatives considered

**Leave `NORMAL` unrecorded, since it is a common default.** Rejected. The defect was never the
value; it was that no decision existed, and an unrecorded posture stays invisible until something
forces a comparison. An unrecorded default under an accounting record is a hole regardless of which
value sits in it.

**Move to store-wide `FULL` immediately.** Rejected for now, and deliberately not rejected on the
grounds that it is too slow, since that is currently an assumption. It is deferred until D3 prices
it, because it cuts directly against the concurrency work and a blunt change made without numbers
would be as unexamined as the default it replaces.

**Treat this as part of the write-path record.** Rejected. Merging them is exactly the conflation
INV-2 forbids, and a single record would let a reader take a write-path guarantee as a durability
guarantee.
