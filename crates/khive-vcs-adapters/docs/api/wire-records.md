# Intermediate entity and edge records

`EntityRecord` and `EdgeRecord` are the adapter output shapes consumed by the standard KG import
pipeline; adapters do not write them directly to the database.

## `EntityRecord`

The record carries UUID `id`, string `kind`, optional `entity_type`, non-empty `name`, optional
description, JSON properties, tags, and optional RFC 3339 creation/update timestamps. The adapter
reserves `entity_type`, `created_at`, and `updated_at` before folding unknown input keys into
properties, so those compatibility fields never appear twice.

## `EdgeRecord`

The record carries UUID `edge_id`, source and target IDs, relation, weight, JSON properties, and
optional timestamps. Weight defaults to `0.7`. Custom deserialization rejects NaN and infinities,
then rejects finite values outside `[0, 1]`; direct Rust construction remains the caller's
responsibility.

## Error taxonomy

`MissingField`, `InvalidField`, `Parse`, `UnknownKind`, and `UnknownRelation` are fatal record or
source errors. `NotYetImplemented` identifies formats deferred from this crate. Fatal errors are
returned through iterator items and must be handled atomically by the import caller; non-fatal
optional-field issues belong in `FormatAdapter::warnings()`.
