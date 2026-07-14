# PR #969 implementation notes

## Changes

- Restored the original `KhiveRuntime::merge_entity` and `merge_note` signatures as wrappers and added `merge_entity_with_reason` / `merge_note_with_reason` for the additive audit field.
- Applied the runtime secret gate to every supplied merge reason before record reads or writes.
- Preserved every supplied reason in audit payloads, including an explicit empty string; omitted reasons still omit the payload key.
- Routed the KG merge verb through the reason-aware runtime APIs and documented the full merge wire surface in `verbs()` metadata and `AGENTS.md`.
- Corrected merge audit test comments to cite accepted ADR-014.

## Regression coverage

- Entity and note secret-shaped reasons are rejected without changing either record or emitting merge events.
- Entity and note empty reasons are retained as `"reason": ""`.
- Legacy entity and note runtime calls compile and execute without a reason argument.
- Merge handler metadata exposes both substrates plus `kind`, `strategy`, `content_strategy`, `dry_run`, and `reason`.

## Verification

- Focused runtime reason/security/compatibility tests: pass.
- Pack-KG merge metadata test: pass.
- `cargo clippy -p khive-runtime -p khive-pack-kg --lib -- -D warnings`: pass.
- `cargo fmt --all -- --check`: pass.
- A broader two-package test run reached the two pre-existing `beyond_scan_cliff` pack tests, both of which exceeded 60 seconds; the run was stopped before completion to honor the task deadline.

## Domain utility

`domain_utility: { value: low, note: "The accepted curation ADR and existing runtime/pack contracts directly governed this compatibility and secret-gate fix; no external domain composition was needed." }`
