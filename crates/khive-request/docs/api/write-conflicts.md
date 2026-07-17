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

Unknown tools, missing fields, non-string values, and dynamic `$prev` arguments contribute no key because their target is not statically knowable. `create` is excluded because its UUID is generated later and database uniqueness constraints own concurrent-create conflicts.

## Substrate separation

Entity keys and edge keys intentionally differ. Updating entity `X` and linking from `X` do not conflict: the first writes `entity:X`, while the second writes an edge record identified as `edge-natural:X:Y:relation`.

Bulk and singleton links use the same key builder so equivalent entries collide. The bulk extractor skips malformed/non-object entries; verb validation reports their shape errors elsewhere.

## Relation and endpoint canonicalization

Relation keys are lowercased, hyphens become underscores, and other non-ASCII-alphanumeric/non-underscore characters are removed. The aliases `competeswith` and `composedwith` normalize to their underscored forms.

The local symmetric set is deliberately conservative: only `competes_with` and `composed_with`. For those relations, endpoints are lexicographically ordered so `A→B` and `B→A` yield one key. Directional relations retain endpoint order. Keeping this small table local avoids making `khive-request` depend on the full domain-type registry.

## Batch preflight boundary

Sequential chains may repeat a key because execution order is defined. Parallel conflict detection records the first tool claiming each key and reports the second as `DslError::WriteKeyConflict`.

The batch-scanning helper is currently test-only; the production integration surface is the public per-op extractor. This distinction keeps parser output transport-agnostic while allowing an execution layer to choose envelope or whole-batch conflict policy.
