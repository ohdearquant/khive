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

1. A new versioned migration adds `version` to `entities`, defaulted so existing rows are valid, with
   the counterpart in `entities-ddl.sql` for fresh stores.
2. A trigger increments it on update, mirroring `bump_note_version`, so the version moves for every
   writer rather than only for writers that remember to move it. A trigger rather than handler code,
   for the same reason notes use one: a write path that forgets is the failure the fence exists to
   catch.
3. `Entity` carries `version`, and entity reads return it. A caller cannot use a fence it cannot
   observe.
4. `update` accepts a positive `expected_version` for entities, checked **inside the writer
   transaction**, refusing a stale value without mutation, with the same `reason`,
   `expected_version` and `current_version` fields notes return. Omitting it preserves today's
   unconditional semantics exactly.

The refusal shape is shared rather than parallel. Two refusal shapes that mean the same thing is how a
client ends up special-casing a substrate, which is the state this record is removing.

### What this does not do

- It does not rewrite stored entities. Existing rows start at the default version.
- It does not make the fence mandatory. A caller that never passes `expected_version` sees no change.
- It does not decide what a no-op update should do. Whether an update that changes nothing bumps the
  version is a separate open question that applies to both substrates, and whatever is decided there
  applies here unchanged: the two substrates stay symmetric, which is the whole point of this record.

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
- Entity read responses gain a field. Additive for a client that ignores unknown fields, and a change
  for anything asserting an exact shape.
- The migration backfills nothing but does rewrite the table if SQLite chooses to, which is an upgrade
  cost proportional to the entity count and should be measured and recorded in this ADR before it
  merges, the way the listing index's build cost was.
- `merge` and `delete` also write entities. Whether those paths participate in the fence is decided by
  the implementation and asserted either way, since a merge that bypasses the fence makes the
  guarantee conditional on which verb a competing writer used.

## Acceptance

- A stale entity update is refused **without mutation**: the assertion reads back every field the
  update would have changed, and the version, and finds them unchanged. A refusal that still wrote
  something is the failure this record exists to prevent, and asserting only the error misses it.
- A current-version update succeeds and the version moves by exactly one. "Moves" is not enough: a
  trigger that double-bumps breaks every caller that read a version and wants to write once.
- A note arm as the control for the shared refusal shape: the same `reason` and the same field names,
  asserted against one definition rather than two string literals.
- An arm proving the check happens inside the writer transaction rather than before it: two writers
  racing on one entity, one of which must be refused. Without this the fence is a pre-flight read and
  the race it was built for is still open.
- An arm reading an entity and asserting `version` is present, since a fence a caller cannot observe
  is not usable.
- An arm for an entity update with no `expected_version`, asserting it still succeeds unconditionally.
- The migration's cost at the fleet's entity count, measured on a synthetic store and written into
  Consequences before merge.
