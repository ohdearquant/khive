# Request DSL Protocol

The request DSL is the transport-neutral operation envelope defined by ADR-016. This page is the concise syntax reference; parser behavior, limits, and `$prev` resolution are expanded in the neighboring API topics.

## Forms

### Function-call form (single op)

```
verb(arg=value, arg=value)
```

### Function-call batch (parallel)

```
[verb1(arg=value), verb2(arg=value), ...]
```

Ops run concurrently. Results are collected in input order.
Maximum 100 ops per request (`MAX_OPS`).

### Chain form (sequential)

```
verb1(arg=value) | verb2(id=$prev.field)
```

Ops run sequentially. Each op may reference the prior op's result via `$prev`.
Aborts on the first failure.

### JSON form (parallel)

```json
[{"tool": "verb", "args": {"key": "value"}}, ...]
```

or a single object (treated as `Single` mode):

```json
{ "tool": "verb", "args": {} }
```

JSON form always runs in `Parallel` (or `Single`) mode. `$prev` references are
not supported in JSON form — use the function-call chain form instead.

## `$prev` path semantics (chain mode only)

`$prev` refers to the full result of the preceding op. A dot-separated path
selects a nested field:

| DSL                   | Meaning                             |
| --------------------- | ----------------------------------- |
| `$prev`               | whole prior result                  |
| `$prev.id`            | field `id` in the prior result      |
| `$prev.result.id`     | nested field                        |
| `$prev[0].id`         | array index then field              |
| `$prev.items[1].name` | field, then array index, then field |

`$prev` references may appear inside array and object literals:

```
create(...) | assign(depends_on=[$prev.id, "other-uuid"])
```

### Escape rule

To pass the **literal** string `$prev.id` as a value (not a reference), escape
the leading `$` with a backslash inside a quoted string:

```
create(...) | update(id="\\$prev.id")
```

The DSL source `"\\$prev.id"` deserializes to `\$prev.id`; the parser strips
the leading `\` and stores `$prev.id` as a concrete string value.

## Write-key conflict detection (ADR-038 preflight)

Execution layers can derive per-operation write keys and reject parallel ops
that target the same stored record.

Write ops and their conflict keys:

| Verb     | Conflict key                                      |
| -------- | ------------------------------------------------- |
| `update` | `entity:<id>`                                     |
| `delete` | `entity:<id>`                                     |
| `merge`  | `entity:<into_id>`, `entity:<from_id>`            |
| `link`   | `edge-natural:<source_id>:<target_id>:<relation>` |

`link` writes an **edge** record, not the entity. An `update(id="X")` and a
`link(source_id="X", ...)` in the same batch do **not** conflict — they target
different substrates.

Chain mode skips write-key preflight because sequential ordering is explicit.
See [`write-conflicts.md`](write-conflicts.md) for bulk links, symmetric edges,
and the production integration boundary.

## Error types (`DslError`)

| Variant                  | When                                       |
| ------------------------ | ------------------------------------------ |
| `Empty`                  | Input is blank                             |
| `TooManyOps`             | Batch exceeds `MAX_OPS` (100)              |
| `UnexpectedChar`         | Parser saw an unexpected character         |
| `UnexpectedEof`          | Input ended too early                      |
| `InvalidIdentifier`      | Verb or arg name starts with a digit       |
| `DuplicateArg`           | Same arg name appears twice in one op      |
| `InvalidValue`           | Literal value could not be parsed          |
| `InvalidJson`            | JSON-form input is malformed               |
| `UnclosedString`         | String literal is not terminated           |
| `UnclosedBracket`        | `[`, `{`, or `(` has no matching close     |
| `PrevRefOutsideChain`    | `$prev` in a non-chain context             |
| `PrevRefInJsonForm`      | `$prev` string found in JSON-form input    |
| `MixedSeparators`        | `,` and `\|` mixed at the top level        |
| `EmptyBatch`             | `[]` with no ops                           |
| `UnsupportedVerbNesting` | More than one dot in a verb name (`a.b.c`) |
| `WriteKeyConflict`       | Two parallel ops target the same record    |

The full taxonomy, including input-size, nesting, and reserved-envelope errors,
is in [`limits-and-errors.md`](limits-and-errors.md).

## Testing

Public behavior is covered in `tests/parser.rs`; private parser phases and the
test-only batch conflict helper are covered beside their implementations.
