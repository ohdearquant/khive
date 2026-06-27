# ADR-045: Verb Response Presentation Modes

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive
**Extended by**: ADR-078 (Output Format and Shape-Aware Rendering), which introduces an orthogonal
`format` axis (`json` / `auto` / `table`) and revises Agent-mode redundancy rules. ADR-078
§7.1 partially supersedes the P-C1 implementation behavior that kept `full_id` present in all
modes; see the note below at the `include_full_id` override section.
**Depends on**:

- ADR-016 (Request DSL — short-UUID-prefix resolution on input)
- ADR-017 (Pack Standard — handler return shape)
- ADR-016 (Request DSL — single-tool `request` MCP wire envelope)

## Context

khive verb handlers return full, normalized payloads — every field present even
when empty, full UUIDs in canonical 8-4-4-4-12 form, full ISO-8601 timestamps,
deeply-nested empty containers. This is correct for _handler logic_ (deterministic
shape, easy to test, easy to round-trip) but expensive for _agents reading the
output_: a typical `list(kind=task, limit=10)` response is 8KB of which roughly
half is whitespace, dashes, empty arrays, and timestamps the agent will never
parse.

Agents are token-budgeted; humans want pretty output for inspection; tooling
(scripts, dashboards) wants the full schema. These three audiences want
different shapes of the same data. v1 forces handlers to pick one — and they
picked the full shape, so agents pay the verbose cost on every call.

Ocean's directive (2026-05-23):

> verb should include a handler for verbose output, aka if not verbose output,
> we will normalize the data into agent friendly manner, like short datetime
> instead of full iso, short id instead of full uuid, drop empty fields from
> appearing in response...etc the handlers themselves still need to give full
> output, it is the actual end user that will require different output
> presentations.

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
    /// Full canonical shape. Default for `kkernel call` and CI / scripted callers.
    Verbose,
    /// Pretty-printed terminal output. Default for `khive` CLI.
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

| Caller surface               | Default mode | Override                                                     |
| ---------------------------- | ------------ | ------------------------------------------------------------ |
| MCP (`request` tool)         | `Agent`      | envelope-level `presentation_per_op` array (see §Wire shape) |
| `kkernel call <pack> <verb>` | `Verbose`    | `--presentation agent` or `--presentation human` flag        |
| `khive` CLI                  | `Human`      | `--json` for `Agent`, `--verbose` for `Verbose`              |
| HTTP gateway (future)        | `Agent`      | `?presentation=verbose` query parameter                      |

The presentation argument is parsed by the runtime envelope, not by the
handler. Handlers MUST NOT inspect or branch on the mode.

### 3. Transformation rules

#### `Agent` mode (token-efficient)

| Field type                           | Verbose form                                        | Agent form                                                                                                                                              |
| ------------------------------------ | --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| UUID                                 | `"a1b2c3d4-e5f6-7890-abcd-ef1234567890"` (36 chars) | `"a1b2c3d4"` (8 chars — first segment)                                                                                                                  |
| Timestamp (ISO-8601)                 | `"2026-05-23T16:18:15.234567Z"` (27 chars)          | `"2026-05-23T16:18"` (16 chars — minute granularity) OR relative `"3m ago"` if < 24h (sampled once per `present_response()` call — see §Implementation) |
| Empty string `""`                    | included                                            | dropped                                                                                                                                                 |
| Empty array `[]`                     | included                                            | dropped                                                                                                                                                 |
| Empty object `{}`                    | included                                            | dropped                                                                                                                                                 |
| `null` field                         | included                                            | dropped (except lifecycle markers — see below)                                                                                                          |
| Nested object with all empties       | included                                            | dropped                                                                                                                                                 |
| Repeated field with `null` entries   | included                                            | empties filtered out                                                                                                                                    |
| Score fields (see §Score truncation) | `0.1234567890`                                      | `0.123` (3-significant-digit truncation)                                                                                                                |

**Drop semantics — lifecycle `null` preservation:** Drop `[]`, `{}`, and `""`.
Do NOT drop `null` for fields whose absence carries lifecycle meaning. The
following field names are preserved as `null` in Agent mode regardless of other
rules:

- `completed_at`, `deleted_at`, `due_at`, `read_at`, `started_at`,
  `superseded_at`, `applied_at`, `withdrawn_at`, `reviewed_at`
  (all `*_at` lifecycle markers)
- `parent_id`, `superseded_by`, `replaced_by` (relationship markers)

Other `null` fields ARE dropped. Pack authors can declare additional
preserved-null fields via `PackPresentationPolicy::preserve_null_fields()`.

**Score truncation:** Agent mode truncates the following field names (and only
these) to 3 significant figures:

- `score`, `score_breakdown.*` (all nested keys), `salience`, `decay_factor`,
  `rrf_score`, `similarity`, `cross_encoder_score`, `graph_proximity_score`

All other `f32`/`f64` fields (e.g., `weight` on edges, future numeric attrs)
pass through canonical. Pack authors declare additional truncated fields via
`PackPresentationPolicy::truncated_score_fields()`. Type-based truncation is
forbidden.

Short UUIDs in Agent mode echo back the same form the caller could pass on
input (ADR-016 short-UUID-prefix resolution) — the canonical form is also
included as `full_id` if the caller needs disambiguation, but is NOT included
by default.

### `include_full_id` override (envelope-level)

Independent of `PresentationMode`, callers may pass `include_full_id=true` at the
envelope level (alongside `presentation`) to force full UUIDs in the response even
under `PresentationMode::Agent` (which normally returns 8-char shortcodes). This is
a separate axis from presentation mode and does NOT create a fourth mode.

> **Note (ADR-078 §7.1)**: ADR-078 makes `full_id` suppression explicit for `format=auto` and
> `format=table`. In those formats, `full_id` is omitted regardless of `PresentationMode`. It is
> retained in `format=json`, in `PresentationMode::Verbose`, and when `include_full_id=true` is
> set. This resolves the discrepancy between this ADR's stated "NOT included by default" intent
> and the P-C1 code rule in `presentation.rs` that was keeping `full_id` unconditionally.

Score truncation preserves ordering (3 sig figs is enough to compare scores)
without burning tokens on float noise.

#### `Verbose` mode (canonical)

No transformation. The handler's return value is serialized as-is. This is the
shape that round-trips through CI / scripted callers without surprises.

#### `Human` mode (pretty-printed terminal)

**MCP/runtime boundary: `Human` is a no-op at this layer.**

When `presentation=human` is sent over the MCP wire or `kkernel call`, the
runtime returns canonical (verbose) JSON — identical to `Verbose`. No
transformation is applied inside `khive-runtime::presentation`. This is a
deliberate design decision, not an omission:

1. MCP responses are consumed over a JSON transport. Injecting ANSI escape
   codes, table-layout whitespace, or terminal glyphs into JSON would corrupt
   the response for every non-terminal consumer.
2. The `khive` CLI applies its own second-pass formatting after receiving
   verbose JSON from the runtime. The CLI does NOT pass `presentation=human`
   over MCP; it uses `presentation=verbose` (or the default agent mode) and
   applies the terminal transform in `khive-cli::format::pretty` before printing.

**Consequence for callers**: agents or scripts that pass `presentation=human`
receive verbose JSON. This is documented behavior. The table below describes
what the CLI layer produces for human-facing output after its own formatting
pass, but that transform lives at the CLI level — not in the runtime.

| Field type   | CLI Human form (post-MCP, CLI layer)                                 |
| ------------ | -------------------------------------------------------------------- |
| UUID         | First-segment short form, dimmed in terminal color                   |
| Timestamp    | Relative ("3 minutes ago") for recent, absolute date for old         |
| Empty fields | Dropped (same as Agent)                                              |
| Boolean      | `✓` / `✗` glyphs (only if TTY)                                       |
| Score        | Bar visualization or rounded number                                  |
| Long strings | Truncated to terminal width with ellipsis; full text via `--verbose` |

Human mode terminal formatting is delegated to `khive-cli::format::pretty` —
this ADR specifies its existence but not the exact formatting rules (those
evolve with the CLI UX).

### 3.5. Error envelopes are never transformed

**Error envelopes are NEVER transformed.** When a verb returns
`{ok: false, tool: "...", error: "...", aborted: bool}`, the envelope passes
through canonical regardless of `PresentationMode`. Error strings — including
UUIDs in error messages — remain full-form for debugging. The transform applies
only to the `result` field of successful envelopes
(`{ok: true, tool: "...", result: <transformed>}`).

### 4. Where the transformation runs

**Chain `$prev` substitution operates on canonical (verbose) handler output.**
The `Present` transform runs AFTER the entire request batch — including all
`$prev` chain substitutions — completes, at the response-envelope boundary.
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
│       (skipped for error envelopes — §3.5)               │
│    3. Serialize to wire JSON                             │
└─────────────────────────────────────────────────────────┘
                            ↓
                       Wire JSON
```

The transformation runs once, after the handler returns, before serialization.
Implementation lives in `khive-runtime::presentation`:

```rust
pub trait Present {
    fn present(value: serde_json::Value, mode: PresentationMode) -> serde_json::Value;
}

pub fn present_response(
    response: serde_json::Value,
    mode: PresentationMode,
    now_unix_seconds: i64,
) -> serde_json::Value;
```

Pack handlers are unaware of mode. Tests against handler outputs always check
verbose shape — golden outputs don't need to be mode-aware.

### 5. Handler invariants

Handlers MUST:

- Return canonical verbose-shape JSON regardless of caller mode
- Include every field declared in the verb's response schema (use `null` for
  missing, not omit) — the presentation layer trims, not the handler
- Use full ISO-8601 timestamps
- Use full canonical UUIDs

Handlers MUST NOT:

- Inspect the request envelope for `presentation` field
- Branch behavior on caller identity
- Return different schemas for different callers
- Pre-truncate or pre-shorten anything that the presentation layer should handle

Pre-truncation by the handler is a particularly common temptation ("the agent
won't read past 10KB anyway, let me cap the result here"). It's wrong — the
verbose / scripted caller wants the full data, and the agent's truncation
belongs in the presentation layer where it can be tuned per-deployment.

### 6. Per-verb opt-out (escape hatch)

Some verbs return data where Agent-mode trimming is wrong — e.g., a verb that
explicitly returns "the full canonical UUID of X for downstream use" wants the
full form. Verbs declare:

```rust
pub trait VerbHandler {
    /// Default: VerbPresentationPolicy::Standard.
    /// Override to ::AlwaysVerbose for verbs whose semantics demand full output.
    fn presentation_policy(&self) -> VerbPresentationPolicy {
        VerbPresentationPolicy::Standard
    }
}

pub enum VerbPresentationPolicy {
    Standard,
    AlwaysVerbose,
    // future: AlwaysHuman, AlwaysAgent for verb-specific overrides
}
```

The following verbs are declared `AlwaysVerbose`. Treat omission from this
table for a NEW pack verb as a CI failure during pack registration.

| Verb                     | Default policy | Rationale                                                     |
| ------------------------ | -------------- | ------------------------------------------------------------- |
| `get`                    | AlwaysVerbose  | Caller needs full UUID to chain into other ops                |
| `query`                  | AlwaysVerbose  | User-projected fields; transform on user projections is wrong |
| `traverse`               | AlwaysVerbose  | Path UUIDs needed for follow-up                               |
| `neighbors`              | AlwaysVerbose  | Same as traverse                                              |
| `link` (create response) | AlwaysVerbose  | Edge IDs needed for follow-up                                 |
| `kg.export` (future)     | AlwaysVerbose  | Byte-fidelity for snapshots                                   |
| `kg.snapshot` (future)   | AlwaysVerbose  | Same                                                          |
| `kg.commit` (future)     | AlwaysVerbose  | Same                                                          |
| Everything else          | Standard       | Apply per-mode transform                                      |

The declaration lives in the pack's verb registration, not the handler body —
it's metadata, not logic.

### 7. Examples

#### `list(kind=task, limit=2)` in three modes

**Verbose (handler return):**

```json
{
  "ok": true,
  "tool": "list",
  "result": {
    "items": [
      {
        "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "kind": "task",
        "title": "Draft ADR-045",
        "status": "next",
        "priority": "p1",
        "assignee": "lambda:khive",
        "created_at": "2026-05-23T16:00:00.000000Z",
        "updated_at": "2026-05-23T16:18:15.234567Z",
        "completed_at": null,
        "due_at": null,
        "tags": [],
        "dependencies": [],
        "result": null,
        "namespace": "lambda:khive"
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
        "kind": "task",
        "title": "Draft ADR-045",
        "status": "next",
        "priority": "p1",
        "assignee": "lambda:khive",
        "created_at": "2026-05-23T16:00",
        "updated_at": "3m ago",
        "completed_at": null,
        "due_at": null,
        "namespace": "lambda:khive"
      },
      {/* ... */}
    ],
    "total": 2
  }
}
```

Note: `completed_at` and `due_at` are preserved as `null` (lifecycle markers —
§3 Drop semantics). `tags`, `dependencies`, `result`, and `page_cursor` are
dropped (`[]`, `[]`, `null` non-lifecycle, `null` non-lifecycle respectively).

On synthetic 10-item task listings with full timestamps and UUIDs, the Agent
transform reduced response JSON byte length by ~55–60%. On smaller responses
(single record, few fields), savings are proportionally lower (~20–30%).
Benchmark in `tests/presentation_savings.rs` (to be added).

**Human (terminal):**

```
ID        STATUS  PRIORITY  TITLE              UPDATED
a1b2c3d4  ▶ next  p1        Draft ADR-045      3m ago
...
2 tasks
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
   verbose canonical shape — it would be broken if mid-chain a `presentation`
   transform stripped fields the next op needs.

### Why short-UUID first 8 chars (not 12, not 16)

ADR-016's short-UUID-prefix resolution requires 8+ hex chars. The output side
matches the input side — agents that copy a `"a1b2c3d4"` back into a verb call
get the same record. Longer prefixes (12, 16) add tokens without strengthening
the round-trip contract.

### Why "3m ago" (not always absolute timestamps)

Two reasons:

1. **Token cost**: "3m ago" is 7 chars; `"2026-05-23T16:15:32.123Z"` is 24
   chars. Multiplied across timestamp-heavy responses (task lists, event logs)
   the saving is real.
2. **Agent affordance**: agents reason about recency much more than absolute
   time. "Is this recent?" is a faster decision from "3m ago" than from a
   precise timestamp.

Verbose mode preserves absolute time for tooling that needs to compare
timestamps across systems.

### Why empty-field dropping (with lifecycle-null preservation)

Empty arrays and empty objects burn tokens like empty strings do. Aggressively
dropping empty-meaningful structures (a `tags: []` field on a task with no
tags is meaningful absence, but the agent doesn't need to _see_ the meaningful
absence — it can infer from the field's absence).

The risk is ambiguity: did the field not exist, or was it empty? For agent
mode this is acceptable because the verb's response schema is documented
(ADR-017 §pack manifest declarations) — the agent knows the field exists, it
just isn't populated.

However, blanket `null`-dropping is wrong for lifecycle-marker fields.
`completed_at: null` means "not done" in GTD; dropping it makes the record
indistinguishable from one where `completed_at` was never defined. The
preserve-null allowlist (§3 Agent table) resolves this: lifecycle `*_at`
fields and relationship markers pass through as `null`; purely optional
informational nulls are dropped.

### Why per-verb opt-out

Some verbs return UUIDs or timestamps as the _primary product_ (`kg.snapshot`
returns a `snapshot_id`; `event.created` returns a precise `created_at`).
Trimming these would damage the verb's contract. The opt-out is rare but
named.

### Why not introduce a fourth "minimal" mode

Three modes are enough: tools (verbose), agents (agent), humans (human).
Adding more invites bikeshedding without clear use cases.

## Alternatives Considered

| Alternative                                                            | Why rejected                                                                             |
| ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Handler-side rendering                                                 | Couples handlers to presentation logic; tests grow combinatorially                       |
| Always-minimal output                                                  | Breaks scripted callers that round-trip the response                                     |
| Mode declared once per session, not per call                           | Some workflows mix verbose and agent calls; per-call control is the safer default        |
| Use Accept headers (HTTP-style)                                        | MCP doesn't have headers; introducing them for one ADR's worth of feature is overkill    |
| Truncate to top-N keys based on size budget                            | Brittle; the same call from two agents would return different shapes — non-deterministic |
| Strip empties only at MCP boundary, leave canonical for `kkernel call` | Already the design — `kkernel call` defaults to verbose                                  |

## Consequences

### Positive

- Agent token budget cuts ~20–60% on list-heavy responses (payload-shape dependent) without losing semantic content.
- Handler tests stay simple — one golden output per verb, not three.
- CLI gets pretty output for free via the `Human` mode dispatch.
- Future presentation needs (e.g., a "summary" mode that bullets a long
  document) plug into the same Present trait.

### Negative

- New surface (`PresentationMode`, `Present` trait, per-verb policy) — three
  more concepts in the pack-author mental model. Mitigated by handler-side
  invariance: most pack authors never see this.
- Agent mode's "3m ago" is locale-dependent if khive ever ships i18n. v1 ships
  English-only relative-time formatting; i18n is a separate ADR if needed.

### Neutral

- The transformation runs per-response; CPU cost is negligible (microseconds
  on KB-sized JSON).
- `present_response()` is pure — easy to fuzz-test for round-trip invariants
  (e.g., `present(verbose) == verbose`, `present(agent).fields ⊆ verbose.fields`).

## Implementation

### Crate placement

- `PresentationMode` enum + `Present` trait: `khive-runtime::presentation`
- Per-verb policy: stored on `HandlerDef` (ADR-017/023) in `khive-runtime::registry`
- Transformation logic: `khive-runtime::presentation::transform`
- MCP `request` arg parsing for `presentation` / `presentation_per_op`:
  `khive-mcp::server`
- CLI flag parsing: `khive-cli::common`

### `now` frozen-time semantics

`now: i64` (Unix seconds) is sampled ONCE per `present_response()` call and
passed as a parameter through the transform tree. All relative datetime
renderings within a response use the same `now`. The `Present` trait signature
is:

```rust
pub fn present(value: Value, mode: PresentationMode, now_unix_seconds: i64) -> Value
```

Tests inject a fixed clock for deterministic replay. Same input + same `now`
→ identical output bytes.

### Chain `$prev` invariant

**`present(chain_intermediate)` is never observed.** The `Present` transform
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
  "ops": "[list(kind=task), get(id=a1b2c3d4)]",
  "presentation": "agent",
  "presentation_per_op": ["verbose", "agent"]
}
```

`presentation` is the batch default. `presentation_per_op` (optional) overrides
per-op by index. The argument name `presentation` is RESERVED at the
request-envelope level and CANNOT be used as a verb argument name.

Passing `presentation` inside the function-call args (e.g.,
`list(kind=task, presentation=agent)`) is REJECTED as a parse error — it
collides with the reserved envelope key. Default is `agent` for MCP.

### Migration

No schema migration. Handlers retain their canonical shape. The transformation
layer is purely additive.

**Migration policy:** Agent mode ships as the default for MCP/stdio responses
IN this release. An escape hatch `KHIVE_DEFAULT_PRESENTATION=verbose` is
available for one minor version (v0.2.x). The escape hatch is removed in v0.3.
Verbose remains the default for `kkernel call` and library callers.

Agents that previously parsed against full-shape MCP responses must either
migrate to Agent-mode shapes or set `presentation=verbose` per call (or
`KHIVE_DEFAULT_PRESENTATION=verbose` globally during the transition window).

## References

- ADR-016 (Request DSL) §"UUID arguments" — short-prefix resolution on input
  (this ADR's output-side counterpart)
- ADR-017 (Pack Standard) — verb declaration, handler return shape
- ADR-016 (Request DSL) — single-tool `request` envelope shape
- Ocean directive 2026-05-23 — "verb should include a handler for verbose output…"
