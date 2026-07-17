# khive-brain-core — design rationale

Narrative background for non-obvious design choices in this crate — the "why" a caller
does not need in order to use the public contract correctly. Public item contracts stay
complete and self-sufficient in the `.rs` source; this file holds the extended rationale.
See [snapshot validation](api/snapshot-validation.md) for the technical reference of the
related validation contract.

## `sort_fallback_candidates` — why it's a standalone helper

Extracted into a standalone helper (rather than inlined into
`BrainState::resolve_with_match`) so it can be unit-tested with intentionally
unsorted input, providing fail-before/pass-after coverage that does not rely
on `HashMap` randomization to expose non-determinism.
