# Pack scaffold API

The template crate demonstrates the minimum `Pack`, `PackFactory`, inventory registration,
`PackRuntime`, vocabulary, and handler wiring required by a non-KG khive pack.

## `TemplatePack`

`TemplatePack::new(runtime)` stores the runtime handle used by handlers. `Pack::NAME` is the
canonical `template` prefix, note/entity kinds come from `vocab.rs`, `HANDLERS` references the
static table in `pack.rs`, and `REQUIRES` declares `kg`.

The factory name, `Pack::NAME`, and verb prefix must agree. `inventory::submit!` contributes one
factory registration at link time; the runtime later constructs the pack and reads the same
dependency list.

## Handler table and visibility

Each `HandlerDef` declares name, description, parameter schema, async function, and visibility.
`Visibility::Verb` exposes the entry through the MCP `request` surface;
`Visibility::Subhandler` is internal or CLI-only. Non-KG names use `<pack>.<verb>` to avoid
cross-pack collisions.

## Dispatch

`PackRuntime::dispatch` matches the names declared in `TEMPLATE_HANDLERS` and delegates to the
handler. An unknown name returns `RuntimeError::InvalidInput`; adding a table entry without a match
arm is therefore detectable rather than silently ignored.

## `template.my_verb`

The example handler accepts an object with a non-empty string `name` and returns
`{"ok": true, "name": name}`. Missing, non-string, or empty input returns
`RuntimeError::InvalidInput`. It demonstrates validation only and performs no storage mutation.

## Vocabulary constants

`NOTE_KINDS` contains the example `template_note`; `ENTITY_KINDS` is empty. A real pack replaces
both lists with non-overlapping governed values before registration.
