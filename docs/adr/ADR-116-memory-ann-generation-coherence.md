# ADR-116: Durable Per-Model Generation Coherence for the Memory ANN Warm Path

**Status**: Proposed\
**Date**: 2026-07-17\
**Scope**: memory-pack ANN freshness across processes, persisted memory-ANN snapshots, and the
SQLite write/migration contract\
**Decision owner**: khive maintainers\
**References**: issue #752; ADR-062; ADR-079; ADR-107

## Status and relationship to prior decisions

This MUST be a **new ADR**, not an amendment buried in ADR-062.

ADR-062 decided FTS/ANN key consolidation and the model-global memory index key. It did not define
cross-process cache coherence. More importantly, accepted ADR-107 now defines the memory ANN's
eventual-consistency contract, a global durable epoch, five-second debounced checks, and stale
serving. Those are the contracts this decision changes. Amending only ADR-062 would leave two
accepted, mutually incompatible authorities.

This ADR therefore:

- **retains** ADR-062's one-memory-index-per-model and
  `global::memory_vamana::{model}` snapshot key;
- **corrects** ADR-062's legacy-snapshot purge predicate;
- **does not amend** ADR-079, whose persisted-segment decision is explicitly knowledge-pack scoped;
- **supersedes** ADR-107's memory-ANN stale-serving/freshness provisions and its global
  `memory_ann_epoch` protocol, while retaining its single-flight, RAII guard-release, bounded
  rebuild, hydration, and cold-start mechanics where they do not conflict here.

While this ADR is Proposed, ADR-107 remains the accepted contract. Acceptance of this ADR MUST
include, in the same change, an amendment to ADR-107 adding a forward link that marks its
memory-ANN freshness provisions superseded by ADR-116, so the corpus never carries two accepted,
competing authorities.

## Teardown: refute first

### “Only two external writer paths matter, so bump only those”

This survives only under today's deployment convention, not under issue #752's stated invariant.
Explicit bumps in `kkernel exec --atomic` and `kkernel reindex` would cover the two known exposures
and avoid a durable write on routine daemon mutations. It is a coherent lower-cost design if khive
is willing to declare that no other process may write a shared database.

Khive has no enforceable single-writer-process identity at the storage boundary, however. A second
`KhiveRuntime`, a new admin command, or a future direct store consumer is indistinguishable from the
daemon to the vector store. Its correctly fired in-process hook still cannot reach a resident
daemon. Selective bumps would therefore replace a data invariant with a caller-registration
convention and would not cover issue #752's third affected path (“any other external process”).

**Verdict**: reject known-path-only bumps. Every committed mutation that changes a model's ANN input
MUST advance that model's durable generation in the mutation's SQLite transaction. Where the
mutation's own code performs the advance (the vector write sites below), the added write MAY be
coalesced to once per affected model per transaction/batch. The note-liveness triggers advance once
per qualifying row instead, because SQLite has no statement-level triggers. Both are correct: the
contract is equality-based, any strict advance invalidates, and the durable cost is bounded per
commit, not per increment.

### “A durable write per note is too expensive”

The concern is real: SQLite serializes writers, and repeatedly updating a small counter set can make
those rows/WAL pages hot. A naive implementation that increments once for every row in
`insert_batch` is rejected.

The bounded design adds one small counter DML operation per affected model per committed write
transaction, after successful row savepoints are known. A batch of 1,000 note-vector replacements
for one model advances once, not 1,000 times. Single-note traffic still pays one counter DML per
model, but it already pays embedding and vector DELETE+INSERT work. Avoiding that DML is not worth
making coherence depend on which executable happened to perform the write.

The implementation MUST benchmark write amplification, but a slow result is a reason to coalesce
or improve the DML, not to restore an incomplete invalidation scheme.

### “Count plus dimensions is a freshness fingerprint”

Refuted. `compute_memory_fingerprint` is only `(COUNT(*), dimensions)`
(`crates/khive-pack-memory/src/ann.rs:937-969`). A same-cardinality, same-model re-embed changes every
embedding while preserving both values. It is a cheap sanity prefilter, never a freshness proof.

Exact generation equality closes that hole **if and only if** every mutation obeys the transactional
bump invariant. Current `main` also persists a BLAKE3 hash over the ordered graph-input rows
(`ann.rs:1071-1080`). Keep that hash as an independent corruption/bypass detector; it is not a
substitute for the generation because it is only recomputed during snapshot restore.

### “The current durable epoch already fixes #752”

Refuted for this decision's required semantics. Current `main` has a global `memory_ann_epoch`, a
five-second debounce, and a read helper that maps acquisition/query/missing-row failures to zero
(`ann.rs:134-220`). The recall path searches the installed graph even when freshness is false
(`handlers/common.rs:923-933`). ADR-107 explicitly accepts up to five seconds of post-reindex stale
serving and arbitrary rebuild-time rank staleness.

That is a valid availability trade, but it is not issue #752's settled fail-closed, per-model,
every-warm-hit contract. This ADR chooses the latter.

### Corpus check: prior decisions the packet missed

The khive flywheel surfaced ADR-107 and its implementation in PR #812. It also surfaced the prior
lesson that count/dimensions cannot certify content and that one-shot guards must release on every
task exit. The new mechanism preserves ADR-107's guard/rebuild work but replaces its global epoch,
debounce, and stale-serving policy. The existing ordered content hash is retained rather than
duplicated.

## Decision

Use a **durable, per-model SQLite generation** as the sole authority that may certify an installed
memory ANN graph as current.

For model `m`, define:

- `Gdb(m)`: the generation in `memory_ann_generations`;
- `Gann(m)`: the generation stamped on an installed `AnnBridge`;
- `Gsnap(m)`: the generation in a persisted memory snapshot envelope.

An installed graph MAY be searched only when a fresh database read in the current recall observes:

```text
Gann(m) == Gdb(m)
```

A persisted snapshot MAY be restored only when:

```text
snapshot_format_version is supported
AND Gsnap(m) == Gdb(m)
AND cheap fingerprint matches
AND ordered content hash matches
```

The generation equality is authoritative. Fingerprint and content hash are additional rejection
gates.

The recall's coherence linearization point is its successful durable-generation read. A mutation
that commits before that read MUST be observed. A mutation that commits after it is concurrent with
that recall; the next recall reads the newer generation and MUST NOT use the older graph. This ADR
does not claim serializability between an ANN search and a concurrently committing writer.

## Mechanism comparison

| Mechanism                              | Safety and convergence                                                                                                                                     |                                                   Hot-read cost | Write/operational cost                                                                                                                | Decision                                                                                                                                  |
| -------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------: | ------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Durable per-model generation in SQLite | Survives restart and process boundaries; exact equality gives a structural oracle; transaction rollback rolls back the signal                              |    One fresh point-read set per recall, batchable across models | One small DML per affected model per committed transaction/batch; schema + write-site audit                                           | **Chosen**                                                                                                                                |
| Daemon notification channel            | Fast eager eviction while the daemon is reachable; requires durable catch-up state to cover loss, restart, offline writers, and notify-before-commit races | Potentially zero DB reads after a proven subscription watermark | New IPC protocol, acknowledgements, replay/watermark, daemon discovery, and failure recovery                                          | Rejected: a notification channel without a durable watermark is lossy; adding one recreates the chosen mechanism with more moving parts   |
| Startup/periodic revalidation          | Startup validation catches restart staleness; strong polling can eventually catch external writers                                                         |                                         No read on most recalls | Staleness window equals poll interval; count/dims is unsound; strong hash is O(N); independently sampled fingerprints can race builds | Rejected: cannot provide “next recall” coherence without polling on every recall, at which point a generation row is cheaper and stronger |

In-process hook eviction remains an eager latency optimization. It MAY schedule a rebuild or remove
an installed bridge immediately after commit. It MUST NOT certify freshness, advance an independent
authoritative counter, or let a caller skip the durable read.

## Schema and forward migration

Add `crates/khive-db/sql/011-memory-ann-generations.sql`:

```sql
CREATE TABLE IF NOT EXISTS memory_ann_generations (
    model      TEXT PRIMARY KEY,
    generation INTEGER NOT NULL DEFAULT 0
        CHECK (generation >= 0)
);
```

Register it in `crates/khive-db/src/migrations.rs` as `V11_UP` with a new
`VersionedMigration { version: 11, name: "memory_ann_generations", ... }`. DDL MUST live in the SQL
file and be pulled with `include_str!`. V1 MUST NOT be edited. `scripts/lint-sql.sh` MUST pass.

The generations table and the note triggers are core `khive-db` schema rather than memory-pack
schema deliberately: the triggers attach to the core `notes` table and must hold for every
deployment that can mutate notes, regardless of which packs are loaded. This is a scoped exception
to pack-scoped schema ownership (ADR-028), recorded here so schema authority stays unambiguous.

The same V11 file defines these note-liveness triggers after the table:

```sql
CREATE TRIGGER memory_ann_notes_liveness_au
AFTER UPDATE OF deleted_at, namespace ON notes
WHEN (OLD.deleted_at IS NULL) <> (NEW.deleted_at IS NULL)
  OR (OLD.deleted_at IS NULL AND NEW.deleted_at IS NULL
      AND OLD.namespace IS NOT NEW.namespace)
BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1 FROM memory_ann_generations
        WHERE generation = 9223372036854775807
    ) THEN RAISE(ABORT, 'memory ANN generation overflow') END;
    UPDATE memory_ann_generations SET generation = generation + 1;
END;

CREATE TRIGGER memory_ann_notes_liveness_ad
AFTER DELETE ON notes
WHEN OLD.deleted_at IS NULL
BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1 FROM memory_ann_generations
        WHERE generation = 9223372036854775807
    ) THEN RAISE(ABORT, 'memory ANN generation overflow') END;
    UPDATE memory_ann_generations SET generation = generation + 1;
END;
```

Equivalent SQL is acceptable only if the names, event columns, `WHEN` semantics, overflow abort,
and transaction behavior remain identical.

There is deliberately no INSERT trigger on `notes`, and its soundness depends on an ordering
invariant that MUST be enforced, not assumed. A note row without a vector is not ANN graph input,
and the vector insert performs the model-specific bump — but if a `kind='note'` vector could be
committed before its note row exists, a later live-note INSERT would adopt that orphan vector into
the JOIN without any generation advance, and `Gann == Gdb` would certify a graph missing it.

No trigger can close this on the vector table: every `vec_{model_key}` table is a SQLite `vec0`
virtual table (`crates/khive-db/src/backend.rs:398`), and SQLite rejects `CREATE TRIGGER` on
virtual tables. The parent-before-vector invariant is therefore enforced at the vector-write
chokepoint in `khive-db`: every helper that inserts a `kind='note'` vector row (the site-1 and
site-2 paths below) MUST verify, inside the same transaction or savepoint as the insert, that a
`notes` row for `subject_id` exists (live or soft-deleted), and MUST fail the vector write
otherwise. With parent-before-vector enforced at every helper path, a live-note INSERT can never
make a pre-existing vector ANN-eligible: a vector inserted while its note is soft-deleted is
excluded by the liveness JOIN, and a later un-delete changes `deleted_at` nullness, which fires
the liveness UPDATE trigger. Entity-kind vector rows are not constrained by the check.

Direct `vec_*` DML that bypasses the helpers is outside this guard's mechanical reach — an
inherent limitation of `vec0`, not a design choice. Such a writer is already bound by the site-6
contract to carry coupled generation DML, and a writer violating that contract defeats generation
coherence directly, guard or no guard, so the guard's enforcement scope matches the trust boundary
the vector-side contract already has. The direct-DML inventory oracle below turns a new bypass
into a test failure rather than a silent gap.

The check-then-insert sequence is race-free under SQLite's write serialization, and this ADR
relies on that property explicitly rather than assuming it: the parent-liveness SELECT and the
vector INSERT execute inside one write transaction, SQLite permits exactly one writer at a time
(WAL-mode readers never interleave writes), so no concurrent connection can delete the note row
between the check and the insert. A note deleted after the vector's transaction commits is the
ordinary liveness case: the deletion fires the note-liveness trigger and stales the graph.

Generation values use SQLite's non-negative signed 64-bit range. Increment at the maximum is a
write error; the enclosing corpus mutation MUST roll back rather than wrap or reset.

Model-specific vector DML uses insert-or-increment with an overflow guard, and treats an affected
row count other than one as an error:

```sql
INSERT INTO memory_ann_generations(model, generation) VALUES (?1, 1)
ON CONFLICT(model) DO UPDATE SET generation = generation + 1
WHERE generation < 9223372036854775807;
```

The existing pack-owned `memory_ann_epoch` is not migrated into the new table: it is global and
cannot be losslessly split by model. It MAY remain as an unused compatibility table for one release;
new code MUST neither read nor write it. Dropping it is a later cleanup migration, not part of this
correctness cut.

Model rows are lazy. Before a cold build or snapshot restore, `ensure_ann_for_model` MUST execute an
idempotent `INSERT ... ON CONFLICT DO NOTHING` for the model outside the warm-hit path, then read the
row. Vector mutation DML also uses insert-or-increment, so a newly introduced model cannot miss its
first mutation. A missing row on the warm-hit path is an error, not generation zero.

## Transaction-coupled mutation contract

The invariant is:

```text
commit(changes ANN input for model m) => commit(advance Gdb(m)) in the same transaction
rollback(corpus mutation)             => no change to Gdb(m)
failure(advance Gdb(m))               => rollback corpus mutation
```

“ANN input” is the actual graph-input predicate in `ann.rs`: vector rows with
`kind='note'`, `field='note.content'`, the target embedding model, and a joined live note row. It is
not limited to notes whose note-kind is `memory`; current graph construction filters note substrate
vectors and applies memory-kind/visibility filtering after hydration.

### Required vector write sites

1. **Single insert/update** — `replace_vector_row_dml` / `vec_upsert_atomic_dml`, reached by
   `SqliteVecStore::insert` and `update` (`vectors.rs:325-379`, `503-535`, `648-747`, `812-907`).
   Membership MUST be computed from both sides of the replacement: `replace_vector_row_dml`
   deletes every row for `(subject_id, namespace)` in the model table before inserting the new
   `(kind, field)` row (`vectors.rs:342-377`), so a replacement whose inserted row is not
   `note.content` can still remove a prior `note.content` row from the graph input. Advance the
   row named by `embedding_model`, before releasing/committing the enclosing
   savepoint/transaction, when the inserted postimage is ANN input or the deleted preimage
   contained ANN input. Implementations MAY establish the preimage with a pre-delete existence
   check or `DELETE ... RETURNING`, and MAY conservatively advance on every note-substrate
   replacement for the model instead.

2. **Batch insert/reindex** — `batch_insert_vectors_dml` (`vectors.rs:402-492`). Track whether at
   least one record's committed savepoint changed ANN input under the site-1 preimage/postimage
   rule. Advance the model exactly once after the loop when `affected > 0`; a generation failure
   rolls back the outer batch transaction. Failed record savepoints do not independently advance
   it.

3. **Single/bulk delete** — `SqliteVecStore::delete` and `delete_subjects`
   (`vectors.rs:909-918`, `1085-1117`). Determine whether relevant rows were actually deleted and
   advance the model in the same writer transaction. The flag-off path MUST gain an explicit
   transaction where DELETE and generation DML would otherwise autocommit separately.

4. **Orphan sweep** — `orphan_sweep_dml` / `SqliteVecStore::orphan_sweep`. A non-dry-run sweep that
   deletes at least one relevant note vector advances the model once in its existing transaction.
   Dry run and zero-row deletes do not advance it.

5. **Cross-model subject purge/merge** — `delete_subject_from_vector_tables`
   (`vectors.rs:382-400`) runs inside caller-owned merge transactions. It MUST receive model identity
   (or conservatively advance all initialized model rows) and couple advancement to any relevant
   deletion. Sanitized table names MUST NOT be reverse-parsed into model identifiers.

6. **Any direct `vec_*` writer** — code that bypasses these helpers, including future admin paths,
   MUST carry equivalent generation DML in the same transaction. Post-commit hooks and a begin/end
   reindex marker are not equivalent. `kkernel reindex` MUST reach the batch helper or explicitly
   satisfy this rule for each affected model/batch.

After this transition, `begin_reindex_epoch` and the completion-side global epoch bump in
`reindex.rs` have no authority and SHOULD be removed rather than maintained as a second freshness
system. Reindex snapshot deletion MAY remain as defense in depth.

Counter increments performed by Rust-side vector DML MAY be coalesced once per model per SQLite
transaction. The V11 note-liveness triggers cannot coalesce, because SQLite triggers execute per
row; their cost model is specified in the next section. Increments MUST NOT be moved to post-commit
effects in either case.

### Note membership triggers and atomic delete plans

Vector DML covers embedding changes and additions. Note-row liveness can change the JOIN predicate
without changing vector bytes, so V11 also adds triggers that advance **all initialized model rows**
for:

- a real note row deletion;
- an `UPDATE OF deleted_at` whose nullness changes;
- an `UPDATE OF namespace` whose value changes, because the rebuilt bridge's namespace metadata is
  derived from the note row.

`memory.prune` uses `SqlNoteStore::delete_note(..., Soft)` (`handlers/prune.rs:130-145`), so the
`deleted_at` trigger covers it in the deletion transaction. Raw note soft/hard deletes receive the
same protection. Inserts do not need a note trigger: a note without a vector is not graph input, and
the subsequent vector insert owns the per-model bump.

SQLite triggers execute once per affected row; there is no statement-level trigger, and an
enclosing transaction does not merge trigger executions. Bulk deletion of N live notes therefore
fires the trigger N times, each execution running the overflow guard and one UPDATE over the M-row
counter table, and each model's generation advances by N. That is semantically correct: the
coherence contract tests equality only, any strict advance invalidates, and the counter value has
no arithmetic meaning. What the transaction boundary changes is the cost model, not the increment
count. Inside one transaction the N×M row updates are page-cache mutations against a tiny table
with a single durable commit; split across N autocommit transactions they become N durable commits
and N separately observable invalidation points. Bulk deletion MUST therefore execute its note-row
mutations within a single enclosing transaction, for atomicity and to bound durable cost to one
commit per batch, not because trigger advances coalesce (they do not). `memory.prune` currently
soft-deletes N notes in N separate transactions (`handlers/prune.rs:130-145`), so it MUST wrap the
batch in one transaction. `delete_subjects` and any other multi-note deletion path carry the same
requirement. If per-row trigger execution ever measures as a bulk-deletion bottleneck, the remedy
is the membership-targeted Rust-side invalidation described below, not weakening the trigger.

That measurement is a required acceptance gate, not an open question. The benchmark suite MUST
soft-delete 10,000 live notes with three initialized model rows inside one transaction and compare
against the identical batch with the V11 triggers absent. Acceptance requires at most 1.5x the
trigger-free batch latency and exactly one durable commit. The M in the N×M cost is the number of
initialized embedding models — single digits in every supported deployment — and each trigger
execution is one overflow probe plus one UPDATE over that M-row table, so the expected result is
page-cache-bound; the gate exists to prove it rather than assume it.

Advancing all initialized models rather than only the models that hold a vector for the mutated
note is a deliberate simplification, because a SQL trigger cannot cheaply compute per-note model
membership and cannot query `vec0` virtual tables at all. The amplification is real: one liveness
change can stale every initialized model at once, forcing each queried model onto exact search
until its own rebuild completes. Two bounds keep that trade governed. First, rebuild scheduling
MUST cap concurrent memory-ANN model rebuilds at two unless a deployment explicitly raises it;
remaining models queue on the existing single-flight machinery, so an invalidation burst cannot
become an unbounded CPU and I/O storm. Second, the stale-window section below attaches numeric
acceptance bounds to exact-search fallback latency and rebuild duration, so the degraded mode is
measured and gated rather than merely described. A retired model that no recall queries incurs
only a wasted lazy rebuild, with no effect on served recall latency, and the single-transaction
requirement above bounds durable cost to one commit per batch.

Note-mutation authority is trusted in this cut as a property of khive's architecture, not merely
an assumption of this ADR. ADR-007 Rev 6 defines namespace as write attribution, never isolation:
khive has no tenant-isolation model at any storage layer, so a deployment serving mutually
untrusted tenants from one shared database is outside khive's supported trust topology altogether,
independent of this mechanism. The fence here restates that system boundary for the specific new
hazard the triggers introduce: because the all-model liveness triggers give any authorized
note-mutator a corpus-wide invalidation primitive, such a deployment MUST NOT enable this cut as
its recall path. No runtime topology probe could enforce the fence — khive has no tenant concept
to detect — so enforcement sits where all khive authorization sits, the Gate (ADR-018): a
deployment that exposes note-mutation verbs to semi-trusted callers MUST bound mutation rates in
Gate policy, the single seam every mutation path already crosses. A future amendment MAY add
model-membership-targeted note-liveness invalidation that computes the affected models in the Rust
delete path and advances only those, which removes the cross-model over-invalidation and, adopted
together with Gate-level rate bounding, is the migration path if khive later serves untrusted
co-tenants from one database.

Current note persistence uses `INSERT OR REPLACE` at three independent sites: the canonical
single-note upsert (`stores/note.rs:35-81`), the batch upsert (`stores/note.rs:377`), and note-merge
(`runtime/src/curation.rs:1709`). Before enabling a DELETE trigger, all three MUST route through one
shared true `INSERT ... ON CONFLICT(id) DO UPDATE` statement, or an equivalently proven non-delete
form. `INSERT OR REPLACE` is a DELETE followed by an INSERT, so a salience-only upsert through any of
these paths looks like a row delete to SQLite and spuriously advances every model. One raw writer
left in place defeats the prerequisite. This is a required migration with all three sites
enumerated, not optional cleanup.

The trigger update column list/`WHEN` predicates MUST exclude `salience`, `decay_factor`,
`properties`, `status`, `expires_at`, and content-only changes. Salience-only updates MUST leave all
generations unchanged. Content changes advance only when their replacement vector commits.

Atomic note delete plans (`atomic_prepare.rs:1083-1158`) execute the note-row mutation and vector
purges inside one atomic unit. The note liveness trigger supplies the transaction-coupled generation
advance; `PostCommitEffect::NoteDeleted` remains eager in-process scheduling only. A plan MUST NOT
depend on that post-commit effect for coherence, and a rolled-back atomic unit MUST not advance the
generation.

## Warm-hit and build/install protocol

### Warm hit

For every `memory.recall` that has an installed graph:

1. Read the durable generations for all queried models from SQLite. Implementations MAY batch all
   model keys into one statement and SHOULD reuse one reader acquisition.
2. If the query fails, a row is absent/malformed, or `Gann != Gdb`, do not call ANN search for that
   model. Route that model to exact sqlite-vec search and schedule/reuse the single-flight rebuild.
3. Only equality permits `search_loaded`.

No debounce, time cache, process-local epoch, or “serve stale while rebuilding” exception may bypass
step 1. This contradicts ADR-107 intentionally.

The added hot-path cost is contractually bounded to **one additional SQLite statement per recall**
when model reads are batched, returning **M two-column/one-row records for M queried models**. It is
a primary-key point-read set: no corpus scan and no vector/blob read. The performance gate is a
file-backed WAL benchmark with warm page cache at one and three models: added generation checking
MUST be at most 1.0 ms absolute p95 and at most 5% of end-to-end warm `memory.recall` p95 — the
permitted cost is the smaller of the two. At the currently measured baselines (PR #1083: 4.351 ms
one-model and 9.032 ms three-model warm p95), the 5% bound is the binding constraint, roughly
0.22 ms and 0.45 ms respectively; the absolute cap binds only if baselines grow past 20 ms. If the
gate fails, optimize batching/reader reuse; do not debounce the correctness read.

### Build, snapshot load, and install

For a model build/restore attempt:

1. Ensure the model generation row exists; read `g_start`.
2. Load a candidate snapshot or scan/build the live corpus.
3. A snapshot candidate is eligible only under the version/equality/hash rules below.
4. Immediately before snapshot persistence/install, re-read the durable row as `g_end`.
5. On read failure or `g_end != g_start`, discard the candidate. Do not persist or install it.
6. Persist/install stamped with `g_start` only after equality. Snapshot persistence SHOULD read the
   generation again inside its writer transaction and condition the replace on equality, preventing
   an older cross-process builder from overwriting a newer snapshot.

A mutation that commits after the final read is ordered after this build's coherence point. It
advances the row; the next recall rejects the installed generation. An older snapshot written in
that gap is likewise rejected by exact generation equality.

### Stale-window load behavior

Invalidation routes recalls to exact sqlite-vec search until a rebuild installs a matching
generation, and bulk liveness mutations invalidate every initialized model at once, so the
stale-window cost MUST be characterized, not left open. The window's duration is capped by the
retained single-flight, bounded-time rebuild: concurrent recalls never trigger parallel rebuilds,
and the first successful rebuild restores ANN service for its model. Under sustained mutation the
build-race rule keeps discarding candidates and the system remains on exact search; that is the
correct state under this contract (correct but slower), and it degrades no further — exact search
is the floor, not a cliff. The performance gate therefore extends beyond the warm read check: the
benchmark suite MUST record exact-search fallback p50/p95 at the gate corpus scale under
concurrent recalls during an in-flight rebuild, so operators can size corpora against the
documented fallback latency. Recording is not sufficient: at the gate corpus scale, acceptance
requires exact-search fallback p95 at most five times the same configuration's warm ANN recall
p95, and single-model rebuild duration at most 60 seconds. These bounds hold at gate scale;
production-scale rebuild cost is a rollout concern addressed in the upgrade section below. A
benchmarked fallback is the availability trade this ADR makes explicit; an unmeasured one would
be a regression channel. Rebuild scheduling MAY debounce or
batch retriggering under sustained invalidation; the durable generation read on the recall path
MUST NOT be debounced (the fail-closed rule above).

## Snapshot versioning

Persist memory snapshots as a versioned JSON envelope, extending the current content-hash wrapper:

```text
PersistedMemorySnapshotV2 {
    format_version: 2,
    generation: non-negative i64,
    content_hash: CorpusContentHash,
    snapshot: VamanaSnapshot,
}
```

The exact numeric version may change at implementation time if a repository-wide format registry exists;
the load behavior may not:

- legacy bare `VamanaSnapshot` blobs are stale;
- the current `{snapshot, content_hash}` wrapper without a generation is stale;
- unknown versions, malformed generations, and decode failures are stale;
- `generation != Gdb(model)` is stale without an O(N) hash scan;
- equality proceeds through the cheap count/dimension check and existing ordered content-hash check;
- any rejection rebuilds from authoritative vector/note rows and writes the new envelope.

No legacy blob may be decoded and then stamped with the current generation. That would recreate the
same-count/same-dimension false certification described in #752.

## Upgrade rollout

Every persisted memory snapshot that predates this ADR is stale by construction: bare
`VamanaSnapshot` blobs and the current hash-only wrapper both fail the envelope rules above. The
upgrade is therefore a one-time full rebuild for every configured model, and it MUST be planned
rather than incidental — repository documentation records production from-scratch memory-ANN
rebuilds exceeding 300 seconds, so an unplanned window would mean prolonged exact-search-only
vector recall for every model at once.

1. The V11 migration MUST NOT delete or rewrite existing persisted snapshots; rejection happens
   lazily at load, model by model, under the envelope rules.
2. On the first post-upgrade start, the existing startup warming path MUST schedule a rebuild for
   every registered model under the rebuild concurrency cap, without blocking serving. Recalls
   during the window take the characterized exact-search floor; nothing may shortcut the rebuild
   by stamping a legacy snapshot with a current generation.
3. The benchmark suite MUST measure the post-upgrade degraded window at the gate corpus scale:
   elapsed time from first start to all models warm, and exact-search p95 during the window, both
   within the stale-window bounds above. The recall path's warm-read budget during and after the
   window remains the binding warm-hit gate: the smaller of 1.0 ms and 5% of warm p95.
4. Operators running production-scale corpora SHOULD prewarm before switching traffic: run the
   admin warm/reindex path against the upgraded binary so rebuilds complete before the deployment
   serves recalls. The release upgrade note MUST document this expectation and the rebuild cost
   drivers (corpus size, model count).

## Fail-closed semantics

| Site                                       | Failure                                                        | Required behavior                                                                                                                               |
| ------------------------------------------ | -------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| Warm generation read                       | reader acquisition, SQL, missing row, malformed/negative value | Treat installed ANN as untrusted; exact sqlite-vec for that model; schedule/reuse rebuild; rate-limited diagnostic                              |
| Exact fallback                             | sqlite-vec search fails                                        | Propagate the existing storage error; never fall back to stale ANN                                                                              |
| Build start/final generation read          | any error                                                      | Do not load, persist, or install the candidate; keep ANN ineligible and use exact/Cold policy                                                   |
| Generation bump                            | insert/update/constraint/overflow failure                      | Fail and roll back the corpus mutation's transaction                                                                                            |
| Mutation rollback                          | any reason                                                     | Generation change rolls back with it                                                                                                            |
| Snapshot read/decode/version/equality/hash | any error/mismatch                                             | Treat as snapshot miss; rebuild; never stamp it current                                                                                         |
| Snapshot write                             | derived-state write fails after final equality                 | A correctly stamped in-memory candidate MAY install; warn and rebuild on next restart. The failure cannot authorize an older persisted snapshot |
| Eager in-process hook                      | absent/fails                                                   | No correctness loss; the next durable read detects the generation. Rebuild latency may increase                                                 |
| Reindex                                    | any mutation's coupled generation write fails                  | That mutation batch rolls back and reindex reports failure/non-zero under its normal policy                                                     |

## Sibling fix: correct the legacy memory-snapshot predicate

`retrieval_snapshots.namespace` stores the full composite key. Therefore the current predicate:

```sql
index_type = 'memory_vamana' AND namespace != 'global'
```

matches active `global::memory_vamana::{model}` rows and is wrong
(`reindex.rs:844-875`). Replace it with a predicate that preserves the exact active prefix, for
example:

```sql
WHERE index_type = 'memory_vamana'
  AND namespace NOT GLOB 'global::memory_vamana::*'
```

The prefix match MUST be case-sensitive. SQLite `LIKE` is ASCII case-insensitive by default, so a
`NOT LIKE` form would also retain legacy rows whose namespace differs from the retained key only by
case (`GLOBAL::memory_vamana::…` passes namespace validation); `GLOB` compares case-sensitively.
The regression suite MUST include an uppercase-namespace legacy row.

This purge deletes legacy `{namespace}::memory_vamana::{model}` rows but preserves active global
rows. Active-row invalidation after reindex MAY remain a defense-in-depth latency optimization; it
is not the coherence mechanism. This sibling fix is required even if the generation work is split
into another implementation PR.

## Test oracles

Recall results alone are not an oracle: FTS fusion and post-hydration deletion/namespace filtering
can make a stale ANN appear correct. Tests MUST assert generations, ANN-route counters, and
`memory.ann_warm` phase events.

1. **Migration**: upgrade V10 to V11; assert table, constraints, and triggers; assert V1 unchanged;
   run `scripts/lint-sql.sh`.
2. **Mutation/rollback matrix**: successful single and batched note-vector insert/update/delete,
   subject purge, and orphan sweep advance the expected model row once per transaction; dry-run,
   zero-row, entity-only, and failed-record paths do not. Inject a failure after vector DELETE and a
   generation-write failure; assert both vector data and generation roll back.
3. **Note trigger matrix**: prune/soft delete, raw soft delete, hard delete, and namespace move
   advance initialized models in the same transaction. Assert a strict advance, not a delta of
   exactly one: the per-row triggers make a multi-row batch's delta equal the affected row count.
   Salience/decay/properties/status/expiry-only updates do not advance. An atomic note-delete
   rollback does not advance; commit does. Verify `INSERT OR REPLACE` is gone or cannot trigger
   delete semantics.
4. **Two runtimes, one file DB**: seed all notes, warm runtime A, reset route counters/events, mutate
   through runtime B, then recall through A. Assert A observes a higher durable generation and
   performs **zero ANN searches at the stale generation**; it uses exact search until a
   `PhaseStarted`/`PhaseCompleted` `memory.ann_warm` pair installs the matching generation. Only then
   may the warm-route counter advance.
5. **Build race**: pause between corpus scan and final generation read, mutate through B, release,
   and assert `DiscardedStaleBuild`, no install, and no stale snapshot replace.
6. **Snapshot compatibility**: reject a bare legacy blob, the current hash-only wrapper, unknown
   versions, generation mismatch, and equal-generation/hash mismatch. A same-count/same-dimension
   vector replacement MUST reject the old snapshot.
7. **Fail-closed read**: inject generation reader acquisition/query/malformed-row failures with a
   warm entry present. Assert warm-route count stays zero and exact-route telemetry fires.
8. **Real-process reindex**: keep a daemon/runtime warm on a file DB; spawn the real `kkernel
   reindex` process using an embedder that changes bytes without changing model/count/dimensions;
   wait for its successful exit; on the next recall assert the durable transition and no stale ANN
   search, then assert a warm phase completes before ANN is used again. A manual same-process epoch
   bump is insufficient for this test.
9. **Sibling predicate**: seed an active `global::memory_vamana::*` row, legacy
   `local::memory_vamana::*` rows, and unrelated knowledge `local::vamana::*` rows. The legacy purge
   removes only the legacy memory rows.
10. **Performance**: record the one/three-model warm-read benchmark and counter-write throughput.
    Enforce the warm p95 gate above; report write amplification rather than hiding it.
11. **Orphan guard**: through each vector-write helper path (single insert/update and batch),
    attempt to write a `kind='note'` vector row whose `subject_id` has no `notes` row; assert the
    write fails inside its transaction with no vector row persisted and no generation advance.
    Assert an entity-kind row is not constrained. Assert a vector written under a soft-deleted
    note stays JOIN-excluded and that un-deleting the note advances the generation. Then create
    the note and its vector in the normal order and assert ANN eligibility with a matching
    generation advance.
12. **Stale-window fallback**: invalidate an installed graph, issue concurrent recalls during the
    single-flight rebuild, and record exact-search p50/p95 at the gate corpus scale plus the
    rebuild duration. Assert every stale-window recall used exact search (zero stale-ANN
    searches), that rebuild completion restores the ANN route, and that both numbers sit within
    the stale-window acceptance bounds.
13. **Bulk-liveness throughput**: soft-delete 10,000 live notes with three initialized model rows
    in one transaction; assert exactly one durable commit, a strict all-model advance, and latency
    within 1.5x the trigger-free baseline batch.
14. **Direct-DML inventory**: a source-level test scans workspace source for statements that write
    `vec_*` tables and compares the found sites against an explicit allowlist maintained in the
    test, seeded from the implementation-review audit. A site missing from the allowlist fails the
    suite; an allowlist entry is added only in the same change that gives that site its coupled
    generation DML. Note-table DML needs no inventory: note liveness is covered mechanically by
    the V11 triggers regardless of which code path writes the row.

## Alternatives considered

In addition to the three mechanism families above:

### Global epoch rather than per-model generations

One row and one comparison are simple and match current ADR-107. Rejected because a re-embed/write
for one model invalidates every model and prevents exact snapshot/model reasoning. Note-liveness
triggers may still conservatively advance all initialized per-model rows when the note change cannot
identify a model; vector DML remains model-specific.

### Durable bumps only at `--atomic` and `reindex`

This is the strongest rejected alternative. It has lower routine write cost and is sufficient for
the two currently known external commands. Rejected because it does not cover a second ordinary
runtime or future writer and has no structural way to enforce its topology assumption.

### Keep ADR-107's five-second epoch poll and content hash

Already implemented and attractive for availability: most recalls add no DB read and stale indexes
continue serving. Rejected because it explicitly permits a post-commit staleness window and fails
open on epoch read errors, contrary to #752's required next-recall/fail-closed semantics.

## Risks and unknowns

- **Counter-page contention**: measure single-write and batch throughput. Mitigate with one bump per
  model/transaction, not per record.
- **Hot-read regression**: batch model reads into one statement/reader and enforce the p95 gate.
- **Trigger interaction with REPLACE**: mandatory true UPSERT prerequisite; test salience-only paths.
- **Uncatalogued direct `vec_*` SQL**: implementation review MUST inventory direct DML and either
  route it through the helpers or add same-transaction generation DML. Future direct writers are
  prohibited by contract, and the inventory becomes a permanent source-level test (oracle 14), so
  a new bypass is a suite failure rather than a review-time hope.
- **Generation rows for retired models**: retain them; their cardinality is embedding-model count,
  not note count. Cleanup is optional future work.
- **Cross-model over-invalidation on shared backends**: note-liveness triggers advance all
  initialized models, so on a shared multi-tenant database an authorized writer's note mutations
  invalidate co-tenant model caches. This is bounded by the rebuild concurrency cap, the numeric
  stale-window gates, and the single-writer-authority trust model (Gate authorization, ADR-018),
  and deferred to a future membership-targeting amendment; this cut does not isolate co-tenants.
- **Upgrade window**: every pre-ADR snapshot rebuilds once after migration. Bounded by the rollout
  contract: lazy rejection, capped-concurrency prewarm at first start, and documented operator
  prewarm for production-scale corpora.
- **Availability trade**: exact fallback may be slower than ADR-107's stale serving during rebuild.
  This is deliberate. Operators may not re-enable stale ANN without a later ADR changing the
  consistency contract.

## Implementation fences

### MAY

- Batch generation reads for all recall models into one SQL statement.
- Coalesce multiple relevant Rust-side vector mutations to one increment per model per SQLite
  transaction; the note-liveness triggers advance per row and are exempt.
- Retain eager in-process eviction/rebuild scheduling and the ordered content hash.
- Retain active snapshot deletion after reindex as defense in depth.
- Leave the old `memory_ann_epoch` table unused for one compatibility release.

### MAY NOT

- Cache or debounce the durable generation across recalls.
- Serve/search an installed ANN after a generation read error or inequality.
- Treat count/dimensions, snapshot presence, in-process generation, notification, or post-commit
  hook success as proof of freshness.
- Advance generations after commit or allow a generation-write failure to commit corpus changes.
- Increment once per row from Rust-side vector DML in a successful batch when one per
  model/transaction suffices; the V11 note triggers are per-row by SQLite semantics and exempt.
- Fire generation triggers for salience-only or other score/metadata-only updates.
- Decode a legacy snapshot and stamp it with the current generation.
- Edit V1, put V11 DDL inline in Rust, or keep pack-owned ad hoc creation as the schema authority.
- Add new direct `vec_*` DML without transaction-coupled generation logic.

### Verify by

- V10→V11 migration and SQL lint;
- mutation/rollback and trigger matrices;
- two-runtime shared-file test;
- real-process unchanged-shape reindex test;
- build/install race and legacy snapshot tests;
- generation-read failure injection with a zero warm-route count;
- one/three-model hot-path p95 benchmark;
- vector-write orphan-rejection test through every helper path;
- stale-window exact-search fallback benchmark with zero stale-ANN searches, within its bounds;
- bulk-liveness trigger throughput benchmark within its bound;
- direct `vec_*` DML inventory scan;
- post-upgrade rollout window measurement.

## Consequences

The memory ANN no longer serves a graph that it cannot prove current at the recall's database-read
coherence point. Cross-process writers and restarts share the same proof, and rollback preserves the
proof automatically. The cost is one fresh small SQLite read per recall, one coalesced counter
write per affected model per vector-DML transaction (per affected row on the note trigger path,
still one durable commit per batch), plus exact-search latency while a stale graph rebuilds.

This is a deliberate reversal of ADR-107's availability-first stale-serving choice. Reviewers must sign off on that product-intent change before implementation merges.
