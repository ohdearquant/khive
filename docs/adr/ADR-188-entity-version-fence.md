# ADR-188: Persisted entity revisions and the update version fence

- **Status**: Accepted 2026-09-22 at scope: entities.version and its trigger; every canonical writer advances it exactly once, typed upsert included (INSERT ... ON CONFLICT(id) DO UPDATE, never INSERT OR REPLACE); generic update accepts expected_version with typed version_conflict; get, list and search expose it. Deferred: merge and delete caller fences (unfenced merge and delete still advance the version once, so a fenced update racing them is refused on its next write), #2718 no-op parity, SQL-level refusal of hand-written replacement outside the store API. Migration measurement recorded in Consequences.
- **Date**: 2026-09-22
- **Originally proposed**: 2026-09-13
- **Issue**: #2673; entity no-op parity (#2718) remains deferred
- **Depends on**: [ADR-015](ADR-015-schema-migrations.md) (versioned SQL migrations)
- **Relates to**: [ADR-014](ADR-014-curation-operations.md) (curation),
  [ADR-172](ADR-172-versioned-notes-compare-and-set.md) (the existing note contract)

## Historical context — before the entity version change

The 2026-09-13 proposal inspected revision `b15c2c8d0`. At that revision,
`crates/khive-db/sql/entities-ddl.sql` and `khive_storage::entity::Entity` had no `version`,
and the existing entity migrations added `content_ref` but no version counter. Entity updates
refused `expected_version` with "expected_version, fence and embed apply only to notes".
These are observations about that pre-change revision, not the current interface.

Notes already had a persisted counter and a writer-transaction `expected_version` check with
`reason=version_conflict`, `expected_version` and `current_version` details. Entities first
needed a stored counter and a read projection; there was no existing entity fence to enable.
Their timestamp-based internal snapshot checks could not detect a writer that left the
timestamp unchanged. A persisted revision supplies that missing comparison.

The original proposal also included merge/delete caller fences and entity/note no-op parity.
The accepted implementation scope below is narrower. Those future requirements are retained
explicitly; this revision neither implements them nor changes note semantics.

## Decision — persisted revisions and guarded update

### Storage invariant

Migration V37, `crates/khive-db/sql/037-entity-versions.sql`, adds
`version INTEGER NOT NULL DEFAULT 1` to `entities`; `entities-ddl.sql` carries the same column
and guards for fresh stores. Existing rows and first insertions start at one. The version is
an integer independent of `updated_at`.

Every successful subsequent typed entity-row write and every admitted direct SQL `UPDATE`
advances the stored version by exactly one. Writers explicitly set `version = version + 1`.
The `entities_version_insert_guard` trigger requires an inserted version of one;
`entities_version_update_guard` requires `NEW.version = OLD.version + 1` and rejects a
non-integer, an omitted/skipped increment, a jump or overflow. These are rejecting guards,
not a trigger that supplies an increment for a writer that omitted it.

The invariant covers upsert, batch upsert, guarded replacement, actual merge survivor/source
updates, soft deletion, restoration and namespace moves. It applies per row actually
updated. A losing conditional insertion, refused CAS, repeated soft delete of an existing
tombstone or restoration of an already-live row does not advance a version when it performs
no row update. Attachment-only operations mutate the attachment substrate, not the entity
row; an upsert that also writes attachments still advances the entity revision once.
Hard deletion removes the row; a later insertion starts a new row at one.

Typed upserts use `INSERT ... ON CONFLICT(id) DO UPDATE`, not replacement. Input
`Entity.version` is a read projection, not a caller-selected destination revision: inserts
start at one and updates increment the stored counter. Import and sync preserve local
revision history rather than importing another store's counter.

### Read projection and snapshot invalidation

`Entity` carries the persisted integer. Entity creation/update results, `get`, `list` and
hydrated entity search results expose it; `updated_at` remains a timestamp rather than a
version alias.

The existing internal timestamp/deletion CAS also compares the prepared entity's persisted
version. A competing write invalidates an older snapshot even if the timestamp did not
change. This snapshot guard applies independently of whether the caller supplied
`expected_version`. Omitting a caller revision is not permission to overwrite a stale
internally prepared snapshot.

### Caller fence on entity update

Generic entity `update` accepts an optional positive `expected_version`, representable as a
signed 64-bit integer. Zero, negative values, non-integers and out-of-range values are invalid
input. Omission or `null` supplies no caller revision assertion.

Ordinary and atomic entity updates use the shared writer-transaction guard. It reads the
live row's current version inside the transaction before that update plan's row or audit
statements run. A mismatch returns typed `conflict` with these string-valued details:

```json
{ "reason": "version_conflict", "expected_version": "1", "current_version": "2" }
```

The detail names and value representation match the note refusal contract. A missing or
deleted target follows the existing missing-target/snapshot refusal path; this change does
not turn every failure to find a live row into a version mismatch. A current caller version
does not bypass the existing snapshot CAS or other validation.

A stale update leaves its target unchanged. An atomic conflict rolls back the whole unit,
including earlier entity and audit writes in that unit. This is a domain-write guarantee;
it does not disable ordinary request auditing outside the unit.

The `fence` and `embed` parameters remain note-only. Caller `expected_version` fences for
entity `merge` and `delete` are not part of this implementation. Their existing row updates
nevertheless advance revisions, so an entity snapshot predating such an update is stale.
Public atomic merge remains inadmissible. Its retained internal planner is not a new public
surface and increments only the rows it actually updates.

### Identical entity updates retain their existing write behavior

An accepted entity update remains a write even when its requested fields equal the stored
fields, with or without `expected_version`. It advances version once and advances
`updated_at` strictly; it does not return `unchanged=true`. Stale identical fenced updates
refuse like any other stale update. This implementation does not extend ADR-172's note no-op
rules to entities and does not rewrite those note rules.

### Raw replacement boundary

Raw `INSERT OR REPLACE` of entity rows is forbidden by the typed store contract. The
`khive-db` integration test `issue2673_no_raw_entity_replace_outside_migrations` inventories
in-tree Rust/SQL sources for the forbidden form, with historical migration exemptions.
That source inventory is not runtime SQL admission.

The insert/update guards do not stop a privileged raw replacement from deleting an existing
row and reinserting it at version one. Hard deletion followed by insertion likewise starts
a new row lifetime. The accepted update fence therefore does not claim to detect these
identity resets across arbitrary raw SQL. Enforcing the replacement prohibition at SQL
admission, or retaining a persistent identity ledger across deletion/replacement, remains
separate work; no such enforcement is supplied by this migration.

## Alternatives considered

- **Compare only `updated_at`.** Rejected: a timestamp is not a counter, and a write that
  preserves it must still invalidate a prepared snapshot.
- **Keep the revision inside `properties`.** Rejected: the concurrency token belongs to the
  substrate row and must be checked independently of caller-managed properties.
- **Check at the Gate or before writer admission.** Rejected: permission to write does not
  establish that the observed row is still current. The authoritative check belongs inside
  the writer transaction.
- **Automatically bump an omitted increment.** The implementation instead requires every
  writer to supply exactly the next version and has SQLite reject violations. This makes a
  forgotten writer change fail explicitly rather than hiding it behind an automatic bump.
- **Leave entities without a caller version.** Rejected for `update`: callers need an
  observable revision and a transactional way to assert it. Broader verb coverage remains
  an explicit follow-up, not a guarantee of this initial update fence.

## Consequences of the accepted scope

- One column and two guard triggers are added to the primary entity table. Every entity
  `UPDATE` must include its increment; old writers that omit it fail after migration.
- Read responses gain `version`, which affects clients asserting an exact response shape.
  The accepted entity update result does not gain an identical-patch `unchanged` result.
- Ordinary and atomic entity update gain a typed conflict path. A version-only competing
  write also invalidates internal snapshot CAS, independently of timestamp movement.
- Unfenced merge/delete behavior is retained while their actual row mutations advance
  versions. Caller fences on those verbs remain deferred; the current interface is not the
  proposal's eventual all-curation-verb fence.
- The raw replacement limitation above remains. A source convention and inventory test
  cannot substitute for admission of privileged SQL. Until SQL-level refusal is implemented,
  `issue2673_no_raw_entity_replace_outside_migrations` in
  `crates/khive-db/tests/entity_write_inventory.rs` guards the crate-wide source inventory,
  including must-match controls proving that the scanner recognizes the forbidden form.
- The constant-default column addition does not rewrite existing entity rows: the measured
  write-ahead log after V37 is the same size at zero, 9,285 and 100,000 rows, and the
  migration takes under a millisecond at 100,000 rows (record below). This is a synthetic
  measurement on one host, not a conclusion about production-scale performance.

### Migration measurement record

Run on 2026-09-23 (UTC) with the procedure below.

- **Source**: the tree of commit `0d6b328b1ebfce7080c5faafb66eb86c34eb715a` (this change on base
  `0b46c8c63f7f723c78f5e1a8cc49d2fc56d05b2c`). Recording these results changes only this document.
- **Host**: Apple M4 (Mac16,10), 16 GiB memory, internal solid-state storage (APFS), macOS 27.0.
  The database lived in the system temporary directory on that volume. Release profile, bundled
  SQLite 3.53.2, WAL with synchronous NORMAL. The host was idle before each run (98.6% and 98.9%
  CPU idle), and no other build or test ran during timing.
- **Selection**: listing the ignored test by exact name selected one test. Each run exited 0 with
  `1 passed; 0 failed; 0 ignored` and two `synthetic_v36_to_v37` records reporting schema 37. The
  fixture times only `run_migrations`, then asserts that every seeded row survived with integer
  version 1.
- **Populations**: 9,285 rows, the caller-visible live lower bound observed 2026-09-15T16:40:27Z
  (excluding tombstones and other namespaces, so not a complete count), and 100,000 rows as
  synthetic stress.

Complete records, in run order (each population run emits its zero-row control first):

```json
{"db_size_before":667648,"elapsed_us":630,"label":"synthetic_v36_to_v37","page_size":4096,"platform":"macos-aarch64","rows":0,"schema_version":37,"sqlite_version":"3.53.2","wal_bytes_after":24752}
{"db_size_before":5525504,"elapsed_us":723,"label":"synthetic_v36_to_v37","page_size":4096,"platform":"macos-aarch64","rows":9285,"schema_version":37,"sqlite_version":"3.53.2","wal_bytes_after":24752}
{"db_size_before":667648,"elapsed_us":610,"label":"synthetic_v36_to_v37","page_size":4096,"platform":"macos-aarch64","rows":0,"schema_version":37,"sqlite_version":"3.53.2","wal_bytes_after":24752}
{"db_size_before":54059008,"elapsed_us":851,"label":"synthetic_v36_to_v37","page_size":4096,"platform":"macos-aarch64","rows":100000,"schema_version":37,"sqlite_version":"3.53.2","wal_bytes_after":24752}
```

| Rows    | Database before  | Elapsed | Same-run zero-row control | WAL after    |
| ------- | ---------------- | ------- | ------------------------- | ------------ |
| 9,285   | 5,525,504 bytes  | 723 µs  | 630 µs                    | 24,752 bytes |
| 100,000 | 54,059,008 bytes | 851 µs  | 610 µs                    | 24,752 bytes |

The write-ahead log after migration is 24,752 bytes at every population, so the migration writes
no per-row pages. Elapsed time above the same-run zero-row control is 93 µs at 9,285 rows and
241 µs at 100,000 rows: 2.6 times the residual for 10.8 times the rows, from one sample each. That
residual is reported as measured. Telling a row-proportional component apart from run-to-run
variation would need repeated samples.

## Acceptance for the current scope

These are validation requirements, not reported test outcomes:

1. V37 gives existing rows version one without changing their fields. Fresh-schema and
   migrated-schema guards agree; invalid insert versions, missing/jumped increments and
   overflow refuse without row mutation. An admitted increment moves by exactly one.
2. Typed upsert/batch/conditional insertion, import/sync, lifecycle and namespace-move
   writers obey the row-write invariant. Losing conditional writes do not advance it.
3. Public entity reads expose the persisted integer, separate from timestamp formatting.
4. A stale update preserves the complete target record. A current version succeeds once;
   malformed/nonpositive preconditions refuse. A note arm checks the same typed conflict
   detail names and string values.
5. Controlled preparation/commit interleaving proves the caller check occurs inside the
   writer transaction. Two writers asserting one revision have one winner. A later atomic
   conflict rolls back earlier entity and audit mutations in the unit.
6. A competing write that preserves `updated_at` invalidates an older version-bearing
   snapshot. Omitting the caller fence still preserves internal snapshot checks.
7. Identical entity updates, fenced and unfenced, retain write behavior and advance once;
   note behavior remains unchanged. Entity `fence`/`embed` remain refused.
8. The raw-replacement source inventory rejects new non-migration replacement writers,
   without being presented as runtime SQL enforcement.
9. The migration measurement described below is recorded in Consequences before merge.

Current source anchors include `migrations_tests::issue2673_v37_initializes_and_guards_entity_versions`,
`issue2673_entity_versions_cover_typed_storage_writers`, the runtime `entity_write_tests`,
and `khive-pack-kg/tests/entity_versions.rs`. The first is a correctness test for V37; it
does not measure migration cost.

## Deferred requirements — not part of the accepted implementation

### Merge and delete caller fences

The proposed `merge.expected_version` belongs to surviving `into_id`, not `from_id`. Both
rows must be reread inside the merge transaction and retain the existing both-side snapshot
and safety checks. `force=true` may bypass only the existing safety floor, never this version
fence. `dry_run=true` must check the survivor fence without mutation. A real merge must
advance each survivor/source row it updates once, as the current row invariant requires.

The proposed `delete.expected_version` must also guard hard deletion of an existing
tombstone. Soft deletion advances once; hard deletion removes the row only after checking
its current revision. Atomic delete must use the same caller guard. Public atomic merge
remains outside the supported surface unless separately admitted.

Future acceptance must prove stale merge preserves both rows and emits no merge event,
stale delete preserves its target, unfenced behavior is retained, and a source mutation
after merge preflight still triggers the existing safety refusal even when the survivor's
caller fence is current. These are not shipped caller-parameter guarantees of #2673.

### Entity no-op parity (#2718)

The retained proposal extends the ADR-172 Amendment 5 no-op definition to entities: after
validation, normalization, applicable hooks and property merging, an identical patch with
no `expected_version` and no explicit embedding request becomes a mutation-free assertion.
Canonical and atomic results would disclose `unchanged=true`; version, `updated_at`, stored
representation, indexes and mutation events would stay untouched. The writer transaction
must recheck the prepared snapshot; a prior atomic operation can invalidate it and cause
whole-unit rollback. A raw version-only write or update/delete must invalidate an old no-op
snapshot too.

Under that future rule an accepted identical fenced patch remains a write: version advances
once, `updated_at` advances strictly and `unchanged=true` is never returned. Stale identical
fenced patches refuse. One eligibility predicate and shared comparison helpers are proposed
for both substrates, with a shared canonical/atomic contract table in validation.

Object key order and tag order would be insignificant; duplicate tag counts and other array
ordering remain significant. Entity tags are in the tags column; note tags are in
`properties.tags`. An entity's custom `properties.tags` remains an ordinary ordered property.
Entity type aliases compare after vocabulary normalization. Explicit `embed` remains
note-only and preserves existing note write behavior, including `embed=false` on identical
data. This future entity parity work neither changes the current entity write behavior nor
redefines the independently governed note semantics.

## Migration measurement procedure

The earlier proposal described `synthetic_v34_to_v35` output reporting schema 35, but the
implementation preceding this revision had no measurement fixture or output. Entity
versions are migration V37; V34→V35 is not this migration. The existing
`issue2673_v37_initializes_and_guards_entity_versions` test is a correctness test, not a
replacement benchmark.

The ignored `migrations::entity_version_measurement::entity_version_migration_measurement`
test in `crates/khive-db/src/entity_version_migration_measurement.rs` accepts
`KHIVE_ENTITY_VERSION_ROWS` and uses the actual migration set for synthetic V36→V37 stores. Record its exact source revision before invoking it. First list ignored
tests under that name and require exactly one selected test; zero or multiple matches
invalidate the run. Then set `KHIVE_ENTITY_VERSION_ROWS` to the chosen population and run
with `--ignored --nocapture`. Require successful exit, exactly one passed test, zero
failures/ignored tests, and two records labelled `synthetic_v36_to_v37` reporting schema 37:
zero rows and the requested population. `running 0 tests`, missing records or exit status
alone is not a measurement.

Preserve the proposed bound of 1,000,000 seeded rows. The proposed populations are 9,285
rows (a caller-visible live lower bound observed 2026-09-15T16:40:27Z, excluding tombstones
and other namespaces) and 100,000 rows (synthetic stress). Neither is the complete fleet
count. Record population provenance; a full-table count, if obtained, must explicitly cover
tombstones and all namespaces and include its query and observation time.

Only `run_migrations` is to be timed. Use khive's bundled SQLite, WAL/NORMAL and a warm cache
after seeding, then assert every existing row reads version one. Required output includes
row count, elapsed microseconds, SQLite version, platform, page size, database size before
migration and WAL bytes after migration. Record source revision, host/storage hardware,
build profile and complete output here before merge. A result that scales with row count
must be reported as a finding against the expected cost, not rounded away.

Results are recorded in Consequences under "Migration measurement record".
