# ADR-172: Versioned notes — compare-and-set updates, keyed create-if-absent, and a durability option

- **Status**: Proposed
- **Date**: 2026-09-07
- **Relates to**: [ADR-007](ADR-007-namespace.md) (by-id resolution is unscoped), [ADR-067](ADR-067-write-owner-daemon.md)
  (single writer), [ADR-096](ADR-096-warm-daemon-per-request-identity.md) (per-request identity), V13
  (`013-list-cursor-sequences.sql`, trigger-maintained ledgers), [ADR-111](ADR-111-blob-store.md) (fsync on put)

## Context

A state layer that keeps one versioned document per key on top of khive needs three things the note
surface does not export today, all measured at `654b12b3` against the installed daemon:

1. **No conditional update on the wire.** `update(help=true)` lists `id`, `kind`, `name`, `description`,
   `content`, `salience`, `decay_factor`, `relation`, `weight`, `properties`, `tags`, `entity_type`. Nothing
   lets a caller say "apply this only if the note is still at the revision I read". Internally the runtime
   already performs a compare-and-swap: `update_note_from_snapshot_with_embedding_report`
   (`crates/khive-runtime/src/curation.rs`) reads a snapshot and persists through
   `replace_note_if_unchanged(note, expected_updated_at, expected_deleted_at)`, whose statement is one
   conditional `UPDATE ... WHERE id = ? AND updated_at = ? AND deleted_at IS ? AND ? > updated_at`
   (`crates/khive-db/src/stores/note.rs`, `note_replace_if_unchanged_statement`). The guard exists; the
   revision it guards is the server's own read, never the caller's.
2. **No unique key on a note.** `notes.name` is nullable and unindexed (`crates/khive-db/sql/schema.sql`), so
   "create this document unless one already exists under this key" has nothing to refuse on. Two creators
   racing on the same name both succeed.
3. **Listing cannot express "keys under this prefix, since this time, carrying all of these tags".** `list`
   takes `tags` (any-of, case-insensitive) and the V13 `after` cursor; `NoteFilter` already carries
   `min_created_at` (`crates/khive-storage/src/note.rs`) but no verb parameter reaches it, and there is no
   prefix predicate at all.

Two facts bound the design:

- By-id verbs resolve without a namespace filter (ADR-007, `unscoped_by_id`): a caller holding an id can
  read and overwrite a note in any namespace. A version precondition therefore protects against lost
  updates, not against other principals; authorization stays the Gate's seam.
- Thirty-three `UPDATE notes` statements live across eight files (`grep -rn "UPDATE notes" crates
  --include='*.rs'`, test modules excluded): the note store, pending-events replay, gtd, memory, schedule,
  the atomic-message path, and curation. A revision maintained by hand at each site would drift at the
  first site that forgets it.

A fourth fact matters to any caller whose contract is "a returned write has survived machine death": every
connection sets `PRAGMA synchronous = NORMAL` (`crates/khive-db/src/pool.rs`, three sites). In WAL mode
that survives process death and can lose the tail of the log on OS crash or power loss. The blob store is
not better off: `FsBlobStore::put` syncs the temporary file, renames it into the shard and returns
without an fsync of the shard directory, freshly created shard directories get no barrier, and the
dedup branch returns after an mtime touch (`crates/khive-db/src/stores/blob.rs`). On Linux a directory
entry needs its own fsync to be durable; on macOS a plain `fsync` does not flush the drive cache. So
neither plane meets the stronger contract today, and no claim in this ADR rests on either doing so.

## Decision

### 1. `notes.version`, maintained by the database, not by callers

Migration `028-notes-version.sql` adds `version INTEGER NOT NULL DEFAULT 1` to `notes` and one trigger:

```sql
CREATE TRIGGER IF NOT EXISTS bump_note_version
AFTER UPDATE ON notes
WHEN NEW.version = OLD.version
BEGIN
    UPDATE notes SET version = OLD.version + 1 WHERE id = NEW.id;
END;
```

Every existing row starts at 1. Every statement that touches a note row, today's thirty-three and any
future one, advances the version in the same transaction, because the trigger runs inside it. The `WHEN`
clause makes a statement that sets `version` explicitly a no-op for the trigger; nothing in tree does, and
a test asserts that no production statement writes the column (the population is the grep above, named
in the test so a new site is a red test rather than a silent exemption). `recursive_triggers` is off by
default, so the trigger's own `UPDATE` does not re-fire it. The V13 ledgers set the precedent: a
database-maintained monotonic value assigned by a trigger in the substrate's own transaction.

`version` is returned on every note read (`get`, `list`, `search`, `create`, `update`), as an integer.

### 2. `expected_version` on `update`

`update(id=..., expected_version=N, ...)` for a note succeeds only if the row's `version` is `N` at the
moment the conditional `UPDATE` runs. Implementation: the existing `replace_note_if_unchanged` statement
gains `AND version = ?expected` when the caller supplied one, so the check and the write are one statement
inside the writer's `BEGIN IMMEDIATE` (ADR-067). No new transaction shape, no read-then-write in the
handler. The internal snapshot guard stays as it is; the caller's precondition composes with it.

On mismatch the verb fails with `KhiveError::conflict`, `code: "version_conflict"`, and
`details: {"expected_version": N, "current_version": M}`. `M` is read after the failed statement, inside
the same writer request, so the caller's next attempt can carry it. A `version_conflict` performs no
mutation: `version` does not advance, `updated_at` does not move.

Omitting `expected_version` keeps today's behaviour exactly. Entities and edges are out of scope here;
they carry the same internal snapshot guard (`stale_entity_snapshot_error`, `stale_edge_snapshot_error`)
and can take the same parameter by a later amendment once notes have proven the shape.

### 2b. `fence`: a precondition on a second keyed note, same transaction

A state layer that leases a run to one worker needs a document write to be refused when the run's lease
document is no longer at the generation the worker holds. That is a precondition on a different row,
evaluated in the same transaction as the write. `update` and `create` (§3) accept an optional
`fence={"key": K2, "kind": <note kind>, "expected_version": G}`. Inside the writer request, before the
conditional write, the fence row is resolved by key in the caller's primary namespace and its `version`
compared to `G`; a missing row or a different version fails the whole request with `KhiveError::conflict`,
`code: "fence_conflict"`, `details: {"key": K2, "expected_version": G, "current_version": M | null}`, and
nothing is written. The fence is checked only when supplied; whether a write without a fence should be
refused for a run that has a lease is policy above khive and is not decided here.

### 3. `key`, and create-if-absent

Migration 028 also adds `key TEXT` (nullable) to `notes` and a partial unique index:

```sql
CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_namespace_kind_key
    ON notes(namespace, kind, key)
    WHERE key IS NOT NULL AND deleted_at IS NULL;
```

`key` is a caller-chosen string, at most 512 bytes, no U+0000, unique among live notes of one kind in one
namespace. Existing rows have `NULL` and are untouched. `create(kind="note", note_kind=..., key=K, ...)`
inserts the note or fails with `KhiveError::conflict`, `code: "key_conflict"`, `details: {"key": K,
"existing_id": <uuid>}` when a live note already holds `K`; the index is what refuses, so two racing
creators get exactly one success. `get(kind="note", key=K)` resolves by key within the caller's primary
namespace, the same scope rule as prefix resolution in ADR-007. `key` is immutable after create;
`update` does not accept it. A soft-deleted note releases its key; a hard delete does too.

`name` is unchanged and stays free-form. The two words mean different things: a `name` is what a human
calls a note, a `key` is what a program looks it up by.

### 4. Three list predicates

`list(kind="note", ...)` gains:

- `key_prefix`: rows whose `key` starts with the string. Implemented as a range predicate on the index
  in §3, never `LIKE`, so `%` and `_` in a key need no escaping: `key >= ?p AND key < ?s`, where `?s` is
  the lexicographic successor of the prefix computed by the caller of the statement: drop every trailing
  U+10FFFF code point, then increment the last remaining code point (skipping the surrogate block, so
  U+D7FF steps to U+E000). SQLite's default `BINARY` collation compares UTF-8 bytes, and UTF-8 preserves
  code point order, so every key that starts with the prefix sorts in `[p, s)`. When nothing remains
  after the drop, or the prefix is empty, there is no upper bound and the predicate is `key >= ?p`
  alone. `p || CHAR(0x10FFFF)` is not a valid upper bound: the keys `p` + U+10FFFF and any extension of
  it start with `p` and sort at or above it, so a `<` bound omits them.
- `updated_after`: RFC 3339 timestamp, inclusive, against `updated_at` (the last write, which is what a
  "changed since" question asks; `created_after` against `created_at` is added alongside since
  `NoteFilter.min_created_at` already exists and costs nothing).
- `tag_mode`: `"any"` (today's behaviour, the default) or `"all"`.

A keyed listing is a request carrying `key_prefix`; `key_prefix=""` selects every keyed note. It admits
only rows with `key IS NOT NULL`, so the cursor below never compares a NULL. `updated_after` without
`key_prefix` is a plain filter on the unkeyed listing, which keeps its insertion order and cursor and
does include unkeyed rows. A keyed listing is ordered `updated_at DESC, key DESC` and paginates by
keyset on that pair: `next_after` is an opaque cursor encoding the last row's
`(updated_at, key)`, passed back as `after`. This is a different cursor from the V13 insertion-sequence
walk, which stays the order for unkeyed listings; a document updated after the cutoff moves to the front
of a keyed listing, which the insertion cursor could never show. `tag_mode`, `tags`, `note_kind` and
`namespace=` compose with both. Offset mode is unchanged.

### 5. A per-deployment durability option

`[storage] synchronous = "normal" | "full"` and `[storage] fullfsync = false | true` in `khive.toml`,
applied at the three pragma sites in `pool.rs`. These settings cover the record plane only: the blob
store's own `sync_all` calls are not `F_FULLFSYNC` on macOS and its directory barriers are missing, and
that repair is separate work under the same deployment option, not decided here. Defaults are today's
values (`normal`, `false`); nothing
changes for a deployment that does not opt in. `fullfsync` maps to `PRAGMA fullfsync` and
`PRAGMA checkpoint_fullfsync`, which matter on macOS, where plain `fsync(2)` does not flush the drive's
write cache and only `F_FULLFSYNC` does. The daemon reports both values in `db_diagnostics` so a caller can
assert the deployment it was promised.

Measured on this development machine (Apple silicon, APFS, load average about 7), one 200-byte row per
`BEGIN IMMEDIATE ... COMMIT`, WAL mode, 1,000 commits per row, wall clock per commit:

| setting                             | mean     | p50      | p99      |
| ----------------------------------- | -------- | -------- | -------- |
| `synchronous=NORMAL` (today)        | 0.028 ms | 0.019 ms | 0.055 ms |
| `synchronous=FULL`                  | 0.075 ms | 0.057 ms | 0.125 ms |
| `synchronous=FULL` + `fullfsync=ON` | 1.07 ms  | 0.51 ms  | 5.6 ms   |

For scale, one warm `whoami()` over the daemon socket from a Python client on the same machine measured
4.8 ms mean, 4.05 ms p50, 18.4 ms p99 over 200 calls. `FULL` alone is within noise of the request cost;
`FULL` with `fullfsync` adds roughly a fifth to the mean and can double a p99 write. These are commit-path
numbers under a synthetic table, not end-to-end verb latencies; the verb-level measurement is an acceptance
item below.

### 6. Out of scope

Ordered per-stream append with an expected sequence is a separate proposal; nothing here pre-empts its
shape, and the `fence` in §2b is the row-level primitive it can reuse. Request ordering is unchanged: a
`[...]` request runs its operations concurrently, a `|` chain runs them in order; a caller that needs
several dependent writes applied in order sends a chain or one request per write. Entity and edge `expected_version` are a later amendment. Cross-namespace key
uniqueness is not offered: the index is per namespace by construction.

## Alternatives considered

| Alternative                                              | Why not                                                                                                                                                                                                                        |
| -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Expose `expected_updated_at` instead of adding `version` | A microsecond timestamp round-trips through presentations as a string and through clients as floating point; an integer revision is exact, and the internal guard already needs both `updated_at` and `deleted_at` to be safe. |
| Maintain `version` in each `UPDATE notes` statement      | Thirty-three sites in eight files today; the first site that forgets it silently breaks the precondition for every caller. The trigger closes the population.                                                                  |
| Reuse `name` as the unique key                           | `name` is free-form and unindexed; making it unique would reject existing data and change the meaning of a human-facing field. A separate nullable column changes nothing for existing rows.                                   |
| A `LIKE 'prefix%'` predicate for `key_prefix`            | Needs escaping of `%` and `_`, and a leading-anchored `LIKE` uses the index only under `case_sensitive_like`; the range form needs neither.                                                                                    |
| `key < p \|\| CHAR(0x10FFFF)` as the range's upper bound | Omits `p` + U+10FFFF and every key extending it, which are permitted keys that start with `p`; the successor bound admits them, and the no-successor case degrades to a lower bound only.                                      |
| Client-side read-then-write with a retry loop            | Not compare-and-set: two processes can both read version N and both write; the conformance test for a state layer is exactly that race, cross-process.                                                                         |
| Make `FULL` (or `FULL` + `fullfsync`) the default        | The measured cost is small at `FULL` but the p99 at `fullfsync` is a visible regression for every deployment that does not need it; an opt-in that the diagnostics report is the honest shape.                                 |

## Consequences

- One migration (028), additive: two columns, one index, one trigger. Rollback is a schema-version pin,
  as with every migration in the chain.
- Every note response grows an integer field. Clients that ignore unknown fields are unaffected.
- The `replace_note_if_unchanged` statement gains one optional predicate; every other note write is
  untouched and still advances `version` through the trigger.
- `create` for notes gains `key`; `get` gains `key` for notes; `list` gains three parameters; `update`
  gains `expected_version`. The help text for each names the conflict codes.
- Two new conflict codes, `version_conflict` and `key_conflict`, both under `kind: conflict`, both carrying
  the values a caller needs to recover.
- This is the idempotency story for note writes. `request_id` is correlation and not an idempotency key;
  the version precondition is what makes a blind retry after a lost response safe: it either applies
  once or reports the version the earlier attempt produced.
- There is no restore path for a soft-deleted note today. If one is added, restoring a note whose key a
  live note has since taken fails with `key_conflict`; the index decides, not the handler.

## Acceptance

Stated before implementation, checked at the PR that lands the code:

1. **Compare-and-set, cross-process.** Two processes read a note at version N and both send
   `update(expected_version=N)`. Exactly one succeeds, the other receives `version_conflict` with
   `current_version: N+1`, and `version` is N+1 after both, never N+2. Run through two socket clients and
   through two `kkernel exec --strict` processes.
2. **Create-if-absent, cross-process.** Two processes `create(key=K)` concurrently: one success, one
   `key_conflict` naming the winner's id, one live row.
3. **Trigger coverage.** A test enumerates every production `UPDATE notes` statement (the grep in Context,
   with a control that the enumeration is non-empty) and asserts none writes `version`; a second test runs
   one statement from each file against a fixture row and reads `version` advancing by exactly one.
4. **Mutation.** With the `AND version = ?` predicate removed, test 1 goes red (both updates succeed);
   with the trigger removed, test 3 goes red. Both logs retained beside the PR evidence.
5. **Listing.** Fixtures where `tag_mode="all"` and `"any"` yield different counts; `key_prefix` with a
   key containing `%` and `_`; `key_prefix=p` over the keys `p`, `p` + `"a"`, `p` + U+10FFFF,
   `p` + U+10FFFF + `"x"` and `p`'s successor, returning the first four and not the fifth, repeated
   with a prefix ending in U+10FFFF and with a prefix ending in U+D7FF, and with the successor
   computation stubbed to the `CHAR(0x10FFFF)` form as the mutation arm that must go red;
   `updated_after` inclusive on a boundary timestamp, with an older document
   updated after the cutoff appearing first; two documents with equal `updated_at` ordered by `key DESC`;
   each walked across at least two pages with the keyed cursor, and the page set equal to the unpaged set;
   a fixture mixing unkeyed and keyed notes where `key_prefix=""` returns only the keyed ones and
   `updated_after` alone returns both in insertion order.
6. **Fence.** A write with `fence` at the right generation succeeds; at a stale generation, or when the
   fence row is missing, it fails with `fence_conflict` and neither row changes. Cross-process, as in 1.
   The cross-process compare-and-set control in 1 is new acceptance written for this ADR; no existing
   test is cited as already covering it.
7. **Already-stale client.** B commits at version N before A sends `update(expected_version=N)`; A gets
   `version_conflict` with `current_version: N+1` and writes nothing. The runtime's internal snapshot
   guard is not the check here; the caller's precondition is.
8. **Lost acknowledgement.** A's update at `expected_version=N` commits and A never sees the response; A
   retries the identical request. The retry gets `version_conflict` with `current_version: N+1`, and the
   row advanced exactly once. Absent-key create races the same way: two `create(key=K,
   expected_version=0)` from two processes, one success, one `key_conflict`, one live row.
9. **Migration.** Upgrading a populated pre-028 database leaves every existing row at `version = 1` with
   `key IS NULL`, and the index creation succeeds with duplicate `name` values present.
10. **Durability option.** A deployment started with `synchronous = "full"` reports it in `db_diagnostics`;
    the verb-level cost of `create` under each of the three settings is measured over the socket, 1,000
    calls each, and recorded in the PR.
