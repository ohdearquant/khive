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
    FOREIGN KEY (note_id) REFERENCES notes (id)
);

CREATE TRIGGER IF NOT EXISTS refuse_stream_foreign_note
BEFORE INSERT ON note_streams
WHEN NOT EXISTS (SELECT 1 FROM notes
                 WHERE id = NEW.note_id AND namespace = NEW.namespace AND deleted_at IS NULL)
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;

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
(`crates/khive-db/src/pool.rs`), ties each row to an existing note and refuses a rewrite of that note's
`id` (`notes.id` is the table's primary key; `namespace` is a separate column with no unique key of its
own, so the namespace match is the trigger's job), and `refuse_stream_foreign_note` requires the note
to be live and in the ledger row's namespace. No backfill: existing notes belong to no stream.

Because the entry is a note it inherits the writer, the gate and admission path, the `version` column
and trigger from ADR-172, `search`, and the `list(after=)` cursor walk over `notes_seq`, where it appears
in store insertion order like any other note (ordinary unkeyed pages order by `created_at DESC, id ASC`,
as today). The ordered surface is the verb family below. This ADR defines no audit event of its own:
today's note create emits none on the generic path, and a stream append is the same write. The
dispatch layer's per-call gate audit row (one per verb call whenever an event store is configured,
`crates/khive-runtime/src/pack.rs`) applies to the stream verbs as to every other verb, refused calls
included, and is not this ADR's event.

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
    Clarification (2026-09-09): ADR-172 Amendment 3 adds a non-empty ordered list of fence objects;
    list refusals include a zero-based string `details.index`, including for a one-element list.
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
- Two triggers on `notes` and three on `note_streams`, plus a foreign key. The `notes` triggers' `WHEN`
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
   `seq_conflict` carrying `next_seq = 2`; the note count and the ledger count are unchanged from before
   the call. The audit population is read as domain events only, excluding the dispatch layer's
   per-call gate row, which the refused call does write; that domain count is a control that must stay
   at zero delta (§1: the generic note create emits no domain audit event today, so a nonzero delta
   here means a writer this ADR does not know about). `append(expected_seq=2)` then succeeds with
   `seq = 2`.
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
   the head plus two, an insert naming a `note_id` absent from `notes`, one naming a note that exists
   in another namespace, and one naming a note whose `deleted_at` is set, all fail as constraint or
   trigger errors. `stream.stat.count` still equals `head_seq` after all of them.
8. **Mutation.** With the `expected_seq` predicate removed, test 3 goes red; with the fence check removed,
   test 4 goes red; with the note insert committed before the ledger insert instead of in one
   transaction and the ledger insert forced to fail, test 3's unchanged-count assertion goes red; with
   any one of the five triggers, the `CHECK` or the foreign key dropped, the corresponding arm of test
   7 goes red. All logs retained beside the PR evidence.
9. **Migration.** The migration applies to a populated store and to an empty one; no existing note joins
   a stream; a database at the previous version migrates and passes tests 1 and 7.
10. **Batch order.** A `|` chain of three appends to one stream returns `seq` 1, 2, 3 in that order; a
    `[...]` of three appends to one stream returns the set `{1, 2, 3}`, and the test asserts the set, not
    the order.

## Related

- ADR-007 (namespaces), ADR-088 (note kinds), ADR-172 (`version`, `key`, `fence`, durability), migration
  V7 (`notes_seq`) and V13 (list cursor ledgers).

## Amendment 1 (2026-09-08): `stream.batch`, one request over several streams, all-or-nothing under a fence

**Status**: Proposed.

### The gap

§5 says a request array of appends is dense but unordered, and that a caller wanting order on one
stream sends a chain or carries `expected_seq`. A state layer built on streams needs a third shape
that neither the array nor the chain gives: one request that appends to several streams in the
order written, writes a keyed document beside them, checks the caller's authority once, and either
commits every member or writes nothing. A chain aborts the remainder after a failure but keeps what
already committed; an array commits siblings but assigns numbers in admission order and returns
nothing about a member's place in the caller's list. Neither form can be composed into the third by
the caller, because the fence is per transaction and checking authority once is the point.

The consumer's own state layer carries two batch forms, and its suite proves both. Its fenced form
takes an allow-listed member set, opens one transaction, checks every version the caller observed
before the first write, and rolls the whole batch back on any member's failure, raising to the
caller; its executed tests assert that after a foreign or stale fence neither the head nor the
stream record landed, and that every observation check precedes every insert. Its production caller
issues an expiry batch and ignores the return list, which is only sound because a conflict cannot
leave a partial commit. Its unfenced form is the opposite: each member owns its connection, a
failing member returns as that member's value, and its siblings stand. The conformance case for the
unfenced form (two appends to one stream, a read of an unknown object, an unknown verb and a head
write):

```text
res = store.batch([
  ("append", {"stream": "b", "record": {"n": 1}}),
  ("append", {"stream": "b", "record": {"n": 2}}),
  ("get", {"ref": "0" * 64}),
  ("nope", {}),
  ("write_head", {"key": "h", "kind": "job", "doc": {}}),
])
assert [r.seq for r in res[:2]] == [1, 2]
assert isinstance(res[2], NotFound) and isinstance(res[3], Refused) and res[4].version == 1
assert [e.record["n"] for e in store.read("b")] == [1, 2]
```

Both forms are load-bearing, so the verb carries both and names which one it is running.

### A1.1 `stream.batch`

**Implementation (2026-09-09):** Object fences and version observations execute in the atomic writer transaction; keyed `write` members execute in either batch mode.

`stream.batch(ops, fence=None, observed=None, atomic=None, namespace=None)` takes a list of member
operations, each `{"op": "append", "stream": S, "record": R, "expected_seq": N | null}` or
`{"op": "write", "key": K, "kind": <note kind>, "doc": D, "tags": [...] | null, "embed": bool | null,
"expected_version": V | null}` (the keyed document write of ADR-172 §2 and §3; `tags` and `embed` as
its Amendment 2 defines them, `embed` defaulting by the note kind). Common to both modes:

- Members are validated for shape before anything is written: a member that is not an object, a
  member without an `op` string, or a record over the note content limit refuses the whole batch
  with `KhiveError::invalid_input` and writes nothing. An op string that names no member operation
  is not a shape error: it is that member's refusal, `unknown_op`, and the mode below decides
  whether it stops the batch or returns as the member's value.
- Decision (2026-09-10): a batch names each `(kind, key)` write target at most once, in either
  mode. A repeated target is a shape error (`invalid_input`, naming the member index), not a
  per-member `key_conflict`: version observations are taken once before the first member runs, so
  a second write to the same key inside one request would either observe a stale version or
  conflict with its own sibling, and neither outcome is useful to a caller. Create-then-update is
  two requests, the second carrying the version the first returned.
  Clarification (2026-09-10): this once-per-`(kind, key)` target rule is shared with ordered fence lists in
  [ADR-172 Amendment 3](ADR-172-versioned-notes-compare-and-set.md): it applies independently to each
  fence list and to a batch's keyed-write members. The batch-wide `fence` deliberately remains
  object-only.
- A member refusal, wherever it surfaces, carries the ADR-172 §2 error shape plus
  `domain_disposition: not_committed` (ADR-133 Amendment 3), and a `key_conflict` names the holder
  as `existing_id` (ADR-179 D5), so the consumer rule of ADR-133 Amendment 3 reads it without a
  special case.
- Authority is checked once for the batch, on the caller's namespace, before the first write.
- Appends to one stream are numbered in list order, so a batch's numbers on one stream increase
  with list position; appends to different streams are independent. Adjacency is a property of
  the mode: an atomic batch's appends to one stream take consecutive numbers, because they are one
  transaction; a per-member batch's appends may have another process's append between them, and
  only the union is dense (§3).
- Reads (`get`, `stream.read`, `stream.stat`) are not members: a batch is a write primitive. The
  consumer's case reads an unknown object inside its batch; on khive that read is issued beside
  the batch by the adapter, and the assertion on its value is unchanged.
- The result is `{"results": [<member result>...], "committed": true}` in list order; a member
  result is the append's `{"seq", "id", "created_at"}` or the write's `{"id", "version"}`.

`atomic` selects the mode. It defaults to whether `fence` is present, so a fenced batch is atomic
unless the caller says otherwise, and the caller may not say otherwise: `atomic=false` with a
`fence` is refused as `invalid_input`, because a fence that admits partial commits is the failure
the consumer's production caller cannot detect. `atomic=true` without a fence is allowed.

**Atomic (fenced) mode.** The whole list runs inside one writer transaction. The `fence` is
ADR-172 §2b, evaluated once inside that transaction before the first write. `observed`, when
present, is a list of `{"key": K, "version": V}` the caller read before composing the batch; every
entry is checked inside the transaction before the first write, and a mismatch refuses the batch
with `version_conflict` naming the key. A member's own refusal (`unknown_op`, `seq_conflict`,
`version_conflict`, `key_conflict`) refuses the whole batch: the transaction rolls back, nothing
is written, and the error carries that member's refusal plus `member` (the list index); the
disposition is the `domain_disposition: not_committed` every member refusal already carries, and
no parallel boolean rides beside it. The caller may ignore the result list on success, because
success means every member committed.

Correction (2026-09-10): if a positive-version `write` member's prepared target is deleted and
recreated under the same `(kind, key)` before commit, the member refuses with `version_conflict`.
The atomic batch returns `member` and `domain_disposition: not_committed`, with no member writes
committed; the replacement does not turn this confirmed refusal into an unknown storage outcome.

Allocation (2026-09-10): a successful atomic batch reads each appended stream's initial head once
inside its writer transaction, then allocates that stream's sequences from a transaction-local
cache in member order. The cache is never reused across transactions; per-member mode retains
its independently admitted transactions and permits intervening writers.

**Per-member (unfenced) mode.** Each member runs in its own writer transaction, in list order, so
one stream's numbers increase with list position but need not be adjacent. A member's own refusal (`unknown_op`, `seq_conflict`,
`version_conflict`, `key_conflict`) is returned as that member's value and its siblings stand; the result is still
`{"results": [...], "committed": true}`, where `committed` says the request as a whole ran to the
end, and each member's outcome is its own entry. `observed` is refused in this mode
(`invalid_input`): an observation set is a precondition for a transaction, and there is none here.

The consumer's allow-list admits reads inside its fenced form. khive does not, and the two
observation needs it serves are covered without them: a version precondition is `observed`, and a
read-your-write inside one fence has no consumer today. If one appears, the members widen by a
further amendment; the adapter carries the divergence until then.

### A1.2 What does not change

§5 stands for the request array and the chain. `stream.append` alone is unchanged. The `write`
member depends on ADR-172's keyed create and versioned write (§2, §3) landing; the append-only
form of the batch does not, and implementation lands appends first and the `write` member with the
keyed write. The density
invariant (§3) holds inside an atomic batch by construction and across a per-member batch by
serial execution under the writer lock.

### Acceptance

1. **Per-member order and values.** The conformance case above, unfenced, with the read issued
   beside the batch: `results[0].seq == 1`, `results[1].seq == 2`, the unknown-op member refused
   as a value (`unknown_op`, `domain_disposition: not_committed`), the document write at version 1, and `stream.read("b")` returning records 1 then 2.
2. **Atomic refusal writes nothing.** Under a fence, a batch of three appends where the second
   carries a stale `expected_seq` is refused as a whole with `seq_conflict` and `member: 1`; the
   note count, the ledger count and every named stream's head are unchanged; the audit population
   is read as domain events only. The same for a stale fence and for a missing fence row.
3. **Observed before the first write.** A fenced batch carrying `observed` with one stale version
   is refused with `version_conflict` naming that key and writes nothing; a control with current
   versions commits every member. The statement trace shows every observation check before the
   first insert.
4. **Per-member refusal keeps siblings.** Unfenced, the same three appends return `seq_conflict`
   as the second member's value and commit the first and the third with consecutive numbers; the
   stream reads two records.
5. **Authority once.** A batch whose caller lacks write authority on the namespace is refused as a
   whole before any member runs, in both modes; a control with authority and the same members
   commits.
6. **Two processes, atomic.** Two processes issue atomic batches to one stream concurrently; each
   batch's appends are consecutive within the batch, and the union of numbers is dense.
7. **Two processes, per-member.** Two processes issue per-member batches to one stream concurrently
   across repeats; the union is dense and each batch's numbers increase in list order, and the test
   does not assert adjacency. A control counts the repeats in which another process's number fell
   between two members of one batch and requires at least one, so the arm is known to exercise the
   interleaving it permits.
8. **Mode arguments.** `atomic=false` with a `fence`, and `observed` without atomic mode, are each
   refused as `invalid_input` and write nothing; `atomic=true` without a fence behaves as arm 2.
9. **Mutation.** With atomic refusals demoted to member values, arm 2 goes red; with `observed`
   checked after the first write, arm 3's unchanged-count assertion goes red; with per-member
   refusals promoted to whole-batch refusals, arm 4 goes red; with per-stream numbering assigned
   outside the transaction, arm 6 goes red; with per-member mode run as one transaction, arm 7's
   interleaving control goes red.

## Amendment 2 (2026-09-08): the ledger refuses updates, an isolating fixture for the check constraint

Adopted on the source audit that preceded the first implementation of Amendment 1. Nothing above is
withdrawn; item 1 adds a guard the Decision claimed by implication, item 2 corrects an acceptance
arm that could not fail.

1. **No update on the ledger.** The schema guards `note_streams` against inserts that break order or
   membership and against deletes, and says nothing about updates, so a direct `UPDATE note_streams`
   could move a `seq` or rebind a `note_id` past every invariant claimed above. A sixth trigger
   closes it, in the same migration as the rest of the streams schema:

```sql
CREATE TRIGGER IF NOT EXISTS refuse_stream_ledger_update
BEFORE UPDATE ON note_streams
BEGIN
    SELECT RAISE(ABORT, 'stream_member');
END;
```

A ledger row is immutable once written; the only write to `note_streams` is an append. The
membership trigger `refuse_stream_foreign_note` also refuses an insert naming a `note_id` already in
the ledger (`OR EXISTS (SELECT 1 FROM note_streams WHERE note_id = NEW.note_id)` in its `WHEN`),
because `INSERT OR REPLACE` resolves the `UNIQUE (note_id)` conflict by deleting the old row without
firing the delete trigger while recursive triggers are off, which would re-seat an entry at a new
`seq` past every guard above. Acceptance 7 gains, in its direct-statement list, an
`UPDATE note_streams` that changes `seq` or `note_id` and an `INSERT OR REPLACE` naming a member's
`note_id` at the next `seq`, each leaving count and head unchanged; acceptance 8 counts six triggers.
2. **The check constraint is overdetermined for `seq = 0`.** `refuse_stream_gap` refuses `seq = 0`
on its own, because zero is never one more than the head, so dropping `CHECK (seq > 0)` alone
leaves acceptance 7's `seq = 0` arm green and proves nothing about the check. The mutation arm for
the check drops `refuse_stream_gap` as well: with both removed the `seq = 0` insert succeeds
(red); with only the trigger removed it still refuses through the check (control). Acceptance 8
reads accordingly. The check stays in the schema as the one guard that does not depend on a head
read.
3. **Count and head are read from one snapshot.** `stream.stat` reads `COUNT(*)` and `MAX(seq)` in
one statement over the same rows, so the equality acceptance 7 asserts compares two readings of
one snapshot, and a divergence between them is a ledger defect, never a race between two reads.

## Amendment 3 (2026-09-08): entries are not embedded by default, prefix truncation, two acceptance corrections

**Status**: Proposed.

### The gap

§1 makes an entry a note, and the note path embeds every note it inserts with every registered
embedding model unless one model is named. That default is right for a note a caller wants back by
similarity and wrong for a record stream: `stream.read` walks by sequence and never by similarity, and
the consumer this ADR serves appends one record per run event. On the reference deployment the vector
index segments of the busiest namespace were measured being rewritten every two to three minutes, about
440 MiB a cycle, under ordinary write load (#2446); a recorder stream would multiply that without a
single reader ever asking for a stream entry by vector. §3 and §6 leave retention to a later ADR; the
consumer's state layer now names its recorder as the largest caller, so the retention shape has to exist
before that recorder moves.

Two acceptance items are corrected here on findings from the first implementation of Amendment 1.

### A3.1 `embed` on `stream.append` and on batch append members

`stream.append(..., embed=false, embedding_model=None)`. `embed` defaults to `false`. An entry appended
with `embed=false` gets no embedding rows and no vector-index work, and is never a candidate for
similarity `search` or `recall`; lexical indexing and `list` are unchanged, so it stays findable by text
and by walk. With `embed=true` the entry is embedded exactly as `create(kind="note")` embeds: every
registered model, or the one `embedding_model` names. `embedding_model` without `embed=true` is refused
as `invalid_input` and nothing is written. A `stream.batch` append member carries the same two fields
with the same defaults, and the keyed `write` member carries `tags` and `embed` as ADR-172 Amendment 2
defines them (A1.1's member shape lists both), `embed` defaulting to `false` for the `head` kind and
`true` for every other kind.

Preparation (2026-09-10): eligible embedded append and keyed-create members are prepared together
before the member write transactions: one embedding request per selected model within its supported
batch size, bounded chunks above that size, and one shared vector-schema writer acquisition across
those models. This adds no aggregate admission limit. Positive-version keyed updates retain their
separate preparation; embedding defaults and model selection are unchanged.
With no eligible embeddings, this preparation invokes no embedding provider or vector-schema writer.

The note's content limit, the audit event and the ledger row are unchanged; an unembedded entry is a
whole entry in every respect this ADR defines.

### A3.2 `stream.truncate`

`stream.truncate(stream, before_seq, namespace=None)` removes every entry of the stream whose `seq` is
below `before_seq`, ledger row and note together, as a hard delete inside one writer transaction, and
returns `{"removed": N, "floor_seq": F, "head_seq": H}`. Numbering continues from `head_seq`; density
holds from `floor_seq`; `expected_seq` is unaffected because the head does not move. The call is
idempotent and never fails for a range: `before_seq` at or below the floor removes nothing, `before_seq`
above `head_seq + 1` is clipped to it, and an unknown stream returns `removed: 0, floor_seq: 1,
head_seq: 0`. Write authority on the namespace is required, as for `append`. One audit event records the
call with `stream`, `before_seq` and `removed`, not one per entry.

The floor is persisted, not derived: one row per truncated stream in
`note_stream_floors(namespace, stream, floor_seq)`, written in the truncate transaction, so a stream
emptied by truncation still knows where its numbering stands, which `MIN(seq)` cannot say. `stream.read`
and `stream.stat` gain `floor_seq` (`1` when never truncated). A reader whose `after + 1` is below the
floor receives entries from the floor and learns from `floor_seq` that a prefix it never saw is gone.
That is the only truncation signal and it is enough: a reader that must not miss entries reads before
the writer truncates, and arranging that is the retention policy's job, not khive's. `count` becomes
`head_seq - floor_seq + 1`, still read from the one snapshot Amendment 2 item 3 requires; §2's
`count == head_seq` holds exactly for a stream never truncated.

§3's triggers refuse every delete of a member note and Amendment 2's ledger guards refuse every ledger
delete; truncation is the one authorized path, and its authorization is a row, not a bypass. The
truncate transaction first inserts `(namespace, stream, before_seq)` into `note_stream_truncations`; the
ledger delete trigger's `WHEN` exempts a row whose `(namespace, stream)` has an open truncation with
`seq < before_seq`; the transaction deletes the ledger rows, then the notes (whose member trigger no
longer fires, the ledger rows being gone), then the truncation row, and commits. A direct `DELETE`
outside a truncate transaction sees no truncation row and is refused as today; a truncation row cannot
outlive its transaction. Embedding and lexical rows of the removed notes go the way any hard delete takes
them.

Retention policy stays outside: which streams to truncate, at what age or count, is decided by the
layer that named the streams (a scheduled job or the consumer itself); khive does not decide what to
keep. Drop of a whole stream remains out of scope: truncating to `head_seq + 1` leaves an empty stream
with its floor, which is exactly what a later reader needs to see.

### A3.3 Amendment 1 acceptance 9, corrected

Acceptance 9 says that with `observed` checked after the first write, arm 3's unchanged-count assertion
goes red. It does not: the transaction still rolls back on the conflict and the counts stay unchanged
whichever order the statements ran in. What reddens is arm 3's statement-trace assertion, that every
observation check precedes the first insert. Acceptance 9 reads accordingly: the trace assertion is the
order control and the unchanged-count assertion is the rollback control, two controls, not one.

### A3.4 The check is inside the transaction, proven by mutation

The property the consumer's state layer depends on is that a stale generation writes nothing, and that
property lives in the transaction boundary, not in the fence's shape. For `stream.append` with a
`fence` and for an atomic `stream.batch` with a `fence` or `observed`, the check is evaluated inside the
writer transaction before the first insert. Mutation arm: move the check outside the transaction (read
the version, then begin, then insert) and the stale-fence and stale-observed arms must go red, meaning
something was written or a count moved; restore, and they go green. Both runs are quoted with exit
codes. ADR-172 Amendment 2 records the same arm for `expected_version` and `fence` on documents.

### Acceptance

1. **No embedding by default.** After a default `stream.append`, the entry's note has no embedding row
   for any registered model and the vector-index queue is empty; the `embed=true` control yields one
   row per registered model, the count named.
2. **Not a similarity candidate.** The unembedded entry's own content as a `search` query returns no
   hit for it; the `embed=true` control returns it.
3. **Still listed and found by text.** The default entry is returned by `list` and by lexical search.
4. **Truncate keeps numbering.** Five appends, `truncate(before_seq=3)`: `removed 2, floor_seq 3,
   head_seq 5`; `stat` count 3; `read(after=0)` returns 3, 4, 5 with `floor_seq 3`; the next append
   is 6.
5. **Whole prefix.** `truncate(before_seq=6)` on the same stream: `removed 3, floor_seq 6, head_seq 5`,
   count 0, the next append is 6.
6. **Idempotent and clipped.** Repeating arm 5 removes 0; `before_seq=100` on a five-entry stream
   removes 5 and reports `floor_seq 6`; an unknown stream removes 0 with `floor_seq 1, head_seq 0`.
7. **Guards unchanged.** Outside a truncate, a direct delete of a member note or ledger row is refused
   as in §3 and Amendment 2, count and head unchanged.
8. **Atomic.** A failure injected between the ledger delete and the note delete leaves count, floor and
   head unchanged.
9. **Mutation.** With `embed` ignored, arm 1's two counts read the same (red). With the floor derived
   from `MIN(seq)` instead of persisted, arm 5's `floor_seq` reads 1 (red). With the truncation row
   left in place after commit, a later direct ledger delete succeeds (red), which is what proves the
   row is the authorization.

## Amendment 4 (2026-09-09): an `observed` entry may assert that a key is unheld

**Status**: Proposed.

**Implementation (2026-09-09):** `observed` entries with a version or `null` are checked inside the atomic writer transaction before member writes.

### The gap

Amendment 1 A1.1 gives an atomic batch an `observed` list of `{"key": K, "version": V}` entries, each
checked inside the transaction before the first write. An exact version says "this key is held, at this
version". It cannot say "this key is not held", and the consumer's state layer needs exactly that.

Its publication fence has three forms beside the lease generation. Two of them are versions of a live
row and map onto `observed` as written: a claim that a `(run, holder, generation)` is a live lease
becomes one entry per claim, and a release claim is the same entry on the release key. The third does
not. A handle opened while a run had no lease publishes under the predicate _the lease is still absent,
or it is still held by the holder this handle captured_. Its suite asserts that a foreign lease
appearing after the handle opened refuses the publication and writes nothing. An exact-version entry
cannot express the absent half, and the caller cannot decompose the disjunction by sending two batches:
the point of the fence is that the predicate and the writes share one transaction.

### A4.1 `version: null`

An `observed` entry is `{"key": K, "kind": <note kind>, "version": V | null}`.

`kind` completes A1.1's two-field spelling rather than leaving it to the implementation. A key is
unique among live notes of one kind in one namespace (ADR-172 §3), so a key alone does not name a row:
without `kind` an entry either resolves ambiguously, or it reads across kinds and lets a note some other
pack keyed the same way refuse a batch it has nothing to do with. `fence` (ADR-172 §2b) already carries
the kind for the same reason, and the batch's own `write` member names one, so this is the shape the
rest of the surface already uses.

- `version: V` is unchanged: the entry holds when a live note of that kind holds `K` in the caller's
  primary namespace at exactly version `V`. A missing row or a different version refuses the batch with
  `version_conflict` naming the key, as A1.1 says.
- `version: null` holds when **no** live note of that kind holds `K` in that namespace. A live holder at
  any version refuses the batch with `version_conflict`, `details` naming the key and the holder's
  `current_version`; nothing is written. A soft-deleted note has released its key (ADR-172 §3), so it
  does not hold it here either.

Everything else about `observed` stands: it is atomic mode only, refused with `invalid_input` in
per-member mode, and every entry is checked inside the writer transaction before the first write.

### A4.2 The disjunction decomposes on the caller's side

The caller reads the lease before composing the batch, so by the time it composes it has observed one of
two concrete states, and it sends the entry for the state it saw: its own generation, or null. What the
batch has to guarantee is not the disjunction but that the state it observed still holds at the write.
A holder that appears between the read and the batch refuses the null entry; a holder that renews
between them refuses the version entry; a lease released between them refuses the version entry, and the
caller re-reads and re-composes. That is the same contract every other `observed` entry has, so `null`
adds a value, not a rule.

No `key_conflict` case is added. A `write` member that creates a key another live note holds still
refuses with `key_conflict` and `existing_id` exactly as A1.1 says; an `observed` entry never creates
anything, so it can only ever produce `version_conflict`.

### Acceptance

Every arm names its command; the atomic-mode counts are read as domain events only, as in Amendment 1
acceptance 2.

1. **Unheld and observed unheld.** A batch carrying `{"key": K, "kind": <k>, "version": null}` for a key
   no live note holds commits every member; the stream heads move by exactly the members' appends.
2. **Held and observed unheld.** With a live note holding `K` at version 1, the same batch is refused
   with `version_conflict` naming `K` and `current_version` 1; the note count, the ledger count and
   every named stream's head are unchanged. Repeated with the holder at version 5, to show the refusal
   does not depend on the version being the initial one.
3. **Kind is part of the key.** A live note of kind `A` holding `K` does not refuse an entry naming
   kind `B` and the same `K`; the batch commits. The control is arm 2 with the kinds equal.
4. **Released key.** A soft-deleted note that held `K` does not refuse a null entry; a hard-deleted one
   does not either. The control is arm 2 with the note live.
5. **Mixed list.** One batch carrying a null entry and a version entry commits when both hold, and is
   refused naming the offending key when either does not, in both directions, with nothing written.
6. **Cross-process.** Arms 1 and 2 through the socket, with the holder created by a second OS process
   between the caller's read and its batch, so the refusal is a real race and not a self-inflicted one.
7. **Mutation.** With a null entry treated as no check at all, arm 2 goes red. With a null entry
   compiled as `version = 0`, arm 1 goes red. With the null check moved after the first insert, arm 2's
   statement-trace assertion goes red (Amendment 3 A3.3: the trace is the order control, the unchanged
   counts are the rollback control). Both runs quoted with exit codes.

## Amendment 5 (2026-09-10): an `observed` entry may assert that a document's time field is still in the future, and write members return their write time

**Status**: Proposed.

**Implementation (2026-09-10):** `live_until` checks share one SQL clock reading inside the atomic writer transaction; write results return the stored `updated_at` before commit.

### The gap

Amendment 4 lets a caller pin what it read: an `observed` entry with a version holds only while the
document it checked is still the document there, because a keyed note's version moves on every write
to it. That closes every race in which the competing writer changes the document. It does not close
the one in which nobody writes. A lease that merely runs out changes no document, so it bumps no
version, so every observation still holds, and a batch composed under a lease that expired between the
caller's read and the commit is written as if the lease were live. The consumer's own store evaluated
the lease's expiry against the clock of the transaction doing the writing; the khive route evaluates
it against the clock of the read that preceded the request, and the window is the request itself. Its
suite has the arm: a lease written to expire in one second, the batch dispatched after that second
with no writer touching the head, and the contract's stale-generation refusal expected. Today the
batch commits.

The caller cannot close this by reading again, because the read and the write would still be two
transactions. The check has to run on the writer's clock inside the transaction that writes, which is
where the other `observed` checks already run. The document's shape is the caller's, so the caller
names the field.

### A5.1 `live_until`

An `observed` entry may carry `live_until`:

```json
{"key": K, "kind": <note kind>, "version": V, "live_until": "<field path>"}
```

- `live_until` names a field in the observed note's document (the keyed `doc` of ADR-172 §3, as
  written) by a dotted path, `"expires_at"` or `"lease.expires_at"`. The entry holds when the live
  note of that kind holding `K` in the caller's primary namespace is at exactly version `V` **and**
  the named field holds an RFC 3339 timestamp strictly later than the transaction's clock.
- The transaction's clock is one reading, taken by one statement inside the writer transaction after
  it opens and before the first `observed` check, and shared by every entry in the list. It is the
  same source that stamps the row's `updated_at` (A5.2), so the reading appears in the statement trace
  of Amendment 3 A3.3 and the `now` an `expired` refusal carries is on the clock khive itself writes.
  It is never the caller's clock, never the request's arrival time and never a process clock read
  outside the transaction.
- A field that is absent, or whose value is not an RFC 3339 timestamp, refuses the batch with
  `live_until_unreadable`: kind `conflict` (§2, ADR-172 §2), `reason: "live_until_unreadable"`,
  `details` naming the key, the kind, the version, the field and the value found; nothing is written.
  A value without an offset is not RFC 3339 and is unreadable. A liveness the entry cannot read is not
  a liveness it may assume.
- A timestamp at or before the transaction's clock refuses the batch with `expired`: kind `conflict`,
  `reason: "expired"`, `details` naming the key, the kind, the version, the field, the value found and
  the clock reading (`now`) it was compared with, so the caller can see the window it lost; nothing is
  written. The comparison is between instants, the value's offset honoured.
- Both refusals are transaction-time precondition outcomes on store state, so they take the kind
  Amendment 1's `version_conflict` takes, and every `details` value is a string as ADR-172 §2
  requires: `key`, `kind` and `field` as given, `version` as its decimal string, `value` as the field's
  JSON text (a found value may be a number, an object or `null`, and is rendered as JSON), `now` in
  the form `updated_at` takes.
- `live_until` requires a version. With `version: null` the entry is `invalid_input` before any
  member writes: an absent document has no field to read.
- Everything else about `observed` stands: atomic mode only, refused with `invalid_input` in
  per-member mode, every entry checked inside the writer transaction before the first write, the
  version half refusing exactly as Amendment 1 and Amendment 4 say.

Correction (2026-09-10): an unreadable refusal does not carry the value. `live_until` is a
caller-chosen path into a document the caller named but need not be able to read, so echoing
whatever the path lands on turns one authorized `stream.batch` into a read of any field of that
document. `live_until_unreadable` therefore carries `value_type` in place of `value`, one of
`absent`, `null`, `boolean`, `number`, `string`, `array`, `object`, where `absent` is the path
resolving to nothing and `null` is a JSON null found at it; the two are distinguished, which is
what the earlier "the value found" left open for a field that is not there. `expired` keeps
`value`: reaching it requires the field to have parsed as an RFC 3339 timestamp, so the value it
names is the deadline the entry pinned, and the caller needs it beside `now` to see the window it
lost. Acceptance arm 3 reads `value_type` and asserts `value` is absent.

No predicate on the field's meaning is added. khive compares one timestamp with one clock; whether the
field is a lease expiry, a handle deadline or anything else is the caller's convention, as the fence
key's convention is (§2 alternatives, "the layer that owns the convention passes the fence").

### A5.2 `updated_at` on write member results

A `write` member's result is `{"id": <id>, "version": <v>, "updated_at": <timestamp>}`, for a create
and for an update alike, where `updated_at` is the write's own time as the note row records it in that
transaction (ISO 8601 with microseconds, the form `created_at` already takes on append results). The
consumer's head record carries the write's own timestamp, and without this field the caller has to
read the head back and may only trust the time while the version it finds is still the one it wrote.
A result field is a value; a later writer moving the version does not change it.

### A5.3 The `observed` help names its mode

The `observed` parameter's own help states that it requires atomic mode and that supplying `observed`
alone does not select it: `atomic` defaults to whether `fence` is present, and an `observed` list
without `fence` and without `atomic=true` is refused with `invalid_input` naming the mode. The
`atomic` help line says the same from its side. The behaviour is unchanged; the sentence was missing
where the caller reads.

### Acceptance

Every arm names its command; atomic-mode counts are read as domain events only, as in Amendment 1
acceptance 2.

1. **Expired.** A keyed note holding `K` at version 1 whose document has `expires_at` one second in
   the future; a batch carrying `{"key": K, "kind": <k>, "version": 1, "live_until": "expires_at"}`
   dispatched after that second, with no writer touching the note, is refused with `expired`,
   `details` naming `K`, `expires_at`, the value and `now` later than it; the note count, the ledger
   count and every named stream's head are unchanged. The control is the same batch dispatched
   before the second elapses, which commits every member.
2. **Live and pinned.** With the field an hour in the future, the entry holds at version 1 and is
   refused with `version_conflict` at version 2 after one intervening write; the version half is
   unchanged by the presence of `live_until`.
3. **Unreadable.** The same batch against a document with no `expires_at`, then against one whose
   `expires_at` is the string `"soon"`, is refused with `live_until_unreadable` naming the field and
   the value found, nothing written. The control is arm 2's live document.
4. **Dotted path.** `live_until: "lease.expires_at"` reads a nested field; the control is the flat
   path on the same document.
5. **Null version.** `live_until` with `version: null` is `invalid_input` before any member writes.
6. **Mixed list.** A version-only entry and a `live_until` entry in one list commit when both hold
   and refuse naming the offending key when either does not, in both directions, nothing written.
7. **The writer's clock.** Arm 1's refusal carries `now`; the statement trace (Amendment 3 A3.3)
   shows the clock read inside the writer transaction, after the transaction opens and before the
   first `observed` check, and the same reading serves every entry in the list.
8. **`updated_at`.** A `write` member's result carries `updated_at`; a read of the note immediately
   after shows the same value; a second writer moving the version does not change the first result.
   Both create and update.
9. **Help.** `stream.batch(help=true)` names the atomic-mode requirement under `observed` and the
   `live_until` shape; `observed` without `fence` and without `atomic=true` is refused naming the
   mode.
10. **Cross-process.** Arms 1 and 2 through the socket, the expiry elapsing while a second OS
    process holds the note, so the refusal is a real window and not a self-inflicted one.
11. **Mutation.** With `live_until` treated as no check, arm 1 goes red. With the clock taken from
    the request's arrival time instead of inside the transaction, arm 7's trace assertion goes red.
    With an unreadable field treated as live, arm 3 goes red. With `updated_at` omitted, arm 8 goes
    red. Each run quoted with its exit code.
