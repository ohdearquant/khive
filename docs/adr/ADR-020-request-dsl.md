# ADR-020: Request DSL — Batch Operations via Function-Call Syntax

**Status**: accepted (v0.2 — supersedes the verb-flat MCP surface from ADR-023)\
**Date**: 2026-05-15 (planned); 2026-05-18 (accepted)\
**Authors**: Ocean, lambda:khive

## Context

Agents frequently need to perform multiple KG operations in one logical step — create several
entities at once, link several edges, run a few queries together. Two design directions present
themselves:

1. **Add `_batch` variants** of every operation (`entity_batch_create`, `edge_batch_create`, etc.).
   Each new tool requires its own param struct, its own validation, its own error reporting. The
   surface grows by N for every N ops we want batchable.
2. **One generic `request` tool** that accepts a batch of any existing operations. The surface grows
   by 1, total. Agents compose.

Option 2 wins on three dimensions: surface size, expressiveness, and consistency. It's also closer
to how agents think — "do these things together" — rather than memorizing N batch variants.

This ADR defines the DSL.

## Decision

Add **one MCP tool** named `request` that accepts a batch of operations expressed in function-call
syntax.

### Syntax

A request is a string. Two equivalent forms are accepted:

**Function-call form** (canonical, agent-friendly):

```
[tool_name(arg=value, arg=value), tool_name(arg=value)]
```

**JSON form** (for tools that prefer structured input):

```
[{"tool": "tool_name", "args": {"arg": "value"}}, {"tool": "tool_name", "args": {...}}]
```

A single operation (no batching) is also accepted:

```
tool_name(arg=value)
```

### Semantics

- Operations inside `[...]` are separated by **`,`** and dispatched **in parallel**.
- All operations must succeed for the request to be considered fully successful, but a per-op error
  is reported in the response — the request itself does not fail just because one op did.
- Maximum **100 operations per request** to bound memory and prevent abuse.
- Order of results matches the order of input operations.
- The tool exposed for batch use is the same set of tools available individually —
  `create(kind="entity")`, `update(kind="entity")`, `link`, etc. The `request` tool is a thin
  dispatcher over the existing tool registry.

### What's NOT in v0.1

- **Sequential chain mode (`|` separator).** Deferred. Agents needing sequence can call `request`
  multiple times in series.
- **`$prev` / `$prev.field.path` substitution.** Deferred with chain mode.
- **Mixing `,` and `|` in one request.** Not applicable since `|` isn't supported.
- **Positional arguments** beyond the per-op named arguments. All args must be named.

### Argument values

Argument values follow JSON literal syntax inside the function-call form:

- **String**: `"hello world"` (double-quoted)
- **Number**: `42`, `3.14`, `-1e6`
- **Boolean**: `true`, `false`
- **Null**: `null`
- **Array**: `[1, 2, 3]`, `["a", "b"]`
- **Object**: `{"key": "value", "nested": {...}}`

String escapes: `\\`, `\"`, `\n`, `\t`, `\r`. Other backslash-prefixed characters are literal.

### Wire shape (MCP tool params)

```json
{
  "ops": "[create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention\"), create(kind=\"entity\", entity_kind=\"document\", name=\"Attention Is All You Need\")]"
}
```

### Wire shape (response)

```json
{
  "results": [
    { "ok": true, "result": { ... entity 1 JSON ... } },
    { "ok": true, "result": { ... entity 2 JSON ... } }
  ],
  "summary": {
    "total": 2,
    "succeeded": 2,
    "failed": 0
  }
}
```

If an op fails:

```json
{
  "ok": false,
  "error": "invalid relation: 'related_to' — must be one of: contains | part_of | ..."
}
```

The `ok` discriminant + per-op result/error means an agent can inspect any operation independently,
even when others succeed.

## Worked examples

**Create multiple entities in one call**:

```
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"QLoRA\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"DoRA\")]")
```

**Create entities and link them** (sequential needed → two `request` calls):

```
# First call: create entities, get their IDs
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"A\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"B\")]")
# → returns IDs id_a, id_b
# Second call: link them
request(ops="[link(source_id=\"<id_a>\", target_id=\"<id_b>\", relation=\"extends\", weight=0.9)]")
```

**Bulk-update a set of edges**:

```
request(ops="[update(kind=\"edge\", id=\"...\", weight=0.85), update(kind=\"edge\", id=\"...\", weight=0.7)]")
```

**Mixed operations** (the point of the generic tool — any combination):

```
request(ops="[create(kind=\"entity\", entity_kind=\"document\", name=\"...\"), delete(kind=\"edge\", id=\"...\"), update(kind=\"entity\", id=\"...\", description=\"new\")]")
```

## Rationale

### Why function-call syntax over JSON-only?

Function-call syntax (`link(source_id="...", relation="extends")`) is denser than the equivalent
JSON object. For an LLM generating a tool call, dense input means less context burned. Agents also
reason about function calls naturally — it matches the calling convention they use for tools
generally.

JSON form stays available as a fallback for tools that emit structured objects more easily than they
handle string templating.

### Why parallel-only for v0.1?

Chain mode requires `$prev` substitution and reference resolution — a non-trivial parser feature
that benefits from the parallel case being well-tested first. Most batch patterns are independent
ops (create N entities, link N edges); these are parallel-friendly. Sequential workflows can be done
with two `request` calls until v0.2 adds chains.

### Why 100 ops per batch?

Resource bounds: prevents an agent from accidentally submitting a 10k-op request that locks the
runtime. 100 is well past typical agent usage (~5-20 ops in practice). If we hit the limit in real
use, raise it; if we never do, the bound was right.

### Why one generic tool instead of per-operation batch variants?

- **Surface stays compact**: one batcher, not one per CRUD verb. Smaller for agents to discover.
- **Consistency**: any tool is batchable as soon as it exists. No "is this tool batchable?"
  question.
- **Composability**: agents can mix operations in one call (create + delete + link) — impossible
  with per-op batch variants.

### Why "request" as the name?

It's the generic word for "this is a structured ask of the system." Alternatives considered: `batch`
(too narrow — implies homogeneous ops); `exec` (too imperative); `call` (overloaded with single-call
semantics); `do` (too generic and clashes with reserved words). `request` is neutral and matches
existing convention in other MCP systems.

## Alternatives Considered

| Alternative                                      | Pros                                      | Cons                                                      | Why rejected                            |
| ------------------------------------------------ | ----------------------------------------- | --------------------------------------------------------- | --------------------------------------- |
| Per-operation `_batch` tools (one per CRUD verb) | Tool-specific validation, clear schemas   | One new tool per existing verb, no cross-tool batching    | Surface bloat without composability win |
| JSON-array only                                  | No DSL parser needed                      | More tokens per request, less natural for LLMs            | Function-call form is the dense default |
| Allow sequential pipe-separated ops in v0.1      | Closes the gap to ADR-014 merge workflows | Requires `$prev` substitution, doubles parser complexity  | Deferred to v0.2; v0.1 is parallel-only |
| Single-flight only (no batching)                 | Smallest possible API                     | Misses the whole point                                    | Doesn't solve the problem               |
| YAML/TOML format                                 | Familiar for ops, multiline support       | More parser complexity, agents don't natively emit either | JSON literal values cover all cases     |

## Consequences

### Positive

- One generic batch tool eliminates the need for per-op batch variants throughout the API.
- Agents can compose any mix of operations in a single call.
- Tool surface stays compact (one `request` tool covers all batched work).
- Parser produces structured `ParsedOp` objects internally — testable in isolation.
- Forward path to chains (`|`) and `$prev` is clear without breaking the v0.1 shape.

### Negative

- The DSL is a non-standard parser an agent must learn. Mitigated: it's literally function-call
  syntax that agents already produce natively for tool invocation.
- Per-op error reporting means callers must check `ok` on each result — slightly more code than "one
  big error or all success."
- Batch failures don't roll back. v0.1 has no transaction support across ops; if op 3 of 5 fails,
  ops 1-2 are committed and ops 4-5 still attempted. Document this clearly.

### Neutral

- ~~The parser lives in `khive-mcp` as a private module~~ — superseded 2026-05-18: the parser
  lives in its **own crate**, `khive-request`. Two rationales: (1) every transport (MCP, future
  HTTP gateway, FFI, CLI) parses the same shape, so it doesn't belong to any one of them; (2) the
  *parse → compile → dispatch → execute → return* pipeline is shared between this DSL and LNDL
  (Lion Natural Directive Language). Keeping the parser in its own crate makes adding pipe chains,
  LNDL frontends, or bash-style conventions a pure-parser change with zero impact on runtime
  layering.

  Public types: `parse_request`, `ParsedRequest`, `ParsedOp`, `DslError`, `MAX_OPS`.

## Implementation Status (shipped 2026-05-18)

| Step                                                                  | Where                                                              | Status |
| --------------------------------------------------------------------- | ------------------------------------------------------------------ | ------ |
| Standalone crate: parser + AST                                        | `crates/khive-request/`                                            | done   |
| Parser (hand-written recursive descent; JSON form via `serde_json`)   | `crates/khive-request/src/lib.rs`                                  | done   |
| MCP tool: single `#[tool] request`, parallel dispatch via `join_all`  | `crates/khive-mcp/src/server.rs`                                   | done   |
| Tool param struct                                                     | `crates/khive-mcp/src/tools/request.rs`                            | done   |
| 17 parser unit tests + 13 MCP integration tests                       | `crates/khive-request/src/lib.rs`, `khive-mcp/tests/`              | done   |

The flat verb tools previously listed in ADR-023 (`create`, `get`, `list`, `update`, `delete`,
`merge`, `search`, `link`, `neighbors`, `traverse`, `query`) are now reached *through* `request`
— their verb names and per-pack semantics are unchanged; only the wire shape moved.

## Open Questions

1. **All-or-nothing semantics flag?** Currently best-effort (run all, report per-op). A
   `transactional: true` flag could short-circuit on first failure. Add only if a real workflow
   needs it.
2. **Schema-aware validation?** Currently each op's args pass through to the underlying tool, which
   validates them. A pre-validation pass that checks against the target tool's JSON schema could
   fail-fast before any dispatch. Defer — the underlying tool's validation is the authoritative
   check.
3. **Result shape for void ops?** `delete(kind="entity")` returns `{deleted: bool, id: ...}`. `link`
   returns the new Edge. Different shapes per tool. Document that the `result` field carries
   whatever the tool would have returned standalone.
4. **`request` as a recursive op?** Could `request` itself be one of the ops inside a batch?
   Currently no — the parser would accept it but the dispatcher rejects nested requests to avoid
   recursion bombs. Re-enable if a real workflow needs it.

## References

- ADR-014: KG Curation Operations (provides the operations that benefit from batching)
- ADR-019: Note Kind Taxonomy (parallel curation: closes another free-string gap)
- RFC 8259: JSON spec (governs the literal-value subset of the DSL)
