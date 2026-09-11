# Atomic Plan — Prepared Write-Plan Shapes

`atomic_plan.rs` (ADR-099, cross-op atomicity for bulk apply) defines the prepared write-plan
*shapes* consumed by the atomic runner (`docs/api/atomic_runner.md`): one family per admissible
verb group. This document covers why plans are structured the way they are — the design
rationale behind guard placement is not obvious from the type shape alone.

## Why guards are per-statement, not per-plan

A guard is attached to the exact `PlanStatement` it validates, never to the plan as a whole:
affected-row counts come back per-statement or as a batch total, so a plan-level guard field
could not tell a runner which statement's count it is checking. Each plan therefore carries
`Vec<PlanStatement>` (or, for `merge`, the split `rewires`/`lifecycle` fields), and the runner
applies each statement individually, checking any present guard against that statement's own
affected-row count before moving to the next. The typed GTD same-status no-op is a read
assertion: the runner uses the same transaction's writer connection to execute its `SELECT`
and checks its result-row count instead. It requires one exact snapshot match without an
`UPDATE`, so successful no-ops do not advance note versions. A failed read assertion rolls
back the whole unit just like a failed DML guard (ADR-099, 2026-09-09 amendment).

## Inherited Note Embedding

A content-changing note update with no explicit `embed` flag carries a potential
reindex and a private inheritance marker. Immediately before its DML, the writer
checks scoped membership across persisted virtual `vec_*` tables, including
retired models. Vector publication can leave note.version unchanged, so a
prepare-time check would miss vectors published while the update waits to write.
An unembedded note resolves to a mutation notification only. The runner collects
the resolved effect after successful savepoint release and exposes it for
execution only when the whole unit commits. Explicit on/off behavior is unchanged.
