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
MUST advance that model's durable generation in the mutation's SQLite transaction. The added write
MAY be coalesced to once per affected model per transaction/batch; the contract is not one increment
per row.

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
and transaction behavior remain identical. There is deliberately no INSERT trigger: a note row
without a vector is not ANN graph input, while the vector insert performs the model-specific bump.

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
   After a successful `note.content` replacement, advance the row named by `embedding_model` before
   releasing/committing the enclosing savepoint/transaction.

2. **Batch insert/reindex** — `batch_insert_vectors_dml` (`vectors.rs:402-492`). Track whether at
   least one relevant record's savepoint succeeded. Advance the model exactly once after the loop
   when `affected > 0`; a generation failure rolls back the outer batch transaction. Failed record
   savepoints do not independently advance it.

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

Counter increments MAY be coalesced once per model per SQLite transaction. They MUST NOT be moved to
post-commit effects.

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

Current note persistence uses `INSERT OR REPLACE` (`stores/note.rs:35-81`). Before enabling a DELETE
trigger, canonical note upsert MUST be changed to a true `INSERT ... ON CONFLICT(id) DO UPDATE` (or
an equivalently proven non-delete form). Otherwise a salience-only upsert can look like a row delete
to SQLite and spuriously advance every model. This is a prerequisite, not optional cleanup.

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
MUST be at most 1.0 ms absolute p95 and at most 5% of end-to-end warm `memory.recall` p95. If the gate
fails, optimize batching/reader reuse; do not debounce the correctness read.

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
  AND namespace NOT LIKE 'global::memory_vamana::%'
```

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
   advance initialized models in the same transaction. Salience/decay/properties/status/expiry-only
   updates do not. An atomic note-delete rollback does not advance; commit does. Verify
   `INSERT OR REPLACE` is gone or cannot trigger delete semantics.
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
  prohibited by contract.
- **Generation rows for retired models**: retain them; their cardinality is embedding-model count,
  not note count. Cleanup is optional future work.
- **Availability trade**: exact fallback may be slower than ADR-107's stale serving during rebuild.
  This is deliberate. Operators may not re-enable stale ANN without a later ADR changing the
  consistency contract.

## Implementation fences

### MAY

- Batch generation reads for all recall models into one SQL statement.
- Coalesce multiple relevant mutations to one increment per model per SQLite transaction.
- Retain eager in-process eviction/rebuild scheduling and the ordered content hash.
- Retain active snapshot deletion after reindex as defense in depth.
- Leave the old `memory_ann_epoch` table unused for one compatibility release.

### MAY NOT

- Cache or debounce the durable generation across recalls.
- Serve/search an installed ANN after a generation read error or inequality.
- Treat count/dimensions, snapshot presence, in-process generation, notification, or post-commit
  hook success as proof of freshness.
- Advance generations after commit or allow a generation-write failure to commit corpus changes.
- Increment once per row in a successful batch when one per model/transaction suffices.
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
- one/three-model hot-path p95 benchmark.

## Consequences

The memory ANN no longer serves a graph that it cannot prove current at the recall's database-read
coherence point. Cross-process writers and restarts share the same proof, and rollback preserves the
proof automatically. The cost is one fresh small SQLite read per recall and one coalesced small
write per affected model/transaction, plus exact-search latency while a stale graph rebuilds.

This is a deliberate reversal of ADR-107's availability-first stale-serving choice. Reviewers must sign off on that product-intent change before implementation merges.
