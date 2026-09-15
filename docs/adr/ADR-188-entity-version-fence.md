# ADR-188: Entities get a version, and then a fence

- **Status**: Proposed
- **Date**: 2026-09-13
- **Depends on**: [ADR-015](ADR-015-schema-migrations.md) (schema changes ship as a new versioned
  migration over a `.sql` file)
- **Relates to**: [ADR-014](ADR-014-curation-operations.md) (the curation verbs this fence guards)

## Context

Two writers holding the same observed version of an entity both update it, and the second silently
replaces the first. For notes this is already solved: `update` accepts `expected_version`, checks it
inside the writer transaction, and refuses a stale write with `reason=version_conflict` plus
`expected_version` and `current_version`. For entities the parameter is refused outright, with
"expected_version, fence and embed apply only to notes".

The obvious reading is that a guard exists and is switched off for one substrate. That reading is
wrong, and it is worth stating plainly because the work depends on it.

### What was measured

At `b15c2c8d0`:

- `crates/khive-db/sql/entities-ddl.sql` declares `entities` with `id`, `namespace`, `kind`,
  `entity_type`, `name`, `description`, `properties`, `tags`, `created_at`, `updated_at`,
  `deleted_at`, `merged_into`, `merge_event_id`. There is no `version` column.
- The only `ALTER TABLE entities` in the migration set adds `content_ref`.
- `khive_storage::entity::Entity` has no `version` field.

Notes have both: a `version` column and a `bump_note_version` trigger that increments it on any update
that did not set it explicitly.

So entities have no version at all. A fence cannot be unlocked for them, because there is nothing to
compare against. This is a schema change and a read-surface change before it is a guard change, which
is why it is recorded rather than fixed in place.

## Decision

**Give entities a version, on the same terms notes have one, and then accept the same fence.**

1. A new versioned migration runs `ALTER TABLE entities ADD COLUMN version INTEGER NOT NULL DEFAULT 1`,
   with the counterpart column in `entities-ddl.sql` for fresh stores. SQLite does not rewrite the
   table for a column added with a constant default, so the upgrade cost is expected to be
   near-constant rather than proportional to the entity count. That expectation is measured at the
   fleet's entity count and the number written into Consequences before this merges. A measurement
   that disagrees is a finding about the migration, not a number to round off.
2. A trigger increments it on update, mirroring `bump_note_version`, so the version moves for every
   writer rather than only for writers that remember to move it. A trigger rather than handler code,
   for the same reason notes use one: a write path that forgets is the failure the fence exists to
   catch.
3. `Entity` carries `version`, and entity reads return it. A caller cannot use a fence it cannot
   observe.
4. `update`, `merge` and `delete` each accept a positive `expected_version` for entities, checked
   **inside the writer transaction**, refusing a stale value without mutation, with the same
   `reason`, `expected_version` and `current_version` fields notes return. Omitting it requires no
   caller revision assertion; identical unfenced updates follow the no-op rule below. Every verb that writes an entity carries
   the parameter, because a guarantee that depends on which verb a competing writer happened to call
   is not a guarantee: a fenced `update` racing an unfenced `merge` loses in silence.

For `merge`, `expected_version` belongs to the surviving `into_id`, the record the caller
continues using. It does **not** assert a version for `from_id`. Both rows are reread inside the
merge transaction, and the existing both-side snapshot and safety checks still apply. The one
parameter cannot name two revisions. `force=true` bypasses the existing safety floor only; it never
bypasses the version fence. `dry_run=true` checks the same survivor fence without mutation. After a
real merge, both the survivor update and source tombstone advance their own versions once.

For `delete`, the fence also applies to hard deletion of an existing tombstone. A soft delete advances
the version once; hard delete removes the row after checking its current version. Atomic `update`
and `delete` use the same transaction guard; atomic merge remains outside the existing supported
surface.

Entity and note updates share the ADR-172 Amendment 5 no-op definition (#2718): after validation,
normalization, hooks where applicable, and property merging, an identical patch with no
`expected_version` and no explicit embedding request is a mutation-free assertion. Canonical and
atomic results disclose `unchanged=true`; version, `updated_at`, stored representation, indexes,
and mutation events stay untouched. The writer transaction rechecks the prepared snapshot, so an
earlier operation in the atomic unit can invalidate a no-op and roll back the unit. An accepted
identical patch with `expected_version` is always a write: it advances version by exactly one and
`updated_at` strictly, and never returns `unchanged=true`. Stale identical fenced patches refuse.

The implementation uses one eligibility predicate and shared comparison helpers, tested across
both substrates. Object key order and tag order are insignificant; duplicate tag counts and other
array ordering remain significant. Entity tags live in the tags column; note tags live in
`properties.tags`. An entity's custom `properties.tags` remains an ordinary ordered property.
Entity type aliases compare after vocabulary normalization. Explicit `embed` remains note-only
and preserves existing note write behavior, including an explicit `embed=false` on identical data.

The refusal shape is shared rather than parallel. Two refusal shapes that mean the same thing is how a
client ends up special-casing a substrate, which is the state this record is removing.

### What this does not do

- It does not rewrite stored entities. Existing rows start at version 1.
- It does not make the fence mandatory. Unfenced changes still require no caller version.
- It does not change note semantics or add `fence`/`embed` to entities. It extends the existing note
  no-op rule to entities as part of this version implementation (#2718).

## Alternatives considered

- **Compare `updated_at` instead.** Entities already carry it, so no schema change. Rejected: a
  timestamp is not a counter. Two writes inside the same clock tick are indistinguishable, the value
  moves for reasons unrelated to the write, and a client that stores it is holding a value whose
  comparison semantics depend on the clock rather than on the store.
- **Keep a version inside `properties`.** No migration, and it is what a caller would build for
  itself today. Rejected: a trigger cannot maintain it, so it moves only when a writer remembers, and
  the writer that forgets is exactly the one the fence is for.
- **Enforce optimistic concurrency at the Gate.** Wrong seam. The Gate answers whether a caller may
  write; this is whether the write is still valid, and the only place that can be decided is inside
  the writer transaction.
- **Document last-writer-wins for entities and close the issue.** Honest, and it leaves a caller with
  no way to write safely to a shared record. The asymmetry with notes is not a design, it is an
  accident of which substrate got the feature first.

## Consequences

- One more column and one more trigger on `entities`, the graph's primary table. The trigger fires on
  every entity update, which is a cost paid by every writer, including those that never fence.
- Entity upserts use `INSERT ... ON CONFLICT(id) DO UPDATE`, because `INSERT OR REPLACE`
  would delete/reinsert the row and reset the default revision. Input `Entity.version` is a read
  projection: fresh inserts start at one and existing upserts advance the stored revision. Import
  and sync therefore use local revisions rather than importing a counter from another store.
- Entity read responses gain a field. Additive for a client that ignores unknown fields, and a change
  for anything asserting an exact shape.
- Canonical and atomic entity/note write results add `unchanged=true` for accepted identical
  unfenced patches that qualify for the no-op rule above. This is additive for clients that ignore
  unknown fields; clients asserting an exact response shape are affected.
- An atomic no-op rechecks its prepared snapshot inside the writer transaction. An earlier operation
  in the same unit can invalidate that assertion, causing the whole unit to roll back, including its
  earlier mutations.
- The migration adds a column with a constant default, which SQLite records in the schema without
  rewriting the table. The measured cost at the fleet's entity count is written here before this
  merges, the way the listing index's build cost was. If that cost turns out to scale with the row
  count, the assumption above is wrong and that is the finding.
- Three verbs grow a refusal path rather than one. `merge` and `delete` carry the parameter on the
  same terms as `update`, which is what makes the guarantee unconditional and is the reason the cost
  is worth paying.

## Acceptance

- A stale entity update is refused **without mutation**: the assertion reads back every field the
  update would have changed, and the version, and finds them unchanged. A refusal that still wrote
  something is the failure this record exists to prevent, and asserting only the error misses it.
- A current-version update succeeds and the version moves by exactly one. "Moves" is not enough: a
  trigger that double-bumps breaks every caller that read a version and wants to write once.
- Identical fenced entity and note patches advance by exactly one and never disclose `unchanged`;
  identical unfenced patches disclose `unchanged=true` and preserve the exact stored record,
  including version and `updated_at`. Both canonical and atomic tests use one contract table.
- A no-op prepared before a raw version-only writer or a prior atomic update/delete refuses its
  stale snapshot. The atomic unit rolls back earlier mutations too.
- A note arm as the control for the shared refusal shape: the same `reason` and the same field names,
  asserted against one definition rather than two string literals.
- An arm proving the check happens inside the writer transaction rather than before it: two writers
  racing on one entity, one of which must be refused. Without this the fence is a pre-flight read and
  the race it was built for is still open.
- An arm reading an entity and asserting `version` is present, since a fence a caller cannot observe
  is not usable.
- An arm for an entity update with no `expected_version`, asserting it still succeeds unconditionally.
- A stale `merge` is refused without mutation of either side: neither the surviving entity nor the
  one that would have been merged away has moved, and no merge event was recorded.
- A stale `delete` is refused and the entity is still readable afterwards.
- A `merge` and a `delete` with no `expected_version`, each asserting today's unconditional behaviour
  is unchanged.
- A source mutation after the caller's merge preflight still triggers the existing transactional
  safety refusal, even when `into_id` satisfies its version fence.
- The migration's cost at the fleet's entity count, measured on a synthetic store and written into
  Consequences before merge, with the row count stated beside it so the shape of the cost is readable
  and not only its magnitude.

### Migration measurement procedure

The implementation packet for #2673 and #2718 must deliver `khive-db`'s ignored
`entity_version_migration_measurement` test and its `KHIVE_ENTITY_VERSION_ROWS` input. This
ADR-only change does not publish that fixture. Run the procedure only from the implementation
revision containing it, and record that exact revision beside the result.

Before measuring, list the ignored tests under that name and require exactly one selected test;
zero matches or multiple matches invalidate the run. Then set `KHIVE_ENTITY_VERSION_ROWS` to the
chosen synthetic population and run with `--ignored --nocapture`. Require a successful exit,
exactly one passed test with zero failures and zero ignored tests, and two measurement records
labelled `synthetic_v34_to_v35`: one with zero rows and one with the requested population, both
reporting schema version 35. A successful exit alone, missing records, or `running 0 tests` is not
a measurement and must not be recorded as one.

The fixture must bound seeding to 1,000,000 rows and report both empty and populated V34 stores.
The validation packet will run 9,285 rows (a caller-visible live lower bound observed
2026-09-15T16:40:27Z, excluding tombstones and other namespaces) and 100,000 rows (synthetic stress).
Neither is the complete fleet count. Record population provenance beside each result; a full-table
count, if obtained, must explicitly include tombstones and all namespaces. Only `run_migrations`
is timed. The fixture must use khive's bundled SQLite, WAL/NORMAL, and a warm cache after seeding,
and check that every existing row reads version one afterward. Required output includes row count,
elapsed microseconds, SQLite version, platform, page size, database size before migration, and WAL
bytes after migration. Record the exact source revision, host/storage hardware, fleet-count query
and time, build profile, and output here before merge. No measurement has been performed in the
source-only implementation packet.
