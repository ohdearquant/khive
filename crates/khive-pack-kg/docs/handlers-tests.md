# handlers/tests.rs — internal rationale

Source: `crates/khive-pack-kg/src/handlers/tests.rs` (`#[cfg(test)]`, never rendered on
docs.rs). This guide holds rationale trimmed from oversized inline `//` comments.

## Issue #543: hint/validator divergence sweep

Hints must equal the validator's own acceptance set, not a separate hand-authored table
(the person→project gap, issue #60, was exactly this divergence class).

`valid_relations_hint_matches_real_validator_acceptance_across_all_entity_kind_pairs` is a
generative cross-check: for EVERY (source_kind, target_kind) entity pair in the 9x9 closed
entity-kind taxonomy, the hint set returned by `valid_relations_for_entity_pair` must equal
the set of relations the REAL production validator (`KhiveRuntime::link` ->
`validate_edge_relation_endpoints`) actually accepts for that pair. This calls production
code on both sides — it never re-implements the rule check inline.

Issue #621 flagged that a five-pair spot check missed an `EntityOfType`-scoped divergence
(see `valid_relations_hint_covers_formal_pack_entity_of_type_rules`); this test sweeps the
full closed kind space so a future divergence at any pair fails loudly rather than only at
hand-picked pairs.

`annotates` requires a note source (never an entity), so it can never be accepted for an
entity→entity pair regardless of kind and is skipped (matches the hint function's own
scope, which never emits "annotates"). `supersedes`/`supports`/`refutes` DO validate
entity→entity pairs against the same base allowlist the hint function reads, so they stay
in scope.
