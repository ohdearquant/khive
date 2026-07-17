# Change-Set Envelope

`Envelope` records stage-time producer provenance once for the whole ordered operation list. Operations remain producer-agnostic and do not reference the envelope.

## Schema version

`CURRENT_SCHEMA_VERSION` is the only version this crate emits and accepts. `Envelope::new` always stamps that value, while decoding rejects any other version before reading operations.

## Identity fields

`producer` is an opaque interactive-agent or pipeline identity. `producer_model_family` is an opaque model-family token consumed by the cross-family review gate. `staged_at` is a caller-supplied wall-clock timestamp; the constructor does not read a clock.

The wire shape denies unknown fields so provenance typos cannot silently disappear during a round trip.

## Batch provenance

`batch_id` is an optional, producer-assigned opaque identifier. `Envelope::new` leaves it absent, and serde omits the key entirely rather than writing `null`. `with_batch_id` consumes the envelope, stores the converted string, and returns the updated value.

When a commit lands the change-set, a present batch ID is used verbatim as its provenance trailer. If absent, the committing tool derives a deterministic fallback from `producer` and `staged_at`. Producers without their own batching model are not required to invent an identifier.
