# JSON array adapter

`JsonFormatAdapter` eagerly parses a top-level JSON array into independently drainable entity and
edge result streams.

## Construction and errors

`new(json_input)` rejects invalid JSON or a non-array top level with `AdapterError::Parse`.
Every array element must be an object; a scalar element is `AdapterError::InvalidField` naming its
record index. Parsing is eager, so constructor success means structural parsing is complete, while
individual record-validation errors remain in the iterators.

`new_with_valid_kinds` has the same behavior but accepts caller-supplied entity kinds in addition to
the base `EntityKind` taxonomy and aliases. Extra kinds compare case-insensitively. A runtime with a
merged pack kind registry should pass it here so kinds such as `resource` are not rejected before
runtime validation.

## Entity/edge dispatch

Key matching is ASCII case-insensitive. An object with both `source`/`from` and `target`/`to` is an
edge; every other object is parsed as an entity. Entity and edge iteration drain their stored
results, so a second call produces no records.

## Entity parsing

Required non-empty fields are `kind` and `name`; an absent ID receives a new UUID. Reserved fields
include `entity_type`, description, tags, timestamps, and properties. Remaining unknown keys fold
into the properties object rather than being discarded. Unknown kinds fail unless accepted by the
base taxonomy, an alias, or the supplied extra-kind set.

## Edge parsing

Source, target, and relation are required. Relations always use the closed `EdgeRelation` taxonomy.
Weight defaults to `0.7` and must be finite in `[0, 1]`; IDs default to new UUIDs. Unknown edge
relations are fatal regardless of entity schema mode.

## Warnings and streaming

Non-fatal issues accumulate in `warnings()`. The current implementation holds the complete parsed
array and is not a streaming `Read` adapter; streaming is deferred to a later pipeline phase.
