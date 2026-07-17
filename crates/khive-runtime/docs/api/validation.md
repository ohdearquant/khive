# KG Validation Pipeline

`validation.rs` defines the pack-contributed KG validation API: the `ValidationRule` /
`ValidationContext` / `Violation` types a pack declares through `PackRuntime::validation_rules()`.
Two distinct validation surfaces exist under ADR-034, in different states of wiring — this
document keeps them separate, because only one of them runs today.

## ADR Links

- [ADR-034](../../../../docs/adr/ADR-034-kg-validation-pipelines.md) — KG validation pipelines specification

## 1. Shipped: the data-driven TOML RulePass

What `kkernel kg validate` actually executes today is the ADR-020 built-in structural checks
plus an optional TOML RulePass: when `.khive/kg/rules.toml` exists and `--no-rules` is not set,
each `[[rules]]` entry (`kind = "entity"` or `"edge"`, optional `condition`, optional
`require_field`, `message` with `{id}` substitution) runs after the structural pass. The TOML
file also accepts five opt-in named rule-class tables, evaluated after the generic `[[rules]]`
entries: `edge_endpoint_types` (endpoint contract against the live pack/base allowlist),
`edge_direction_conventions`, `dangling_refs`, `naming_conventions`, and `citation_date_lint`
(full syntax: `RulesFile` in `crates/kkernel/src/kg/validate.rs` and the kg-rules guide,
`crates/kkernel/docs/kg-rules.md`). Severity for these data-driven rules is configured per
rule in `rules.toml`:

- `Error` — `kkernel kg validate` exits with code 1.
- `Warning` — reported; no exit-code effect unless `--strict`.
- `Info` — informational; no exit-code effect.

This TOML pass does not go through the Rust `ValidationRule` API below — it is a separate,
data-driven runner.

## 2. Declared but deferred: the Rust pack-validator API

The types in `validation.rs` are shipped API surface, but **the CLI runner does not call them**
(ADR-034 "What changes and what does not"): `kkernel kg validate` never invokes
`PackRuntime::validation_rules()` against live corpus data, `VerbRegistry::all_validation_rules`
currently has no non-test caller, no runtime-populated `ValidationContext` is constructed, and a
declared rule cannot yet affect validation output, CLI exit status, or auto-fix behavior. Wiring
this API into the CLI runner is explicitly deferred by ADR-034.

### Rule Declaration

Pack authors declare an array of `ValidationRule` in their `PackRuntime::validation_rules()`
method. Rule IDs must follow the `<pack>/<rule-id>` namespace convention. Built-in rules
use no pack prefix (e.g. `"min-edge-density"`).

### Rule Shape

```rust
pub struct ValidationRule {
    pub id: RuleId,          // "<pack>/<rule-id>"
    pub severity: Severity,  // Info | Warning | Error
    pub description: &'static str,
    pub check: RuleFn,       // fn(&ValidationContext) -> Vec<Violation>
    pub fix: Option<FixFn>,  // None = unfixable
}
```

The `Severity` values carry the same intended exit-code semantics as §1, but for
pack-declared rules those semantics are design intent, not current behavior — nothing
executes these rules yet.

### GraphPatch

`GraphPatch` is a deferred stub (ADR-034 §auto-fix write path is deferred). The auto-fix
write path is not implemented; `fix: Some(...)` is reserved for future use.

### Invariants (design contract for the deferred runner)

- Rule IDs must be unique across all loaded packs.
- Pack-contributed rules must carry the pack namespace prefix.
- `CorpusCheck` receives a `GraphSnapshot`; it must not reach through to the storage layer.

### Failure Modes (apply once the runner is wired)

| Condition           | Behaviour                                                 |
| ------------------- | --------------------------------------------------------- |
| Duplicate rule ID   | Not enforced at boot in v0.2; first registered wins       |
| Fix function panics | Propagates as runtime panic; fix functions must not panic |
| Config key unknown  | Ignored; rules use their default severity                 |
