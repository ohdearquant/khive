# Request and the Verb-Dispatch DSL

khive exposes one MCP tool: `request`. Its `ops` argument holds one or more
verb calls. The tool parses those calls, routes each verb to the loaded pack,
and returns an outcome for every operation.

One tool keeps the MCP surface stable while packs can add or remove verbs. A
client only needs to call `request`; it selects the operation in `ops`. Use the
[API reference](api-reference.md) for the current verb catalog and each verb's
parameters. This page covers how to compose calls.

## Choose an input form

The function-call form is compact when writing a call directly:

```text
request(ops="search(kind=\"entity\", query=\"LoRA\")")
```

The JSON form represents the same operation when a client is already building
structured JSON:

```text
request(ops="{\"tool\":\"search\",\"args\":{\"kind\":\"entity\",\"query\":\"LoRA\"}}")
```

Both forms produce the same operation for non-chain work. JSON may be a single
object as above or an array of objects. Function-call form is generally easier
to read and write by hand; JSON avoids manual string construction for deeply
nested argument values.

### Quote values as JSON

Argument values use JSON literals even in the function-call form. Strings are
always double-quoted:

```text
get(id="abc")
```

Not:

```text
get(id=abc)
```

Numbers, booleans, `null`, arrays, and objects use their normal JSON syntax.
Argument names remain unquoted identifiers.

## Run independent operations in parallel

Wrap independent function-call operations in `[...]`, separated by commas:

```text
request(ops="[stats(), memory.recall(query=\"attention cache behavior\", limit=3)]")
```

The equivalent JSON batch is:

```text
request(ops="[{\"tool\":\"stats\",\"args\":{}},{\"tool\":\"memory.recall\",\"args\":{\"query\":\"attention cache behavior\",\"limit\":3}}]")
```

A batch accepts at most 100 operations. Its operations run concurrently, with
no ordering guarantee; results are returned in input order. Use it for work
that has no dependency between operations. JSON form is always single or
parallel, so it is also limited to independent operations.

## Pass a result to the next operation

Use `|` when a later operation needs an earlier result. A chain runs in order,
and `$prev` selects data from the operation immediately before it:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\") | link(source_id=$prev.id, target_id=\"<document-id>\", relation=\"introduced_by\")")
```

`$prev` refers only to the immediately preceding result. For example, in
`a() | b() | c(id=$prev.id)`, the reference in `c` reads `b`'s result, not
`a`'s. If a later operation needs a non-adjacent result, split the work into
separate `request` calls or make that result the immediate predecessor.

`$prev` is available only in function-call chains. JSON form and parallel
batches reject it. A failed chain operation prevents subsequent operations
from running; completed operations are not rolled back.

## Read the result envelope

`request` returns a `results` array and an aggregate `summary`. Each completed
operation has one of these shapes:

```json
{ "ok": true, "tool": "search", "result": { "...": "..." } }
```

```json
{ "ok": false, "tool": "get", "error": "not found: ..." }
```

For example, a parallel batch can return:

```json
{
  "results": [
    { "ok": true, "tool": "stats", "result": { "...": "..." } },
    { "ok": false, "tool": "memory.recall", "error": "..." }
  ],
  "summary": { "total": 2, "succeeded": 1, "failed": 1, "aborted": 0 }
}
```

A failure in a parallel batch does not stop its siblings. In a chain, entries
after the failure are returned as `{ "ok": false, "tool": "...", "aborted": true }`;
the summary records their count in `aborted`.

## Distinguish syntax errors from operation errors

An invalid DSL string never reaches a verb handler. Lexing and parsing failures
such as unterminated strings, malformed JSON, too many operations, or invalid
use of `$prev` are reported by MCP as an `invalid_params` RPC error. Correct
the `ops` string and submit the request again.

Once the DSL parses, validation or execution failures from an individual verb
are returned in that operation's `{ "ok": false, "error": ... }` entry. This
is why a parallel batch can partly succeed: each valid, non-conflicting
operation has its own outcome.

## Gotchas

- **Bareword values are not strings.** Write `query="LoRA"`, not `query=LoRA`.
- **Do not mix top-level separators.** Commas inside `[...]` mean parallel
  work; `|` means a sequential chain. A request that mixes them at the top
  level is invalid. Nested JSON arrays and objects may still contain commas.
- **A request boundary has a cost.** Each `request` call adds a transport and
  dispatch round trip. The server keeps long-lived state warm between calls,
  but `$prev` exists only inside one chain and is not a cross-call cache. Batch
  independent work and chain direct dependencies when that reduces calls.
- **Choose the form by dependency.** A JSON array is not a sequence. Use a
  function-call chain when one operation needs another operation's output.

For verb names, required arguments, and return details, see the
[API reference](api-reference.md).
