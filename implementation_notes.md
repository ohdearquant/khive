# Implementation notes

- `crates/khive-pack-kg/src/handlers/search.rs` now records the ordered served UUIDs as both `candidates` and `selected` in every `SearchExecuted` payload.
- `crates/khive-db/src/stores/event.rs` now decodes `SearchExecuted` separately from recall, validates `result_kind`, and projects entity searches as entity referents and note searches as note referents.
- KG integration coverage now verifies payload completeness, Candidate/Selected roles, referent kinds, positions, and `observed`/`selected` event-filter queryability for both entity and note searches.
- Storage coverage rejects unknown search result kinds atomically; existing synthetic search events now declare their result substrate explicitly.

## Verification

- `cargo fmt --manifest-path crates/Cargo.toml --all -- --check`
- `cargo test --manifest-path crates/Cargo.toml -p khive-pack-kg --test integration search_entity_emits_exactly_one_search_executed_event -- --exact`
- `cargo test --manifest-path crates/Cargo.toml -p khive-pack-kg --test integration search_note_emits_exactly_one_search_executed_event_with_note_result_kind -- --exact`
- `cargo test --manifest-path crates/Cargo.toml -p khive-db search_executed_rejects_unknown_result_kind`
- `cargo test --manifest-path crates/Cargo.toml -p khive-db stores::event::tests` (32 passed)
- `cargo test --manifest-path crates/Cargo.toml -p khive-runtime synthetic_edge_observed_as_selected_returns_memory_note -- --exact`
- `cargo test --manifest-path crates/Cargo.toml -p khive-pack-brain event_counts_tests` (15 passed)

## Domain utility

`high` — ADR-041 directly specifies the Candidate/Selected role mapping, ordered positions, and typed referent contract implemented by this fix.
