# ADR-114: Code-Audit Derived Report, Not Agent Findings

**Status**: Accepted

**Date**: 2026-07-16

## Context

khive's code-map database (ADR-085 Amendment 2, `code.ingest`) records L1 manifest
dependencies and L1.5 regex-derived import edges for a source tree. Once that map exists, the
natural next question is what to build on top of it: a pipeline that reads structural facts
(fan-in, layering, cycles, duplicate content) and turns them into something a maintainer can
act on.

Two shapes were considered for that pipeline:

1. Agents read the map (and, eventually, git history), form judgments about what is wrong with
   the code, and write `finding` notes — or free-form judgments — into a graph.
2. A deterministic process reads the map and emits a versioned report artifact: a fixed set of
   named signals, each carrying an observed value, an evidence trail, and an explicit status
   describing how much the signal can be trusted.

Shape 1 was rejected. A structural proxy — zero observed import in-edges, a duplicate content
hash, an edge whose direction crosses a policy-declared layer boundary — is evidence, not a
verdict. The map is known to be incomplete (regex-based import resolution has documented
false-negative gaps; no per-file history exists yet), so an agent asked to "find defects" from
these facts either overstates confidence or silently launders an absence into a claim. Treating
the pipeline's output as an agent's judgment call also collides with the existing `finding` note
kind (ADR-085 Amendment 3), whose only writer is the reviewed `kkernel code-ingest` CLI path
ingesting a human-approved `findings.json` sweep — not an automated structural pass.

## Decision

The code-audit pipeline emits a derived, deterministic **report artifact**, not agent findings.

- The report is produced by `kkernel code-audit`, a read-only admin command over a dedicated
  code-map database. It never writes to any graph (production or map), never creates `finding`
  notes, and never calls `memory.remember`.
- Every signal in the report carries a `status` of `observed`, `candidate`, or `unavailable` —
  never a bare zero or a boolean verdict. `observed` means the query result is a definite fact
  given the current schema (a layering-policy violation, a manifest/import mismatch, a graph
  cycle). `candidate` means the signal is a structural proxy with known false-negative or
  false-positive gaps (zero-in-edge modules, duplicate content hashes) — absence of evidence is
  not evidence of absence. `unavailable` means the underlying facts do not exist yet in the
  phase-1 schema (churn, dead-file, and orphan-test signals, which need per-file history).
- The words **defect** and **finding** are reserved for a human-approved interpretation step
  that happens outside this pipeline. A maintainer (or a future, explicitly reviewed process)
  reads the report and decides whether a signal represents a real problem worth acting on; that
  decision, if it produces a `finding` note, still goes through the existing
  `kkernel code-ingest` path — this pipeline does not gain a shortcut into that surface.
- No component in this pipeline creates `finding` notes, writes agent judgments into either
  graph, or presents a signal as a defect. This is a scope fence, not an implementation detail:
  a future phase that adds file/history facts (ADR-085/088 amendment, tracked separately) may
  upgrade some `unavailable` signals to `observed` or `candidate`, but the report-vs-findings
  boundary itself does not move without a further ADR.

## Consequences

- The report is safe to run on a schedule or in CI without a review gate, because it cannot
  mutate state and its output is not itself a claim of correctness.
- Consumers (maintainers, or a future summarization step) must do their own interpretation work;
  the pipeline does not save them that step, by design.
- Every signal declaring its own status makes the report's confidence legible at read time,
  instead of requiring a reader to know which facts the pipeline had available when it ran.
- A later phase that wants to present something as a defect needs an explicit, separately
  reviewed path — this ADR does not define one, and implementations must not invent one as a
  shortcut.

## Alternatives Considered

- **Agents interpret signals inline and write `finding` notes.** Rejected: conflates a
  structural proxy with a reviewed judgment, and bypasses the existing reviewed ingest path for
  `finding` notes (ADR-085 Amendment 3).
- **Persist every signal as a graph note regardless of interpretation.** Rejected: the report is
  a view artifact recomputed from source facts, not a durable record that needs graph
  provenance; persisting it as notes would also invite exactly the interpretation drift this
  ADR is meant to prevent (see khive's data-vs-view principle: query results are not something
  to write back as if they were independently observed data).
