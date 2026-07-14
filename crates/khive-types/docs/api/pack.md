# Pack Trait — Atomic Admissibility

`Pack` (`crates/khive-types/src/pack.rs`) is the trait every khive pack
(kg, gtd, memory, ...) implements to register its verbs, vocabulary, and
edge-endpoint rules with the runtime. This is the function-specific
technical reference for the ADR-099 D3 atomic-unit admissibility rules that
gate which verbs may run inside a `--atomic` op batch.

## ADR-099 D3 atomic-admissibility rejection classes

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

## `ATOMIC_MAX_OPS_DEFAULT` = 2000 — rationale

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
