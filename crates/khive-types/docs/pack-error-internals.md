# Pack + error internals

Long-form rationale extracted from `src/pack.rs` and `src/khive_error.rs` doc-comments.
Public-item contracts stay complete in the source; this file carries the
"why", design history, and cross-references.

## `pack.rs`

### ADR-099 D3 atomic-admissibility rejection classes

See `crates/khive-types/src/pack.rs` — `ATOMIC_KNOWN_UNIMPLEMENTED_VERBS`.

`propose` / `review` / `withdraw` and `merge` are all members of
`ATOMIC_ADMISSIBLE_VERBS` (ADR-099 D3 intends every one of them to eventually
gain a prepare/apply seam), but none has a full-parity seam implemented yet,
so they are rejected up front rather than admitted with a silent gap:

- `propose` / `review` / `withdraw` (ADR-046's event-sourced change-proposal
  lifecycle): their apply path is a changeset-interpreter over a dedicated
  `proposals_open` table, not a small number of guarded DML statements — no
  prepare implementation exists at all.
- `merge`: a full-parity atomic prepare (field folding, survivor FTS/vector
  reindex, loser index purge, merge provenance, same-kind rejection, graceful
  edge-conflict resolution — see `curation::merge_entity_sql`) was drafted
  and unit-tested for B3, but deferred rather than shipped:
  `curation.rs`'s edge-rewire conflict handling does per-row procedural
  branching (read, canonicalize, probe for a conflicting triple,
  delete-and-refresh vs. update-in-place) that cannot be expressed as
  ADR-099 D1's static predicate/guard plan shape. Full parity is not
  achievable without either accepting a documented behavioral gap or a
  design change to the plan model — bias toward deferral over shipping a
  partially scoped atomic merge. `merge` stays admissible under the
  *non-atomic* verb path.

ADR-099 B3 (governance verbs) note: this set previously passed the static
pre-runtime admissibility check (since it's a subset of
`ATOMIC_ADMISSIBLE_VERBS`) and only failed later, inside
`atomic_prepare::prepare_op`, AFTER `KhiveRuntime::new` had already run.
`atomic_admissibility` now checks this set FIRST so the CLI boundary
(`khive_request::atomic::check_atomic_admissible`) rejects these verbs before
any runtime is built or any write attempted — the same before-any-write
guarantee every other rejection class gets.

The `atomic_admissible_list_matches_adr` test in `pack.rs` pins the exact
`ATOMIC_ADMISSIBLE_VERBS` set as a drift-pin against ADR-099 D3's literal
list, so an edit to the const forces the editor to also touch the test and
its ADR citation.

### `ATOMIC_MAX_OPS_DEFAULT` = 2000 — rationale

See `crates/khive-types/src/pack.rs` — `ATOMIC_MAX_OPS_DEFAULT`.

ADR-099 migration step 7 / B3. The ADR does not pin an exact number — D2
defers the precise threshold to harness measurement ("a recommended default
on the order of a few thousand ops ... configurable with a conservative
default", Open Question 2), so this constant is an explicit interim choice,
not a value read directly out of the ADR text.

Rationale for 2000 specifically:
- inside D2's "a few thousand" band
- comfortably bounds the duration of the single cross-process `BEGIN
  IMMEDIATE` hold an atomic unit takes on the daemon's writer lock (ADR-099
  D5 daemon-coexistence)
- cheap to override per invocation (`kkernel exec --atomic --atomic-max-ops
  N`) without touching this default

Revisit once the load-harness (ADR-067 Component A) has real
per-op-count latency data under contention.

## `khive_error.rs`

### `Details::build` — bounding/truncation algorithm

See `crates/khive-types/src/khive_error.rs` — private fn `Details::build`.

Takes `ordinary` (already capped at 8 entries, with `total_ordinary` the true
pre-cap count) plus a reserved-key `collisions` count, and produces the wire
shape: at most 8 entries, with a `DETAILS_TRUNCATED_KEY` indicator entry
appended whenever anything was dropped (either overflow past 7 ordinary
pairs, or a stripped reserved-key collision). This is the shared
bounding/truncation logic used by both the public `Details::new` constructor
and the `serde::Deserialize` impl.

### `Details` deserialization — round-trip detection of self-truncated maps

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
