# Note field handling: tags and patch applicability

Technical reference for how the `create`/`update` handlers merge note tags and enforce which
fields are patchable per substrate (`handlers/common.rs`, `handlers/update.rs`).

## `merge_note_tags`

Merges the top-level `tags` create-param into `properties["tags"]` for a note. Notes have no
dedicated tags column (see `search.rs`'s `tag_filter` handling) — `properties["tags"]` is the
storage convention already used by `memory.remember`
(`khive-pack-memory/src/handlers/remember.rs`) and by this pack's own `search`/`list`
note-tag filters. Without this merge, `create(kind=note, tags=[...])` silently dropped the
tags (#747).

Precedence: an empty/absent `tags` param leaves `properties` untouched. A non-empty `tags`
param always WINS over any `properties["tags"]` the caller also supplied — the top-level,
typed param is the more explicit signal, so it overwrites rather than merges with a
same-named nested key.

## `reject_inapplicable_fields` (`handlers/update.rs`)

Field applicability guard — authoritative field sets per substrate. Source of truth:
`handler_defs.rs:241-243` + `EntityPatch`/`NotePatch`/`EdgePatch` in
`crates/khive-runtime/src/curation.rs`:

| Substrate | Patchable fields |
| --------- | ----------------- |
| Entity    | `name`, `description`, `tags`, `properties` |
| Note      | `name`, `content`, `salience`, `decay_factor`, `properties` (notes have NO top-level tags column; tags live in `properties["tags"]`) |
| Edge      | `relation`, `weight`, `properties` |

Any present-but-inapplicable field is rejected with a fail-loud error naming the offending
field and listing the substrate's valid set. This function MUST be updated whenever
`UpdateParams` or a patch struct changes.
