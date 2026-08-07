# khive-request

Transport-agnostic parser for khive's verb-dispatch request DSL. Parses function-call
syntax, JSON, and (in the future) LNDL / pipe / bash-style syntaxes into a single
`ParsedRequest` AST that any transport (MCP, HTTP, CLI) can execute.

## Usage

```rust
use khive_request::{parse_request, ExecutionMode};

// Single op
let req = parse_request(r#"search(kind="entity", query="LoRA")"#)?;
assert_eq!(req.mode, ExecutionMode::Single);

// Parallel batch
let req = parse_request(r#"[memory.recall(query="x"), memory.remember(content="y")]"#)?;
assert_eq!(req.mode, ExecutionMode::Parallel);

// Independent chain groups in one parallel batch
let req = parse_request(
    r#"[create(kind="concept", name="A") | get(id=$prev.id),
        create(kind="concept", name="B") | get(id=$prev.id)]"#,
)?;
assert_eq!(req.groups.len(), 2);

// Chain — `$prev` resolves against the immediately preceding op's result
let req = parse_request(r#"create(kind="concept", name="X") | link(source_id=$prev.id, target_id="...", relation="extends")"#)?;
assert_eq!(req.mode, ExecutionMode::Chain);

for op in &req.ops {
    // op.tool: String, op.args: BTreeMap<String, ArgValue>
    println!("{} -> {:?}", op.tool, op.args);
}
```

JSON form (`[{"tool": "...", "args": {...}}, ...]`) is also accepted and always parses
to `ExecutionMode::Parallel` — it has no chain syntax, so a literal `$prev` in JSON
form is a parse error (`DslError::PrevRefInJsonForm`).

## Result types

- `ParsedRequest { ops: Vec<ParsedOp>, mode: ExecutionMode, groups: Vec<ExecutionGroup> }`
- `ParsedOp { tool: String, args: BTreeMap<String, ArgValue> }`
- `ArgValue` — `Value(serde_json::Value)` for concrete JSON, `PrevRef { path }` for a
  `$prev`/`$prev.field.path` back-reference, or `Array`/`Object` containers that hold
  further `ArgValue`s (a pure-JSON array or object with no embedded `$prev` folds
  straight to `Value`). `ArgValue::resolve_prev` / `resolve_all` resolve references
  against a preceding op's result.
- `DslError` — a typed enum (`TooManyOps`, `UnexpectedChar`, `InvalidIdentifier`,
  `PrevRefOutsideChain`, `ReservedEnvelopeArg`, …), surfaced as `invalid_params`
  at the MCP boundary. `WriteKeyConflict` remains available to the test-only
  batch scanner; production conflict handling uses per-op execution envelopes.

## Semantics

- **`MAX_OPS`** (100) caps operations per request; exceeding it is `DslError::TooManyOps`.
- **`$prev` is chain-only.** A `$prev` reference outside `|` chain mode, or anywhere in
  JSON form, is rejected at parse time rather than deferred to a runtime lookup miss.
- **Parallel batches may contain chain groups.** Commas delimit independently
  scheduled groups; `|` joins sequential operations within one group. Failure
  aborts only that group's tail, and the 100-op cap counts flattened operations.
- **Write-key conflict detection** (`write_keys_for_op_pub`) is a preflight check over a
  parallel batch: two ops that target the same UUID via `update`/`delete` (`id`),
  `merge` (`into_id`/`from_id`), or `link` (`source_id`/`target_id`) receive
  operation-local conflict errors while non-conflicting operations still dispatch.
  In bracketed chain groups, conflicts are checked across groups: preceding
  non-conflicting members execute, the actual conflicting member fails, and only
  that group's tail aborts.
- **`RESERVED_ENVELOPE_ARGS`** (`presentation`, `presentation_per_op`) are
  envelope-level fields; passing them inside a verb's own argument list is rejected
  (`DslError::ReservedEnvelopeArg`).
- Mixing `,` and `|` without batch brackets is rejected (`DslError::MixedSeparators`), as is
  a dotted verb name with more than one level, e.g. `a.b.c` (`UnsupportedVerbNesting`) —
  only single-level `pack.verb` names are supported.

## Where this sits

`khive-request` depends only on `serde`/`serde_json` and has no dependency on
`khive-runtime` or any pack — it is pure syntax, consumed only at the transport
dispatch boundary:

```text
types -> score -> storage -> db -> query -> runtime -> pack-* -> mcp
                                                                   ^
                                                          khive-request parses
                                                          the `request` tool's
                                                          input string here
```

`khive-mcp`'s `request` tool calls `parse_request`, then routes each `ParsedOp`
through the runtime's `VerbRegistry::dispatch`. The DSL shape and grammar are
specified in
[ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md).

## License

Apache-2.0.
