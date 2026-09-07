# ADR-174: Ordered streams — dense per-stream append with `expected_seq` and a lease fence

- Status: Proposed
- Date: 2026-09-07
- Depends on: ADR-172 (`version`, `key`, `fence`). It merges first; the migration here takes the number after
  ADR-172's.

## Context

A state layer that records a run keeps two shapes of data: documents that are overwritten (a run's
lease, its latest status) and records that are only ever appended (what the run did, in order). ADR-172
gave documents compare-and-set, a unique key and a lease fence. This ADR gives the appended shape its
own primitive.

The contract such a layer needs from its store, read from a reference implementation on SQLite: every
stream numbers its records densely from 1; an append may be conditioned on the number the new record will
receive, and fails without writing when another writer got there first; an append may be fenced on a
lease document's revision, checked inside the same transaction as the insert; a read returns the records
after a given number, in order, with no gaps; two processes appending to one stream interleave without
sharing or skipping a number.

khive has no verb for this today. What exists is close but is not it:

- `notes_seq` (V7) is a durable, never-reused `AUTOINCREMENT` ledger assigned in the same transaction as
  the note insert, and `list(after=)` walks it. It is global to the store, not per stream, and it is not
  dense: a hard delete leaves a hole, and two streams interleave in it.
- Events are the audit plane. `create(kind="event")` is refused; there is no public append and no
  replay contract, and turning an observability record into an application transcript would make the
  audit plane depend on caller-written content.
- A version query followed by a `create` is not an append: two callers can both read the same last
  number and both insert.

The single-writer task (`with_writer_tx`, `BEGIN IMMEDIATE`) already serialises every note write in one
transaction, and ADR-172 §2b already defines a fence evaluated inside that transaction. The missing piece
is a per-stream sequence with its own uniqueness, assigned by the writer beside the note.

## Decision

### 1. A stream entry is a note plus a row in a per-stream ledger

A stream is a name, at most 512 bytes, no U+0000, scoped to one namespace like every other record
(writes pin to the writer's namespace unless `namespace=` names a visible one, ADR-007). An entry is an
ordinary note whose content is the appended record, joined to the stream by a new table:

```sql
CREATE TABLE IF NOT EXISTS note_streams (
    namespace TEXT    NOT NULL,
    stream    TEXT    NOT NULL,
    seq       INTEGER NOT NULL CHECK (seq > 0),
    note_id   TEXT    NOT NULL UNIQUE,
    PRIMARY KEY (namespace, stream, seq),
    FOREIGN KEY (namespace, note_id) REFERENCES notes (namespace, id)
);

CREATE TRIGGER IF NOT EXISTS refuse_stream_gap
BEFORE INSERT ON note_streams
WHEN NEW.seq != (SELECT COALESCE(MAX(seq), 0) + 1 FROM note_streams
                 WHERE namespace = NEW.namespace AND stream = NEW.stream)
BEGIN
    SELECT RAISE(ABORT, 'stream_gap');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_ledger_delete
BEFORE DELETE ON note_streams
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;
```

`seq` is assigned by the writer inside the transaction that inserts the note, as one more than the
stream's current highest `seq` (`0` for a stream with no rows). The single-writer transaction is what
makes two concurrent appends take consecutive numbers rather than the same one. The schema is the
backstop, and it holds against any writer, not only the verb: `refuse_stream_gap` admits exactly the
next number, so a duplicate, a skip and a zero all fail as constraint errors; the ledger cannot lose a
row; the foreign key, enforced because the pool turns `foreign_keys` on for every connection
(`crates/khive-db/src/pool.rs`), ties each row to a live note in the same namespace and refuses a
rewrite of that note's `id`. No backfill: existing notes belong to no stream.

Because the entry is a note it inherits the writer, the gate and admission path, the `version` column
and trigger from ADR-172, `search`, and the `list(after=)` cursor walk over `notes_seq`, where it appears
in store insertion order like any other note (ordinary unkeyed pages order by `created_at DESC, id ASC`,
as today). The ordered surface is the verb family below. This ADR defines no audit event: today's note
create emits none on the generic path, and a stream append is the same write.

### 2. Verbs

Three verbs, registered by the kg pack because the entries are its notes:

- `stream.append(stream, record, expected_seq=None, fence=None, note_kind="observation", tags=None, namespace=None)`
  inserts the note and the ledger row in one writer transaction and returns `{"seq": N, "id": <uuid>,
  "created_at": ...}`. `record` is a JSON value stored as the note's content; the existing note content
  limits apply unchanged, and this ADR adds none.
  - With `expected_seq=N`, the append succeeds only when `N` is the number this entry would receive.
    Otherwise it fails with `KhiveError::conflict`,
    `details: {"reason": "seq_conflict", "stream": S, "expected_seq": "N", "next_seq": "M"}`, and
    nothing is written: no note, no ledger row. `M` is read inside the same request so the caller's next
    attempt can carry it. Refusals in this ADR use the error shape ADR-172 §2 defines: `kind`
    `conflict`, `code` unchanged, the discriminator in `details.reason`, every value a string.
  - `fence` is ADR-172 §2b unchanged: `{"key": K, "kind": <note kind>, "expected_version": G}` names a
    keyed note whose `version` must equal `G`, checked before the insert in the same transaction; a
    missing row or a different version fails with `details.reason = "fence_conflict"` and nothing is
    written. The fence is
    checked only when supplied. Whether an unfenced append to a stream that belongs to a leased run
    should be refused is policy above khive: the layer that knows which streams belong to which run
    decides it, and passes the fence when it applies.
- `stream.read(stream, after=0, limit=1000, namespace=None)` returns
  `{"entries": [{"seq", "id", "record", "created_at"}...], "head_seq": H, "next_after": N | null}`:
  the entries with `seq > after` in ascending `seq`, at most `limit` of them, straight off the primary
  key. `head_seq` is the stream's highest `seq` at the time of the read (`0` when the stream has no
  rows), so a reader knows whether it is caught up without a second call; `next_after` is the last
  returned `seq` when `head_seq` is beyond it, and `null` when the page reached the head. A stream with
  no rows reads as empty, not as an error: an empty stream and an unknown stream are the same thing.
- `stream.stat(stream, namespace=None)` returns `{"head_seq": H, "count": H}` for the same reason
  `blob.stat` exists: the head without the bodies. `count` equals `head_seq` by construction (§3) and is
  reported so that a divergence, which would mean the density invariant broke, is visible from the
  outside.

### 3. Entries are immutable and streams are dense, enforced at the schema

Density means a number, once assigned, is never removed; §1's ledger constraints hold that side. An
entry's note therefore cannot be deleted, soft or hard, and its record cannot change. Rather than ask
every one of the existing delete and update sites to check membership, two triggers on `notes` close the
population the way ADR-172's trigger closed the `UPDATE` sites:

```sql
CREATE TRIGGER IF NOT EXISTS refuse_stream_entry_delete
BEFORE DELETE ON notes
WHEN EXISTS (SELECT 1 FROM note_streams WHERE note_id = OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;

CREATE TRIGGER IF NOT EXISTS refuse_stream_entry_rewrite
BEFORE UPDATE OF content, properties, deleted_at, namespace, kind ON notes
WHEN EXISTS (SELECT 1 FROM note_streams WHERE note_id = OLD.id)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;
```

The runtime maps that abort to `KhiveError::conflict`,
`details: {"reason": "stream_member", "id": <uuid>, "stream": S, "seq": "N"}`. `properties` is frozen
because that is where a note's tags live (`properties.tags`), so tags are part of the record and are
frozen with it; `name`, `salience` and `decay_factor` stay writable, they are scoring and display knobs
and not the record. A caller that wants to annotate an entry after the fact writes a separate note and
links it with `annotates`. The note upsert statement is `ON CONFLICT(id) DO UPDATE`, which fires the
update trigger; an `INSERT OR REPLACE` path would not fire the delete trigger with `recursive_triggers`
off, and there is none on notes today. The implementing PR asserts that by grepping the statements, and
the acceptance list carries the mutation arm.

Retention is not decided here: streams grow until a later ADR gives them a drop or a truncation, and a
truncation, when it comes, removes a prefix and keeps the numbering, so density from the first
remaining `seq` holds. The reference store this ADR serves has no drop either.

### 4. Reconciling an uncertain acknowledgement

A client that sent `stream.append(expected_seq=N)` and lost the reply retries the same call. Exactly one
of two things is true. The first attempt did not commit, and the retry succeeds with `seq = N`. Or it
did, and the retry fails with `seq_conflict`, `next_seq = N + 1` at least; the client then reads
`stream.read(after=N-1, limit=1)` and compares the record at `N` with what it sent. Equal records mean the
first attempt landed; a different record means another writer took `N`, and the client's own write did
not happen. That comparison is the client's, and it is exact only when the client's records are
distinguishable; a client that appends identical records and needs to tell them apart puts a token of its
own inside the record. This ADR does not add a server-side idempotency key: the sequence number plus the
readable record already give the client a lossless answer, and a second key would have to be stored,
indexed and expired.

Without `expected_seq` there is no reconciliation: the retry appends a second copy. That is the same
trade every append-only log makes, and the parameter is the way out of it.

### 5. Batches

`stream.append` composes with the request array like any verb. Inside one `[...]` the siblings run
concurrently and their numbers are assigned in the writer's admission order, which the request array does
not define; dense, but not in array order. A caller that needs its own order on one stream sends a `|`
chain, or carries `expected_seq` on each. This is ADR-172 §6 restated for streams, so that nobody reads
the array as an ordered transaction.

### 6. Out of scope

- Drop and truncation (§3).
- Cross-stream ordering: `seq` orders one stream; `notes_seq` still orders the store, and a reader that
  wants a global insertion order across streams walks `list(after=)` instead.
- Subscriptions and long polling; `head_seq` is the caught-up signal for a polling reader.
- A per-namespace or per-stream quota. The record size limit is the note's.

## Alternatives considered

| Alternative                                            | Why not                                                                                                                                                                                                        |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Expose events as the stream                            | Events are the audit plane. A caller-written transcript inside it makes audit integrity depend on application content, and `create(kind="event")` is refused for that reason today.                            |
| Per-stream `seq` from `notes_seq`                      | Global, not per stream; a hard delete of any note leaves a hole; two streams interleave. Dense-per-stream cannot be derived from a store-wide ledger without a full scan.                                      |
| A stream row without a note                            | Loses the writer, the gate, the audit event, `search` and `version` for free, and creates a second record kind that every existing tool ignores. The join costs one small table and gains all of it.           |
| Assign `seq` by an `AFTER INSERT` trigger, as V13 does | A trigger cannot evaluate `expected_seq` and refuse the enclosing insert with a structured error the runtime can map; the writer statement can, and the primary key backs it.                                  |
| Refuse unfenced appends on leased streams inside khive | Requires khive to know which stream belongs to which run, which is the caller's naming convention. ADR-172 §2b made the same call for documents; the layer that owns the convention passes the fence.          |
| A server-side idempotency token on `append`            | §4: `expected_seq` plus a readable record already reconcile losslessly; a token is a second index with an expiry policy for the case where the client cannot tell its own records apart, which it can arrange. |
| Allow delete of entries and renumber                   | Renumbering breaks every cursor held by every reader and makes `expected_seq` meaningless across a delete. Density with immutability is the contract; truncation of a prefix is the later, compatible shape.   |

## Consequences

- Notes gain a second insertion path that cannot be undone. A note that is a stream entry is the first
  kind of note in khive that `delete` refuses; tools that assume every note is deletable meet
  `stream_member` and must say so rather than retry.
- Two triggers on `notes` and two on `note_streams`, plus a foreign key. The `notes` triggers' `WHEN`
  clause is one indexed lookup on `note_streams.note_id` (`UNIQUE` gives the index) per delete or
  content update, the same order of cost as the ADR-172 version bump; the gap trigger is one primary-key
  seek per append.
- An entry's tags cannot change after append, because they live in `properties`. Annotation is a
  separate note.
- The per-stream `MAX(seq)` read inside the writer is a single index seek on the primary key
  `(namespace, stream, seq)`; it does not grow with the stream.
- Streams have no retention until a later ADR. A deployment that records long runs plans its disk on
  that basis. Durability is ADR-172 §5, unchanged: an appended entry is as durable as any other note
  under the deployment's `synchronous` setting.
- `expected_seq` is the only reconciliation path (§4). Clients that append without it accept
  duplicates on retry.

## Acceptance

Stated before implementation, checked at the PR that lands the code; every arm names its command.

1. **Dense from one, per stream.** Appends to two streams number each from 1 independently; a read of
   each returns the records in order with `seq` equal to `1..n`.
2. **Concurrent, cross-process.** Two processes append 50 records each to one stream through the socket;
   the union of returned `seq` values is exactly `1..100` with no repeat and no gap, and `stream.read`
   returns them in that order. In-process tasks alone do not satisfy this arm.
3. **`expected_seq` conflict writes nothing.** With one entry present, `append(expected_seq=3)` fails with
   `seq_conflict` carrying `next_seq = 2`; the note count, the ledger count and the audit event count are
   unchanged from before the call; `append(expected_seq=2)` then succeeds with `seq = 2`.
4. **Fence.** An append with `fence` at the right version succeeds; at a stale version, or when the fence
   row is missing, it fails with `fence_conflict` and neither the stream nor the fence row changes.
   Cross-process, as in 2.
5. **Ordered read and pagination.** A stream of 25 entries read with `limit=10` yields three pages whose
   concatenation equals the unpaged read, `next_after` equal to 10, 20 and then `null`, and `head_seq =
   25` on every page. An unknown stream reads as `entries: []`, `head_seq: 0`.
6. **Reconciliation.** Simulate a lost acknowledgement by committing an append and discarding its reply;
   the retry with the same `expected_seq` fails with `seq_conflict`, and `stream.read(after=N-1, limit=1)`
   returns the record the first attempt carried. A control where another writer took `N` first returns a
   different record.
7. **Immutability and density at the schema.** Through the verbs: `update` of an entry's `content` or
   `properties` and `delete` (soft and hard) of an entry fail with `stream_member`; `update` of its
   `salience` succeeds. Through direct statements against a migrated scratch database, because no verb can
   issue them: an `UPDATE notes` moving an entry's `namespace`, `kind` or `id`, a `DELETE FROM
   note_streams`, an `INSERT` into `note_streams` with `seq` equal to `0`, to the current head, or to
   the head plus two, and an insert naming a `note_id` absent from `notes`, all fail as constraint or
   trigger errors. `stream.stat.count` still equals `head_seq` after all of them.
8. **Mutation.** With the `expected_seq` predicate removed, test 3 goes red; with the fence check removed,
   test 4 goes red; with the note insert committed before the ledger insert instead of in one
   transaction and the ledger insert forced to fail, test 3's unchanged-count assertion goes red; with
   any one of the four triggers, the `CHECK` or the foreign key dropped, the corresponding arm of test
   7 goes red. All logs retained beside the PR evidence.
9. **Migration.** The migration applies to a populated store and to an empty one; no existing note joins
   a stream; a database at the previous version migrates and passes tests 1 and 7.
10. **Batch order.** A `|` chain of three appends to one stream returns `seq` 1, 2, 3 in that order; a
    `[...]` of three appends to one stream returns the set `{1, 2, 3}`, and the test asserts the set, not
    the order.

## Related

- ADR-007 (namespaces), ADR-088 (note kinds), ADR-172 (`version`, `key`, `fence`, durability), migration
  V7 (`notes_seq`) and V13 (list cursor ledgers).
