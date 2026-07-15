# Snapshot validation

Technical reference for how `khive-brain-core` validates a persisted `BrainState` /
`EntityPosteriors` snapshot on load, and the exact corruption shapes each guard rejects
rather than silently repairs.

## `validate_brain_state_snapshot_with_capacity` — `entity_posterior_order` version gating rationale

The function rejects rather than silently repairs two shapes of corruption
that `#[serde(default)]` would otherwise let through unnoticed:

- **Version 0 with a non-empty order.** Version 0 is the legacy,
  pre-ordering compatibility case and is only ever valid with an EMPTY
  order — restore falls back to the deterministic ascending-`Uuid` order in
  that case (see `EntityPosteriors::from_snapshot` in `posterior.rs`). A
  version-0 snapshot with a non-empty order is not legacy compatibility
  data; full-coverage order is only ever written under version 1, so a
  non-empty order on version 0 is necessarily partial. Because
  `entity_posteriors_version` silently resolves to `0` on an omitted JSON
  field, this case must be caught explicitly rather than passed through to
  restore, which would otherwise silently normalize e.g. `[A]` order over
  `{A, B}` posteriors to `[A, B]` — masking data loss as a successful
  restore.
- **Version ≥ 1 with partial coverage.** Combined with the duplicate/unknown-id
  checks (which already guarantee every order id is a distinct, valid map
  key), requiring the order length to equal the `entity_posteriors` length
  proves the id sets are identical — no `entity_posteriors` key is missing
  from the order. A current-format snapshot with partial order is corruption,
  not a compatibility case, and is rejected here rather than silently
  normalized at restore.

This is the capacity-aware counterpart to `validate_brain_state_snapshot`,
used at the persistence load boundary, so a crafted or legacy oversized
snapshot is rejected outright rather than silently truncated.
