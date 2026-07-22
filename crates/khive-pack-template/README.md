# khive-pack-template

Reference template for authoring a new khive pack (ADR-023 §8). This crate is
not consumed as a dependency — it is a scaffold you copy.

## Usage

```bash
cp -r crates/khive-pack-template crates/khive-pack-<name>
```

Then, per `docs/design.md` in this crate:

1. Rename the crate in `Cargo.toml` (`name`, `description`).
2. Set `PACK_NAME` in `lib.rs` to your pack's canonical name (e.g. `"exp"`).
3. Update `NOTE_KINDS` / `ENTITY_KINDS` in `vocab.rs`.
4. Add verbs to the `HandlerDef` table in `pack.rs`; implement them in
   `handlers.rs`.
5. Add the crate to the workspace `Cargo.toml`.
6. Force-link it in `khive-mcp/src/pack.rs` and `kkernel/src/lib.rs` (a
   `pub use` referencing any public type — this is what makes the linker
   include the pack's `inventory::submit!` factory in the final binary).
7. Add the crate dependency to `khive-mcp/Cargo.toml` and `kkernel/Cargo.toml`.

## What the scaffold demonstrates

`TemplatePack` implements `khive_types::Pack` (`NAME = "template"`, one note kind
`"template_note"`, `REQUIRES = ["kg"]`) and `khive_runtime::pack::PackRuntime`,
and registers itself with `inventory::submit! { khive_runtime::PackRegistration(&TemplatePackFactory) }`
— the same self-registration mechanism every khive pack uses (ADR-027). Its one
handler, `template.my_verb`, shows the required non-`kg`-pack verb naming
(`<pack>.<verb>`) and basic parameter validation:

```rust
// handlers::handle_my_verb — rejects a missing/empty "name" field
pub(crate) async fn handle_my_verb(
    _runtime: &khive_runtime::KhiveRuntime,
    _token: &khive_runtime::NamespaceToken,
    params: serde_json::Value,
) -> Result<serde_json::Value, khive_runtime::RuntimeError> {
    // { "name": "<string>" } -> { "ok": true, "name": "<string>" }
}
```

Dispatched through the MCP `request` DSL once loaded:

```text
request(ops="template.my_verb(name=\"example\")")
```

`tests/integration.rs` covers the pattern every new pack's tests should follow:
valid input, invalid input, and dispatch of an unknown verb.

## Where this sits

`khive-pack-template` depends on `khive-types` (`Pack`, `HandlerDef`,
`Visibility`) and `khive-runtime` (`PackRuntime`, `PackFactory`,
`KhiveRuntime`), `REQUIRES` [`khive-pack-kg`](https://crates.io/crates/khive-pack-kg)
like every other pack, and is never force-linked into a shipping binary — it
exists purely as the copy-me reference for
ADR-023 (Pack Verb Surface, Visibility, and Composition)
§8 and
ADR-027 (Dynamic Pack Loading via Self-Registration).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
