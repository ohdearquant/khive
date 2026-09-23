# Request Parsing and Execution Model

## ADR Compliance

### ADR-016: Request DSL

This crate is the primary implementation of the request DSL.

- The `request` tool accepts a verb-dispatch DSL string and routes each parsed
  op through the loaded packs.
- Single ops, parallel batches `[...]`, sequential chains `op1 | op2($prev)`,
  bracketed batches of chains `[op1 | op2, op3]` (ADR-016 Amendment 2), and raw
  JSON form are all supported.
- Hard cap of 100 ops per request (`MAX_OPS`) prevents unbounded memory growth
  and keeps latency predictable, counted across every op in a request
  regardless of how many units a bracketed batch partitions them into.
- `ExecutionMode` (`Single`, `Parallel`, `Chain`) encodes how the dispatcher
  must execute the returned `ParsedRequest`. `ParsedRequest.ranges` carries the
  finer-grained partition into units: length-one ranges for an ordinary flat
  batch, longer ranges where a unit is itself a chain.
- Chain mode uses `$prev` / `$prev.dotted.path` references that are substituted
  at dispatch time; the parser validates their form but not their values. A
  unit inside a bracketed batch resolves `$prev` the same way, scoped to that
  unit's own ops only.

### Write-Key Conflict Detection (ADR-038)

Parallel-batch preflight check to prevent two ops in the same batch from
targeting the same record for mutation.

Keys are substrate-prefixed so entity writes and incident edge writes do not
false-conflict. The parser exposes per-operation extraction for the MCP
dispatcher's envelope policy while keeping storage and registry concerns out of
this crate. Exact key construction is documented in
[`docs/api/write-conflicts.md`](api/write-conflicts.md).

## Design Decisions

### Single-pass recursive-descent parser

The parser is implemented as a single-pass recursive-descent parser. All
phases (lexer helpers, JSON-form, function-call batch, chain, `Parser` struct,
and path-resolution utilities) share deeply-coupled private functions
(`scan_value_end`, `scan_string_end`, `split_path`, `apply_path_segment`,
`find_prev_ref_pos`). These helpers are encapsulated within the `parser/`
module; none leak to the public surface.

### Module structure

| Module                             | Contents                                                   |
| ---------------------------------- | ---------------------------------------------------------- |
| `types.rs`                         | Execution modes, parsed values/ops, limits, and `DslError` |
| `parser/dispatch.rs`               | Top-level form detection and request assembly              |
| `parser/parser_impl.rs`            | Function-call recursive descent                            |
| `parser/path.rs`, `parser/scan.rs` | `$prev` traversal and bounded scanners                     |
| `conflict.rs`                      | ADR-038 write-key extraction                               |
| `atomic.rs`                        | ADR-099 atomic-admissibility preflight                     |
| `lib.rs`                           | Thin shim — module declarations and re-exports only        |

### `$prev` references

References are represented rather than resolved during parsing, which keeps the
parser independent of result envelopes and execution. Quoted promotion,
escaping, nested container resolution, and missing-path behavior are documented
in [`docs/api/previous-result.md`](api/previous-result.md).

### Bounded parsing

Input size and container depth are independent limits: a small payload can
still be dangerously deep. Function form tracks depth during construction;
JSON form uses a quote-aware pre-pass before `serde_json` recursion. The limits
and rationale are documented in
[`docs/api/limits-and-errors.md`](api/limits-and-errors.md).

## API references

- [`docs/api/request-dsl.md`](api/request-dsl.md) — accepted wire forms
- [`docs/api/parsing.md`](api/parsing.md) — `parse_request` routing and syntax
- [`docs/api/previous-result.md`](api/previous-result.md) — `$prev` paths and resolution
- [`docs/api/write-conflicts.md`](api/write-conflicts.md) — ADR-038 key extraction
- [`docs/api/atomic-admissibility.md`](api/atomic-admissibility.md) — ADR-099 preflight
- [`docs/api/limits-and-errors.md`](api/limits-and-errors.md) — resource bounds and `DslError`
