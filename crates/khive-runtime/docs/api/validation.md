# KG Validation Pipeline

`validation.rs` defines the pack-contributed KG validation pipeline: how a pack declares a
`ValidationRule`, how severity levels map to `kkernel kg validate` exit behavior, and where rule
configuration is read from at runtime.

## ADR Links

- [ADR-034](../../../docs/adr/ADR-034-kg-validation-pipelines.md) — KG validation pipelines specification

## Rule Configuration File

Rules are configured at `.khive/kg/rules.toml` (TOML format, per ADR-034).
The `ValidationContext::config` map is populated from that file at runtime.

Note: source comments in `validation.rs` that reference `rules.yaml` are a documentation
error; the canonical format is `.toml` per ADR-034.

## Rule Declaration

Pack authors declare an array of `ValidationRule` in their `PackRuntime::validation_rules()`
method. Rule IDs must follow `<pack>/<rule-id>` namespace convention. Built-in rules
use no pack prefix (e.g. `"min-edge-density"`).

## Rule Shape

```rust
pub struct ValidationRule {
    pub id: RuleId,          // "<pack>/<rule-id>"
    pub severity: Severity,  // Info | Warning | Error
    pub description: &'static str,
    pub check: RuleFn,       // fn(&ValidationContext) -> Vec<Violation>
    pub fix: Option<FixFn>,  // None = unfixable
}
```

## Severity Levels

- `Error` — `kkernel kg validate` exits with code 1.
- `Warning` — reported; no exit-code effect unless `--strict`.
- `Info` — informational; no exit-code effect.

Severity can be overridden per rule in `.khive/kg/rules.toml`.

## GraphPatch

`GraphPatch` is a deferred stub (ADR-034 §auto-fix write path is deferred). The auto-fix
write path is not yet implemented; `fix: Some(...)` is reserved for future use.

## Invariants

- Rule IDs must be unique across all loaded packs.
- Pack-contributed rules must carry the pack namespace prefix.
- `CorpusCheck` receives a `GraphSnapshot`; it must not reach through to the storage layer.

## Failure Modes

| Condition           | Behaviour                                                 |
| ------------------- | --------------------------------------------------------- |
| Duplicate rule ID   | Not enforced at boot in v0.2; first registered wins       |
| Fix function panics | Propagates as runtime panic; fix functions must not panic |
| Config key unknown  | Ignored; rules use their default severity                 |
