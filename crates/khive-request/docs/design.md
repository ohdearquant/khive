# khive-request Design

## ADR Compliance

### ADR-016: Request DSL

This crate is the primary implementation of the request DSL.

- The `request` tool accepts a verb-dispatch DSL string and routes each parsed
  op through the loaded packs.
- Single ops, parallel batches `[...]`, sequential chains `op1 | op2($prev)`,
  and raw JSON form are all supported.
- Hard cap of 100 ops per request (`MAX_OPS`) prevents unbounded memory growth
  and keeps latency predictable.
- `ExecutionMode` (`Single`, `Parallel`, `Chain`) encodes how the dispatcher
  must execute the returned `ParsedRequest`.
- Chain mode uses `$prev` / `$prev.dotted.path` references that are substituted
  at dispatch time — the parser validates their form but not their values.

### Write-Key Conflict Detection (ADR-038)

Parallel-batch preflight check to prevent two ops in the same batch from
targeting the same record for mutation.

- Write ops checked: `update`, `delete`, `merge`, `link`.
- Keys are substrate-prefixed to avoid false positives between entity and edge
  substrates: entity writes use `entity:<uuid>`, edge writes use
  `edge-natural:<source>:<target>:<relation>`.
- `create` is excluded: its UUID is generated server-side and unknown at parse
  time; DB-level uniqueness constraints handle concurrent creates.
- Chain mode skips conflict detection: sequential ops are ordered by definition
  and the runtime resolves `$prev` references between them.
- `write_keys_for_op_pub` is exported so the MCP server can build per-op
  envelopes without invoking the full batch-level check.

## Design Decisions

### Single-pass recursive-descent parser

The parser is implemented as a single-pass recursive-descent parser. All
phases (lexer helpers, JSON-form, function-call batch, chain, `Parser` struct,
and path-resolution utilities) share deeply-coupled private functions
(`scan_value_end`, `scan_string_end`, `split_path`, `apply_path_segment`,
`find_prev_ref_pos`). These helpers are encapsulated within `parser.rs`; none
leak to the public surface.

### Module structure

| Module | Contents |
|--------|----------|
| `types.rs` | `ExecutionMode`, `ArgValue`, `ParsedOp`, `ParsedRequest`, `DslError`, `MAX_OPS` |
| `parser.rs` | Recursive-descent parser, path utilities, all private helpers |
| `conflict.rs` | Write-key conflict detection (`write_keys_for_op_pub`) |
| `lib.rs` | Thin shim — module declarations and re-exports only |

### `$prev` reference semantics

- Quoted `"$prev.id"` promotes to `ArgValue::PrevRef` identically to the
  unquoted form `$prev.id`.
- To pass the literal string `$prev.id` as a value, escape the leading `$`
  with a backslash in a quoted string: `"\\$prev.id"` deserializes to
  `\$prev.id`; the parser strips the backslash and returns a concrete
  `ArgValue::Value("$prev.id")`.
- `$prevish.id` does NOT match — the prefix boundary is `.` or `[` only.
- Malformed bracket indices in quoted refs (negative, non-numeric, unclosed)
  are treated as literal values rather than errors.

### JSON-form vs. function-call form

JSON form (`[{"tool":"...","args":{...}},...]`) always runs in `Parallel` or
`Single` mode. `$prev` substitution is not supported in JSON form — the parser
detects `$prev` strings recursively inside nested arrays and objects and
returns a typed `PrevRefInJsonForm` error.

## Consistency Notes

- `check_write_key_conflicts` is `pub(crate)` and only reachable from inline
  tests in `conflict.rs`. Integration tests in `tests/parser.rs` use the
  public `write_keys_for_op_pub` instead to test conflict-key extraction
  directly.
- The `write_keys_for_op` alias in `conflict.rs` (test-only) delegates to
  `write_keys_for_op_pub` to avoid duplicating logic.
