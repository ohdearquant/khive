# Write-Conflict Keys

`write_keys_for_op_pub` derives conservative, substrate-prefixed keys for operations that can target the same stored record. The MCP dispatcher uses these keys to build per-operation conflict envelopes without coupling request parsing to storage.

## `write_keys_for_op_pub`

Only statically available string arguments contribute keys:

| Tool shape                                                   | Keys                                     |
| ------------------------------------------------------------ | ---------------------------------------- |
| `update(id=...)`                                             | `entity:<id>`                            |
| `delete(id=...)`                                             | `entity:<id>`                            |
| `merge(into_id=..., from_id=...)`                            | one `entity:` key per ID                 |
| singleton `link(source_id=..., target_id=..., relation=...)` | one natural edge key                     |
| bulk `link(links=[...])`                                     | one natural edge key per complete object |
| `merge(..., dry_run=true)`                                   | none: the op writes nothing              |

A singleton `link` may also spell its fields `source`, `target` and `kind`, the accepted aliases of `source_id`, `target_id` and `relation`. Each field reads the canonical name first and the alias only when the canonical name is absent, so both spellings of one edge produce the same key and conflict with each other. A call giving both spellings of a field is refused by the handler, so the choice of key for it does not matter. Entries inside `links=[...]` keep the canonical names only.

Unknown tools, missing fields, non-string values, and dynamic `$prev` arguments contribute no key because their target is not statically knowable. `create` is excluded because its UUID is generated later and database uniqueness constraints own concurrent-create conflicts.

A `merge` carrying a literal `dry_run=true` contributes no key either: it reads the pair, evaluates the safety floor and the strategy, and returns a prediction, so it targets no stored record. Only the literal boolean counts. An absent, `false` or non-boolean `dry_run` keeps the keys, by the same rule as above: an argument the parser cannot read as a preview stays conservative. A preview may therefore sit in a batch beside a write to the same record, and its prediction describes the state it read, which is what a read beside a parallel write already means here.

## Substrate separation

Entity keys and edge keys intentionally differ. Updating entity `X` and linking from `X` do not conflict: the first writes `entity:X`, while the second writes an edge record identified as `edge-natural:X:Y:relation`.

Bulk and singleton links use the same key builder so equivalent entries collide. The bulk extractor skips malformed/non-object entries; verb validation reports their shape errors elsewhere.

## Relation and endpoint canonicalization

Known relations use `khive_types::EdgeRelation` to canonicalize accepted spellings and symmetric endpoints. Case-insensitive names, hyphenated names and supported squashed aliases therefore share the stored snake_case key.

Unknown relation strings still contribute a key: their original spelling and endpoint order are retained. Relation validation remains the handler's responsibility. The key builder uses the existing `khive-types` dependency and has no local relation table.

## Batch preflight boundary

Sequential chains may repeat a key because execution order is defined. The test-only batch checker records the first tool claiming each key and reports the second as `DslError::WriteKeyConflict`. Production uses the public key extractor and participant helpers below to retain per-entry refusals without changing parser admission.

## Conflict participants

`write_key_conflict_ops` returns one sorted, unique index list per operation in a
flat batch. Each list contains every operation sharing one of that operation's
conflicting keys, including the operation itself. For example, operations 0 and 2
claiming the same key both report `[0, 2]`. The union is direct: keys A, A+B, B
produce `[0, 1]`, `[0, 1, 2]`, `[1, 2]`, not a transitive group for every entry.
Existing flat admission also refuses a key repeated inside a single operation;
its unique diagnostic is `[i]`.

`unit_write_key_conflict_ops` uses the parser's unit ranges for a parallel batch
of chains. A key must occur in different units to conflict; once it does, every
leaf claiming it participates, including repeated claims inside one unit. All
indexes refer to the flattened request, never positions local to a chain.

MCP adds `conflict_ops` to entries refused by this preflight. An innocent leaf
aborted with a conflicting unit reports `[]`; directly conflicting leaves retain
their participant lists even when their existing entry is marked `aborted`.
Failure/abort positions, messages and `not_committed` dispositions are unchanged.
Unrelated results and ordinary chain failures omit the field. Ordered top-level
chains and transactional atomic execution do not acquire parallel admission.
The local ops-file serial scheduler still uses flat batch admission; its indexes
are relative to the dispatched chunk, unlike its separate global `op_index`.

`DslError::WriteKeyConflict` carries the sorted owners of its reported key and
keeps its existing display message. Its batch-scanning constructor remains
test-only: parsing does not reject a whole request because of a conflict. MCP
preserves this typed field when converting such an error to structured data.
