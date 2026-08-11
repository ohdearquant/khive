# ADR-147: Repository Showcase Bundle — `khive.repo.v1`

**Status**: accepted\
**Date**: 2026-08-07\
**Authors**: khive maintainers

## Context

The git pack ingests repository history (`git.digest`: commits, issues, pull requests as
note kinds with lifecycle and precedence edges) and the code pack ingests codebase
structure (`code.ingest`, under a 22-rule edge vocabulary). They write separate graph
databases with independently minted identifiers. The exporter is the first place those
stores are reconciled into one typed graph and consumed as a product surface.

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

| Section      | Contents                                                                                                                                                                                                                                                                                                                                                                                    |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `meta`       | Repository identity (owner, name, explicitly supplied default branch or an unavailable reason), ingest provenance: HEAD commit SHA, ingest timestamp, producing tool versions, and an explicit truncation disclosure per bounded section                                                                                                                                                    |
| `graph`      | The entity/edge slice at module granularity: project, packages, modules; contains and depends_on edges; history notes (commits, issues, pull requests) and the exporter-derived commit-to-module linking edges (D5), each typed as derived rather than ingested, bounded and paged. Symbol-tier node types (function, datatype) are typed in the schema and carry an empty collection in v1 |
| `aggregates` | Precomputed, chart-ready data (D3); every aggregate names its time window and its truncation rule                                                                                                                                                                                                                                                                                           |
| `capability` | Static declaration: read-only, no writes, no live queries; renderers drive labeling from these fields, never from hardcoded strings                                                                                                                                                                                                                                                         |

Absence is encoded, never invented: a repository with no issues carries an empty issue
series with its own disclosure, not a fabricated zero-filled chart.

### D3 — Chart and analysis contract

The differentiator is the typed graph, so the showcase leads with graph views and backs
them with graph-derived analyses. Generic forge statistics are included only where they
are cheap and expected.

Every view below is tagged with what it consumes, along two independent axes.

The first axis is granularity. **Repository** views consume repository-wide history and
do not pretend to be module analyses. **Module** views are fully specified and rendered
in v1. **Module (symbol-tier drill-down deferred)** views render in v1 at module
granularity and gain a deeper level when symbol-tier ingest lands, with no schema break,
because the graph section already types its nodes.

The second axis is the history-structure join, and it is the one that decides whether a
view can exist at all. `git.digest` records each commit's changed paths, but neither
ingest emits a commit-to-module edge. Views tagged **join** therefore depend on linkage
that the exporter derives from those paths and the code map's pinned `source_path`
facts, per D5; views tagged **history only** or **structure only** need no join and are
computable from one store alone.

**Graph views** (rendered from `graph`):

1. **Structure graph** _(module; structure only; symbol-tier drill-down deferred)_ — project →
   packages → modules with typed edges, zoom and filter by subtree, node degree
   available for sizing. Drill-down below a module activates with the symbol tier.
2. **History-structure navigation** _(module; join)_ — selecting a module surfaces the
   commits that touched it; selecting a commit highlights the subgraph it touched.
   Pull-request and issue facets are available only when the ingested history contains
   an explicit evidence chain to a linked commit; otherwise each facet is marked
   unavailable rather than inferred from text or repository membership. Both directions
   read exporter-derived linking edges (D5), top-N per entity with full counts disclosed.
3. **Dependency topology** _(module; structure only)_ — module-level depends_on adjacency with cycle
   detection (cycles precomputed and listed), fan-in and fan-out per module.

**Analyses** (rendered from `aggregates`):

4. **Hotspot quadrant** _(module; join)_ — per module: change frequency (commits touching it,
   by window) against fan-in. The high-churn, high-fan-in quadrant is the risk surface.
5. **Hidden coupling** _(module; join)_ — top-N module pairs that co-change in the same
   commits but share no structural edge, with co-change count and support window.
6. **Structure treemap** _(module; structure only, activity coloring is a field-level join;
   symbol-tier sizing deferred)_ — module tree sized by contained source-file count from
   the manifest tier. Recent-activity color is emitted only when the join is available;
   otherwise that field is explicitly unavailable and the structure-only treemap still
   renders. Sizing by contained symbol count activates with the symbol tier.
7. **Cadence timeline** _(repository; history only)_ — commit counts per week, release tags,
   pull-request lead-time percentiles, issue open/close series. Each series owns its
   availability and bound; in particular, tag data omitted for a reproducible build is
   unavailable rather than an empty release history.
8. **Ownership** _(module; join)_ — per-module author concentration and a bus-factor
   indicator derived from commit authorship. Repository-wide author concentration
   needs no join and is reported in v1 regardless.
9. **De-facto API surface** _(module; structure only; symbol-tier ranking deferred)_ — modules ranked
   by dependent count, which is the module-granularity reading of "what the rest of the
   repository actually relies on." Ranking individual functions and datatypes activates
   with the symbol tier.
10. **Scorecard** _(field-tagged — each field carries its own granularity and join tag)_ — a derived header: repository age, package and module
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

### D5 — The history-structure join is the exporter's job

The fusion the showcase sells does not exist in either store. `git.digest` persists
repository-relative `changed_paths` on commit notes, while `code.ingest` persists
repository-relative `source_path` plus `source_revision` on module entities in its own
database. Their identifiers are minted independently, and no ingested edge relates a
commit to a module. The exporter therefore derives that edge while reconciling both
identifier spaces into deterministic bundle identifiers.

So two things have to happen at export time, and the exporter owns both:

1. **Commit-to-path linkage.** Consume the durable `changed_paths` facts from
   `git.digest`. The pinned clone is the verification and fallback source when a legacy
   commit note lacks that field; fallback use is disclosed in edge provenance.
2. **Path-to-module resolution and cross-store identity.** Resolve a changed path against
   the current module whose `source_revision` equals the bundle HEAD and whose
   `source_path` is an exact match. Reconcile both stores into deterministic natural
   bundle identifiers; never publish either store's random row identifier as wire
   identity.

The resulting linking edges are a **bundle-level construct**, not a claim that either
pack emits them. They are typed and disclosed as derived, so a reader can tell which
edges came from an ingest verb and which the exporter computed.

Two consequences worth stating rather than discovering later. Resolution is imperfect —
a changed path may be outside the declared language capability, may name a deleted file,
or may map to no current module. The bundle separates those classes and reports residual
paths and unreached module identities instead of dropping them silently, because a join
that quietly discards its misses inflates every metric built on it. The bundle shape does
not change when path provenance changes; only the derivation recorded on the edge does.

An alternative worth naming: teaching `git.digest` to emit path edges natively would
serve every consumer, not just this one, and is the better long-term home. It is not a
prerequisite here because the showcase pipeline holds the clone anyway, and blocking a
product surface on a pack enhancement trades a shippable slice for a dependency.

### D6 — Honesty constraints carried from the ingest layer

- `code.ingest` indexes every accepted project and module into the map database's
  full-text store. The static bundle nevertheless has no live-query dependency:
  search is neither required by the export contract nor exposed by the slice-1
  renderer.
- Every bounded listing in the bundle discloses its own truncation, and every
  aggregate names the window it was computed over. A chart whose input was truncated
  says so in the bundle, so the renderer can label it.
- Ingest provenance pins the HEAD commit SHA; a bundle is a statement about one
  commit, not about "the repository." Mutable forge issue and pull-request state is a
  separately timestamped observation and is never claimed to be frozen by that SHA.
  Tags and a forge's default-branch pointer are mutable too: a byte-reproducible build
  either omits tags and supplies the intended default-branch label explicitly, or
  records those fields as separately observed inputs. Neither is inferred from a
  mutable remote ref while claiming pinned-SHA reproducibility.

## Non-Goals

- No LLM-generated narratives or summaries in v1; every number in the bundle is
  mechanically derived and reproducible from the complete declared input set. The
  pinned SHA freezes commit and tree data; any included forge or tag observation is
  named separately and is not represented as SHA-frozen state.
- No write surface of any kind, and no live graph queries from the browser.
- Not the review lane: ADR-146 (forge-native KG review, in flight) covers KG review;
  this ADR shares rendering components with it but binds none of its decisions.
- No forge API dependency in slice 1 beyond what `git.digest` already ingests. Legacy
  changed-path fallback is read from the pinned clone. Release tags are an optional,
  explicitly selected clone-ref observation and are omitted from the reproducible
  golden vector.

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
- A full static bundle is large enough that producer bounds, compact serialization,
  and client-side byte limits are part of the product contract rather than optional
  optimizations.

### Neutral

- Bundle size bounds (top-N per section, paging) will need tuning against real
  repositories; the golden vector fixes the contract, not the bounds, which are
  schema-visible limits.

## Implementation Status

Implemented in the open-source repository:

- `khive repo build` composes a bounded public clone, an exhaustive cursor loop over
  `git.digest`, Rust-only `code.ingest` into a separate map database, and atomic export.
  `khive repo export` exposes the read-only two-store export seam independently.
- `khive-repo-showcase` owns the closed Rust model, deterministic natural identities,
  derived history-structure join, precomputed analyses, canonical serialization, and
  validation. The checked JSON Schema and the TypeScript closed model validate the same
  golden bytes.
- The slice-1 Next.js renderer resolves only curated repository URLs to same-origin
  static assets. It implements all ten module/repository views, renders per-section
  availability and truncation, and preserves the review workbench at `/review`.

The first golden target is this repository at
`c2979d2443738a075e55a170c772d1dc86cf0f91`. The producer exhausted 938 commits,
ingested 658 Rust modules across 43 package/project nodes, and resolved all 658 current
Rust source paths to module entities. Historical coverage records 7,558 changed-path
events: 4,344 Rust paths were in scope, 4,309 matched the current map, 3,214 non-Rust
events were out of scope, and 35 historical Rust events were unresolved and named in
the bundle. Functions, datatypes, and interfaces remain typed empty pages; the four
forge cadence series are unavailable in the commits-only reproducible golden rather
than represented as zero.

The implementation confirmed the two-store and cursor-bounded properties above and
also corrected assumptions that did not survive contact with current producers:
`git.digest` already persists changed paths, `code.ingest` already persists exact
repository-relative source paths and indexes accepted entities for text search, and
the producer's Rust module-path rules differ from the earlier illustrative table for
`src/main.rs`, nested `mod.rs`, and `build.rs`.

## References

- [D5 join feasibility exhibit](D5-JOIN-FEASIBILITY.md)
- [`khive.repo.v1` JSON Schema](../schemas/khive-repo-v1.schema.json)
- [Repository showcase CLI](../../crates/kkernel/docs/repo-showcase.md)

- [ADR-085](ADR-085-code-pack.md): Code pack — structure ingestion
- [ADR-101](ADR-101-kg-changeset-model.md): Change-set model (contrast: D3 alternative)
- [ADR-145](ADR-145-local-first-kg-workbench.md): Local-first KG workbench — the
  bundle-contract discipline this ADR reuses
- ADR-146: Forge-native KG review (proposed, in flight) — sibling lane
  sharing rendering components

## Amendment 1 — Operator-configured DB snapshot delivery (2026-08-11)

The static golden remains the portable contract fixture and the public, zero-service
fallback. A local operator may additionally serve a completed repository analysis from
a server-private materialization when the purpose is to inspect a repository already
registered on that machine.

This mode does not turn slice 1 into on-demand ingest:

1. The operator runs `khive repo build` out of band. Each successful run owns a fresh,
   dedicated history database, code-map database, pinned checkout, and canonical
   `khive.repo.v1` report. A failed or incomplete run is never promoted.
2. The Next.js server accepts only a closed, configured analysis ID. Browser input never
   supplies a repository URL, filesystem path, database path, executable, or argument.
   Looking up an unknown ID performs no filesystem or process work.
3. Request handling performs no clone, ingest, export, SQLite open, or child-process
   execution. It reads the completed server-private report, enforces the browser byte
   ceiling, validates the closed wire model, and returns the exact bounded report.
4. Analysis roots and their parents are operator-owned, server-private, and not writable
   by untrusted local principals. A promoted analysis directory is immutable. The reader
   refuses symlink components, verifies canonical containment both before and after open,
   and compares the opened handle's file identity with the final path before reading.
   Run directories, database paths, validation failures, and SQLite details remain
   server-private. Responses use stable sanitized errors and are marked
   `private, no-store`.
5. A UI that selects this source calls it a **DB-backed snapshot** and names its pinned
   SHA and generation time. It must not call it live: the two stores were reconciled at
   build time and are immutable for that analysis ID until the operator promotes another
   successful run.

Here, SQLite is the analysis source of truth and JSON is the bounded read model between
the Rust exporter and renderer. This defines how a later UI adapter can remove a
checked-in browser asset from its active path without duplicating the cross-store join
in TypeScript. The server boundary can land independently while the static adapter
remains the default. This amendment does not authorize request-time `repo export`,
arbitrary-URL ingestion, or direct browser/Next.js access to SQLite. Those remain slice
2 and require the queueing, sandboxing, resource-limit, and abuse-control design in D4.
