# ADR-187: Pin the comm seeks' index instead of hoping the planner picks it

- **Status**: Proposed
- **Date**: 2026-09-13
- **Relates to**: [ADR-186](ADR-186-note-listing-order-index.md) (the note listing order index, which
  is deferred behind this record)

## Context

The comm read path compiles its own SQL. A `NoteFilter` carrying `PropertyFilter`s is turned into a
statement by the filter compiler, and which index serves that statement is left to SQLite's planner.

Two of those seeks have a selectivity guarantee that callers depend on. The recipient seek is served
by `idx_notes_message_recipient_direction`, and the unread seek by
`idx_notes_unread_probe_recipient_direction`, whose partial `WHERE` clause is written to match the
exact predicate the unread filter compiles to. The comment on that index states the guarantee plainly:
work is proportional to the caller's own unread inbound set, never to other actors' backlog, never to
total mailbox size, and never to the recipient's own outbound send history.

That guarantee currently rests on a cost estimate. The planner compares candidates, and what it
compares changes whenever the schema changes.

### What was measured

Adding one unrelated index, `(namespace, created_at DESC, id ASC)` partial on `deleted_at IS NULL`,
flipped the unread seek to

```
SEARCH notes USING INDEX idx_notes_namespace_created (namespace=?)
```

on three of the six plan arms in `comm_filter_plan_tests`: the fresh-bootstrap arm, the fresh half of
the recreated-unread arm, and the arm whose whole purpose is that unread work stays bounded as other
mailboxes grow. The pre-analyzed-upgrade arm passed, so the boundary is whether `sqlite_stat1` exists.
A kind-bearing variant of the same index, sharing the comm indexes' `(namespace, kind)` prefix, fails
identically.

The consequence of the flip is not a slower query. It is the loss of the guarantee: an unread listing
for a recipient with nothing unread would walk the namespace's whole message history newest-first.

### What already exists

This repository already pins an index where the guarantee matters. `PROBE_SQL` in the comm pack names
`idx_notes_unread_probe_recipient_direction` and `idx_comm_message_to_actor` with `INDEXED BY`, in
hand-written SQL. So the mechanism, the risk it carries, and the decision that the risk is worth
taking are all precedent here. What is missing is that the compiled filter path does not do the same
thing, which is why a hand-written statement is stable and the compiled one is not.

## Decision

**The compiled comm seeks carry `INDEXED BY` for the index whose guarantee they depend on.**

A pin is emitted only when the compiled predicate matches the index it names: the equality terms the
index's key columns expect, in the spelling the filter compiler emits, and a `WHERE` clause that
implies the index's partial predicate. When the shape does not match, no pin is emitted and the
planner chooses as it does today. A wrong pin is worse than no pin, so the condition is narrow and
stated in code beside the emitter.

`INDEXED BY` is a constraint rather than a hint: SQLite refuses the statement if the named index
cannot serve it. That is the property that makes it useful here, since a silent fallback is exactly
the failure this record is about, and it is the same exposure `PROBE_SQL` already accepts. The pinned
indexes are declared in the fresh-store DDL and created by migration, so a store that can run these
verbs has them.

A store that does not have them is broken, and this record says so rather than routing around it: a
missing pinned index is a store-integrity error, surfaced loudly with the index named, never a
fallback to an unpinned plan. Migration runs before any verb, so mid-upgrade is not a state in which
these seeks execute.

## Alternatives considered

- **Leave it to the planner.** The status quo, and the measurement above says it breaks the guarantee
  the moment an unrelated index is added. It also fails quietly: the query still returns correct rows.
- **Guarantee statistics instead.** Run `ANALYZE` on a schedule so the planner always has
  `sqlite_stat1`. Rejected: it makes a maintenance task a correctness precondition, and the failing
  window is exactly the store that has not analyzed yet.
- **Never add another namespace-leading index.** This is what the repository does today by accident.
  It is not a rule anyone can follow, because the constraint is invisible from the place a new index
  is added.
- **Pin with `+` prefixes on the competing terms** to make the other index unusable rather than naming
  the wanted one. It expresses the same intent indirectly and is harder to read at the call site.

## Consequences

- A schema change can now fail loudly instead of quietly: dropping or renaming a pinned index turns
  the seek into an error rather than a slow query. That is the intended direction, and the acceptance
  includes an arm that names it.
- The filter compiler gains a rule that couples it to specific index names. That coupling already
  exists implicitly, in the comments on the indexes and in the plan tests; this makes it explicit.
- ADR-186's listing index becomes addable. It is deferred behind this record precisely because the
  planner, not the index, is the problem.

## Acceptance

- The two index-name arms, on both store shapes. With the comm indexes present, the inbox plan names
  `idx_notes_message_recipient_direction` and the unread plan names
  `idx_notes_unread_probe_recipient_direction`, asserted by index name, on the fresh-bootstrap store
  and on the pre-analyzed-upgrade store as separate arms, because catalog order differs between them.
- **The arm that closes the class**: with `idx_notes_namespace_created` from ADR-186 added to the test
  schema, both plans are unchanged. This is the arm that makes the listing index safe to land, and it
  fails today.
- A no-pin arm: a filter whose shape does not match the pinned index emits no `INDEXED BY` and still
  returns the right rows. Without it, an emitter that pins unconditionally passes everything else.
- A rows arm: for each pinned seek, the ids returned with the pin equal the ids returned without it, on
  a fixture where a wrong index would return a different page. A pin that changes the answer is a
  defect, not an optimization.
- **A pinned index missing from the schema is a store-integrity error, surfaced loudly, and never a
  fallback to an unpinned plan.** The pinned indexes are created by migration before any verb runs, so
  a store executing these seeks without them is broken and has to say so; mid-upgrade is not a state
  in which these seeks execute. The arm drops the index and asserts the seek returns that error with
  the index name in it. A silent fallback here would reintroduce exactly the quiet loss of the
  guarantee this record exists to remove.
- **The match condition is asserted against the compiler's own emitted predicate text**, by feeding
  the emitted SQL back to the matcher, rather than against a spelling written by hand in the test. A
  hand-written spelling makes the arm pass while a compiler change quietly un-pins every seek, which
  is the same silent-loss shape one layer down.
