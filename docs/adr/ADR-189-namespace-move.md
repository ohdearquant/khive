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
one, for reasons that are properties of the schema rather than matters of taste: six of the affected
tables are fts5 virtual tables that do not accept `UPDATE` at all, the vector tables are not
enumerable from any static list, and the ANN bookkeeping has ordering semantics that an `UPDATE`
silently violates. Each of those is developed below.

### What was measured

At `b17951432`, unless a line says otherwise.

**The namespace-carrying tables.** Parsing every `CREATE TABLE` body under `crates/khive-db/sql` and
`crates/khive-db/src/migrations.rs` for a namespace column returns 24 of 39 declared tables:

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

**Uniqueness.** Namespace participates in nine constraints, four unique indexes and five composite
primary keys:

| Constraint                      | Columns                                                          |
| ------------------------------- | ---------------------------------------------------------------- |
| `idx_notes_namespace_kind_key`  | `(namespace, kind, key)` where `key` is not null and not deleted |
| `idx_graph_edges_unique_triple` | `(namespace, source_id, target_id, relation)`                    |
| `idx_knowledge_atoms_ns_slug`   | `(namespace, slug)`                                              |
| `idx_knowledge_domains_ns_slug` | `(namespace, slug)`                                              |
| `brain_implicit_mass` PK        | `(profile_id, namespace, target_id)`                             |
| `brain_profile_snapshots` PK    | `(profile_id, namespace)`                                        |
| `ann_consumer_watermark` PK     | `(consumer, namespace, embedding_model)`                         |
| `ann_consumer_pending` PK       | `(consumer, namespace, embedding_model)`                         |
| `note_streams` PK               | `(namespace, stream, seq)`                                       |

A census of every `CREATE UNIQUE INDEX` touching namespace in `khive-db` returns those four indexes
and no fifth.

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

- A record kind present in the source namespace with **no route** refuses the entire move.
- A record kind **routed with zero rows** succeeds and reports a count of zero.

The second is the reason the counts exist. A host that binds a pack which writes nothing needs
"routed, nothing there" to be distinguishable from "you forgot this one", and an implementation that
collapses them makes the map unverifiable by its own caller.

### Derived rows move with their parent

Only kind-bearing records are routed. Everything else is carried:

- the six fts5 tables and the two rowid maps, with their parent note or entity,
- `knowledge_sections` with its atom,
- `note_streams` and `proposals_open` with the namespace they belong to,
- every `vec_*` row, with its subject.

None of these appears in the route map. A caller cannot route them independently, because they have
no independent existence.

### A collision refuses the whole move

Consolidating two trees written by two clients is the case this primitive exists for, so two rows
holding the same `(namespace, kind, key)` or the same `(namespace, slug)` after the move is the
expected shape rather than an exotic one. `note_streams` makes it near-certain: `seq` is a
per-namespace sequence, so two streams that both start at 1 collide on their first row.

The primitive refuses the whole move on any collision against any of the nine constraints, and names
the colliding `(table, namespace, key)` rows in the refusal.

It does not offer a conflict policy. An `ON CONFLICT` that drops or replaces picks a winner over a
caller's data, and does so while satisfying the counts-in-equals-counts-out assertion, which is the
worst available combination: the destructive outcome and the reassuring receipt arrive together.
Resolution is a decision someone makes with the rows in front of them.

### One transaction, through the writer task

The whole move runs inside one `BEGIN IMMEDIATE` in the writer task, reads included, for the reason
`text.rs:414` already records. Partial application is not a state this primitive can leave behind.

### Vectors: rewrite the row, never re-embed

Namespace is not an input to an embedding. A re-index pass would recompute byte-identical vectors,
so the expensive option buys nothing over carrying the bytes across.

It is also not optional. Because the delete path is keyed on `(subject_id, namespace)`, a vector
left under the source namespace survives a later delete of its record under the target namespace,
and then keeps answering searches for content the caller deleted. Doing nothing to the vectors is
the choice that breaks silently.

Since no `UPDATE` against vec0 exists in the tree, the vector row moves by the same delete and
re-insert the fts5 tables use, carrying the stored embedding. One mechanism covers every virtual
table.

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
target namespace. They move with the records. `brain_serve_ledger` moves with them; its row read is
by id, and the namespace column on the row is carried rather than reinterpreted.

`events` stays where it is. The event log is the record of what happened under the namespace it
happened under, and rewriting it makes the history claim something that did not occur. The
consequence is real and is stated here so that it is a documented cost rather than a surprise: a
caller reading an audit trail that spans a move reads both namespaces.

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
- A route map missing a kind that has rows refuses without writing anything, and a routed kind with
  zero rows succeeds reporting zero.
- A collision on any of the nine constraints refuses the whole move and names the rows. The fixture
  carries at least the `(namespace, kind, key)` and `(namespace, slug)` shapes.
- Full-text search and vector recall return the moved records under the target namespace and nothing
  under the source, and a delete issued after the move removes the vector.
- Soft-deleted records move with `deleted_at` intact.
- An ANN consumer that was caught up before the move consumes the moved subjects afterwards, which
  is the arm that fails if the write log is rewritten instead of appended to.
- The fixture is built through the store's own writers. A SQL seed produces no fts5 shadow rows, no
  write-log entries and no vector rows, so it would exercise everything except the part that is
  hard.
