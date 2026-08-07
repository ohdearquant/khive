# ADR-147: Repository Showcase Bundle — `khive.repo.v1`

**Status**: proposed\
**Date**: 2026-08-07\
**Authors**: khive maintainers

## Context

The git pack ingests repository history (`git.digest`: commits, issues, pull requests as
note kinds with lifecycle and precedence edges) and the code pack ingests codebase
structure (`code.ingest`, under a 22-rule edge vocabulary). Together they place a
repository's history and structure in one typed graph. Nothing yet consumes that fusion
as a product surface.

The code pack's vocabulary spans two tiers, and only one of them is emitted today.
`code.ingest` currently produces the **module tier**: projects, packages, and modules
with their dependency edges. The **symbol tier** (function and datatype entities) is
declared in the pack vocabulary but not yet produced by the ingest call. This ADR is
therefore specified at module granularity throughout, and every symbol-tier view is
marked as such where it appears.

The nearest consumer pattern already shipped: [ADR-145](ADR-145-local-first-kg-workbench.md)
proved that a versioned, strictly validated JSON bundle with a shared golden vector lets
a Rust producer and a TypeScript UI evolve against one contract. This ADR applies the
same discipline to a read-only repository showcase: enter a public repository, see its
structure, history, and their fusion rendered as an interactive graph and charts.

The showcase is deliberately static-first. Precomputed bundles for a curated set of
repositories exercise the full ingest pipeline with zero write surface, no service
dependencies, and cacheable output. On-demand ingest of arbitrary repositories is a
later slice with its own operational design.

## Decision

### D1 — One versioned bundle, produced offline

The unit of the showcase is a `khive.repo.v1` JSON bundle produced by a one-shot
pipeline:

```text
clone <public repo> → git.digest (history) → code.ingest (structure)
                    → export one khive.repo.v1 bundle
```

The bundle is the only interface between the pipeline and any renderer. CLI, CI, and
the browser consume identical bytes. The contract ships as a JSON Schema
(`docs/schemas/khive-repo-v1.schema.json`) with at least one golden vector produced
from a real repository, and the TypeScript consumer validates with a closed wire model
(strict object schemas), both properties carried over from ADR-145 as requirements,
not suggestions.

### D2 — Bundle shape

Top-level sections, each independently bounded:

| Section      | Contents                                                                                                                                                                                                                                                                                                   |
| ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `meta`       | Repository identity (owner, name, default branch), ingest provenance: HEAD commit SHA, ingest timestamp, producing tool versions, and an explicit truncation disclosure per bounded section                                                                                                                |
| `graph`      | The entity/edge slice at module granularity: project, packages, modules; contains and depends_on edges; history notes (commits, issues, pull requests) and their linking edges, bounded and paged. Symbol-tier node types (function, datatype) are typed in the schema and carry an empty collection in v1 |
| `aggregates` | Precomputed, chart-ready data (D3); every aggregate names its time window and its truncation rule                                                                                                                                                                                                          |
| `capability` | Static declaration: read-only, no writes, no live queries; renderers drive labeling from these fields, never from hardcoded strings                                                                                                                                                                        |

Absence is encoded, never invented: a repository with no issues carries an empty issue
series with its own disclosure, not a fabricated zero-filled chart.

### D3 — Chart and analysis contract

The differentiator is the typed graph, so the showcase leads with graph views and backs
them with graph-derived analyses. Generic forge statistics are included only where they
are cheap and expected.

Every view below is tagged with the ingest tier it consumes. **Module** views are fully
specified and rendered in v1. **Module (symbol-tier drill-down deferred)** views render
in v1 at module granularity and gain a deeper level when symbol-tier ingest lands, with
no schema break, because the graph section already types its nodes.

**Graph views** (rendered from `graph`):

1. **Structure graph** _(module; symbol-tier drill-down deferred)_ — project →
   packages → modules with typed edges, zoom and filter by subtree, node degree
   available for sizing. Drill-down below a module activates with the symbol tier.
2. **History-structure navigation** _(module)_ — selecting a module surfaces the
   commits, pull requests, and issues that touched it; selecting a commit highlights
   the subgraph it touched. Both directions come from the linking edges in the bundle,
   top-N per entity with full counts disclosed.
3. **Dependency topology** _(module)_ — module-level depends_on adjacency with cycle
   detection (cycles precomputed and listed), fan-in and fan-out per module.

**Analyses** (rendered from `aggregates`):

4. **Hotspot quadrant** _(module)_ — per module: change frequency (commits touching it,
   by window) against fan-in. The high-churn, high-fan-in quadrant is the risk surface.
5. **Hidden coupling** _(module)_ — top-N module pairs that co-change in the same
   commits but share no structural edge, with co-change count and support window.
6. **Structure treemap** _(module; symbol-tier sizing deferred)_ — module tree sized by
   contained source-file count from the manifest tier, colored by recent activity.
   Sizing by contained symbol count activates with the symbol tier.
7. **Cadence timeline** _(history only)_ — commit counts per week, release tags,
   pull-request lead-time percentiles, issue open/close series.
8. **Ownership** _(module)_ — per-module author concentration and a bus-factor
   indicator derived from commit authorship.
9. **De-facto API surface** _(module; symbol-tier ranking deferred)_ — modules ranked
   by dependent count, which is the module-granularity reading of "what the rest of the
   repository actually relies on." Ranking individual functions and datatypes activates
   with the symbol tier.
10. **Scorecard** _(module)_ — a derived header: repository age, package and module
    counts, activity trend, top hotspots, dependency-cycle count, ownership warnings.
    A symbol count is reported as unavailable in v1 rather than as zero, per the
    absence rule in D2.

Each analysis in the schema names its input fields, window, bound, and tier; a renderer
must be able to draw every chart from the bundle alone, offline. Analyses that would
require semantic search or live queries are out of contract for v1.

### D4 — Static-first ladder

| Slice | Scope                                                                                                                                                                                                        |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 1     | Curated showcase: bundles for a small set of public repositories, built offline, shipped as static assets; the site resolves an entered repository URL against this set                                      |
| 2     | On-demand ingest of arbitrary public repositories: a service with queueing, sandboxing, resource limits, and abuse controls; separate design, not constrained here beyond consuming the same bundle contract |

Slice 1 has no server-side execution triggered by user input. The input box is a lookup
against precomputed bundles, and a miss renders an honest "not in the showcase set yet"
state.

### D5 — Honesty constraints carried from the ingest layer

- `code.ingest` map output is traversable but not text-searchable today; the bundle
  contract does not depend on full-text search anywhere.
- Every bounded listing in the bundle discloses its own truncation, and every
  aggregate names the window it was computed over. A chart whose input was truncated
  says so in the bundle, so the renderer can label it.
- Ingest provenance pins the HEAD commit SHA; a bundle is a statement about one
  commit, not about "the repository."

## Non-Goals

- No LLM-generated narratives or summaries in v1; every number in the bundle is
  mechanically derived and reproducible from the repository at the pinned SHA.
- No write surface of any kind, and no live graph queries from the browser.
- Not the review lane: ADR-146 (forge-native KG review, in flight) covers KG review;
  this ADR shares rendering components with it but binds none of its decisions.
- No forge API dependency in slice 1 beyond what `git.digest` already ingests from the
  cloned repository.

## Alternatives Considered

| Alternative                               | Why rejected                                                                                                                                                                                        |
| ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| On-demand ingest first                    | Requires a service, sandboxing, and abuse controls before any user sees value; static bundles demo the same pipeline with none of that surface.                                                     |
| Live graph queries against a hosted store | Couples the demo to store uptime and query latency; a static bundle is cacheable, shareable, and fails nowhere.                                                                                     |
| Generic repository statistics dashboard   | Forges already render contributor and activity charts natively; without the graph views this adds nothing distinctive.                                                                              |
| Reusing `khive.review.v1` with a new kind | The review bundle models a proposed change between two states; the showcase models one state plus history aggregates. Forcing both into one schema couples two contracts that evolve independently. |

## Consequences

### Positive

- The full ingest pipeline (clone, history digest, structure ingest, export) gets a
  product consumer and therefore a regression surface: golden bundles fail loudly when
  ingest output drifts.
- The frontend investment (graph rendering, entity cards, chart components) transfers
  directly to the review workbench lane.
- Zero-risk public demo: read-only, static, reproducible from a pinned SHA.

### Negative

- Showcase freshness is manual until slice 2: bundles age with their pinned SHA and
  must be re-produced to advance.
- The one-shot pipeline command does not exist yet; composing clone, digest, ingest,
  and export into one entry point is new (thin) implementation work.

### Neutral

- Bundle size bounds (top-N per section, paging) will need tuning against real
  repositories; the golden vector fixes the contract, not the bounds, which are
  schema-visible limits.

## Implementation Status

`git.digest` and `code.ingest` are live verbs. Two measured facts about them shape the
contract, and both are load-bearing rather than caveats.

The first is the tier fact stated in Context and carried through the normative text:
`code.ingest` today emits the manifest and import-scan tiers (projects, packages and
modules with their dependency edges) and not the symbol tier. Every place this ADR
would otherwise have specified symbols is scoped in situ — the D2 `graph` row types
symbol node types and gives them an empty collection in v1, and D3 tags each of the ten
views with the tier it consumes, three of which (#1, #6, #9) name what activates when
the symbol tier lands. That activation needs no schema break, because the graph section
already types its nodes.

The second is that `code.ingest` writes to a dedicated map database while `git.digest`
writes to a graph store, so the bundle exporter reads two stores and merges at export
time.

A first end-to-end run of the pipeline against this repository has been executed against
a dedicated store, which fixes two operational properties the exporter has to carry: the
history digest is cursor-bounded and returns a done flag, so a caller loops it to
exhaustion rather than issuing one call, and the two verbs land in separate databases as
described above.

To build: the bundle exporter, the one-shot pipeline entry point, the JSON Schema plus
golden vector produced from a real repository, and the frontend. The first golden
bundle target is this repository itself.

## References

- [ADR-085](ADR-085-code-pack.md): Code pack — structure ingestion
- [ADR-101](ADR-101-kg-changeset-model.md): Change-set model (contrast: D3 alternative)
- [ADR-145](ADR-145-local-first-kg-workbench.md): Local-first KG workbench — the
  bundle-contract discipline this ADR reuses
- ADR-146: Forge-native KG review (proposed, in flight) — sibling lane
  sharing rendering components
