# ADR-045: Verb Response Presentation Modes

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Extended by**: ADR-078 (Output Format and Shape-Aware Rendering), which introduces an orthogonal
`format` axis (`json` / `auto` / `table`) and revises Agent-mode redundancy rules. ADR-078
§7.1 omits `full_id` from the opt-in `auto` and `table` views while preserving it in the canonical
`json` representation.
**Depends on**:

- ADR-016 (Request DSL: short-UUID-prefix resolution on input)
- ADR-017 (Pack Standard: handler return shape)

## Context

khive verb handlers return full, normalized payloads: every field present even
when empty, full UUIDs in canonical 8-4-4-4-12 form, full ISO-8601 timestamps,
deeply-nested empty containers. This is correct for _handler logic_ (deterministic
shape, easy to test, easy to round-trip) but expensive for _agents reading the
output_: a typical `list(kind=entity, limit=10)` response can devote substantial
space to whitespace, empty containers, full identifiers, and timestamps that are
not needed for interactive inspection.

Agents are token-budgeted; humans want pretty output for inspection; tooling
(scripts, dashboards) wants the full schema. These three audiences want
different shapes of the same data. A single full-shape default imposes unnecessary
cost on compact interactive output.

The requirement is to preserve one canonical handler response while allowing the
response boundary to select a compact, verbose, or human-readable presentation.

The handler stays canonical. The presentation layer transforms based on the
caller's declared mode.

### Scope

This ADR specifies:

- Three presentation modes (`agent` default, `verbose`, `human`)
- The transformation rules each mode applies
- Where the transformation runs (post-handler, pre-wire)
- How callers select a mode
- What handlers MUST NOT do (e.g., return mode-specific output)

It does NOT specify per-verb custom presentation logic, schema migration to
trim handler outputs at the source, or any change to the canonical Rust types
returned by the runtime.

## Decision

### 1. Three modes

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationMode {
    /// Token-efficient. Default for MCP callers (agents).
    Agent,
    /// Full canonical shape. Default for `kkernel exec` and CI / scripted callers.
    Verbose,
    /// Canonical value reserved for caller-side human formatting.
    Human,
}

impl Default for PresentationMode {
    fn default() -> Self { Self::Agent }
}
```

The handler returns the canonical (== verbose) shape always. The runtime's
response-envelope layer picks the mode based on caller declaration and applies
the corresponding transform.

### 2. Selection rules

| Caller surface                      | Default mode | Override                                                     |
| ----------------------------------- | ------------ | ------------------------------------------------------------ |
| MCP (`request` tool)                | `Agent`      | envelope-level `presentation_per_op` array (see §Wire shape) |
| `kkernel exec '<pack>.<verb>(...)'` | `Verbose`    | `--presentation agent` or `--presentation human` flag        |

The presentation argument is parsed by the runtime envelope, not by the
handler. Handlers MUST NOT inspect or branch on the mode.

### 3. Transformation rules

#### `Agent` mode (token-efficient)

| Field type                           | Verbose form                                        | Agent form                                                                                                                                   |
| ------------------------------------ | --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| UUID                                 | `"a1b2c3d4-e5f6-7890-abcd-ef1234567890"` (36 chars) | `"a1b2c3d4"` (8 chars: first segment)                                                                                                        |
| Timestamp (ISO-8601)                 | `"2026-05-23T16:18:15.234567Z"` (27 chars)          | `"2026-05-23T16:18"` (16 chars: minute granularity) OR relative `"3m ago"` if < 24h (sampled once per `present()` call: see §Implementation) |
| Empty string `""`                    | included                                            | dropped                                                                                                                                      |
| Empty array `[]`                     | included                                            | dropped                                                                                                                                      |
| Empty object `{}`                    | included                                            | dropped                                                                                                                                      |
| `null` field                         | included                                            | dropped except declared lifecycle markers                                                                                                    |
| Nested object with all empties       | included                                            | dropped                                                                                                                                      |
| Repeated field with `null` entries   | included                                            | empties filtered out                                                                                                                         |
| Score fields (see §Score truncation) | `0.1234567890`                                      | `0.123` (3-significant-digit truncation)                                                                                                     |

**Drop semantics and lifecycle `null` preservation:** Drop `[]`, `{}`, and `""`.
Do not drop `null` when a pack declares that the field's absence carries lifecycle
meaning. The KG pack preserves its record and proposal lifecycle markers, including
`deleted_at`, `superseded_at`, `applied_at`, `withdrawn_at`, and `reviewed_at`, plus
the relationship markers `parent_id`, `superseded_by`, and `replaced_by`.

Other `null` fields are dropped. Packs declare any additional preserved-null fields
in their own public presentation contracts; the core list is limited to fields emitted by
the KG pack.

**Score truncation:** Agent mode truncates the following field names (and only
these) to 3 significant figures:

- `score`, `score_breakdown.*` (all nested keys), `salience`, `decay_factor`,
  `rrf_score`, `similarity`

All other `f32`/`f64` fields, such as `weight` on edges and future numeric attributes,
pass through canonical. Type-based truncation is forbidden.

Short UUIDs in Agent mode echo the same form that ADR-016 accepts on input. When a canonical
handler result includes `full_id`, the presentation transform preserves that field in `json` so
callers retain a stable full identifier. ADR-078 removes it only from the opt-in lossy views.

Score truncation preserves ordering (3 sig figs is enough to compare scores)
without burning tokens on float noise.

#### `Verbose` mode (canonical)

No transformation. The handler's return value is serialized as-is. This is the
shape that round-trips through CI / scripted callers without surprises.

#### `Human` mode (pretty-printed terminal)

**MCP/runtime boundary: `Human` is a no-op at this layer.**

When `presentation=human` is sent over the MCP wire or `kkernel exec`, the
runtime returns canonical (verbose) JSON: identical to `Verbose`. No
transformation is applied inside `khive-runtime::presentation`. This is a
deliberate design decision, not an omission:

1. MCP responses are consumed over a JSON transport. Injecting ANSI escape
   codes, table-layout whitespace, or terminal glyphs into JSON would corrupt
   the response for every non-terminal consumer.
2. A terminal or other user interface may apply its own formatting after receiving
   canonical JSON. That caller-side rendering is outside the runtime contract.

Callers that pass `presentation=human` therefore receive verbose JSON. The mode
reserves a stable selection value without introducing terminal-specific bytes into
the MCP response.

### 3.5. Error envelopes are never transformed

**Error envelopes are NEVER transformed.** When a verb returns
`{ok: false, tool: "...", error: "...", aborted: bool}`, the envelope passes
through canonical regardless of `PresentationMode`. Error strings: including
UUIDs in error messages: remain full-form for debugging. The transform applies
only to the `result` field of successful envelopes
(`{ok: true, tool: "...", result: <transformed>}`).

### 4. Where the transformation runs

**Chain `$prev` substitution operates on canonical (verbose) handler output.**
The presentation transform runs AFTER the entire request batch: including all
`$prev` chain substitutions: completes, at the response-envelope boundary.
The transform NEVER runs per-op mid-chain. A chain's intermediate results are
never observed in their presented form.

Three surfaces have transformation hooks:

```
┌─────────────────────────────────────────────────────────┐
│  Handler (in pack)                                       │
│    returns: serde_json::Value (canonical, verbose shape) │
└─────────────────────────────────────────────────────────┘
                            ↓
┌─────────────────────────────────────────────────────────┐
│  Runtime response envelope                               │
│    1. Build {ok: true, tool: <verb>, result: <value>}    │
│    2. Apply PresentationMode transform to result         │
│       (skipped for error envelopes: §3.5)               │
│    3. Serialize to wire JSON                             │
└─────────────────────────────────────────────────────────┘
                            ↓
                       Wire JSON
```

The transformation runs once, after the handler returns, before serialization.
Implementation lives in `khive-runtime::presentation`:

```rust
pub fn present(
    value: serde_json::Value,
    mode: PresentationMode,
    now_unix_seconds: i64,
) -> serde_json::Value;
```

Pack handlers are unaware of mode. Tests against handler outputs always check
verbose shape: golden outputs don't need to be mode-aware.

### 5. Handler invariants

Handlers MUST:

- Return canonical verbose-shape JSON regardless of caller mode
- Include every field declared in the verb's response schema (use `null` for
  missing, not omit): the presentation layer trims, not the handler
- Use full ISO-8601 timestamps
- Use full canonical UUIDs

Handlers MUST NOT:

- Inspect the request envelope for `presentation` field
- Branch behavior on caller identity
- Return different schemas for different callers
- Pre-truncate or pre-shorten anything that the presentation layer should handle

Handlers must return complete canonical data before presentation transforms run.
Size-based omission inside a handler would make the canonical response depend on
the presentation audience and would prevent verbose callers from recovering the
full result.

### 6. Per-verb opt-out (escape hatch)

Some verbs return data where Agent-mode trimming is wrong: e.g., a verb that
explicitly returns "the full canonical UUID of X for downstream use" wants the
full form. Verbs declare:

```rust
pub enum VerbPresentationPolicy {
    Standard,
    AlwaysVerbose,
}
```

`HandlerDef::presentation_policy()` resolves the policy by registered verb name.

The following verbs are declared `AlwaysVerbose`. Treat omission from this
table for a NEW pack verb as a CI failure during pack registration.

| Verb                     | Default policy | Rationale                                                     |
| ------------------------ | -------------- | ------------------------------------------------------------- |
| `get`                    | AlwaysVerbose  | Caller needs full UUID to chain into other ops                |
| `query`                  | AlwaysVerbose  | User-projected fields; transform on user projections is wrong |
| `traverse`               | AlwaysVerbose  | Path UUIDs needed for follow-up                               |
| `neighbors`              | AlwaysVerbose  | Same as traverse                                              |
| `link` (create response) | AlwaysVerbose  | Edge IDs needed for follow-up                                 |
| Everything else          | Standard       | Apply per-mode transform                                      |

The declaration lives in the pack's verb registration, not the handler body:
it's metadata, not logic.

### 7. Examples

#### `list(kind=concept, limit=2)` in three modes

**Verbose (handler return):**

```json
{
  "ok": true,
  "tool": "list",
  "result": {
    "items": [
      {
        "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "kind": "concept",
        "name": "Response presentation",
        "description": "Shape-aware output for verb responses",
        "properties": {},
        "created_at": "2026-05-23T16:00:00.000000Z",
        "updated_at": "2026-05-23T16:18:15.234567Z",
        "tags": [],
        "namespace": "local"
      },
      {/* ... */}
    ],
    "total": 2,
    "page_cursor": null
  }
}
```

**Agent (Agent-mode transform):**

```json
{
  "ok": true,
  "tool": "list",
  "result": {
    "items": [
      {
        "id": "a1b2c3d4",
        "kind": "concept",
        "name": "Response presentation",
        "description": "Shape-aware output for verb responses",
        "created_at": "2026-05-23T16:00",
        "updated_at": "3m ago",
        "namespace": "local"
      },
      {/* ... */}
    ],
    "total": 2
  }
}
```

Note: `properties`, `tags`, and `page_cursor` are dropped because they are empty.

Synthetic response fixtures verify that Agent mode is no larger than the canonical
response for the same value. Exact savings depend on record shape and are not a
normative compatibility guarantee.

**Human (terminal):**

```
ID        KIND     NAME                     UPDATED
a1b2c3d4  concept  Response presentation    3m ago
...
2 concepts
```

## Rationale

### Why the transformation lives in the runtime, not per-handler

Three reasons:

1. **Consistency**: every verb gets the same trimming rules. Inconsistency
   between verbs ("list trims timestamps but get doesn't") is the kind of
   surface that agents stumble on.
2. **Testability**: handlers test their canonical output; the transformation
   is tested independently. One set of golden files, one transformation test
   suite.
3. **Compositional**: when a verb's output is consumed by another verb in a
   chain (`$prev` resolution, ADR-016 §"Chain semantics"), the chain reads the
   verbose canonical shape: it would be broken if mid-chain a `presentation`
   transform stripped fields the next op needs.

### Why short-UUID first 8 chars (not 12, not 16)

ADR-016's short-UUID-prefix resolution requires 8+ hex chars. The output side
matches the input side: agents that copy a `"a1b2c3d4"` back into a verb call
get the same record. Longer prefixes (12, 16) add tokens without strengthening
the round-trip contract.

### Why "3m ago" (not always absolute timestamps)

Two reasons:

1. **Token cost**: "3m ago" is 7 chars; `"2026-05-23T16:15:32.123Z"` is 24
   chars. Multiplied across timestamp-heavy responses (record lists and event logs)
   the saving is real.
2. **Agent affordance**: agents reason about recency much more than absolute
   time. "Is this recent?" is a faster decision from "3m ago" than from a
   precise timestamp.

Verbose mode preserves absolute time for tooling that needs to compare
timestamps across systems.

### Why empty-field dropping (with lifecycle-null preservation)

Empty arrays and empty objects burn tokens like empty strings do. Aggressively
dropping empty-meaningful structures (a `tags: []` field on a record with no
tags is meaningful absence, but the agent doesn't need to _see_ the meaningful
absence: it can infer from the field's absence).

The risk is ambiguity: did the field not exist, or was it empty? For agent
mode this is acceptable because the verb's response schema is documented
(ADR-017 §pack manifest declarations): the agent knows the field exists, it
just isn't populated.

However, blanket `null`-dropping is wrong for lifecycle-marker fields.
`applied_at: null` can mean "not applied" for a KG proposal; dropping it makes the record
indistinguishable from one where `applied_at` was never defined. The
preserve-null allowlist (§3 Agent table) resolves this: lifecycle `*_at`
fields and relationship markers pass through as `null`; purely optional
informational nulls are dropped.

### Why per-verb opt-out

Some verbs return identifiers or timestamps as their primary product. Trimming
these would damage the verb's contract. The opt-out is rare but named.

### Why not introduce a fourth "minimal" mode

Three modes are enough: tools (verbose), agents (agent), humans (human).
Adding more invites bikeshedding without clear use cases.

## Alternatives Considered

| Alternative                                                            | Why rejected                                                                            |
| ---------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| Handler-side rendering                                                 | Couples handlers to presentation logic; tests grow combinatorially                      |
| Always-minimal output                                                  | Breaks scripted callers that round-trip the response                                    |
| Mode declared once per connection, not per call                        | Some workflows mix verbose and agent calls; per-call control is the safer default       |
| Use Accept headers (HTTP-style)                                        | MCP doesn't have headers; introducing them for one ADR's worth of feature is overkill   |
| Truncate to top-N keys based on size budget                            | Brittle; the same call from two agents would return different shapes: non-deterministic |
| Strip empties only at MCP boundary, leave canonical for `kkernel exec` | Already the design: `kkernel exec` defaults to verbose                                  |

## Consequences

### Positive

- Agent presentation removes redundant fields from list-heavy responses without changing canonical handler output.
- Handler tests stay simple: one golden output per verb, not three.
- CLI gets pretty output for free via the `Human` mode dispatch.
- Future presentation needs can extend the same boundary transform.

### Negative

- New surface (`PresentationMode`, `present`, per-verb policy) adds concepts
  to the pack-author mental model. Mitigated by handler-side
  invariance: most pack authors never see this.
- Agent mode's "3m ago" is locale-dependent if khive ever ships i18n. v1 ships
  English-only relative-time formatting; i18n is a separate ADR if needed.

### Neutral

- The transformation runs per-response; CPU cost is negligible (microseconds
  on KB-sized JSON).
- `present()` is pure: easy to fuzz-test for round-trip invariants
  (e.g., `present(verbose) == verbose`, `present(agent).fields ⊆ verbose.fields`).

## Implementation

### Crate placement

- `PresentationMode` enum and `present` function: `khive-runtime::presentation`
- Per-verb policy: resolved from `HandlerDef` (ADR-017/023) through the runtime registry
- Transformation logic: `khive-runtime::presentation`
- MCP `request` arg parsing for `presentation` / `presentation_per_op`:
  `khive-mcp::server`
- CLI flag parsing: `kkernel::exec`

### `now` frozen-time semantics

`now: i64` (Unix seconds) is sampled ONCE per `present()` call and
passed as a parameter through the transform tree. All relative datetime
renderings within a response use the same `now`. The function signature
is:

```rust
pub fn present(value: Value, mode: PresentationMode, now_unix_seconds: i64) -> Value
```

Tests inject a fixed clock for deterministic replay. Same input + same `now`
→ identical output bytes.

### Chain `$prev` invariant

**`present(chain_intermediate)` is never observed.** The presentation transform
runs only at the final response-envelope boundary, after all `$prev`
substitutions for the entire batch are resolved. Intermediate results flowing
through a chain carry the canonical (verbose) shape throughout. This is a
testable invariant: no partial-batch response should ever pass through the
presentation transform.

### Wire shape

The `request` op envelope (ADR-016) gains optional presentation fields at the
envelope level:

```json
{
  "ops": "[list(kind=concept), get(id=a1b2c3d4)]",
  "presentation": "agent",
  "presentation_per_op": ["verbose", "agent"]
}
```

`presentation` is the batch default. `presentation_per_op` (optional) overrides
per-op by index. The argument name `presentation` is RESERVED at the
request-envelope level and CANNOT be used as a verb argument name.

Passing `presentation` inside the function-call args (e.g.,
`list(kind=concept, presentation=agent)`) is REJECTED as a parse error: it
collides with the reserved envelope key. Default is `agent` for MCP.

### Migration

No schema migration. Handlers retain their canonical shape. The transformation
layer is purely additive.

**Compatibility policy:** Agent mode is the default for MCP/stdio responses.
Clients that require the canonical full shape select `presentation=verbose` on
the request envelope. Verbose remains the default for `kkernel exec` and library
callers. Release-specific transition guidance belongs in the public release notes.

## References

- ADR-016 (Request DSL) §"UUID arguments": short-prefix resolution on input
  (this ADR's output-side counterpart)
- ADR-017 (Pack Standard): verb declaration, handler return shape
- ADR-016 (Request DSL): single-tool `request` envelope shape
