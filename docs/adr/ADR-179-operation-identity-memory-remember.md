# ADR-179: Operation Identity on `memory.remember`: Keyed Create, Conflict Names the Holder

- **Status**: Proposed
- **Date**: 2026-09-08
- **Extends**: [ADR-021](ADR-021-memory-pack.md) (memory pack: `memory.remember` creates one note
  of kind `memory`)
- **Relates to**: [ADR-172](ADR-172-versioned-notes-compare-and-set.md) (proposed; this record lands
  its §3 storage substrate, the `notes.key` column and partial unique index, as a first slice and
  nothing else of it), [ADR-133](ADR-133-incidental-writes-off-the-request-hot-path.md) Amendment 3
  (an error's `domain_disposition: "unknown"` is resolved only by a keyed re-read; this record
  supplies the key), [ADR-007](ADR-007-namespace.md) (namespace scope of the key),
  [ADR-014](ADR-014-curation-operations.md) (soft delete releases the key)

## Context

A client that writes a memory and loses the acknowledgement cannot recover safely today. Replaying
`memory.remember(content, source_id)` creates a second memory: `source_id` is an annotation target,
not an identity, and several memories may legitimately annotate one source. A ranked `memory.recall`
is not an absence oracle, so the client cannot prove the first write did not land, and the only
read-back that reaches the row, an edge listing on the source, cannot tell the intended memory from an
unrelated one that annotates the same record. The client is left choosing between a duplicate it
cannot see and a memory it may have lost.

ADR-133 Amendment 3 closes the common arm of this: an obligation error raised after the dispatch now
says `committed` and carries the id. The arm it leaves open is the honest one, `unknown`: the response
never arrived, or the error was raised before the fold. Amendment 3 A3.2 states that `unknown` is
resolved only by an outcome re-read keyed on an identity the caller chose before the call. This record
supplies that identity for the write that a memory-backed agent loop issues most.

The store already refuses duplicates by identity where it has one: `try_insert_note` is an
`INSERT OR IGNORE` followed by an `external_id` verification, and the audit lane appends by generation
id. `memory.remember` exposes no identity of its own. ADR-172 proposes a general `key` on notes with
compare-and-set updates, fences and keyed listings; that record is Proposed on main and larger than
this need. The narrow thing is stated here so it can land first, on ADR-172's storage shape, so that
ADR-172 later finds the column in place rather than a fork of it.

## Decision

### D1. `memory.remember` accepts `key`

`memory.remember(content, key=K, ...)` creates the memory note or, when a live note of kind `memory`
in the write namespace already holds `K`, writes nothing and fails with `KhiveError::conflict`,
`details: {"reason": "key_conflict", "key": K, "existing_id": <uuid>}`. `K` is a caller-chosen
string, at most 512 bytes, without U+0000; a longer or ill-formed key is refused as invalid input and
nothing is written. The write namespace is the one the handler already resolves: an explicit
`namespace=`, else the actor namespace for episodic memories, else `local` for semantic ones. The
same `K` may be held by one live memory in each namespace.

The unique index is what refuses, so two writers racing on one key get exactly one success and one
`key_conflict` naming the same `existing_id`. A replay of the identical request after a lost
acknowledgement is therefore the reconciliation: its answer is either the new id or the existing one,
and either way the caller holds the id of the one memory it intended.

A `key_conflict` is a refusal raised before any domain write; on the wire it carries
`domain_disposition: "not_committed"` (ADR-133 Amendment 3, A3.1).

### D2. Storage: ADR-172 §3's column and index, verbatim

Migration 028 adds `key TEXT` (nullable) to `notes` and the partial unique index ADR-172 §3
specifies:

```sql
CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_namespace_kind_key
    ON notes(namespace, kind, key)
    WHERE key IS NOT NULL AND deleted_at IS NULL;
```

Existing rows hold `NULL` and are untouched. Uniqueness is per note kind, so this record's use of it
is confined to kind `memory`; the `create(key=)`, `get(key=)`, `key_ambiguous` and keyed-listing
surfaces of ADR-172 are not landed here and ADR-172 keeps them. ADR-172 names one migration 028
carrying `version`, its trigger, `key` and the index. This record takes number 028 for `key` and the
index alone; when ADR-172's implementation lands it does not add them again, and its `version` column
and trigger take the next free number, recorded by an amendment to ADR-172.

### D3. The key is immutable and released by deletion

`key` is set at create and never updated; `memory.remember` is the only writer of it under this
record. A soft delete releases the key, because the index excludes rows with `deleted_at` set; a
hard delete releases it too. `memory.prune` soft-deletes, so a pruned memory's key may be reused by
a later write. That is stated as a consequence rather than hidden: a client that needs a pruned key to
stay refused must not reuse it.

A replay that races a prune of the same key is a named consequence, not a discovered one: when the
prune's soft delete commits between the original write and the replay, the key is released and the
replay creates a second memory for one operation identity. Recovery runs in seconds and prune runs
much later, so the window is small; the record accepts it and acceptance 11 measures it rather than
hiding it.

### D4. `key` composes with `source_id`, and constrains nothing else

The `annotates` edge to `source_id` is created only on the successful create; a `key_conflict`
creates no edge and no note. Two memories with different keys may annotate the same source; the
key constrains the memory, never the source. A request without `key` behaves exactly as today.

### D5. What the replay answer may be used for

The caller may treat `key_conflict.existing_id` as the id of its own earlier write, because the key
is the caller's and the namespace is the caller's. It may not treat the absence of a conflict as
proof that no other memory annotates the same source; that is D4's point. An `unknown` disposition
on the replay itself (ADR-133 Amendment 3) is resolved by replaying again; the key makes that safe.

On a `key_conflict` the reconciliation terminates at `existing_id`. Its
`domain_disposition: "not_committed"` describes this attempt only, never the keyed subject, whose
earlier write did commit; a consumer that applied ADR-133 A3.2 mechanically would resend forever.
The rule is therefore: a keyed request that returned `key_conflict` is never resent; the caller
reads `existing_id` and is done.

### D6. A key is an identity only within a pinned namespace

D1 resolves the write namespace from an explicit `namespace`, else the actor's namespace for
episodic memories, else `local`. A lost acknowledgement is usually recovered by a different process,
and in a fleet where the actor is decided by the caller's working directory a recovering process can
resolve a different namespace, miss the index partition entirely and create the duplicate this record
exists to prevent. A recovery replay therefore carries the same explicit `namespace` the original
carried, and a client that recovers on the caller's behalf pins it. A replay that resolves a
different namespace is a different identity and produces a second row; acceptance 10 documents that
outcome so no one mistakes it for a defect of the index.

## Consequences

- A memory-backed loop recovers a lost acknowledgement with one more call and no idempotency store
  of its own.
- One new nullable column and one partial index on `notes`; no change to unkeyed writes, listings or
  recall.
- ADR-172's larger surface is unchanged in scope and gains a landed substrate.
- Out of scope, deliberately: keys on other note kinds, keyed reads, compare-and-set, fences.

## Acceptance

1. **Lost acknowledgement.** `memory.remember(content, key=K, source_id=S)` commits; the identical
   request is sent again. The second call fails with `key_conflict` naming the first id; exactly one
   note and exactly one `annotates` edge exist. Control: the same pair without `key` produces two notes.
2. **Concurrent recovery.** Two processes send the same keyed request at once against two daemons
   over one store; exactly one succeeds, the other gets `key_conflict` with the same id; one row
   exists. The partial unique index is the mechanism and the claim; the daemon count is not.
3. **Decoy annotation.** A different memory with a different key annotating the same `S` coexists with
   the keyed one; the keyed replay still names its own id, not the decoy's.
4. **Unchanged without a key.** The existing `memory.remember` tests pass unmodified; a request
   without `key` never produces `key_conflict`.
5. **Release on delete.** After `memory.prune` soft-deletes the keyed memory, the same key creates a
   new memory; after a hard delete, likewise.
6. **Key validation.** A 513-byte key and a key containing U+0000 are refused as invalid input with
   `stats()` unchanged.
7. **Namespace scope.** Two actors writing episodic memories with the same key hold two rows; one
   actor writing the same key twice holds one.
8. **Disposition interplay.** Under a forced obligation failure the first keyed write returns
   `domain_disposition: "committed"` with the id; a replay returns `key_conflict` with that id and
   `domain_disposition: "not_committed"`, and the reconciliation ends there: the reference client
   surfaces `existing_id` and issues no third call.
9. **Three surfaces.** The MCP `request` tool, `kkernel exec` and the Python client return the same
   `key_conflict` object for the same replay; `Session.remember(key=)` passes the field through.
10. **Namespace pin.** The same keyed request replayed from a process that resolves a different
    actor namespace, without an explicit `namespace`, produces two rows; replayed with the original's
    explicit `namespace` it returns `key_conflict`. Both outcomes are asserted.
11. **Prune race.** A replay concurrent with `memory.prune` of the same key ends in one of two
    states, one row when the replay won or two rows when the prune committed first, and never a
    third; the test runs the race repeatedly and asserts the row count is 1 or 2 on every run.

## Implementation notes

- The insert-then-resolve step can miss when the holder is soft-deleted between the two statements;
  the handler retries the insert once and otherwise returns an error with `domain_disposition:
  "unknown"`.
- `crates/khive-db/sql/028-notes-key.sql` and a `VersionedMigration` at version 28 in
  `crates/khive-db/src/migrations.rs`; the `Note` type gains `key: Option<String>`.
- `crates/khive-pack-memory/src/handlers/common.rs`: `RememberParams` gains `key`; `remember.rs`
  validates it and passes it to the runtime's note creation; the keyed insert follows the
  `try_insert_note` shape (insert, then on zero rows resolve the existing id by
  `(namespace, kind, key)` and return the conflict with it).
- `python/khive`: `Session.remember(..., key=None)`; the conflict object is surfaced unchanged.
