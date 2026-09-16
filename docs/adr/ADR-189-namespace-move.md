# ADR-189: Moving records between namespaces

- **Status**: Proposed
- **Date**: 2026-09-15
- **Depends on**: [ADR-007](ADR-007-namespace.md) (namespace is an attribution-only open string),
  [ADR-005](ADR-005-storage-capability-traits.md) (the store surface this primitive joins)
- **Relates to**: [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) (the write log and
  watermarks this move has to leave consumable), [ADR-015](ADR-015-schema-migrations.md) (why the
  affected-table set cannot be a constant)

## Context

Namespace is attribution-only and an open string: nothing creates a namespace and nothing destroys
one, so a record's namespace is just a column that says who a row is attributed to. That makes
moving records between namespaces sound in principle, and there is no way to do it.

The pressure is a host that has written one logical collection into two namespaces, because two
credential paths resolved differently, and now wants them in one place. New writes are fixable at
the binding. The records already written are not.

The only instrument available today is a hand-written `UPDATE` sweep from the host. It is the wrong
one, for reasons that are properties of the schema rather than matters of taste: two of the affected
tables are fts5 tables over external content, where an `UPDATE` of the namespace succeeds and
changes nothing, the vector tables are not enumerable from any static list, and the ANN bookkeeping
has ordering semantics that an `UPDATE` silently violates. Each of those is developed below.

### What was measured

At `b17951432`, unless a line says otherwise.

**The namespace-carrying tables.** Parsing every `CREATE TABLE` body under `crates/khive-db/sql` and
`crates/khive-db/src/migrations.rs` for a namespace column returns 24 of 39 declarations. A
declaration count is not a table count: `sql/023-fts-record-kind.sql:45,88` drops `fts_entities` and
`fts_notes` and renames `fts_entities_v23` and `fts_notes_v23` over them, so those two names exist in
no migrated store and the list below overstates the population by exactly them. The parse cannot see
it, which is the argument for the census rather than an aside about it.

```
notes entities graph_edges events knowledge_atoms knowledge_domains knowledge_sections
note_streams proposals_open brain_event_log brain_implicit_mass brain_profile_snapshots
brain_serve_ledger ann_write_log ann_consumer_watermark ann_consumer_pending
fts_notes_rowids fts_entities_rowids
fts_notes fts_entities fts_notes_v23 fts_entities_v23 fts_knowledge fts_sections
```

**That enumeration is incomplete, and the omission is the one that matters.** The vec0 vector tables
are created at runtime as `vec_{model_key}`, one per embedding model, at
`crates/khive-db/src/backend.rs:829`. They are declared in no `.sql` file and in no migration, so no
parse of the schema sources can see them, and their set is a property of the store rather than of
the code: it is whichever models that store has embedded with. They are readable only at runtime:

```sql
SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'vec\_%' ESCAPE '\'
```

**The precedent.** `crates/khive-db/src/stores/text.rs:1914` `rename_namespace` already does this
for exactly one of the 24. It reads, deletes and re-inserts inside the writer task's
`BEGIN IMMEDIATE`, and the comment at `:414` records why the read has to be inside: running the
`SELECT` outside that transaction leaves a window in which a concurrent writer can resurrect a stale
row or lose one.

**Uniqueness.** Namespace participates in fourteen constraints, six unique indexes and eight
composite primary keys. The first count taken here was nine, and the five it missed are the
instructive ones - see the note under the table:

| Constraint                      | Columns                                                                  | Reachable |
| ------------------------------- | ------------------------------------------------------------------------ | --------- |
| `idx_notes_namespace_kind_key`  | `(namespace, kind, key)` where `key` is not null and not deleted         | yes       |
| `idx_comm_message_external_id`  | `(namespace, kind, json_extract(properties, '$.external_id'))`, filtered | yes       |
| `graph_edges` PK                | `(namespace, id)`                                                        | yes       |
| `idx_graph_edges_unique_triple` | `(namespace, source_id, target_id, relation)`                            | yes       |
| `idx_knowledge_atoms_ns_slug`   | `(namespace, slug)`                                                      | yes       |
| `idx_knowledge_domains_ns_slug` | `(namespace, slug)`                                                      | yes       |
| `idx_brain_serve_ledger_unique` | `(namespace, target_id, query_class, served_at)`                         | yes       |
| `brain_implicit_mass` PK        | `(profile_id, namespace, target_id)`                                     | yes       |
| `brain_profile_snapshots` PK    | `(profile_id, namespace)`                                                | yes       |
| `fts_notes_rowids` PK           | `(namespace, subject_id)`                                                | yes       |
| `fts_entities_rowids` PK        | `(namespace, subject_id)`                                                | yes       |
| `note_streams` PK               | `(namespace, stream, seq)`                                               | no        |
| `ann_consumer_watermark` PK     | `(consumer, namespace, embedding_model)`                                 | no        |
| `ann_consumer_pending` PK       | `(consumer, namespace, embedding_model)`                                 | no        |

Three are unreachable through this primitive, by decisions taken below rather than by accident:
`note_streams` refuses before any collision is computed, and neither `ann_consumer_*` table is
written at all, because the write log is appended to and no watermark is edited. They stay in the
table because a later decision that moves a watermark makes that primary key reachable again, and
this table is where a reader looks.

**How the first count came to be nine, because it decides how the set is maintained.** The two
enumerations behind it were `CREATE UNIQUE INDEX ... namespace` and composite `PRIMARY KEY`
declarations. Neither is wrong; both stop at the shapes their author had in mind. What they missed:
an index whose third column is an EXPRESSION (`json_extract(properties, '$.external_id')`) rather
than a column name; a `PRIMARY KEY (namespace, id)` on `graph_edges`, read past as "edges are keyed
by id"; a unique index on `brain_serve_ledger`, a table this ADR separately decides to move; and the
two rowid maps, whose `(namespace, subject_id)` keys are the reason a stale fts row can block a
move.

So the refusal set is not maintained by hand. It is derived at move time from
`PRAGMA index_list`/`PRAGMA index_xinfo` over the tables the census finds carrying a namespace
column, which reports primary keys and uniqueness constraints alongside `CREATE INDEX` ones. A
constraint added by a future migration is in the set on the next run, without an edit here.

That read has one gap, and it is the same shape as the misses above. An EXPRESSION key column is
reported with a null name, so an index over `lower(namespace)` names nothing at all. Measured:
`CREATE UNIQUE INDEX i ON t(lower(namespace), id)` gives key rows `-2|NULL` and `0|id`. The comm
external-id index survives the column read only because its namespace is a literal first column and
the expression sits on a different one, which makes it the worst possible witness - it passes while
the class fails.

The closure is exact rather than a caveat. An index carrying any expression key column is
additionally read from its own `CREATE INDEX` text in `sqlite_master`, matching `namespace` as a
word. Only an autoindex has a null `sql`, and an autoindex is a table constraint over a column list,
which cannot carry an expression - so the column read covers exactly what the text read cannot, and
the reverse. The text read is coarser than a parse and errs toward including a constraint, which
refuses a move SQLite would have allowed and is visible in the refusal; a miss would corrupt rows
silently.

**Vectors.** The delete path is keyed on the pair:
`DELETE FROM {table} WHERE subject_id = ?1 AND namespace = ?2`, at `stores/vectors.rs:31`, `:506`
and `:658`. Every vector write is an `INSERT` with replace semantics (`:548`); there is no `UPDATE`
against a vec0 table anywhere in the tree.

**ANN bookkeeping.** `ann_write_log.seq` is `INTEGER PRIMARY KEY AUTOINCREMENT`
(`sql/011-ann-write-log.sql:8`), and `ann_consumer_watermark` is
`PRIMARY KEY (consumer, namespace, embedding_model)` (`:35`).

**Learned state.** `brain_implicit_mass`, `brain_event_log` and `brain_profile_snapshots` are each
read with `WHERE namespace = ?` (`khive-pack-brain/sql/brain_implicit_mass_read.sql`,
`brain_event_log_since.sql`, `brain_profile_snapshot_latest.sql`).

## Decision

Add a move primitive to the store surface.

```
move_namespace(source, routes: [(record_kind, target_namespace)]) -> MoveCounts
```

### The route map is data, not a callback

One source namespace fans out to several targets, so a single rename cannot express the operation.
The routing is supplied as a list of `(record_kind, target_namespace)` pairs rather than as a
function, because a list can be validated whole before the transaction opens, logged, replayed and
tested against a fixture, and a closure can do none of those.

Validation before any write, with two outcomes that must not look alike:

- A subject class present in the source namespace with **no route** refuses the entire move.
- A subject class **routed with zero rows** succeeds and reports a count of zero.

The second is the reason the counts exist. A host that binds a pack which writes nothing needs
"routed, nothing there" to be distinguishable from "you forgot this one", and an implementation that
collapses them makes the map unverifiable by its own caller.

### The route key names a subject class

Four of the namespace-carrying tables hold rows that exist in their own right and have no `kind`
column: `knowledge_atoms`, `knowledge_domains`, `graph_edges` and the stream ledger. A route map
keyed on a bare kind string cannot name them, and cannot be validated against a store that holds
them. So the key names a subject class, qualified by kind for the two classes that carry one:

```
note:<kind>   entity:<kind>   edge   atom   domain
```

The qualification is not decoration. Nothing in the schema stops a note kind and an entity kind
sharing a spelling, and an unqualified key would route both on a store where they do. It also makes
the refusal exact: the message names `note:observation` rather than `observation`.

### Derived rows move with their parent

Only subject classes are routed. Everything else is carried:

- the four fts5 tables and the two rowid maps, with their parent note or entity,
- `knowledge_sections` with its atom,
- `proposals_open` with the namespace it belongs to,
- every `vec_*` row, with its subject,
- the `brain_*` rows, with the subject whose state they hold,
- `ann_write_log`, appended under the target namespace at a fresh `seq`; `ann_consumer_watermark`
  and `ann_consumer_pending` are not written at all.

None of these appears in the route map. A caller cannot route them independently, because they have
no independent existence.

The four fts5 tables do not take one mechanism, and the split is measured rather than assumed.
`fts_notes` and `fts_entities` are ordinary fts5 tables: an `UPDATE` of their `namespace` column is
accepted, preserves the rowid the two maps are keyed on, and leaves the index intact. `fts_knowledge`
and `fts_sections` are declared `content=` over `knowledge_atoms` and `knowledge_sections`, and there
an `UPDATE` of the same column returns success and changes nothing, because the value a reader gets
comes from the content table. That pair is maintained by schema triggers firing on
`UPDATE OF ... namespace` (`sql/schema.sql`, `sql/026-knowledge-fts-repair.sql`), so writing the base
row is both sufficient and the only thing that works. The dangerous half is the second: a sweep that
writes the virtual table directly is not refused, it is answered `rc=0` with nothing done. Measured
on SQLite 3.54.0; the runtime links its own build through `libsqlite3-sys`, so the arm pinning this
belongs in the implementation's tests rather than in this document.

### A collision refuses the whole move

Consolidating two trees written by two clients is the case this primitive exists for, so two rows
holding the same `(namespace, kind, key)` or the same `(namespace, slug)` after the move is the
expected shape rather than an exotic one: two independently written trees each hold a note keyed
`(kind, key)` and an atom keyed `slug`, and after the move both sit in one namespace.

The primitive refuses the whole move on any collision against any reachable constraint, and names
the colliding `(table, constraint, namespace, key)` rows in the refusal.

The enumeration comes from a pre-flight query per constraint, built from `PRAGMA index_xinfo`, so a
refusal lists every collision rather than the first one. An expression index has no column name to
build that query from, so for those the refusal carries the constraint's name and the row the
failing statement was applying, not a full list. That is a stated limit of the enumeration and not
of the refusal: the statements themselves are plain and error, so a collision the pre-flight cannot
enumerate still aborts the move.

It does not offer a conflict policy. An `ON CONFLICT` that drops or replaces picks a winner over a
caller's data, and does so while satisfying the counts-in-equals-counts-out assertion, which is the
worst available combination: the destructive outcome and the reassuring receipt arrive together.
Resolution is a decision someone makes with the rows in front of them.

This rules out one statement in particular. `note_insert_keyed_statement`
(`stores/note.rs:113`) carries `ON CONFLICT(namespace, kind, key) WHERE key IS NOT NULL AND
deleted_at IS NULL DO NOTHING`, which is correct for its own caller
(`khive-runtime/src/atomic_message.rs:603`, where an occupied key means the message already exists)
and wrong for a mover, where it would drop a row and return success. The mover issues a plain
`INSERT` that errors on conflict. Reusing the keyed statement would leave the counts assertion as
the only thing standing between a collision and silent loss, which inverts its purpose: the counts
are a second line of defence, not the first.

### One transaction, through the writer task

The whole move runs inside one `BEGIN IMMEDIATE` in the writer task, reads included, for the reason
`text.rs:414` already records. Partial application is not a state this primitive can leave behind.

The mover issues no `BEGIN IMMEDIATE` of its own. `WriterTaskHandle::send` hands its closure a
connection already inside the transaction it opened and owns the commit or rollback
(`writer_task.rs:73-76`, `:253-258`); a nested bare `BEGIN IMMEDIATE` is a SQLite error, so the
primitive is one closure of statements, not a script.

### One transaction per backend, and a store can have several

A pack can be assigned its own backend, and then its records live in a different SQLite file. That
is not a hypothetical configuration: a `[packs.comm] backend = "comm"` and
`[packs.knowledge] backend = "knowledge"` assignment puts comm's notes and the knowledge atoms in
two files beside the main one, and a route map that sends `note:message` to one namespace and
`note:observation` to another spans two of them. SQLite has no transaction across unattached
databases, so "the whole move is one `BEGIN IMMEDIATE`" is true of one backend and false of a store
that has three.

The primitive is therefore scoped to one backend. It takes the connection it is given, censuses
that backend's schema, and moves that backend's rows in that backend's transaction. A move over a
split store is the same route map applied to each backend in turn, which composes correctly because
a class routed with no rows in this backend succeeds reporting zero: the same rule that makes the
map verifiable by its caller is what makes it re-runnable per file.

What does not compose is atomicity, and the honest statement is that it cannot. A split store gets
N transactions, and a failure in the third leaves the first two applied. The primitive reports
counts per backend so a caller can see where it stopped; it does not offer a distributed commit it
has no mechanism for.

One carry rule depends on this. `brain_*` and `ann_*` rows are carried with the subject whose state
they hold, and that only works while they sit in the same backend as the subject. No pack assignment
splits them today. A deployment that gave the brain pack its own backend would leave those rows with
no subject to be carried by, and the move would silently skip them - so that assignment needs this
paragraph revisited before it is made, rather than after.

### A stream member cannot move at all

`sql/029-note-streams.sql` pins stream membership to a namespace with four triggers, and the pin is
absolute rather than conditional. `refuse_stream_entry_rewrite` aborts any `UPDATE` of a member
note that names `namespace` in its `SET` list; `refuse_stream_entry_delete` aborts the delete;
`refuse_stream_ledger_update` and `refuse_stream_ledger_delete` abort every write to the ledger
rows themselves. `stream_schema_tests.rs:37` already asserts the first of these directly, with
`UPDATE notes SET namespace='other'` in its list of forbidden statements.

So a stream member has no move at all, by update or by delete and reinsert, and the primitive
refuses any move whose routed kinds reach one. The refusal comes from a read of `note_streams` by
`note_id` taken before any write, naming the notes and their `(stream, seq)`, so the caller gets an
enumeration rather than a trigger's `stream_member` abort string from somewhere in the middle of
the transaction.

### Vectors: rewrite the row, never re-embed

Namespace is not an input to an embedding. A re-index pass would recompute byte-identical vectors,
so the expensive option buys nothing over carrying the bytes across.

It is also not optional. Because the delete path is keyed on `(subject_id, namespace)`, a vector
left under the source namespace survives a later delete of its record under the target namespace,
and then keeps answering searches for content the caller deleted. Doing nothing to the vectors is
the choice that breaks silently.

Since no `UPDATE` against vec0 exists in the tree, the vector row moves by delete and re-insert,
carrying the stored embedding. That is a third mechanism, not a shared one: the fts5 tables already
split two ways above, and stating a single mechanism across every virtual table would be the kind of
uniformity claim this document is written to avoid.

The affected vector tables are enumerated from `sqlite_master` at move time and column-validated.
A constant list would be wrong for any store using a model the list was not written against, and
would fail by skipping rather than by erroring.

### The ANN write log is appended to, never rewritten

Rewriting a historical `ann_write_log` entry's namespace places that entry at a `seq` below the
target namespace consumer's watermark, and a consumer already past that `seq` never sees it. The
fast path introduces exactly the silent post-move breakage it appears to avoid.

Instead the move appends one entry per moved subject under the target namespace at a fresh `seq`,
and leaves historical entries under the source namespace for compaction. No watermark is edited and
no `ann_consumer_pending` row is rewritten.

### Learned state moves, and the event log does not

`brain_implicit_mass`, `brain_event_log` and `brain_profile_snapshots` are read namespace-scoped, so
leaving them behind makes retune history, implicit mass and profile snapshots read empty under the
target namespace. They move with the records where that is a defined operation, and for two of them
it is not.

Two of the four brain tables are keyed by subject and two are not:

| Table                     | Key                                  | Carried by  |
| ------------------------- | ------------------------------------ | ----------- |
| `brain_implicit_mass`     | `(profile_id, namespace, target_id)` | its subject |
| `brain_serve_ledger`      | `id`, with `target_id`               | its subject |
| `brain_profile_snapshots` | `(profile_id, namespace)`            | nothing     |
| `brain_event_log`         | `(profile_id, namespace, ...)`       | nothing     |

The first two carry with the record they describe, which is what "moves with the records" means. The
last two are per-namespace aggregates with no subject at all, and under a move that routes different
classes to different namespaces there is no target to carry them to: a snapshot of a profile's state
in one namespace cannot be split across five.

So they move only when the move is TOTAL and SINGLE-TARGET - every subject class present in the
source is routed, and every route names the same target. Otherwise they stay where they are, and the
counts report them as left behind rather than omitting them.

Left behind rather than refused, because the choice is between two recoverable outcomes and one
unrecoverable one. Refusing would block the primitive on exactly its main case, a partitioning move,
over state that re-accumulates from use. Splitting the aggregate would invent numbers. Leaving it
costs a profile its retune history under the new namespaces and says so in the result, which is the
only one of the three a caller can act on.
`events` stays where it is. The event log is the record of what happened under the namespace it
happened under, and rewriting it makes the history claim something that did not occur. That is the
whole reason, and it does not need a cost to justify it.

The cost it does carry is narrow and worth stating precisely, because the wider version invites a
workaround nobody needs. Only a read that FILTERS BY NAMESPACE pays it: `stores/event.rs:1264` and
the list at `:1314` scope by namespace, so after a move those return the moved records' history
under the source namespace and a reader wanting the whole trail asks for both. A read keyed on
anything else - by id at `:387`, or by any caller-side identifier that is not the namespace - is
unaffected, and there is no verb today that publishes a namespace-scoped event read to a consumer.

## Consequences

An edge's namespace is the namespace of the connection that wrote it, not of its endpoints
(`khive-pack-kg/src/handlers/link.rs:63`, `:193`, `:284`). After a kind-partitioned move an edge can
therefore sit in a different namespace from the records it joins, and it hydrates because by-id
`get` is namespace-agnostic. That property is under discussion in #2802. Anything that scopes by-id
reads to the caller's own namespace would break cross-pack neighbours on exactly the trees this
move creates, and whoever takes that issue needs to read this paragraph first.

The affected-table set is not a constant and must not become one. A migration that adds a
namespace-carrying table adds it to this primitive's scope, and a migration that adds a
namespace-bearing uniqueness constraint adds it to the refusal set. Both are silent failures if
missed: the first skips rows, the second corrupts them.

## Acceptance

- A move over a populated store leaves every affected table consistent, asserted per table rather
  than by a spot check, with the vector tables enumerated from `sqlite_master` rather than from a
  list written into the test.
- A concurrent writer during the move cannot produce a resurrected or a lost row.
- A route map naming classes whose rows live in another backend succeeds here reporting zero for
  them, so the same map is re-runnable against each backend of a split store.
- A route map missing a kind that has rows refuses without writing anything, and a routed kind with
  zero rows succeeds reporting zero.
- A collision on any reachable constraint refuses the whole move and names the rows. The fixture
  carries at least the `(namespace, kind, key)` and `(namespace, slug)` shapes.
- The runtime census reproduces the fourteen constraints in the table above on a freshly migrated
  store, including the three this primitive cannot reach and the expression index. Two arms matter
  more than the count: one adds a namespace-bearing unique index and asserts the census finds it
  with no code change, and one adds an index over `lower(namespace)` and asserts the same, with a
  control proving the column read alone does NOT see it.
- Full-text search and vector recall return the moved records under the target namespace and nothing
  under the source, and a delete issued after the move removes the vector.
- Soft-deleted records move with `deleted_at` intact.
- A partitioning move over a namespace holding `brain_profile_snapshots` or `brain_event_log` rows
  succeeds, leaves them in place, and reports them as left behind. A total single-target move over
  the same fixture moves them. The two arms differ only in the route map, so an implementation that
  ignores totality fails one of them.
- A move reaching a stream member refuses before writing anything, names the notes and their
  `(stream, seq)`, and the arm is distinguishable from a trigger abort: the mutation control removes
  the pre-flight read and the same case then fails with `stream_member` from inside the transaction.
- The fixture is parameterized rather than collision-only. A clean move asserting counts in equals
  counts out, non-zero, is the arm that would go missing if every case collided, and it is the arm
  that detects a conflict-swallowing insert.
- An ANN consumer that was caught up before the move consumes the moved subjects afterwards, which
  is the arm that fails if the write log is rewritten instead of appended to.
- The fixture is built through the store's own writers. A SQL seed produces no fts5 shadow rows, no
  write-log entries and no vector rows, so it would exercise everything except the part that is
  hard.

## Amendment 1 (2026-09-15): the source side of a move, and what a partitioning move cannot carry

**Status: proposed.** Three corrections found while implementing the primitive this ADR specifies.
Each is a place where the accepted text describes one side of a two-sided operation, or presumes a
shape of request it does not name.

### 1. The ANN write log takes two entries per moved vector, not one

§"Derived rows move with their parent" says `ann_write_log` is "appended under the target namespace
at a fresh `seq`". That is one side only.

A consumer builds its index per `(namespace, embedding_model)` and advances a watermark over this
log; `ann_consumer_watermark` is keyed `(consumer, namespace, embedding_model)`. An `upsert`
appended under the target tells the target's consumer to take the vector. Nothing tells the source's
consumer to drop it, so the source index keeps answering searches for a subject that is no longer in
its namespace.

This is the failure this ADR already names one layer down, for the vectors themselves: "a vector
left under the source namespace survives a later delete of its record under the target namespace,
and then keeps answering searches for content the caller deleted." The same argument applies to the
consumer's copy of that vector, and the document made it in one place and not the other.

**Corrected text:** per moved vector the move appends a `delete` under the source namespace and an
`upsert` under the target namespace, in that order.

Per MOVED vector, and the qualifier is load-bearing rather than decorative. A target namespace is
allowed to hold vectors of its own already, and they are not part of this move: telling the target's
consumer to upsert them costs a tombstone and an insert each, for an index that was correct before
the request arrived, and the cost scales with the target rather than with the move. The set of moved
vectors has to be read before the rows are rewritten, because afterwards nothing distinguishes them
from the residents.

### 2. "The namespace it belongs to" presumes a total single-target move

§"Derived rows move with their parent" lists `proposals_open` as carried "with the namespace it
belongs to". Read at the schema, it is keyed `proposal_id TEXT PRIMARY KEY`, with `namespace` a
plain column and no reference to any subject.

So it has exactly the problem this ADR already recognises for `brain_profile_snapshots` and
`brain_event_log`: in a partitioning move that routes several classes to several targets, there is
no single namespace left for it to belong to. The phrase presumes a total single-target request
without saying so.

**Corrected text:** `proposals_open` is a namespace-scoped aggregate. It moves when the request is
total and single-target, and is reported as left behind otherwise, in the same way as the brain
aggregates.

### 3. A partial application across backends is a resume point, not a new failure mode

A pack may be assigned its own backend, and SQLite has no transaction across unattached databases.
The primitive therefore operates on the connection it is given, and a store with three backends is
the same route map applied three times.

That composes, because a routed class with no rows succeeds reporting zero. Atomicity does not
compose, and this amendment states the consequence rather than leaving it implied: a backend whose
move did not run holds the state that existed before anyone asked, so a partial application is a
resume point and re-running the same request against the remaining backends is defined.

### Acceptance

This amendment is accepted on an executable arm, not on the text above. Asserting that two
`ann_write_log` rows were appended is a claim about what was written; it passes against a consumer
that never reads them. The arm asserts what the index does:

1. Warm namespace A's consumer and search A for the subject, requiring a **hit**. This pre-state
   control is the load-bearing one: without it, a fixture whose source consumer was never warm
   produces a miss in step 4 for a reason that has nothing to do with the move.
2. Move the subject from A to B.
3. Advance A's consumer past its watermark.
4. Search A, requiring a **miss**.
5. Search B in the same run, requiring a **hit**.

Falsifier: remove the `delete`-under-source append from the implementation and step 4 must FAIL, its
search of A returning the hit the move was supposed to have retired. An arm that cannot be reddened
by removing the mechanism it names is not testing it, and the direction has to be written down
because every step here asserts an absence except the two controls that bracket it.

The arm's home is the knowledge pack, above `khive-db`, because that is where the consumer surface
and the watermark actually live.
