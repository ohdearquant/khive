# khive-pack-template: Design and Scaffold Guide

Reference scaffold for new khive packs (ADR-023 §8).
Reference implementation: `crates/khive-pack-kg/`.

No macros, no DSLs. Plain Rust — rust-analyzer, debugger, and LLMs all work directly without expansion.

## How to create a new pack

1. Copy this crate directory to `crates/khive-pack-<name>/`.
2. Rename the crate in `Cargo.toml` (name, description).
3. Set `PACK_NAME` to your pack's canonical name (e.g. `"exp"`).
4. Update `NOTE_KINDS` / `ENTITY_KINDS` in `vocab.rs`.
5. Add your verbs to `HANDLERS` in `lib.rs`; fill in `handlers.rs`.
   - ADR-023: all non-kg verbs must use `<pack>.<verb>` naming (e.g. `"exp.run"`).
6. Add the crate to the workspace `Cargo.toml`.
7. Force-link in `khive-mcp/src/pack.rs` and `kkernel/src/lib.rs`.
8. Add the crate dep to `khive-mcp/Cargo.toml` and `kkernel/Cargo.toml`.

## Verb naming (ADR-023)

| Pack | Verb format | Example |
|------|------------|---------|
| `kg` | bare | `create`, `link` |
| all others | `<pack>.<verb>` | `template.my_verb`, `gtd.assign` |

## Module layout

```
src/
  lib.rs       — Pack impl, HandlerDef table, PackRuntime dispatch
  handlers.rs  — One async fn per verb
  vocab.rs     — NOTE_KINDS and ENTITY_KINDS constants
tests/
  integration.rs — Smoke tests: valid input, invalid input, unknown verb
docs/
  design.md    — This file
```
