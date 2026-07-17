# Delete and Merge Preimages

Delete and merge operations carry full stage-time preimages so later review, conflict detection, or compensation has the original records needed to reason about a destructive mutation.

## `DeleteOp`

`DeleteOp` contains a target ID, hard/soft flag, and required `DeletePreimage`. The preimage is an entity, note, or edge tagged by `substrate`; `substrate` avoids colliding with the embedded entity/note record's own `kind` field.

Custom deserialization requires `target_id` to equal the embedded record ID. A missing preimage, mismatched ID, unknown field, invalid note salience/decay factor, or invalid edge weight is a parse error. Because `preimage` is not optional, a delete without captured prior state is unrepresentable.

## `MergeOp`

`MergeOp` contains destination `into_id`, removed `from_id`, and a required `MergePreimage`. The preimage holds both complete entities and every incident edge the merge expects to rewire.

Deserialization requires `into_id` and `from_id` to match their embedded entity IDs. Every listed incident edge must touch at least one merge participant; unrelated edges are rejected. Embedded edges also retain the finite `[0.0, 1.0]` weight invariant.

## Strict embedded records

The public `khive_types` record deserializers accept unknown fields, so this crate first decodes private `deny_unknown_fields` mirrors. It then converts to the real `Entity`, `Note`, or `Link` and reuses `Note::is_valid`/`Link::is_valid` for range checks rather than duplicating their full validation logic.
