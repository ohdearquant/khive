# Request Parsing

`parse_request` is the transport-agnostic entry point for the request DSL. It identifies function-call, batch, chain, or JSON form, enforces resource limits and envelope rules, and returns ordered `ParsedOp` values with an explicit `ExecutionMode`.

## `parse_request`

The parser trims outer whitespace, rejects empty or oversized input, then dispatches by the first meaningful byte:

| Prefix                     | Form                                    | Mode       |
| -------------------------- | --------------------------------------- | ---------- |
| `{`                        | one JSON operation object               | `Single`   |
| `[{` (ignoring whitespace) | JSON array of operation objects         | `Parallel` |
| `[` otherwise              | function-call batch                     | `Parallel` |
| identifier                 | function call, optionally followed by ` | ` calls    |

The return value preserves input operation order. Parsing performs no tool execution and does not validate whether a tool is registered.

## Function-call operations

An operation has a bare or one-level dotted identifier and a named argument bag:

```text
verb(arg=value)
pack.verb(arg=value, other={"nested": true})
```

Identifiers follow `[A-Za-z_][A-Za-z0-9_]*`. More than one dot in a tool name is rejected. Arguments use `=` and may contain JSON scalars, arrays, objects, or chain-only `$prev` references. Duplicate argument names are errors, and the parsed bag is a `BTreeMap`, giving deterministic name order.

Pure JSON arrays/objects become one `ArgValue::Value`; a container with any `$prev` descendant retains its recursive `ArgValue::Array` or `ArgValue::Object` shape for later resolution.

## Function-call batches and chains

`[op(...), op(...)]` is parallel and rejects `$prev`. `op(...) | op(...)` is a sequential chain and may contain `$prev` anywhere in an argument value. Empty batches, mixed top-level `,` and `|` separators, trailing input, and more than `MAX_OPS` operations are errors.

Chain parsing does not resolve references; it only represents them. The dispatcher resolves each operation against the immediately preceding result and aborts after a failed step.

## JSON form

JSON form accepts one object or a non-empty array of objects. Every entry requires a string `tool`; `args` is optional and defaults to `{}`, but when present must be an object.

JSON form never represents a chain. The parser recursively rejects string values that are `$prev`, begin with `$prev.`, or begin with `$prev[`, even inside nested arrays and objects. Callers that need substitution must use function-call chain form.

Before invoking `serde_json`, a quote-aware linear scan bounds `[`/`{` nesting because the untyped `Value` deserializer exposes no depth setting. This prevents deeply nested input from reaching unbounded native recursion (CWE-674).

## Envelope-only arguments

`presentation` and `presentation_per_op` belong to the outer request envelope. If either appears inside an operation argument bag, parsing returns `ReservedEnvelopeArg` naming the field and verb.

## Errors

`parse_request` returns `DslError` for malformed syntax, resource-limit violations, illegal `$prev` placement, reserved arguments, or invalid JSON shape. See `crates/khive-request/docs/api/limits-and-errors.md` for the complete taxonomy.
