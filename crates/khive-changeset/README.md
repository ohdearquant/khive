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
  producer model family, and a `schema_version`. No individual op reads it; it exists for a
  cross-family review gate and commit provenance to consume downstream.
- **`Op`** — one of five typed operations, over the same entity/edge/note vocabulary and
  edge-endpoint contract the live request DSL already uses:
  - `Create` — mints a stage-time-stable `Id128` for a new entity or note.
  - `Link` — creates a new edge; `source`/`target` may reference another op's minted id.
  - `Update` — patches an existing entity's, note's, or edge's mutable fields. Carries no
    preimage (see "Known gap" below).
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

## Known gap: `Update` has no preimage

The op-list inversion this artifact is meant to support (a later, separate consumer) needs a
prior-value snapshot for every reversible op. The ADR that defines this model scopes
mandatory stage-time preimage capture to the two *destructive* operations, `delete` and
`merge`, and says nothing about `update`. The ADR that defines op-list inversion, by contrast,
describes an `update`'s inverse as restoring "the prior field values captured at stage time,"
which presumes an `update` op captures priors — something the first ADR never requires.

This crate follows the model-defining ADR's literal, binding text: `UpdateOp` carries no
preimage. An `update` op therefore cannot be surgically inverted today; a future revert of one
falls back to the coarser mechanisms available for any op without a captured preimage. This
gap is a candidate for a future ADR amendment (adding optional stage-time prior-value capture
to `update`), not something this crate has resolved by inventing schema the model ADR does
not specify.

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

Governed by [ADR-101](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-101-kg-changeset-model.md)
(change-set model — D1, D2, D5 crate #1) and consumed by
[ADR-102](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-102-tiered-validate-and-merge.md)
(tiered validate-and-merge, not implemented by this crate).

## License

Apache-2.0.
