# ADR-221: Entity Keys and Aliases

- **Status:** Accepted 2026-11-26 (#6872)
- **Date:** 2026-11-26
- **Grounding commit:** `4821b079b30dc645bbf4c23dc568cf18c453a1c8`
- **Amends:** [ADR-023](ADR-023-declarative-pack-format.md) (the keyed-note amendment's sentence on
  entities), [ADR-014](ADR-014-curation-operations.md) (update and merge write aliases and keys),
  [ADR-113](ADR-113-identifier-continuity.md) Amendment 1 (a merge redirect also carries names and
  keys)
- **Relates to:** [ADR-020](ADR-020-git-native-kg-implementation.md),
  [ADR-115](ADR-115-secret-gate-content-manifest-exemption.md),
  [ADR-172](ADR-172-versioned-notes-compare-and-set.md)

It does not change the status of any other record.

## Context and scope

Issue [#6862](https://github.com/ohdearquant/khive/issues/6862) rows 1-4 ask for a stable caller key
on entities, so that a reload finds the entity it loaded before, and for alternate names that search
and resolve match, so that a renamed or merged entity is still found by the name a caller knows.

Facts at the grounding commit:

- Entities have no key. Notes have `key` with a partial unique index on `(namespace, kind, key)`
  where `deleted_at IS NULL` (`crates/khive-db/sql/028-notes-key.sql`). The keyed-note amendment of
  ADR-023 ends: "No equivalent key exists for entities" (`ADR-023-declarative-pack-format.md` line
  954).
- `update` on an entity accepts `name`, `description`, `properties`, `tags` and `entity_type`; the
  base `kind` cannot change.
- `resolve` stage 3 is an exact, case-sensitive match on a live entity's name.
- The entity full-text document has the name as title and `name description` as body, and the same
  text is embedded (`crates/khive-runtime/src/curation/embedding_text.rs` lines 9-56). Every write,
  merge, repair and reindex path builds it from the entity record through `entity_fts_document`.
- `merge` runs as one SQL transaction (`merge_entity_sql`) and tombstones the removed entity with
  `merged_into`; the removed entity's name then resolves to nothing.
- Entities leave a store through four carriers: the portable archive (`ExportedEntity` in
  `crates/khive-runtime/src/portability.rs`), the ADR-020 NDJSON snapshot and its content hash
  (`crates/khive-vcs/src/sync.rs`, `crates/khive-vcs/src/hash.rs`, and the reader in
  `crates/kkernel/src/kg/archive.rs`), the three-way merge between stores (`crates/khive-merge`),
  and the change-set operations (`crates/khive-changeset/src/op.rs`). The SQL store in `khive-db`
  is the only backend that holds entities.

Out of scope: case-insensitive alias match, aliases in GQL, and row 5 (vocabulary declared as data).

## Decision

### 1. Entity key

`create`, as a single call and as each item of a bulk create, accepts `key`: at most 512 UTF-8
bytes, no U+0000, scanned by the secret gate on its exact bytes before any holder lookup, as note
keys are under ADR-115 Amendment 9 section 5.

The key is unique per `(namespace, kind, key)`, where `kind` is the base entity kind. This departs
from the issue's wording, which asks for per namespace and type: `entity_type` can be changed by
`update` and may be null, so a key scoped on it could move or collide when the type changes. A
caller that wants a key per type puts the type in the key (`paper:smith2024`).

Keys are rows in a new table `entity_keys(namespace, kind, key, entity_id, created_at,
released_at)` with a unique index on `(namespace, kind, key)` over held rows (`released_at IS
NULL`), so a deleted entity keeps its released keys for a restore to take back. A table rather than a column lets a merge move a key to
the kept entity, and lets one entity hold several keys after merges.

A keyed `create` behaves as a keyed note `create` does (ADR-172 Amendment 6):

- the key is not held: a new entity is written and the key is assigned in the same transaction;
- the key is held and the payload is identical (name, description, `entity_type`, typed-equal
  properties, tags as a set, caller-origin aliases as a set): the held id is returned with
  `created: false` and nothing is written;
- the key is held and the payload differs: `key_conflict`, with `existing_id` when the caller may
  list that entity, and the held entity is unchanged.

A reload that should apply changes says so: `on_key: "update"` on `create` (single and bulk) turns a
differing payload into an update of the held entity in the same transaction and answers
`created: false, updated: true`. Without `on_key`, `create` stays the non-upsert ADR-023 describes.
Under `on_key: "update"` the request's aliases replace the entity's caller-origin aliases; aliases
written by rename or merge are never removed by a reload.

Inside one bulk create, items are judged in order in one transaction: a second item with a key that
an earlier item of the same request assigned is judged against that earlier item, exactly as if it
had already been stored.

`get`, `update` and `delete` accept `(kind, key)` in place of an id. After merges one entity can
hold several keys. `delete` releases every key the entity holds. `restore` is all or nothing: it
takes every key back when all are free, and when another live entity holds any of them it refuses
with `restore_key_conflict` naming each taken key, takes back no key and leaves the entity deleted.

### 2. Aliases

An alias is another exact name of a live entity. Aliases are rows in a new table
`entity_aliases(namespace, entity_id, alias, origin, created_at)`, `origin` one of `caller`,
`rename` or `merge`, unique on `(namespace, entity_id, alias)`. The same alias may sit on several
entities: an alias is a name, not a key. Each alias is non-empty, at most 512 UTF-8 bytes, holds no
U+0000, and is scanned by the secret gate on its exact bytes before it is stored.

Callers write aliases with `create(aliases=[...])` and `update(add_aliases=[...],
remove_aliases=[...])`; `remove_aliases` removes caller-origin aliases only. Rename and merge write
them as in section 3. Entity reads return `aliases`, each with its origin.

Search: the entity record carries its aliases, and `entity_fts_document` and
`entity_embedding_text` put them in the body after the name (`name aliases description`); the
title stays the name. Because every path already builds the document from the record, create,
update, merge, repair, reindex and snapshot sync stay in parity without a second builder.

Resolve: stage 3 matches the name or an alias exactly and case-sensitively. Several matching live
entities give `Ambiguous` over exactly that set; a hit through an alias carries `matched_alias`.

Index upkeep: a write that changes aliases (create, update, rename, merge) needs the same index
targets as a name change, full text and every embedding model, and takes the index route that
write already takes. Routes that commit required indexes with the entity do so for aliases too. The
deferred update route records its pending-index obligation in the entity's transaction under
ADR-014 Amendment 7 and reports unfinished targets in the update response. Bulk create keeps its
existing reindex behaviour. No new deferral is introduced.

### 3. Rename and merge

An `update` that changes the name adds the old name as an alias with origin `rename` in the same
transaction, unless the call passes `keep_old_name: false`. Today the `entity_updated` event
payload names only the changed fields; when the name changes it gains `previous_name` and `name`,
so the record's history keeps the old name even when no alias is kept. With an alias kept, the
alias row and its `created_at` also say when the name changed. A keyed `create` with
`on_key: "update"` that changes the held entity's name is such an update and keeps the old name;
`create` takes no `keep_old_name`, so a loader that wants no alias renames the entity with
`update(keep_old_name=false)` before the reload.

An alias never equals its entity's current name, so resolve never answers one entity twice for one
string. `create` (including `on_key: "update"`) and `update(add_aliases=[...])` do not store an
alias equal to the name and list it in the response as not stored. A rename to a name the entity
holds as an alias deletes that alias row, whatever its origin, in the rename's transaction; the old
name becomes a `rename` alias as usual. A merge compares against the kept entity's name after the
merge: it stores no alias equal to that name, and when the merge strategy takes `from`'s name
(`merge_sql.rs` line 264), `into`'s previous name becomes a `merge` alias in its place. Resolve
counts matching entities, not matching strings, so a name and an alias of one entity that both
match are one hit.

`merge(into, from)` inside `merge_entity_sql`: `from`'s name and aliases become aliases of `into`
with origin `merge`, and `from`'s `entity_keys` rows move to `into`. Both entities are read under
the caller's one namespace. A merge without `force` refuses two entities of different kinds
(`crates/khive-runtime/src/curation/merge_sql.rs` lines 231-247, and `validate_entity_merge_floor`
in `crates/khive-runtime/src/curation/types_and_guards.rs` line 140), so a moved row keeps its
`(namespace, kind)`, and because uniqueness is on `(namespace, kind, key)` no key `into` holds can
equal it: moving cannot collide.

`force: true` skips that check (`merge_sql.rs` line 248), so a forced merge can join two kinds. A
key row always carries its holder's kind, so a forced cross-kind merge rewrites each moved row to
`into`'s kind. A rewritten key that `into` already holds is dropped as a duplicate. A rewritten key
that another live entity holds under `into`'s kind refuses the whole merge with
`merge_key_conflict`, naming the key and, when the caller may list it, that entity; nothing is
written. After a forced cross-kind merge the key no longer answers under `from`'s old kind; the
merge result lists each rewritten key, so a caller whose loader still names that kind can re-key
it.

A reload under a key `from` held, in `into`'s kind, then addresses the kept entity. A dry-run merge
lists the aliases and keys it would move, the keys it would rewrite and any `merge_key_conflict`,
and writes nothing. A merge redirect (ADR-113 Amendment 1) is unchanged for ids; names and keys of
the removed entity now reach the kept one directly.

A split that gives the old name to the parent renames the child with `keep_old_name: false`, so the
old name is not left as an alias on both and resolve stays unambiguous.

### 4. Where keys and aliases travel

A key or alias that a carrier drops is row 1 failing one hop later, so each carrier carries both:

- **Portable archive.** `ExportedEntity` gains `keys` and `aliases` (with origin), omitted when
  empty. Import writes them in the entity's transaction and applies the same key-conflict rules as
  `create`.
- **NDJSON snapshot and hash.** The NDJSON entity record gains the same two fields, omitted when
  empty, and both readers (`khive-vcs` sync and the `kkernel` archive reader) read them. The
  canonical hash serializes them only when non-empty, so a snapshot without keys or aliases keeps
  its snapshot id.
- **Three-way merge between stores.** Keys and aliases merge as sets per entity. A key that the
  merged result would place on two different entities is a typed conflict, never a silent winner.
- **Change sets.** `EntityCreateFields` gains `key` and `aliases`; `EntityPatch` and
  `EntityPreimage` gain the alias changes they touch; the merge preimage covers the aliases and
  keys a merge moves. These structs refuse unknown fields, so a reader older than this change
  refuses a change set that carries them rather than applying it without them.

## Migration

Two new tables in a new `sql/NNN-*.sql` migration; no backfill, since no existing entity has a key
or an alias. Existing full-text rows are unchanged until the entity is next written or reindexed.

## Acceptance

1. Keyed create: unheld key writes; identical replay returns `created: false` and writes nothing,
   with aliases given in a different order; a differing payload returns `key_conflict` and leaves
   the held entity unchanged.
2. `on_key: "update"`: a differing reload updates the held entity, replaces caller-origin aliases
   and keeps rename- and merge-origin aliases.
3. Bulk create with two items under one key: the second is judged against the first in the same
   transaction (identical: one entity; differing: `key_conflict` for the second item).
4. The same key under two kinds in one namespace names two entities; the same key in two
   namespaces names two entities.
5. Delete releases every key the entity holds (two keys after a merge); restore takes both back
   when free, and when another entity took one refuses with `restore_key_conflict` naming it,
   takes back neither and leaves the entity deleted.
6. A key or alias that the secret gate refuses is refused before any holder lookup.
7. Resolve by alias returns the entity with `matched_alias`; an alias on two entities returns
   `Ambiguous` over those two; text search finds an entity by an alias it holds.
8. Rename keeps the old name as a `rename` alias and resolve finds the entity by it;
   `keep_old_name: false` keeps no alias and the `entity_updated` event carries `previous_name`.
9. A split that renames the child with `keep_old_name: false` leaves the old name resolving to the
   parent only.
10. Merge moves `from`'s name, aliases and keys to `into`; a reload under `from`'s key updates
    `into`; a dry run lists what would move and writes nothing.
11. Export then import, and snapshot write then sync, round-trip keys and aliases; a snapshot with
    neither keeps the snapshot id it had before this change.
12. A three-way merge that would place one key on two entities reports a conflict.
13. A merge across kinds without `force` still refuses. A forced merge of two kinds rewrites
    `from`'s keys to `into`'s kind and lists them; a rewritten key that another live entity holds
    under `into`'s kind refuses the merge with `merge_key_conflict` and writes nothing, and the dry
    run reports the same conflict.
14. An alias equal to the entity's name is not stored on create, on `add_aliases`, on a rename back
    to a name held as an alias (which deletes that alias), or on a merge, including one whose
    strategy takes `from`'s name; resolve of that string returns the entity once.
