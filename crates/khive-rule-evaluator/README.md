# khive-rule-evaluator

Pure configurable graph validation for the rules used by `kkernel kg validate`
and `kkernel kg commit`. The crate parses TOML and evaluates immutable NDJSON
strings. It has no filesystem, database, runtime, network or ambient clock access.

```rust
use chrono::DateTime;
use khive_rule_evaluator::{
    evaluate, EvaluationContext, EvaluationMode, NdjsonState, Rules,
};

let rules = Rules::parse_toml("[naming_conventions]").unwrap();
let result = evaluate(
    &rules,
    NdjsonState {
        entities: r#"{"id":"e","kind":"concept","name":"Example"}"#,
        edges: "",
        notes: "",
    },
    EvaluationContext {
        mode: EvaluationMode::FullDataset,
        pack_edge_rules: &[],
        now: DateTime::UNIX_EPOCH,
    },
);
assert!(result[0].passed);
```

## Inputs and results

`Rules::parse_toml` rejects unknown fields and validates directional entries before
evaluation, including entries in a disabled section. `RulesError::Syntax` retains
the TOML error; `RulesError::Direction` reports an invalid relation or empty kind
list. Parsed `Rules` cannot be mutated. Individual configuration structures and
pure class functions remain available for callers that already own configuration.

`evaluate` returns ordered `RuleResult` values containing `Violation` records. It
runs generic rules first, followed by these opt-in classes:

1. `edge_endpoint_types`
2. `edge_direction_conventions`
3. `dangling_refs`
4. `naming_conventions`
5. `citation_date_lint`

Absent or disabled sections do not run. Invalid severity is an error finding.
`PartialView` skips only the built-in dangling-reference check: an invalid
`dangling_refs` severity and a generic rule named `dangling-refs` remain visible.
Malformed NDJSON lines remain skipped by this configurable pass; structural
schema, required-file, kind and duplicate checks remain the host's responsibility.

The host supplies pack endpoint rules. The base endpoint table and matcher are
shared with runtime validation through `khive-types`; no parallel ontology is
maintained here. Canonical `source`/`target`/`edge_id` fields take precedence over
legacy aliases even when the canonical value is null or has the wrong type. The
shared ID and record-prefix helpers also serve the CLI's structural checks.

## Host boundary

The CLI keeps rule-file extension checks, filesystem diagnostics, pack metadata,
output formats and fixes. It captures one in-memory view for the configurable
pass; an unreadable input is empty for this pass while structural checks retain
their existing diagnostics. Inputs are no longer reread between rule classes.
Capturing three strings does not promise an atomic snapshot of files being
changed concurrently. Commit-time configurable checks use the already projected
change-set strings; its structural checks still use host temporary files.

`Rules::needs_pack_edge_rules` requests metadata only for an enabled endpoint
section with valid severity. `Rules::needs_current_time` requests one host clock
reading only for an enabled citation section with valid severity. The CLI obtains
that instant after configuration, metadata and input capture, before evaluation;
all citation fields share it. Previously it obtained the instant when reaching
the fifth rule class. A timestamp within that elapsed interval can consequently
cross the future-date boundary. Inactive citation evaluation needs no clock
reading and can use a fixed epoch. This is explicit input capture, with unchanged
date predicates, not nanosecond wall-clock timing equivalence.

## Portability gates

The crate depends on clock-free `chrono` with `std`, plus `khive-types`, `serde`,
`serde_json` and `toml`. CI checks `wasm32-unknown-unknown` compilation and runs the
same public suite natively and under Wasmtime on `wasm32-wasip1`. Two fixed
full/partial fixtures assert literal complete JSON results and emit those bytes
for comparison. CI requires both named transcripts exactly once, compares exact
bytes, and checks that changed, missing or two empty transcripts are rejected.
Host CLI suites continue to cover filesystem, output, exit and Git behavior.
