# ADR-016: Request DSL

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

Agents call khive verbs. The wire format that carries those calls must satisfy
several constraints:

1. **Compact for LLM consumption.** Every token an agent burns on syntax is a
   token it can't spend on reasoning. The wire format must be as dense as JSON
   allows while staying parseable.
2. **Batch- and chain-capable.** Agents frequently want to "do these N things
   together" or "do X then use its result for Y." The wire format must express
   both without forcing N round trips.
3. **One MCP tool, not N.** Adding a tool per verb (or per verb-pack combination)
   creates a discovery burden. Agents shouldn't memorize 40 tools; they call one
   tool with a structured argument.
4. **Transport-agnostic.** The same wire format works over MCP stdio today,
   HTTP later, and FFI in between. The parser does not belong to any one
   transport.
5. **Pack-extensible vocabulary.** Loaded packs add verbs (e.g., GTD adds
   `assign`/`next`/`complete`/`tasks`/`transition`). The DSL accepts any
   registered verb without code changes to the parser.

## Decision

### One MCP tool: `request`

khive exposes exactly one MCP tool, named `request`. It accepts a string
argument that the parser turns into one or more verb invocations:

```text
mcp/tools/list  → returns one tool: request
mcp/tools/call  → request(ops="<dsl-string>")
```

There are no per-verb tools. There is no `create_entity` tool, no `link` tool,
no `merge` tool. Every verb the runtime knows about is reachable through
`request`. Verb selection happens inside the DSL string.

The tool's description lists the currently-registered verb catalog so MCP
clients can discover what's available without trial and error. The list is
generated from the `VerbRegistry` at server startup, reflecting the loaded
packs.

### Three syntactic forms

The DSL accepts three input shapes. All three produce the same parsed
intermediate representation:

**Single operation** (no batching, no chaining):

```text
verb(arg=value, arg=value)
```

**Parallel batch** (operations run independently, results in input order):

```text
[verb1(...), verb2(...), verb3(...)]
```

**Sequential chain** (operations run in order, with `$prev` substitution):

```text
verb1(...) | verb2(arg=$prev.id) | verb3(arg=$prev.field)
```

Mixing `,` and `|` at the top level is rejected as a parse error. A future
extension may define sub-chain bracketing if a real use case justifies the
complexity.

**JSON form** is canonical for programmatic input that produces structured objects
more easily than string templating:

```json
[{"tool": "verb1", "args": {...}}, {"tool": "verb2", "args": {...}}]
```

**JSON form does not support `$prev` substitution.** JSON form always runs in
parallel (`ExecutionMode::Parallel`). Any argument value that is exactly
`"$prev"`, starts with `"$prev."`, or starts with `"$prev["` — including inside
nested arrays or objects — is rejected at parse time with
`DslError::PrevRefInJsonForm { arg_name }`. To use `$prev` substitution, use the
function-call DSL with the `|` chain operator.

Function-call form is canonical for LLM-generated input (denser tokens). Both
forms produce the same `ParsedRequest` AST for non-chain ops.

### Argument value grammar

Argument values use JSON literal syntax inside the function-call form:

| Type                   | Example                                       |
| ---------------------- | --------------------------------------------- |
| String                 | `"hello world"` (double-quoted)               |
| Number                 | `42`, `3.14`, `-1e6`                          |
| Boolean                | `true`, `false`                               |
| Null                   | `null`                                        |
| Array                  | `[1, 2, 3]`, `["a", "b"]`                     |
| Object                 | `{"key": "value", "nested": {...}}`           |
| Reference (chain only) | `$prev`, `$prev.field.path`, `$prev[N].field` |

String escapes follow JSON: `\\`, `\"`, `\n`, `\t`, `\r`. Other backslash
sequences are literal.

#### `$prev` reference grammar

`$prev` references are valid only in chain (`|`) mode. In Single and Parallel
forms they are rejected at parse time with `DslError::PrevRefOutsideChain`.

Reference syntax:

- `$prev` — the whole prior result.
- `$prev.field` and `$prev.field.nested.path` — dot-separated field path.
- `$prev[N]` and `$prev[N].field` — zero-based array index, then optional path.
- Mixed: `$prev.items[0].id` — field, then index, then field.

Array indices must be non-negative integers. Negative indices, non-numeric
content, and unclosed brackets are parse errors for the unquoted form and are
treated as literal strings (not promoted) for the quoted form.

#### Quoted `$prev` strings (CC-3)

A string argument like `"$prev.id"` is treated identically to the unquoted
token `$prev.id` — both produce `ArgValue::PrevRef { path: "id" }`. This
applies to dotted paths and bracket-index paths.

To pass the **literal** string `$prev.id` as a value (not a reference), escape
the leading `$` in the DSL source:

```text
get(id="\\$prev.id")   # delivers the string "$prev.id" to the handler
```

The DSL source `"\\$prev.id"` deserializes to `\$prev.id` (one backslash). The
parser strips the backslash and stores `ArgValue::Value("$prev.id")`. The
handler receives the clean literal string.

Quoted bracket-index strings (`"$prev[N].field"`) are subject to the same
index-validity rules as unquoted refs: only non-negative integers. Malformed
bracket content (negative, non-numeric, missing `]`) is NOT promoted to
`PrevRef` — it is stored as a literal string value.

All arguments are named. Positional arguments are not supported — verbs evolve
over time, and positional binding fragilely couples to argument order.

#### UUID arguments — full or short-prefix

Arguments typed as UUID (entity ids, note ids, edge ids, profile ids) accept
either form on input:

| Form           | Pattern                 | Example                                |
| -------------- | ----------------------- | -------------------------------------- |
| Full canonical | 8-4-4-4-12 hex, dashed  | `a1b2c3d4-e5f6-7890-abcd-ef1234567890` |
| Short prefix   | 8+ hex chars, no dashes | `a1b2c3d4`                             |

The runtime resolves short prefixes against the caller's namespace
(`khive-runtime::operations::resolve_uuid_async`):

- Exactly one match → resolves to the full UUID, op proceeds.
- Multiple matches → `RuntimeError::AmbiguousPrefix { prefix, matches: Vec<Uuid> }`,
  the op fails (the batch is not aborted in parallel mode; chain mode aborts).
- Zero matches → `RuntimeError::NotFound { kind, id }`, same fail-isolation rule.

Short prefixes apply across all substrates the verb's `id` parameter accepts;
the resolver scans entities, notes, and edges in deterministic order. Calls that
need disambiguation should pass the full UUID. Inputs accept full UUID or short
form (8+ hex chars) by default; ADR-045 (Verb Response Presentation Modes)
specifies the output-side short-form representation (8-char prefix in Agent
mode).

### Parallel semantics

Operations separated by `,` inside `[...]` dispatch concurrently. The runtime
issues all of them through the `VerbRegistry` simultaneously and collects
results. Results return in input order (not completion order).

Per-op success or failure is independent. The request envelope reports success
when the parse succeeded; individual ops may succeed or fail without aborting
their siblings:

```json
{
  "results": [
    { "ok": true,  "tool": "create", "result": { ... } },
    { "ok": false, "tool": "link",   "error": "endpoint validation failed: ..." },
    { "ok": true,  "tool": "update", "result": { ... } }
  ],
  "summary": { "total": 3, "succeeded": 2, "failed": 1 }
}
```

The caller inspects each op's `ok` discriminant. There is no all-or-nothing
mode in v1; transactional batches are deferred (see Open Questions).

### Chain semantics

Operations separated by `|` run **sequentially**. Each op waits for the prior
op to complete before dispatching. `$prev` substitution happens at dispatch
time, after the prior op's result is known.

```text
create(kind="entity", entity_kind="concept", name="FlashAttention")
  | link(source_id=$prev.id, target_id="abc-123", relation="extends")
  | update(kind="entity", id=$prev.target_id, description="...")
```

`$prev` resolves to the immediately preceding op's full result JSON.
`$prev.field` and `$prev.field.nested.path` perform dot-path extraction on the
prior result. If a referenced field doesn't exist, the chain aborts with a
typed substitution error whose message includes `"Available top-level fields: [...]"`
— listing the keys actually present in the prior result — to aid debugging.

Bare `$prev` (no path) resolving to a map or array is also an error: the
dispatcher detects the case and emits a substitution error with the available
field names before passing the value to the handler, preventing confusing
downstream type errors.

**Typed parser errors added in v0.2.2:**

| Error                                      | When emitted                                          |
| ------------------------------------------ | ----------------------------------------------------- |
| `DslError::PrevRefOutsideChain { pos }`    | `$prev` in Single or Parallel form                    |
| `DslError::PrevRefInJsonForm { arg_name }` | `$prev` string in JSON-form arg (top-level or nested) |
| `DslError::UnsupportedVerbNesting { pos }` | Verb name with more than one dot (e.g. `a.b.c`)       |

**Chain abort behavior**: If any op fails (or any `$prev` substitution fails),
remaining ops in the chain are NOT dispatched. They appear in the response with
`aborted: true`:

```json
{
  "results": [
    { "ok": true, "tool": "create", "result": { "id": "..." } },
    { "ok": false, "tool": "link", "error": "target not found: abc-123" },
    { "ok": false, "tool": "update", "aborted": true }
  ],
  "summary": { "total": 3, "succeeded": 1, "failed": 1, "aborted": 1 }
}
```

Committed ops are NOT rolled back when a later op aborts. Chains do not provide
implicit cross-op transactions. If atomicity matters, the verb itself must be
atomic (single-backend curation operations from ADR-014 are atomic within their
backend).

### Maximum operations per request

A single `request` carries at most **100 operations** (parallel batch or chain).
This bounds memory and prevents accidental million-op requests from blocking
the runtime.

100 is well past observed agent usage (typical: 5–20 ops). If real usage hits
the limit, raise it; if it never does, the bound is correct.

The parser rejects oversized requests with `DslError::TooManyOps { max: 100,
got: N }` before any verb dispatch happens.

### Parser crate: `khive-request`

The DSL parser lives in its own crate, `khive-request`. Every transport (MCP,
future HTTP gateway, FFI, CLI) parses the same shape. The parser must not
belong to any one transport — it's shared infrastructure.

```text
crates/khive-request/  parses verb-dispatch DSL
  src/lib.rs            ParsedRequest, ParsedOp, ExecutionMode, parse_request
                        DslError, MAX_OPS constant
```

`khive-request` has zero runtime dependencies — it produces a typed
`ParsedRequest` and returns. It does not validate verb names against any
registry, does not check argument types, does not resolve `$prev`. Those are
runtime concerns; the parser produces a pure-syntax representation.

```rust
pub struct ParsedRequest {
    pub ops: Vec<ParsedOp>,
    pub mode: ExecutionMode,
}

pub struct ParsedOp {
    pub tool: String,
    pub args: serde_json::Map<String, serde_json::Value>,
}

pub enum ExecutionMode {
    Single,    // one op, no batching
    Parallel,  // `,` batch
    Chain,     // `|` chain
}

pub fn parse_request(input: &str) -> Result<ParsedRequest, DslError>;
```

The parser is hand-written recursive descent for the function-call form; JSON
form parses via `serde_json` and converts. Parse errors carry input positions
and expected-token hints.

### Dispatch pipeline

After parsing, the dispatch pipeline runs:

```text
1. khive-request::parse_request(input) → ParsedRequest
2. For each op: VerbRegistry lookup (ADR-003)
3. For each op: NamespaceToken passed through to handler (ADR-007)
4. For each op: handler validates args, calls runtime/coordinator
5. Parallel mode: all ops dispatch concurrently
   Chain mode: ops dispatch sequentially, $prev substitution between ops
6. Results collected in input order
7. Response envelope: results + summary
```

Cross-backend dispatch (when a verb's effective backend isn't fixed by the
caller) is handled by the SubstrateCoordinator (ADR-003) after verb lookup.
The DSL doesn't carry backend selection — that's a runtime concern.

### Verb registration is pack-driven

Verbs are not hardcoded in the parser or the MCP server. Each pack registers
its verbs with the `VerbRegistry` at runtime startup (ADR-003, ADR-017). The
registry maps verb names to handler functions.

When the MCP server starts, the registry is final and the verb catalog is
serialized into the `request` tool description for client discovery.
Mid-process verb changes are not supported in v1.

Unknown verb names at dispatch time return
`{ ok: false, tool: "<name>", error: "unknown verb: <name>; registered: [...]"
}` for that op. The request does not abort because of one unknown verb —
sibling ops still attempt.

### Wire shape

MCP `request` tool params:

```json
{ "ops": "<dsl-string>" }
```

The argument name is `ops` for historical reasons (originated when only
parallel batches existed). It carries any of the three syntactic forms.

Response envelope:

```json
{
  "results": [
    { "ok": true,  "tool": "<verb>", "result": <verb-specific-value> },
    { "ok": false, "tool": "<verb>", "error": "<message>" },
    { "ok": false, "tool": "<verb>", "aborted": true }
  ],
  "summary": { "total": N, "succeeded": K, "failed": M, "aborted": A }
}
```

`results.length == summary.total == input ops count`. Order preserves input
order regardless of parallel completion order.

### Gate enforcement

Per ADR-003, gate enforcement is part of the agent-binary (`khive-mcp`)
dispatch path. The gate sits between the parser and the runtime:

```text
parse → for each op: gate.check(verb, args, namespace_token) → dispatch
```

Gate denial returns a per-op error with the gate's denial message. The request
itself parses and runs; ops the gate denies appear as `ok: false` with a
distinguishable error category.

`kkernel` runs without the gate (operator context, ADR-003). The same DSL
parses and dispatches in `kkernel mcp`, but with `AllowAllGate` installed.

### Future frontends

`khive-request` is the substrate for additional input formats. Planned
frontends layer on top of the DSL parser:

- **LNDL (Lion Natural Directive Language)**: human-friendly natural-language
  parsing layer that compiles to the same `ParsedRequest`. Not in v1.
- **Bash-style positional**: shell-style invocations that desugar to the
  function-call form. Speculative; no current use case.
- **CLI flag form**: `khive create --kind=entity --name=...` for `kkernel`
  one-shot invocations. Planned, not v1.

All future frontends produce the same `ParsedRequest` shape. The runtime
doesn't care which parser produced the AST.

## Rationale

### Why one MCP tool (not many)?

40 tools means 40 schemas to maintain, 40 description blobs in the
`tools/list` response, 40 ways for clients to misinterpret the API. One tool
with a structured DSL means one schema, one description, one client-side
discovery surface.

The cost is the DSL — clients have to know how to construct it. The benefit is
that adding a verb requires no MCP surface change; the verb appears in the
catalog by virtue of pack registration.

### Why function-call syntax (not JSON-first)?

For LLM-generated input, function-call form is materially denser than
equivalent JSON. `link(source_id="a", target_id="b", relation="extends")` vs
`{"tool": "link", "args": {"source_id": "a", "target_id": "b", "relation":
"extends"}}` — same intent, half the tokens.

LLMs also produce function-call patterns natively (it's how they invoke tools
in most agent frameworks). Function-call DSL meets them where they already are.

JSON stays available for programmatic clients.

### Why named arguments only?

Verb signatures evolve. Adding an optional argument to `create` shouldn't break
existing callers. Positional binding ties callers to argument order and
position-of-default-values. Named arguments are forward-compatible.

The token cost is negligible — LLM-emitted DSL already uses named arguments
naturally.

### Why parser as its own crate?

Multiple transports (MCP, future HTTP, FFI, CLI) parse the same shape. Putting
the parser inside `khive-mcp` forces every transport to either depend on
`khive-mcp` (which itself depends on the entire MCP stack) or reimplement the
parser. Both are bad.

`khive-request` has zero runtime dependencies. Transports depend on it without
pulling in unrelated machinery. The parser's contract is pure syntax → AST.
Validation lives in the runtime.

### Why parallel mode is best-effort (not all-or-nothing)?

In most cases, an agent issuing 10 parallel ops wants partial-success
visibility. If op 3 fails because a target was deleted, ops 1, 2, 4-10
shouldn't be penalized. Per-op error reporting is the natural shape.

If a caller needs strict atomicity, the right tool is a verb that runs the
sequence as a single unit (e.g., `merge_entity` is one atomic curation
operation, not a batch of `update + delete`). Pseudo-atomic batches across
arbitrary verbs are misleading — they suggest atomicity the runtime can't
deliver across multiple backends or capabilities.

### Why chain mode aborts but doesn't roll back?

Chains express "do A then do B with A's result." If A succeeds but B fails,
the user knows that A's effect is persisted. Implicit rollback would mean
either (a) chains are pseudo-transactions across arbitrary verbs (false for
the same reasons as above), or (b) hidden side effects when B fails that the
caller didn't ask for.

Explicit abort + persisted prior-ops is honest. The caller sees what happened
and decides how to recover.

### Why 100 ops max?

Bounded resources. 100 is well past observed usage. If the limit hits in
production, it gets raised; the constant lives in one place
(`khive-request::MAX_OPS`).

### Why verbs are pack-registered (not parser-known)?

The parser shouldn't know what verbs exist. If it did, adding a verb would
require parser changes — which means a `khive-request` release, which means
coupling unrelated crates. Pack-registered verbs are looked up at dispatch
time against the runtime's `VerbRegistry`. The parser produces strings; the
registry resolves them.

## Alternatives Considered

| Alternative                        | Why rejected                                                                               |
| ---------------------------------- | ------------------------------------------------------------------------------------------ |
| Per-verb MCP tools                 | 40+ tools to maintain; massive `tools/list` response; no batch composability.              |
| JSON-only wire format              | Token-inefficient for LLM-generated input; function-call form is denser.                   |
| Positional arguments               | Brittle as verbs evolve; named args are forward-compatible.                                |
| Parser inside `khive-mcp`          | Couples non-MCP transports to MCP-stack dependencies.                                      |
| All-or-nothing parallel batches    | Misleading atomicity claim; rollback across multiple backends/capabilities isn't possible. |
| Implicit chain rollback            | Hidden side effects when later ops fail; not honest.                                       |
| Higher op limit (1000+)            | Premature; observed usage is well under 100.                                               |
| Verbs in a parser-known closed set | Couples parser to runtime release cycle.                                                   |
| YAML/TOML wire format              | More parser surface; LLMs don't natively emit either; JSON literal values cover all cases. |

## Consequences

### Positive

- One MCP tool — minimal discovery surface, no per-verb tool maintenance.
- Function-call DSL is LLM-dense; chain syntax matches "do A, then B with A's
  result" thinking.
- Parser is transport-agnostic — same DSL works over MCP, future HTTP, FFI.
- Pack-registered verbs appear in the catalog without parser changes.
- Per-op error reporting gives callers fine-grained recovery.
- 100-op limit prevents accidental DoS without restricting normal use.

### Negative

- Clients must construct the DSL string (or use JSON form). Adds a
  client-side parser dependency or a string-templating responsibility.
  Mitigated: the JSON form is always available; client libraries can wrap
  the DSL.
- Parallel mode is best-effort, not transactional. Callers must check per-op
  results.
  Mitigated: that's the only honest shape; transactional batches are deferred
  pending a real use case.
- Chain abort leaves committed prior ops in place. Callers must reason about
  partial state.
  Mitigated: documented; in most patterns the prior op's effect is desired
  anyway.

### Neutral

- LNDL and other natural-language frontends are deferred; they compile to the
  same `ParsedRequest` when they ship.
- The `ops` parameter name is historical; clients name it consistently.
- Verb catalog in tool description regenerates at startup based on loaded
  packs.

## Implementation

- `crates/khive-request/src/lib.rs`:
  - `parse_request(input: &str) -> Result<ParsedRequest, DslError>`
  - `ParsedRequest`, `ParsedOp`, `ExecutionMode` types
  - Hand-written recursive-descent parser for function-call form
  - JSON form via `serde_json::from_str` + conversion
  - `MAX_OPS: usize = 100`
- `crates/khive-mcp/src/server.rs`:
  - Single `#[tool] request(ops: String)` exposed
  - Dispatches parsed ops through `VerbRegistry` (ADR-003)
  - Parallel mode: `join_all`
  - Chain mode: sequential await + `$prev` substitution
  - Response envelope: `results` + `summary`
- `crates/khive-mcp/src/tools/request.rs`: tool param struct and schema.
- `crates/khive-mcp/src/tools/description.rs`: dynamic catalog injection
  from `VerbRegistry`.
- `crates/khive-runtime/src/registry.rs`: `VerbRegistry` — registration,
  lookup, catalog enumeration.

## References

- ADR-003: System Architecture — `VerbRegistry`, dispatch model, gate
  enforcement, binary boundary.
- ADR-007: Namespace — `NamespaceToken` flows through dispatch into every
  handler.
- ADR-008: Query Layer Separation — `khive-query` parses GQL/SPARQL (a
  different kind of input); `khive-request` parses the verb-dispatch DSL.
- ADR-014: Curation Operations — verbs that the DSL invokes (update, merge,
  delete).
- ADR-017: Pack Standard — how packs register verbs with the `VerbRegistry`.
