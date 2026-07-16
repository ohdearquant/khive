# Create and Link Operations

Create and link operations mint stable record IDs at staging time. This lets later operations refer to staged records before the change-set is applied.

## `Op`

`Op` is internally tagged by the snake-case `op` field and has five variants: create, link, update, delete, and merge. Every NDJSON operation line is therefore self-describing without the envelope.

## `CreateOp`

`CreateOp` carries a stage-time ID, namespace, and `CreateTarget`. The target is tagged by `kind` and separates entity fields from note fields.

`EntityCreateFields` carries the closed `EntityKind`, optional governed `entity_type`, name, optional description, properties, and tags. `NoteCreateFields` carries a pack-declared note-kind string, content, properties, tags, optional salience, and optional decay factor. The note kind is intentionally not a closed enum because packs declare it.

Create payloads deny unknown fields. Optional fields use serde defaults; absent maps and vectors become empty, while absent optional scalars remain `None`.

## `LinkOp`

`LinkOp` creates a directed edge with a stable stage-time ID, namespace, source and target IDs, closed `EdgeRelation`, weight, and properties. Endpoints may be IDs staged by another operation in the same or a different change-set.

Deserialization rejects unknown fields and requires weight to be finite and within `[0.0, 1.0]`. The checked wire mirror ensures a serialized staged edge can never carry a weight that the live `khive_types::Link` model would reject.
