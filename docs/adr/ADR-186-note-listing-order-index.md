# ADR-186: One partial index for the note listing order

- **Status**: Proposed
- **Date**: 2026-09-13
- **Depends on**: [ADR-015](ADR-015-schema-migrations.md) (schema changes ship as a new versioned
  migration over a `.sql` file, never as an edit to an existing one)
- **Relates to**: [ADR-013](ADR-013-note-kind-taxonomy.md) (the `kind` discriminant this listing
  optionally filters on)

## Context

`list` over notes issues one statement, built by `build_note_where` plus `NOTE_COLUMNS`:

```sql
SELECT id, namespace, kind, status, name, content, salience, decay_factor,
       expires_at, properties, created_at, updated_at, deleted_at, key, version
FROM notes
WHERE namespace = ?1 AND deleted_at IS NULL [AND kind = ?2]
ORDER BY created_at DESC, id ASC
LIMIT ? OFFSET ?
```

The indexes on `notes` that could serve it are `(namespace)`, `(namespace, kind)` and
`(created_at DESC)`. None produces rows in `created_at DESC, id ASC` order under that filter, and
none is partial on `deleted_at IS NULL`. SQLite therefore establishes the order with a transient
b-tree, and the sorter carries the selected columns, `content` and `properties` included, for every
matching row rather than for the page. The cost is proportional to the size of the namespace and is
paid before `LIMIT` and `OFFSET` apply.

### What was measured

A test in `khive-db` (`stores/note_list_plan_tests.rs`, merged separately as the before-and-after
instrument) runs the production statement against a 20,000-note fixture with `ANALYZE` run, and
reports the plan, the returned ids and the VM steps for each arm:

| arm                                                                 | plan                                                                      | VM steps       |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------- | -------------- |
| kind-filtered, today                                                | `SEARCH ... idx_notes_kind`, `USE TEMP B-TREE FOR ORDER BY`               | 117,107        |
| kind-filtered, `(namespace, kind, created_at DESC, id ASC)` partial | `SEARCH ... (namespace=? AND kind=?)`                                     | 245            |
| kind-filtered, `(namespace, created_at DESC, id ASC)` partial       | `SEARCH ... (namespace=?)`                                                | 1,929          |
| unfiltered, today                                                   | `SCAN ... idx_notes_created`, `USE TEMP B-TREE FOR LAST TERM OF ORDER BY` |                |
| unfiltered, kind-bearing index                                      | unchanged, still sorts                                                    |                |
| unfiltered, `(namespace, created_at DESC, id ASC)` partial          | `SEARCH ... (namespace=?)`                                                |                |
| rare kind, `(namespace, created_at DESC, id ASC)` partial           | unchanged from today                                                      | 721 either way |

Every arm asserts the same ids in the same order, which is the assertion the rest are in service of.

Two of those rows are the decision. A kind-bearing index cannot serve the unfiltered listing, because
with no equality on `kind` its second column stands between the namespace and the ordering. And with a
rare kind the planner declines the namespace-and-time index and keeps its small sort, so that case is
not a regression and not an improvement: the sort only costs when the filtered kind is a large share
of the namespace, or when no kind is named.

### Why it is load-bearing

The heaviest caller is in-tree. The email outbox loop wakes every 5 seconds and calls
`list_undelivered_outbound_messages`, which pages newest-first through `message` notes 200 rows at a
time, up to a 10,000-row scan cap, applying its pending-delivery predicate after the rows come back.
One cycle therefore issues up to 50 of these statements, each sorting the whole message population
before its `OFFSET` applies. Against a store with a large message history that is a continuous
transient-b-tree workload on the serving process.

## Decision

**Add one index, in a new migration:**

```sql
CREATE INDEX IF NOT EXISTS idx_notes_namespace_created
    ON notes(namespace, created_at DESC, id ASC)
    WHERE deleted_at IS NULL;
```

One, not two. The kind-bearing variant is faster for a kind-filtered page, 245 steps against 1,929,
but every index on `notes` is paid for on every note insert and `notes` is the table a serving process
writes most. 1,684 VM steps on a read that already dropped from 117,107 does not buy a second b-tree
on that table. If a future measurement shows a kind-filtered listing dominating some deployment's
load, the kind-bearing index is an additive follow-up with its own evidence.

The partial predicate is not decoration: `deleted_at IS NULL` is emitted by `build_note_where` on
every call, so a partial index over it is both smaller and the only form the planner can prove
applies.

This does not change the query, the verb surface, or any taxonomy. It is a schema addition, and it is
recorded because schema changes are recorded.

### Alternatives considered

- **Both indexes.** Rejected above on write cost, with the number that would justify it stated so a
  later deployment can reopen it.
- **Make the statement selective instead.** The outbox scan reads up to 10,000 rows every 5 seconds to
  find, almost always, nothing pending, because its predicate (outbound direction, no `delivered_at`,
  no terminal delivery state, `next_attempt_at` due) runs in Rust rather than in SQL. That is the
  larger inefficiency and this ADR does not address it. It is a query-shape change with its own
  correctness surface, and pushing it into a schema change would hide it. It is filed separately.
- **Select fewer columns before the page is chosen.** The sorter is expensive partly because it
  carries `content` and `properties`. Selecting ids first and hydrating the page would shrink it
  without any index. It is a real option, it changes the store's read path rather than its schema,
  and it is not needed once the order is served by an index.
- **Do nothing and cap the caller.** Slowing the outbox loop would move the number without changing
  what a listing costs, and every other caller of `list` keeps paying.

## Consequences

- One more b-tree maintained on `notes` inserts, updates of `namespace`/`created_at`, and deletes. The
  partial predicate keeps soft-deleted rows out of it.
- Plan selection for the existing comm indexes is the risk this change carries. `notes` already holds
  `idx_notes_message_recipient_direction` and `idx_notes_unread_probe_recipient_direction`, whose
  leading columns are `(namespace, kind, ...)` and whose selection before `ANALYZE` depends on catalog
  order. A broader `(namespace, created_at DESC, id ASC)` index is a new candidate for those same
  queries, so the acceptance below asserts their plans are unchanged, on a fresh bootstrap as well as
  on an analyzed store.
- Existing stores gain the index at migration time, which is an index build proportional to the live
  note count on first open after upgrade. Measured on a file-backed synthetic store seeded by the same
  fixture generator at 105,000 notes, which is the size of a long-lived store today: **166 ms**, and
  **5.6 MiB** added to a 136 MiB database. The measurement was taken in a debug profile, so a release
  build is not slower. An upgrade window plans for a sixth of a second of extra first-open time at
  that size, and it grows linearly.

## Acceptance

- The `note_list_plan_tests` arms flip from creating the candidate inside the fixture to asserting the
  shipped schema serves the order: the unfiltered and common-kind listings name
  `idx_notes_namespace_created` and no arm's plan contains a temporary b-tree for ordering. The
  measurement is the plan, not a faster millisecond, since a faster millisecond is also what a warm
  page cache produces.
- The ids arm stays as it is and is the gate on correctness: the same page, in the same order, before
  and after.
- The rare-kind arm asserts the unchanged plan, so a later schema change that makes the planner start
  walking the namespace for a rare kind is a red test rather than a silent regression.
- `comm_filter_plan_tests` passes unchanged, including its fresh-bootstrap and pre-analyzed-upgrade
  arms, and its actor-seek assertion still names the recipient and unread indexes. That is the arm
  that would catch this index stealing a selective comm plan, and it is sufficient **only if those
  arms run against a schema that holds the new index**. A suite that passes because the index it is
  guarding against is absent from its fixture is not a guard, so the implementation adds a direct arm:
  with `idx_notes_namespace_created` present, the inbox plan still names
  `idx_notes_message_recipient_direction` and the unread plan still names
  `idx_notes_unread_probe_recipient_direction`, asserted by index name rather than by absence of a
  sorter. That arm runs on **both store shapes**, the fresh bootstrap and the pre-analyzed upgrade,
  because catalog order differs between them and catalog order is what decides selection before
  `ANALYZE` has run. The two shapes are separate arms, not one arm run twice, so a failure names which
  shape broke.
- The migration is a new versioned `.sql` file under `crates/khive-db/sql/`, registered in
  `migrations.rs`, with the fresh-store DDL in `notes-ddl.sql` carrying the same statement, since a
  fresh store does not replay migrations.
