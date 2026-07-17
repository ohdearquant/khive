# Why Change-Sets Carry Preimages

A staged mutation can sit between proposal and application while the live graph continues to change. Preimages preserve what the producer actually observed, allowing review and apply-time logic to distinguish an intentional edit from a stale or conflicting assumption.

## Field-scoped updates

Updates need only the prior values of fields they change. Requiring exact patch/preimage congruence avoids two failure modes: an omitted prior value that makes conflict detection blind, and unrelated prior fields that create false conflicts when the live record changes elsewhere.

The three-state `Option<Option<T>>` representation is necessary for nullable fields. A staged clear must remain distinguishable from leaving the field untouched, and the preimage must capture the value that the clear replaces.

## Full destructive preimages

Deletes remove a complete record, so they capture the complete prior entity, note, or edge. Merges affect both participants and their graph neighborhood, so they capture both entities and the incident edges to be rewired.

Making these preimages required in the serialized type turns missing evidence into a parse failure instead of an apply-time ambiguity. Matching embedded IDs to operation IDs also prevents a syntactically valid preimage from describing a different record.

## Strictness at the change boundary

Change-set files are durable review artifacts, so silently ignoring a misspelled field is more dangerous than accepting forward-compatible data. Private strict wire mirrors therefore reject unknown embedded-record fields even though the general-purpose domain types are more permissive.

Range validation is delegated back to live domain models where possible. This keeps staged history subject to the same salience, decay, and weight invariants as production records without creating a second drifting definition.
