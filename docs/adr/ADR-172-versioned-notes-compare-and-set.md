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
- Thirty-two `UPDATE notes` statements live across eight files (`grep -rn "UPDATE notes" crates
  --include='*.rs'`, with `_tests.rs` files and `tests/` directories excluded): the note store, pending-events replay, gtd, memory, schedule,
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

Every existing row starts at 1. Every statement that touches a note row, today's thirty-two and any
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

Every refusal this ADR introduces uses the error shape the runtime can already emit. `KhiveError`
serialises as `{"kind", "message", "code", "details"}` where `code` is a numeric domain code such as
`runtime:10` or `null`, and `details` is at most eight string-to-string pairs. The refusals below are
`kind: "conflict"` (or `not_found` where stated), `code` unchanged from what the constructor gives today,
and the discriminator is `details.reason`; every detail value is a string, a list is comma-joined, and a
value that has no current row is omitted rather than written as null. A client that wants the
structured object reads `details` verbatim. The Python client in this tree passes a per-op error through
unchanged from the transport, but its typed result model admits only a string in the error field, so the
typed path cannot consume `details`; the change that types the error object as sent is a separate client
fix, in review beside this ADR, and this ADR's recovery contract is reachable from that client only once
it lands.

On mismatch the verb fails with `KhiveError::conflict` and
`details: {"reason": "version_conflict", "expected_version": "N", "current_version": "M"}`. `M` is read
after the failed statement, inside the same writer request, so the caller's next attempt can carry it. A `version_conflict` performs no
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
`details: {"reason": "fence_conflict", "key": K2, "expected_version": "G", "current_version": "M"}`
(the last pair omitted when no row holds `K2`), and nothing is written. The fence is checked only when supplied; whether a write without a fence should be
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
inserts the note or fails with `KhiveError::conflict`, `details: {"reason": "key_conflict", "key": K,
"existing_id": <uuid>}` when a live note already holds `K`; the index is what refuses, so two racing
creators get exactly one success. `get(kind="note", key=K)` resolves by key within the caller's primary
namespace, the same scope rule as prefix resolution in ADR-007. Uniqueness is per note kind, so `K` may
be held by one live note of each kind; `get(kind="note", key=K, note_kind=X)` selects one, and a lookup
without `note_kind` that matches more than one kind fails with `KhiveError::conflict`,
`details: {"reason": "key_ambiguous", "key": K, "kinds": "observation,decision"}` rather than returning
either. `key` is
immutable after create; `update` does not accept it. A soft-deleted note releases its key; a hard delete
does too.

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

A keyed listing also accepts `after_key=K` in place of `after`: the server resolves the live note holding
`K` in the listing's namespace and note kind, takes its current `(updated_at, key)` as the cursor, and
continues from there, whether or not that note itself satisfies the call's other filters. A client that
holds only the last key it saw can therefore resume without a cursor of its own. When no live note holds
`K` the call fails with `KhiveError::not_found`, `details: {"reason": "after_key_missing", "key": K}`,
never silently restarting from the front. A note updated between two pages moves to the front of the
order, so resuming from it skips whatever was written in between; that is a property of last-write
order, and a client that needs a stable walk uses `created_after` on the unkeyed listing instead.

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
| Maintain `version` in each `UPDATE notes` statement      | Thirty-two sites in eight files today; the first site that forgets it silently breaks the precondition for every caller. The trigger closes the population.                                                                    |
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
- `create` for notes gains `key` and `fence`; `get` gains `key` and `note_kind` for notes; `list` gains
  five parameters (`key_prefix`, `updated_after`, `created_after`, `tag_mode`, `after_key`); `update`
  gains `expected_version` and `fence`. The help text for each names the refusal reasons.
- Four conflict reasons, `version_conflict`, `fence_conflict`, `key_conflict` and `key_ambiguous`, all
  under `kind: conflict`, and one `not_found` reason, `after_key_missing`, all carried in
  `details.reason` with the values a caller needs to recover. No new `code` value and no change to the
  error type.
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
   `updated_after` alone returns both in insertion order; `created_after` alone, inclusive on the
   boundary, on the same mixed fixture; `after_key=K` resuming a keyed listing yields exactly the pages
   the cursor walk yields from the same row, also when `K`'s own note fails the call's `tags` filter,
   and `after_key` naming no live note fails with `after_key_missing` and returns no rows.
6. **Fence.** A write with `fence` at the right generation succeeds; at a stale generation, or when the
   fence row is missing, it fails with `fence_conflict` and neither row changes. Cross-process, as in 1.
   The cross-process compare-and-set control in 1 is new acceptance written for this ADR; no existing
   test is cited as already covering it.
7. **Already-stale client.** B commits at version N before A sends `update(expected_version=N)`; A gets
   `version_conflict` with `current_version: N+1` and writes nothing. The runtime's internal snapshot
   guard is not the check here; the caller's precondition is.
8. **Lost acknowledgement.** A's update at `expected_version=N` commits and A never sees the response; A
   retries the identical request. The retry gets `version_conflict` with `current_version: N+1`, and the
   row advanced exactly once. Absent-key create races the same way: two `create(key=K)` from
   two processes, one success, one `key_conflict`, one live row.
9. **Migration.** Upgrading a populated pre-028 database leaves every existing row at `version = 1` with
   `key IS NULL`, and the index creation succeeds with duplicate `name` values present.
10. **Durability option.** A deployment started with `synchronous = "full"` reports it in `db_diagnostics`;
    the verb-level cost of `create` under each of the three settings is measured over the socket, 1,000
    calls each, and recorded in the PR.

## Amendment 1 (2026-09-07): keyed cursor tiebreaker, `after_key` ambiguity, disclosure scope, migration path

Four corrections to §3 and §4, none of which changes a verb signature. Where this amendment and the
sections above disagree, the amendment governs: it supersedes §4's order and cursor sentences (`updated_at
DESC, key DESC`; `next_after` encoding `(updated_at, key)`) and §4's `after_key` resolution sentence, and
it narrows §3's `key_conflict` details.

**Keyed cursor.** A key may be held by one live note of each kind, and a keyed listing that omits
`note_kind` spans kinds, so two rows can share `(updated_at, key)`; a keyset cursor on that pair alone
would drop or repeat a row at a page boundary. The keyed listing is ordered `updated_at DESC, key DESC,
id ASC`, and `next_after` encodes `(updated_at, key, id)`. `id` is the note's primary key, so the triple
is unique and the walk is total. §4's statement of the order and cursor reads with `id` appended.

**`after_key` on an ambiguous key.** `after_key=K` without `note_kind`, when more than one live note in
the listing's namespace holds `K`, fails with `KhiveError::conflict`,
`details: {"reason": "key_ambiguous", "key": K, "kinds": "..."}`, the same refusal §3 gives `get`; it
never picks one of them. With `note_kind` given the resolution is unique.

**Disclosure scope.** `existing_id` in `key_conflict`, `kinds` in `key_ambiguous`, and the
`after_key_missing` answer each say something about which keys are occupied, and each is answered
within the caller's primary namespace only. `get` and `list` are read verbs, so `key_ambiguous` from
`get` and `after_key_missing` from `list` disclose nothing the gate has not already admitted for that
call. `create` is the one write that answers with a read's knowledge, and the gate runs once per
dispatched verb (`crates/khive-runtime/src/pack.rs`, one `GateRequest` for the verb, one `check`), so
the create handler asks a second question before it attaches `existing_id`: a `GateRequest` for verb
`list` in the same namespace by the same caller. When that check allows, `key_conflict` carries
`existing_id`; when it denies, the details are `{"reason": "key_conflict", "key": K}` and nothing else.
A deployment whose policy admits `create` and denies `list` in a namespace therefore learns from a
refused create only that the key is taken, which the refusal itself already says.

**Migration path.** Migration 028 does not exist in the tree yet; it lands with the implementation.
When it does, it is one entry in the versioned chain (`MIGRATIONS`, `crates/khive-db/src/migrations.rs`)
that every database walks from V1 (`crates/khive-db/sql/schema.sql`) on first open, so a freshly built
database and an upgraded one carry the same column, trigger and index; no build path in the tree takes
`schema.sql` alone (`crates/khive-db/src/backend.rs` opens through the chain).

Acceptance, added to the list above:

11. **Tiebreaker.** Two live notes of different kinds holding the same key with an equal `updated_at`,
    walked with a page size of one across the boundary: the page set equals the unpaged set, and the
    order is `id ASC` between them. Mutation: with `id` dropped from the cursor the walk drops or
    repeats one of the two (red). `after_key` on that key without `note_kind` fails with
    `key_ambiguous` and returns no rows; with `note_kind` it resumes after the named one.
12. **Fresh build.** At the PR that lands migration 028: a database created empty walks the chain to 028
    on first open and reports that version through `read_schema_version`; the column, trigger and index
    are present, the same set an upgraded pre-028 database shows in test 9.
13. **Disclosure.** Under a policy that admits `create` and denies `list` in one namespace, a refused
    `create(key=K)` answers `key_conflict` with `key` and no `existing_id`; under a policy that admits
    both, `existing_id` is present. Mutation: with the second gate question removed, the deny arm
    carries `existing_id` (red).

## Amendment 2 (2026-09-08): a `head` note kind for keyed documents, the document kind as a tag, `embed`, and the in-transaction arm

**Status**: Proposed.

### The gap

The consumer whose state layer this record serves writes keyed documents under fourteen document
kinds of its own (`job`, `lease`, `admission`, `fleet/epoch` among them), none of which is a registered
note kind and two of which carry a slash, and it lists by that kind first and pages by key. §3 keys a
note within a registered note kind, so the consumer has no place for its kind except the key string,
which would turn its kind filter into a prefix convention and break a read that knows only the key.
Separately, §1's version and §2's compare-and-set are silent on embedding, and a lease document
rewritten on every renewal would be re-embedded on every renewal.

### A2.1 `head`

The kg pack registers the note kind `head` for keyed documents. Any note kind may still carry a key
(ADR-179 keys memories); `head` is the kind a consumer uses when the note is a document it addresses by
key and nothing else. A `head` note's document is its content as JSON text, as a stream record is
(ADR-174 §2); `properties.tags` carries its tags. `get(kind="note", key=K, note_kind="head")` is the
read and a keyed listing with `note_kind="head"` is the walk. A `head` has no `name`.

### A2.2 The document kind is a tag

The consumer's document kind is the tag `kind:<value>` in `properties.tags`: an open string of at most
64 bytes, no U+0000, slashes allowed. `list(kind="note", note_kind="head", key_prefix=P,
tags=["kind:job"], tag_mode="all", updated_after=T)` is the consumer's list-by-kind. The tag predicate
is evaluated per row inside the keyed index range (namespace, note kind, key prefix), so its cost is
bounded by that range; a deployment whose head population outgrows it gets a column and an index by a
further amendment, with the tag kept. Nothing about the key changes: a key identifies one live `head`
per namespace whatever its document kind, which is what a read by key alone requires.

### A2.3 `embed`

`create(kind="note", ...)` and `update(...)` accept `embed` (boolean). On `create` the default is
scoped by the note kind: `false` for `head`, because a head is the document addressed by key and nothing
else and the gap above names the lease re-embedded on every renewal; `true` for every other kind,
today's behaviour. With `false` the write produces no embedding rows and no vector-index work and the
note is not a similarity candidate, while lexical indexing and listing are unchanged. On
`update` the default keeps the note's current state: a note with embedding rows is re-embedded, a note
without them stays unembedded; `embed=true` or `false` on an update overrides that once. ADR-174
Amendment 3 gives stream entries the same field with `false` as the default.

Clarification (2026-09-09): `update(embed=false)` on an embedded note performs no inference or new
embedding insertion and deletes existing embedding and vector rows in the same writer transaction
as the note update. The no-vector-index-work clause excludes synchronous ANN work: stale segment
entries are removed at the next rebuild, without delaying the write, and cannot make the note a
similarity candidate meanwhile. Acceptance covers embedded-to-off (zero vector rows, absent from
similarity results, still present in lexical search and listing), unembedded-to-on (vector rows and
similarity candidacy restored), and a deletion-removal mutation that makes the off-transition
control fail.

### A2.4 The check is inside the transaction, proven by mutation

`expected_version` (§2) and `fence` (§2b) are evaluated inside the writer transaction, as one
conditional statement or as a check under `BEGIN IMMEDIATE`, never as a read followed by a separate
write. Mutation arm: move the check outside the transaction (read the version, then begin, then write)
and the stale-version and stale-fence arms must go red; restore, and they go green. Both runs are
quoted with exit codes.

### Acceptance

1. `create(kind="note", note_kind="head", key="run/1/lease", content="{}", tags=["kind:lease"])`
   returns version 1; `get(key="run/1/lease", note_kind="head")` returns it at version 1;
   `update(id, content=..., expected_version=1)` returns version 2; `expected_version=1` again is
   `version_conflict` with `current_version "2"` and the content unchanged.
2. Three heads of kinds `job`, `job`, `lease` under one prefix: `tags=["kind:job"]` returns two,
   `tags=["kind:fleet/epoch"]` returns none, the prefix alone returns three, order and cursor per §4.
3. A key held by a `head` and the same key held by a `memory` coexist; `get(key=K)` without
   `note_kind` is `key_ambiguous` naming both kinds.
4. A `head` created without `embed`: no embedding rows, not returned for its own content by similarity
   search, returned by `list` and lexical search; the `embed=true` control has one row per registered
   model, and an `observation` created without `embed` has one row per model too (the default is
   kind-scoped); an `update` without `embed` on the unembedded head leaves it unembedded, and on the
   embedded control re-embeds it.
5. A2.4's mutation arm, both runs quoted.

## Amendment 3 (2026-09-09): ordered fence lists

**Status**: Proposed.

A write may depend on several keyed notes at once. Singleton note `create`, note `update`, and
`stream.append` accept either the existing fence object or a non-empty list of fence objects:

```json
{
  "fence": [
    { "kind": "head", "key": "run/lease", "expected_version": 3 },
    { "kind": "head", "key": "fleet/epoch", "expected_version": 8 }
  ]
}
```

Each entry follows §2b: resolve a live keyed note in the caller's primary namespace and compare its
version. All entries are checked in supplied order inside the same writer transaction as the guarded
write, before its statements. The first missing or stale entry refuses the whole write. A fence entry may name any keyed note of any kind; fence checks only read the fenced
notes. The target's own `expected_version` or stream `expected_seq` still applies.
A prior write inside an atomic unit is visible to its later fence checks; a later refusal rolls back
that entire unit.

The object form's message and details remain unchanged. The list form always adds `details.index`,
a zero-based decimal string, including for a one-element list. This is the only difference between
an object refusal and the equivalent one-element-list refusal. For example:

```json
{
  "reason": "fence_conflict",
  "key": "fleet/epoch",
  "expected_version": "8",
  "current_version": "9",
  "index": "1"
}
```

`current_version` is omitted when the row is missing. The message remains `note fence precondition
failed`; `domain_disposition` is `not_committed` for a confirmed fence refusal. Storage failures with
unknown outcomes retain their existing disposition; a fence parameter alone proves no outcome.

An explicit null, an empty list, a malformed entry, an unknown entry field, an invalid key or note kind,
or a non-positive expected version is `invalid_input` before opening the write transaction. A list
cannot name the same `(kind, key)` twice, even with different versions; the error names both zero-based
indices. This whole class is an `invalid_input` error carried in the message text, with no
`details.reason` discriminator: the base ADR promises `reason` only for `conflict` and `not_found`, and a
client must not look for one here. Identical keys in different note kinds remain distinct. Omitting `fence` keeps today's
unfenced behaviour. A list never changes the successful response or adds writes to a lease.

Clarification (2026-09-10): the once-per-`(kind, key)` target rule applies independently to each ordered
fence list and to the keyed-write members of [ADR-174 A1.1](ADR-174-ordered-streams-append.md); it does
not expand the batch-wide `fence`, which deliberately remains object-only.

Cardinality bound (2026-09-10, #2507): an ordered fence list admits at most **100 entries**.
An oversized list is `invalid_input`, naming both the cap and the count sent in its message,
before interpreting any fence entry or requesting a writer. The same bound applies to
singleton note `create`, note `update`, `stream.append`, and each `stream.batch` append
member, in both atomic and per-member modes. Every append member's shape is validated before
any member transaction starts, so an oversized later member cannot leave earlier writes.
Exactly 100 entries remains valid subject to the existing shape and fence checks. The cap
matches ADR-174's 100 observed-entry allowance: both lists add keyed reads while holding the
writer. This is a per-list limit, not a new aggregate limit across batch members.

The Python client accepts a dictionary or list of dictionaries and preserves entry order. Its generic
`stream.append` builder preserves explicitly supplied null so the server can reject it.

Acceptance extends the existing object-form controls with one-element and two-element lists, valid,
stale and missing entries, first-failure ordering, unchanged target content/version and stream head on
refusal, unchanged lease rows, and malformed-input domain-population controls. Transaction controls
must distinguish a check before transaction admission from one inside the admitted writer transaction,
including another process renewing a lease at that boundary. Checking only the first entry or omitting
`index` must each fail their corresponding list control.
The cardinality controls include exactly 100 distinct entries, 101 entries, an oversized list
with a malformed first entry, unchanged writer-acquisition counters on refusal, and a later
oversized append member in both batch modes. Disabling the cap or moving it below entry
interpretation must make the corresponding over-limit control fail.

Append members of `stream.batch` accept the same optional object or non-empty
list in their own `fence` field. In atomic mode, every member fence is checked in
member order, then fence order, inside the batch writer transaction before any
member writes. A stale or missing fence refuses the entire batch. Its error adds
`member`, the member position as a string, alongside `index` when the supplied
fence was a list. In per-member mode, the failed member carries the same fence
error and `domain_disposition: not_committed`; successful sibling appends remain
committed. The result's list position identifies the member. All members' fence
shapes and kinds are validated before any member transaction starts.

This amendment enables append-member fences only. The batch-wide `fence` and
`observed` fields, their mode defaults, and the `write` member retain their
existing availability rules.
