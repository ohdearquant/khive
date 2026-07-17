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
affected-row count before moving to the next.
