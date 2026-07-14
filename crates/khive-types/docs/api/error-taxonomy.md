# Error Details — Bounding and Truncation

`KhiveError::Details` (`crates/khive-types/src/khive_error.rs`) is the
bounded key/value error-context map carried on every khive error variant.
This is the function-specific technical reference for how it caps size on
construction and preserves that cap through a deserialize round-trip.

## `Details::build` — bounding/truncation algorithm

See `crates/khive-types/src/khive_error.rs` — private fn `Details::build`.

Takes `ordinary` (already capped at 8 entries, with `total_ordinary` the true
pre-cap count) plus a reserved-key `collisions` count, and produces the wire
shape: at most 8 entries, with a `DETAILS_TRUNCATED_KEY` indicator entry
appended whenever anything was dropped (either overflow past 7 ordinary
pairs, or a stripped reserved-key collision). This is the shared
bounding/truncation logic used by both the public `Details::new` constructor
and the `serde::Deserialize` impl.

## `Details` deserialization — round-trip detection of self-truncated maps

See `crates/khive-types/src/khive_error.rs` — `impl<'de> Visitor<'de> for
DetailsVisitor` / `visit_map`.

The map visitor drains to completion regardless of size (fixes #487: a naive
early-exit once 8 entries are collected leaves trailing map bytes unconsumed
and corrupts the surrounding deserializer). Only the first 8 ordinary pairs
are retained in memory as they arrive; pairs beyond that are counted, not
stored, so an adversarially large map can't inflate memory.

`DETAILS_TRUNCATED_KEY` is reserved (PR #549): it is never stored as an
ordinary entry. Instead the visitor tracks whether the wire map looks
*exactly* like khive's own truncated serialization — the reserved key
appears exactly once, as the very last pair, immediately after exactly 7
ordinary pairs, with a value that parses as a count — and if so, restores
that as the trusted drop count (a round-trip of a `Details` khive truncated
itself). Any other occurrence (wrong position, duplicated, or paired with an
ordinary count that isn't 7) is treated as a client-supplied collision:
stripped and folded into `Details::build`'s drop accounting like any other
reserved-key collision.
