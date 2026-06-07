# khive-pack-template Design

Reference scaffold for new khive packs.
Reference implementation: `crates/khive-pack-kg/`.

No macros, no DSLs. Plain Rust — rust-analyzer, debugger, and LLMs all work directly without expansion.

## How to create a new pack

1. Copy this crate directory to `crates/khive-pack-<name>/`.
2. Rename the crate in `Cargo.toml` (name, description).
3. Set `PACK_NAME` to your pack's canonical name (e.g. `"exp"`).
4. Update `NOTE_KINDS` / `ENTITY_KINDS` in `vocab.rs`.
5. Add your verbs to `TEMPLATE_HANDLERS` in `pack.rs`; fill in `handlers.rs`.
   - All non-kg verbs must use `<pack>.<verb>` naming (e.g. `"exp.run"`).
6. Add the crate to the workspace `Cargo.toml`.
7. Force-link in `khive-mcp/src/pack.rs` and `kkernel/src/lib.rs`.
8. Add the crate dep to `khive-mcp/Cargo.toml` and `kkernel/Cargo.toml`.

## Verb naming

| Pack | Verb format | Example |
|------|------------|---------|
| `kg` | bare | `create`, `link` |
| all others | `<pack>.<verb>` | `template.my_verb`, `gtd.assign` |

Verb names are agent-facing strings. They must be unique across all loaded packs. The pack
prefix guarantees no collisions between packs that might independently name a verb `"run"` or
`"list"`. The `kg` pack is exempt because it owns the root entity/note primitives that all
packs depend on.

## Module layout

```
src/
  lib.rs       — TemplatePack struct, Pack trait impl, module declarations (thin shim)
  pack.rs      — TEMPLATE_HANDLERS table, PackFactory, inventory::submit!, PackRuntime impl
  handlers.rs  — One async fn per verb
  vocab.rs     — NOTE_KINDS and ENTITY_KINDS constants
tests/
  integration.rs — Smoke tests: valid input, invalid input, unknown verb
docs/
  design.md    — This file
```

## ADR Compliance

### ADR-023: Pack verb surface, visibility, and composition

- All non-kg pack verbs must be prefixed with the pack name: `<pack>.<verb>`.
- The `kg` pack uses bare verb names (`create`, `link`, `search`, etc.) by convention.
- Each verb in `TEMPLATE_HANDLERS` carries a `Visibility` field:
  - `Visibility::Verb` — exposed on the MCP `request` tool (agent-facing).
  - `Visibility::Subhandler` — internal / CLI-only; not on the MCP wire.
- Pack dependencies are declared in `Pack::REQUIRES`; the runtime validates they are loaded.

### ADR-027: Dynamic pack loading via inventory self-registration

- Each pack crate must call `inventory::submit! { khive_runtime::PackRegistration(&Factory) }`
  exactly once. This causes the linker to include the pack factory in the binary's startup inventory.
- The factory's `name()` must match `Pack::NAME` and `PackFactory::name()`.
- The `requires()` list in the factory must match `Pack::REQUIRES`.

## Consistency Notes

- No discrepancies found between this scaffold and the ADR specifications above.
- `TEMPLATE_HANDLERS` is a static in `pack.rs` (not `lib.rs`) to keep `lib.rs` under 50 lines.
  The `Pack::HANDLERS` const in `lib.rs` re-exports it via `&TEMPLATE_HANDLERS`.
