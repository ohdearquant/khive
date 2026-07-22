# khive-changeset

The KG change-set data model: a producer-agnostic, typed op-list with stage-time-stable
identifiers, and its NDJSON-delta serialization.

A change-set is a durable, inspectable staging artifact between "a producer decided what to
write" and "the write lands in the live graph." A batch producer (an extraction pipeline, an
interactive-agent op recorder, a future bulk-import adapter) emits one, a reviewer can render
it without executing it, and a later stage applies it as a unit. This crate defines the
artifact itself — nothing about how it is validated, tiered, reviewed, or committed lives here.

## What's in the model

- **`ChangeSet`** — an [`Envelope`] plus an ordered `Vec<Op>`. Operation order is semantically
  load-bearing: a `link` op may target the stage-time id an earlier `create` op in the same
  file minted, so order is preserved exactly through serialization.
- **`Envelope`** — change-set-level metadata captured at stage time: producer identity,
  producer model family, a `schema_version`, and an optional `batch_id`. No individual op
  reads it; it exists for a cross-family review gate, commit provenance, and (for `batch_id`)
  the commit trailer to consume downstream. `batch_id` is an opaque, producer-assigned token:
  when a producer supplies one, a commit landing the change-set uses it verbatim as the
  provenance trailer; when absent, the committing tool derives a deterministic fallback from
  `producer` and `staged_at` instead. Absent by default and never serialized (not even as
  `null`) — round-tripping an envelope without one leaves it unset.
- **`Op`** — one of five typed operations, over the same entity/edge/note vocabulary and
  edge-endpoint contract the live request DSL already uses:
  - `Create` — mints a stage-time-stable `Id128` for a new entity or note.
  - `Link` — creates a new edge; `source`/`target` may reference another op's minted id.
  - `Update` — patches an existing entity's, note's, or edge's mutable fields. Carries a
    **required** field-scoped `preimage`: the prior value of exactly the fields the patch
    touches (sets or explicitly clears to null), and nothing else. `UpdateOp`'s fields are
    private; the only ways to build one are the checked `UpdateOp::new` constructor and
    `Deserialize`, and both enforce that the preimage's populated field set matches the
    patch's touched field set exactly — a mismatched pair (a field the patch touches with no
    captured prior value, or a captured prior value for a field the patch leaves unchanged)
    cannot be constructed or deserialized.
  - `Delete` — removes an entity, note, or edge. Carries the full prior record state as a
    **required** field (`preimage`); a `delete` op without one cannot be constructed or
    deserialized.
  - `Merge` — merges two entities. Carries both prior entities and the incident edges the
    merge will rewire as a **required** field, for the same reason.

## NDJSON-delta serialization

`to_ndjson` / `from_ndjson` encode a change-set as one JSON object per line: the envelope as
line 1, then one line per op in stage order. Every line-level type derives
`#[serde(deny_unknown_fields)]`, so a misspelled or extraneous key fails the parse at that
line rather than being silently dropped. This extends to the full-record preimages a `delete`
or `merge` op embeds: `khive_types::{Entity, Note, Link}` accept unknown fields in their own
`Deserialize` impls, so preimages are parsed through crate-private strict mirror structs
(`src/strict.rs`) that add `deny_unknown_fields` before converting into the real types, keeping
the same guarantee without modifying `khive-types` itself. `from_ndjson` also rejects an envelope whose
`schema_version` does not match [`CURRENT_SCHEMA_VERSION`] — an unrecognized version is a
hard error, not a best-effort parse.

This departs deliberately from the whole-graph NDJSON export used elsewhere in this
workspace, which is sorted by primary key for diffability. A change-set's line order is
operation order, not a canonical sort, because operation order carries meaning a snapshot's
row order does not.

The envelope is a header line rather than a sidecar file: a change-set is meant to move as
one artifact between a producer, a reviewer, and an applier, and a header line keeps that
artifact self-contained without a second file that could go missing or drift out of sync.

## Constraints

No filesystem access and no I/O of any kind inside this crate — every function takes an
in-memory value and returns an in-memory value. The crate compiles for
`wasm32-unknown-unknown`; CI additionally executes its test suite under `wasm32-wasip1` (via
a `wasmtime` runner) and asserts pass/fail parity against the native run, since
`wasm32-unknown-unknown` has no standalone test-execution story without extra JS tooling this
repository does not otherwise depend on.

## Where this sits

`khive-changeset` depends only on `khive-types` (entity/edge/note vocabulary — kinds,
relations, and the closed edge-endpoint contract are never redefined here) plus `serde`,
`serde_json`, and `thiserror`. It knows nothing about any producer, any ingester, the rule
evaluator, diff computation, or the CLI — those are separate crates and separate lanes.

Governed by ADR-101
(change-set model — D1, D2, D5 crate #1) and consumed by
ADR-102
(tiered validate-and-merge, not implemented by this crate).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
