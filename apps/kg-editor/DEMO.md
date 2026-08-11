# DB-backed repository analysis demo

This walkthrough demonstrates the repository showcase as an investigation tool,
not a generic dashboard. The example values below come from the khive snapshot
at `288b9eee1f471d505e7f5de33e121c58773a10af`. When using a newer snapshot,
narrate the values and coverage displayed by the UI.

## Prepare the materialized snapshot

The browser does not open SQLite or trigger repository ingestion. An operator
first runs the `khive repo build` pipeline, which joins the history and code-map
databases into one immutable, schema-validated `khive.repo.v1` report. Configure
the allowlisted server route:

```bash
KHIVE_SHOWCASE_ANALYSIS_ROOT=/absolute/path/to/analyses \
KHIVE_SHOWCASE_ANALYSIS_IDS=khive \
npm run dev
```

Place the report at `$KHIVE_SHOWCASE_ANALYSIS_ROOT/khive/khive.repo.v1.json`.
Before presenting, verify that the source badge says **khive DB snapshot**, not
**curated static fallback**.

This setup requires the DB snapshot API route from the companion backend PR (or
its merged equivalent). Without that route, this UI branch intentionally uses
the curated static fallback.

## Five-to-seven-minute flow

### 1. Establish provenance and limits

1. Open `/`, leave `https://github.com/ohdearquant/khive` in the repository
   field, and select **Open repository**.
2. Point to the DB-snapshot badge, pinned SHA, ingestion time, and exporter
   identity.
3. Point to the overview: this snapshot contains 45 packages, 683 modules, 990
   commits, and seven captured dependency SCCs.

Say:

> This is a reproducible analysis of one pinned revision, built from khive's
> history and code-map databases. The browser cannot clone a repository or ask
> the server to ingest an arbitrary URL. Every list carries its own completeness
> and bound.

Explain **Observed** versus **Candidate**. An SCC is an observed graph fact;
hotspot, ownership, and co-change ranks are candidates for human verification.
Also point out the source-role disclosure: the current import scan does not
distinguish production, test, example, and generated modules.

### 2. Start at the writer/storage seam

1. Search for `crates/khive-db/src/pool.rs` and inspect it.
2. Show fan-in 20, fan-out 2, 26 captured commits, and bus factor 1.
3. In **Dependency topology**, follow the SCC member link to
   `crates/khive-db/src/writer_task.rs`.
4. Show its fan-in 9, fan-out 2, 21 captured commits, and the same two-member
   SCC.

Say:

> The tool did more than rank a large file. It found a bidirectional ownership
> boundary: the pool creates and stores the writer task, while the writer task
> asks the pool for a connection and shared counters. That is a concrete
> consolidation candidate—either co-locate the owner or extract a lower-level
> writer-admission contract—without claiming there is a runtime deadlock.

Keep the scope caveat visible: six of `pool.rs`'s 20 captured dependents are
test modules, so 20 is not a production-only fan-in.

### 3. Show an architectural insight that argues against consolidation

Search for `crates/khive-db/src/checkpoint.rs`. It has 38 captured commits and
co-change evidence across DB, runtime, and MCP. Source and ADR inspection
explains why this control plane must remain physically independent of the writer
queue: a dedicated checkpoint connection prevents checkpoint I/O from consuming
writer admission. The useful consolidation target is the runtime-facing
control/metrics facade, not SQLite connection ownership.

### 4. Move to the runtime coordination knot

1. Search for `crates/khive-runtime/src/operations.rs`.
2. Show 90 captured commits, fan-in 4, fan-out 7, and its SCC with `pack.rs` and
   `curation.rs`.
3. Point to the history disclosure: the total is 90 while the inspector shows a
   recent sample from the captured page.
4. Open **Dependency topology** and show SCC membership without implying an edge
   order.

Say:

> Change history and dependency topology converge on the same boundary. The safe
> first extraction is the shared endpoint and resolution policy into a
> dependency-lower contract, followed by the split plans already documented in
> these source files.

### 5. Use a false positive to demonstrate trustworthiness

Open the top **Hidden coupling** candidate. In this snapshot it is
`khive-pack-comm/tests/integration.rs` with `src/handlers.rs`, at 40 co-changes.
The global analysis contains only the top 1,000 of 104,798 candidate pairs.

Say:

> Co-change is not a call edge or an instruction to merge files. This top pair
> is healthy test-to-implementation coordination. The visible path and
> source-role caveat let us falsify the scary interpretation instead of turning
> a rank into a defect.

If time permits, repeat the exercise with the MCP `coordinator.rs`/`server.rs`
SCC. Its reverse edge comes from a `cfg(test)` import, while the production
coordinator seam is one-way.

### 6. Close with one confirmed contract drift

Use repository search to inspect `operations.rs`, `runtime.rs`, and `daemon.rs`
as separate modules, then follow the displayed source paths into the runtime
manifest and imports. These are not direct `pool.rs` dependents in the captured
import graph. The source follow-up is the proof: accepted ADR-005 says
runtime-facing code depends on `khive-storage` traits, but the current runtime
manifest and modules directly expose `khive-db`, `ConnectionPool`, SQLite
planning, checkpoint, and diagnostics types.

Say:

> This is not a claimed runtime failure; it is a confirmed accepted-architecture
> drift. The owner must either amend ADR-005 with a bootstrap/admin/atomic
> SQLite exception or promote these capabilities into the storage contract.

## Two falsifiable consolidation hypotheses

1. **Writer ownership contract:** remove the `pool.rs`/`writer_task.rs` SCC by
   extracting neutral admission/connection state or co-locating lifecycle
   ownership. Preserve queue, strict-routing, WAL, checkpoint, and contention
   tests; re-ingest and require the SCC to disappear without changing
   single-writer semantics.
2. **Runtime endpoint contract:** extract shared endpoint, relation, and
   resolution policy below `operations.rs`, `curation.rs`, and `pack.rs`.
   Preserve pack registration, endpoint-conformance, curation, and public API
   tests; re-ingest and require a one-way dependency graph rather than merely
   fewer lines in one file.

## Closing line

> The UI does not tell me that code is bad. It turns 683 modules into a
> reproducible investigation: provenance, rank, topology, history, bounds, and
> the exact paths needed to test an architecture hypothesis. Dogfooding also
> exposed where the analysis itself must improve—especially production/test
> source-role classification—so the tool is falsifiable rather than decorative.

## Q&A caveats

- The demo snapshot is pinned; it is not the current working tree.
- Code ingestion in this report covers Rust.
- Bus factor describes captured commit-author identities, not present-day
  staffing.
- Hidden-coupling support is co-change frequency in the declared window, not
  causality.
- The browser receives validated JSON transport bytes. “DB-backed” describes how
  the immutable analysis is produced and selected, not live browser-side SQLite
  access.
