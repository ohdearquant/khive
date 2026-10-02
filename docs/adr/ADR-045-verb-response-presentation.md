# ADR-045: Verb Response Presentation Modes

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
**Extended by**: ADR-078 (Output Format and Shape-Aware Rendering), which introduces an orthogonal
`format` axis (`json` / `auto` / `table`) and revises Agent-mode redundancy rules. ADR-078
§7.1 partially supersedes the P-C1 implementation behavior that kept `full_id` present in all
modes; see Amendment 1 below.
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

Design request (2026-05-23):

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
    /// Full canonical shape. Default for `kkernel exec` and CI / scripted callers.
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

| Caller surface                      | Default mode | Override                                                     |
| ----------------------------------- | ------------ | ------------------------------------------------------------ |
| MCP (`request` tool)                | `Agent`      | envelope-level `presentation_per_op` array (see §Wire shape) |
| `kkernel exec '<pack>.<verb>(...)'` | `Verbose`    | `--presentation agent` or `--presentation human` flag        |
| `khive` CLI                         | `Human`      | `--json` for `Agent`, `--verbose` for `Verbose`              |
| HTTP gateway (future)               | `Agent`      | `?presentation=verbose` query parameter                      |

The presentation argument is parsed by the runtime envelope, not by the
handler. Handlers MUST NOT inspect or branch on the mode.

### 3. Transformation rules

#### `Agent` mode (token-efficient)

| Field type                           | Verbose form                                        | Agent form                                                                                                                                              |
| ------------------------------------ | --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| UUID                                 | `"a1b2c3d4-e5f6-7890-abcd-ef1234567890"` (36 chars) | `"a1b2c3d4"` (8 chars — first segment), except strict round-trip fields remain canonical                                                                |
| Timestamp (ISO-8601)                 | `"2026-05-23T16:18:15.234567Z"` (27 chars)          | `"2026-05-23T16:18"` (16 chars — minute granularity) OR relative `"3m ago"` if < 24h (sampled once per `present_response()` call — see §Implementation) |
| Empty string `""`                    | included                                            | dropped, except under a record's `properties` object (Amendment 3)                                                                                      |
| Empty array `[]`                     | included                                            | dropped                                                                                                                                                 |
| Empty object `{}`                    | included                                            | dropped                                                                                                                                                 |
| `null` field                         | included                                            | dropped (except lifecycle markers — see below)                                                                                                          |
| Nested object with all empties       | included                                            | dropped                                                                                                                                                 |
| Repeated field with `null` entries   | included                                            | empties filtered out                                                                                                                                    |
| Score fields (see §Score truncation) | `0.1234567890`                                      | `0.123` (3-significant-digit truncation)                                                                                                                |

**Timestamp row superseded:** [Amendment 9](#amendment-9-2026-09-29-exact-agent-timestamps)
replaces the Agent form in the timestamp row above.

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

Short UUIDs in Agent mode echo back the same form the corresponding parameter
accepts on input (ADR-016 short-prefix resolution). A field consumed by a
full-UUID-only parameter is never shortened. Field-level exceptions keep
`context_entity_id`, `thread_id`, `outbound_ref`, `parent_id`, `session_id`, and
`project_id` canonical at every nesting level; verb-level `AlwaysVerbose` policy keeps
`memory.feedback.target_id` and `comm.delivered.id` canonical. The canonical
record identifier is also included as `full_id` if the caller needs
disambiguation, but is NOT included by default.

### Canonical-ID retention

ADR-078 makes `full_id` suppression explicit for `format=auto` and `format=table`.
The canonical field is retained by the lossless `format=json` default and by
`PresentationMode::Verbose`; callers that require it must select one of those two
implemented paths. Agent presentation continues to shorten the ordinary `id` field.

Score truncation preserves ordering (3 sig figs is enough to compare scores)
without burning tokens on float noise.

#### `Verbose` mode (canonical)

No transformation. The handler's return value is serialized as-is. This is the
shape that round-trips through CI / scripted callers without surprises.

#### `Human` mode (pretty-printed terminal)

**MCP/runtime boundary: `Human` is a no-op at this layer.**

When `presentation=human` is sent over the MCP wire or `kkernel exec`, the
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

ADR-016's optional envelope-level `advisories` array is outside the handler
result and therefore outside every presentation and output-format transform.
The runtime may add that transport-owned array after result presentation; its
objects remain byte-for-byte machine-readable in Agent, Verbose, and Human
modes. This preserves the central invariant here: presentation changes only a
successful envelope's `result`, never sibling envelope metadata.

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
| `brain.feedback`         | AlwaysVerbose  | Feedback target acknowledgement must remain chainable         |
| `memory.feedback`        | AlwaysVerbose  | Exact feedback target acknowledgement must remain canonical   |
| `comm.delivered`         | AlwaysVerbose  | `id` is an exact outbound correlation key                     |
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
        "assignee": "agent:docs",
        "created_at": "2026-05-23T16:00:00.000000Z",
        "updated_at": "2026-05-23T16:18:15.234567Z",
        "completed_at": null,
        "due_at": null,
        "tags": [],
        "dependencies": [],
        "result": null,
        "namespace": "agent:docs"
      },
      {/* ... */}
    ],
    "requested_limit": 2,
    "effective_limit": 2,
    "limit_clamped": false
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
        "assignee": "agent:docs",
        "created_at": "2026-05-23T16:00",
        "updated_at": "3m ago",
        "completed_at": null,
        "due_at": null,
        "namespace": "agent:docs"
      },
      {/* ... */}
    ],
    "requested_limit": 2,
    "effective_limit": 2,
    "limit_clamped": false
  }
}
```

Note: `completed_at` and `due_at` are preserved as `null` (lifecycle markers —
§3 Drop semantics). `tags`, `dependencies`, and `result` are dropped (`[]`,
`[]`, and non-lifecycle `null`, respectively).

#### Amendment 1 (2026-08-08): stable list envelopes are structural

ADR-023's stable pagination envelope is an exception to the generic empty/null
drop rule. Agent mode MUST retain offset-mode `items` even when it is `[]`, and
MUST retain `requested_limit`, `effective_limit`, and `limit_clamped`. Entity,
note, and edge cursor pages similarly retain their substrate-specific
`entities`/`notes`/`edges` array and retain `next_after` even when it is `null`,
along with the same three limit fields. Row fields inside those arrays continue
to receive the ordinary Agent transform.

This exception is envelope-scoped: an unrelated response object containing an
empty field named `items`, `entities`, `notes`, or `edges` still drops it. The
three limit-metadata fields identify a list envelope at the presentation
boundary.

[Amendment 5 (2026-09-14)](#amendment-5-2026-09-14-structural-knowledge-limit-envelopes)
explicitly applies this rule to the new knowledge-list/topic `results` envelopes
on acceptance, while retaining ordinary row transforms.

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

ADR-016's short-UUID-prefix resolution requires 8+ hex chars. For parameters
that accept prefix resolution, the output side matches the input side within
that parameter's declared resolution scope — agents that copy a `"a1b2c3d4"`
back into a verb call get the same record when the prefix remains unique in
that scope. Strict fields stay full because shortening them would break,
rather than strengthen, the round-trip contract.

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

Amendment 9 withdraws this rationale for Agent mode: Agent timestamps are exact, and list rows
carry the relative form beside `created_at`.

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
| Strip empties only at MCP boundary, leave canonical for `kkernel exec` | Already the design — `kkernel exec` defaults to verbose                                  |

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
Verbose remains the default for `kkernel exec` and library callers.

Agents that previously parsed against full-shape MCP responses must either
migrate to Agent-mode shapes or set `presentation=verbose` per call (or
`KHIVE_DEFAULT_PRESENTATION=verbose` globally during the transition window).

## Amendment 1 (2026-08-09): close the request envelope and withdraw the phantom override

The original canonical-ID section described an envelope-level `include_full_id=true`
override that was never implemented in `RequestParams`, daemon forwarding, or the
renderer. That unshipped field is withdrawn rather than added as a third rendering
axis. Canonical IDs remain available through the two implemented mechanisms above:
the lossless `format=json` default and `presentation=verbose`.

The MCP `request` envelope is now closed to undeclared fields. Its serde decoder and
generated JSON Schema reject names outside the published `RequestParams` properties,
so a misspelling such as `presentaton` fails at the call boundary instead of silently
falling back to the default presentation. This closure applies only to the outer MCP
tool envelope; verb arguments continue through the separately governed request DSL and
pack validation seams.

## Amendment 2 (2026-08-14): both envelope-owned names are reserved

The wire-shape section above reserves the argument name `presentation` but is silent on
`presentation_per_op`, even though both fields are owned by the request envelope. The
implementation reserves both: the shared list `khive-types::pack::RESERVED_ENVELOPE_ARGS`
enumerates `presentation` and `presentation_per_op`, the DSL parser rejects either name as
a verb argument, and the runtime registry refuses at boot any verb or subhandler metadata
that advertises either name. This amendment aligns the normative text with that contract:
**both `presentation` and `presentation_per_op` are RESERVED at the request-envelope level
and CANNOT be used as verb argument names.** The reserved-name list is a closed set owned
by this ADR; adding a third envelope field reserves its name here first.

## Amendment 3 (2026-08-24): caller-owned property strings survive agent-mode empty-drop

The agent-mode economy table above drops the empty string unconditionally.
That rule conflates two different kinds of emptiness. Presentation filler —
a handler field that happens to be empty — carries no information and is
correctly dropped. A key under a record's `properties` object exists only
because a caller wrote it, so an empty string there is data: it is what
distinguishes "this property was set to empty" from "this property is absent
or was deleted". Under the unamended rule, the echo of an update that sets a
property to `""` omits the key entirely, and the caller reads a successful
write as a deletion (issue #1995).

**The rule as amended: in Agent mode, empty strings nested under a record's
`properties` object are preserved; the empty-string drop continues to apply
everywhere else.** The token-economy rationale is unaffected — these keys
appear only when a caller wrote them, so nothing machine-generated is
reintroduced. This amendment changes the normative rule; the
presentation-layer implementation change is tracked by issue #1995 and lands
separately, citing this amendment.

Scope note, stated rather than implied: empty arrays and empty objects under
`properties` are still dropped. They have the same set-versus-absent
ambiguity in principle; a caller that needs to distinguish those cases reads
`presentation=verbose`, which remains lossless. `format` alone does not
restore them: the presentation transform runs before format rendering, so
`format=json` under the default Agent presentation serializes the
already-transformed value. Extending the carve-out to container values is a
separate decision that amends this paragraph.

## Amendment 4 (2026-09-03): keyset cursor envelopes are structural

The stable-list-envelope exception (section 7, Amendment 1) extends to keyset
cursor pages that are not ADR-023 envelopes. A response object carrying a
`next_after` key beside a `results` array — the
`knowledge.list(after=…)` page: `results`, `limit`, `order`, `next_after` — is a
cursor envelope. Agent mode retains `results` when it is `[]` and `next_after`
when it is `null`, because an empty page and a null cursor are the walk's
completion signals and a caller that cannot see them cannot terminate. The
exception is envelope-scoped in the same way: a `results` array without a
sibling `next_after` key receives the ordinary transform.

That last sentence is qualified by
[Amendment 5 (2026-09-14)](#amendment-5-2026-09-14-structural-knowledge-limit-envelopes):
the new knowledge-list/topic report siblings also identify a structural envelope
without a cursor. Existing keyset completion behavior does not change.

## References

- ADR-016 (Request DSL) §"UUID arguments" — short-prefix resolution on input
  (this ADR's output-side counterpart)
- ADR-017 (Pack Standard) — verb declaration, handler return shape
- ADR-016 (Request DSL) — single-tool `request` envelope shape
- Design request 2026-05-23 — "verb should include a handler for verbose output…"

## Amendment 5 (2026-09-14): structural knowledge limit envelopes

**Status: Accepted (2026-09-14).**
**Related issue:** #2679.

This amendment accompanies
[ADR-047's 2026-09-14 knowledge list and topic limit reports](ADR-047-knowledge-pack.md#amendment-2026-09-14-knowledge-list-and-topic-limit-reports).
It proposes the observable empty-result consequence of those two verbs adding
`requested_limit`, `effective_limit`, and `limit_clamped` beside `results`.
Numeric-report approval alone does not accept this presentation change. Both
proposals require owner/spec approval before dependent implementation merges;
only the new amendments become Accepted with the later fix.

### Scope of the structural envelope

On acceptance, §7's **Amendment 1 (2026-08-08)** stable-envelope rule explicitly
includes all four `knowledge.list` successes (atom/domain × offset/cursor) and
both `knowledge.topic` successes (query/listing) carrying the three report
siblings. Agent mode must retain the siblings and envelope `results` even when
it is `[]`. This newly retains empty offset-list/topic results previously
removed by ordinary empty-field dropping.

This amendment also qualifies **Amendment 4 (2026-09-03)**'s final sentence:
absence of `next_after` does not imply an ordinary transform when those three
report siblings identify a knowledge limit envelope. A results array without
either structural discriminator still follows the existing ordinary transform.
Knowledge-list cursor pages already retain empty results and `next_after:null`;
this proposal preserves that completion contract without adding cursor fields
to offset pages or to topic.

The exception is scoped to the envelope. Fields within records retain all
existing UUID, timestamp, score and empty-field transformations. No new
per-verb rendering exception, response wrapper or renderer change is required.
Adding report siblings to an envelope does not make an ordinary record or its
properties structural; each object keeps its existing classification.

### Presentation and format behavior

The rule applies before output-format rendering. For each presentation mode
Agent, Verbose and Human, preserve the existing behavior of each format `json`,
`auto` and `table`. Verbose/Human canonical payloads already include empty
results; Agent now retains the scoped empty results in all three formats.
Existing zero/one-row JSON fallback, multi-row tables, sibling scalar rendering,
full_id redundancy policy, field order policy and ordinary record transforms
continue to govern. `format` does not undo the presentation transform.

For example, topic listing with two matching concepts and limit 0 yields the
canonical `{results:[],total:2,requested_limit:0,effective_limit:0,
limit_clamped:false}`. Agent preserves the empty array and report; `total` is
still 2, not the output length. A topic query with effective 0 instead retains its
existing total 0. For an empty list cursor with requested 501, results and
`next_after:null` remain present as before, alongside legacy limit 500 and the
new `501 / 500 / true` report.

Removing the new siblings from a nonempty result must leave the same legacy
payload under the same existing presentation/format rules. Empty offset-list
and topic Agent results have exactly one further allowed structural difference:
`results: []` is now present. Existing empty cursor completion is unchanged.
This contract does not require rendered-byte identity after additive fields.

### Acceptance and mutation witnesses

- **KP-MATRIX:** Exercise all nine presentation × format pairs on zero-, one-
  and multi-row responses, including false/zero report values, full_id, local
  namespace, row order and null cursor. Check complete old row/payload behavior
  as well as new siblings; distinguish empty offset/topic retention from
  already-retained cursor completion.
- **KP-WIRE:** A real MCP request must retain empty offset-list and topic
  results and their three report siblings in Agent output. A populated route
  control must precede each empty-route assertion. A separate ordinary result
  fixture without either structural discriminator must still drop an empty
  results array under the unchanged renderer.
- **KP-MISSING-REPORT:** Independently omit one report sibling from an empty
  atom-offset response and rename one from an empty topic-listing response.
  The intended metadata/empty-array assertion must fail while baseline route
  controls pass. These mutations affect handler output only; the renderer and
  its row, identifier, namespace and cursor-completion controls stay unchanged.

Witnesses and individual mutants are named and frozen before running them.
Baseline controls establish existing transformations before an expected failure
on missing new metadata or the proposed new empty-array retention. Fixed runs
must satisfy the unchanged controls and the new assertions. Zero selected
tests, compilation failures or invalid fixtures are not acceptance or mutant
kills. This proposed text records no executed result.

## Amendment 6 (2026-09-14): exact stream receipt timestamps

**Status: Accepted.**
**Related issue:** #2537.

This amendment extends §6's declaration-based presentation policy and qualifies
§3's Agent timestamp-compaction rule for the closed stream-receipt paths below.
It does not change canonical handler output or the transaction that produces a
receipt. The existing `Standard` and `AlwaysVerbose` policies remain intact.

This is a presentation clarification of the write-receipt value described in
[ADR-174 Amendment 5 §A5.2](ADR-174-ordered-streams-append.md#a52-updated_at-on-write-member-results).

### Closed receipt policies

The registered `HandlerDef::presentation_policy()` declaration supplies two
finite policies: `StreamAppendReceipt` for `stream.append`, and
`StreamBatchReceipts` for `stream.batch`. The response boundary must select the
policy using the actual dispatched verb's registered definition. These policy
variants are the explicit declaration that a result contains the named receipt
values; they are not fields added to the result JSON.

Under Agent presentation, preserve strings byte-for-byte at exactly these
paths, relative to the canonical successful `result`:

| Registered verb | Policy                | Protected string path                          |
| --------------- | --------------------- | ---------------------------------------------- |
| `stream.append` | `StreamAppendReceipt` | Root `/created_at`                             |
| `stream.batch`  | `StreamBatchReceipts` | `/results/<immediate array member>/created_at` |
| `stream.batch`  | `StreamBatchReceipts` | `/results/<immediate array member>/updated_at` |

The append fields retain the appended record's canonical creation time. The
write field retains that write's own stored update time for both create and
update members, in atomic and per-member batches. In particular, the boundary
must not obtain a replacement timestamp by reading the record after the write:
a later writer cannot change the value already captured in the receipt.

The path rule preserves the original string bytes, including fractional
precision; it does not parse, validate, normalize or reconstruct timestamps.
Non-string values continue through the existing presentation rules. The
canonical producers retain responsibility for valid receipt values.

Only an object at the result root can match the append path. The batch paths
require a root object whose `results` value is an array, and apply only to the
named fields directly on its immediate object members. A root array, an
object-valued `results`, an array nested within a member, or fields inside
`details`, `record` or another descendant do not acquire receipt protection.
Existing independent payload guards still apply at their own paths.

No field name, response shape, embedded `tool: "stream.batch"` value, or
caller-supplied marker may select a receipt policy. An unrelated Standard verb
returning a lookalike result remains Standard. No request parameter or result
schema field is added. Any additional producer, protected path or policy
requires a separate declared contract and its own acceptance witnesses; this
amendment does not grant arbitrary handlers a general path-exemption facility.

### Coexistence with ordinary presentation

Receipt protection does not make either stream writer `AlwaysVerbose`. Agent
UUID shortening, score handling, empty/null treatment and ordinary metadata
timestamp compaction remain active outside the named string paths. A single
MCP response can therefore contain an exact write-receipt timestamp and compact
metadata from a subsequent `list` of the same record.

The existing `trigger_at` and `due` timestamp exceptions, object-valued
`properties` protection, opaque stream-entry `record` protection, strict UUID
fields, AlwaysVerbose verbs and structural list/cursor envelopes are unchanged.
In particular, the outer `created_at` on a `stream.read` entry remains ordinary
read metadata; protecting the caller's `record` does not protect that sibling.
The knowledge limit envelopes specified by the preceding amendment keep their
own structural rules and ordinary row transformations.

Verbose and Human remain canonical at the MCP/runtime presentation boundary.
The public generic `present(value, mode, now)` behavior remains Standard;
receipt-aware presentation receives the trusted declaration separately. Pack
handlers and direct runtime/registry callers continue to return canonical JSON
without inspecting the caller's mode or injecting presentation markers.

Raw MCP `RequestParams.presentation=None` selects Agent. In ordinary
`kkernel exec` dispatch, omitting the CLI flag is different: `ExecArgs` supplies
`Some("verbose")` before forwarding to the local or daemon MCP path. Explicit
Agent selects the receipt policies on those paths; explicit Verbose preserves
canonical output. Existing envelope and per-operation override precedence is
unchanged. A local MCP test representing the omitted CLI flag must therefore
pass `Some("verbose")`; raw `None` is an Agent test, not a CLI-default test.

### Response boundary, chaining and formats

Apply the same policy on successful single, parallel and serial-chain response
paths. `$prev` substitution continues to consume the canonical intermediate
result, before any visible presentation transform. No policy marker or shortened
value may enter that canonical chain state.

Whole-operation error envelopes remain untransformed. An error nested inside a
successful per-member batch retains its existing presentation behavior; receipt
protection does not descend into its error details. Help and synthetic successes
do not gain invented receipt fields. Gate, result-depth and response-frame
limits remain in force, including their existing refusal and omission rules.
The policy does not require a field to survive omission of its entire result.

Presentation still precedes ADR-078 format rendering. Preserve the existing
`json`, `auto` and `table` reductions, scalar rendering and structured fallback
behavior in every presentation mode. When an in-scope timestamp is displayed,
its complete string must survive; a second Standard Agent pass must not compact
it again. This rule does not promise that a rendered table preserves every field
of canonical JSON, or change the full_id, namespace or field-order policies.

### Census-known follow-up policies

The closed stream-receipt family and its declaration mechanism define this
amendment's bounded #2537 scope. The following result-carried timestamps remain
unchanged and subject to their existing Standard Agent handling:

| Producer             | Result-carried timestamp paths left unchanged                           |
| -------------------- | ----------------------------------------------------------------------- |
| `gtd.complete`       | Root `/completed_at`                                                    |
| `brain.event_counts` | Root `/since` and `/until`                                              |
| `telemetry.counts`   | `/window/since` and `/window/until`, with existing grouped-key handling |

These are result data, not reclassified metadata. The protected
`properties.completed_at` copy on a GTD note does not protect the separate
top-level completion receipt. Follow-up policies for these census-known
surfaces are separate work to be tracked after this amendment merges; no exact
Agent-output guarantee for them is introduced here. Other metadata, daemon
cursor inspection, native serializers, and proposed/unimplemented timestamp
surfaces likewise receive no new policy from this amendment.

### Acceptance and mutation witnesses

- **SR-RECEIPTS:** Real Agent MCP calls cover standalone append, batch append,
  and batch write create/update in both atomic modes. Establish dispatch,
  identity, sequence/version/count/order and commit controls before comparing
  every receipt timestamp with its canonical own-write oracle. A mixed
  write-then-list response must retain compact metadata and ordinary Agent UUIDs.
- **SR-BOUNDARIES:** Exercise single, parallel and serial chaining, including
  canonical `$prev`, mode overrides and all three formats. Raw omission selects
  Agent; the CLI-defaulted local seam uses explicit Verbose. Whole errors,
  per-member errors, help, depth refusal and frame omission retain their controls.
- **SR-CONTEXT:** At a fixed clock, require ordinary metadata's literal compact
  form beside exact receipt strings. Unrelated verb/marker/shape lookalikes,
  malformed container paths and nested descendants receive no receipt policy.
  Existing properties, trigger/due, opaque-record, AlwaysVerbose and structural
  envelope controls remain unchanged. A protected string is retained without
  reparsing; non-string handling is unchanged.
- **SR-MUTATIONS:** Independently remove each of the three protected-path arms;
  add a global `updated_at` exemption; select the batch policy by response shape;
  inherit it into descendants; substitute AlwaysVerbose for the batch policy;
  omit the serial-chain policy; assign presented output to `$prev`; and apply a
  second Standard pass before rendering. Each wrong edit must fail its named
  behavioral witness, including the unchanged controls that distinguish an
  over-broad fix from correct receipt preservation.

Freeze exact selectors, fixtures, source identities and individual mutants
before execution. A valid tests-only baseline reaches the real handlers and
passes its legacy controls before failing on lost receipt precision. Fixed
runs must satisfy both controls and exactness assertions. Compilation failures,
invalid fixtures, missing cases and zero selected tests are neither a baseline
proof nor mutation kills. Verification outcomes belong to the implementing
change; this amendment defines the accepted receipt contract.

## Amendment 7 (2026-09-15): opt-in parsed note content

**Status: Accepted.** [Ratified by the owner on 2026-09-15](https://github.com/ohdearquant/khive/issues/2757#issuecomment-5684308086).
This contract supersedes the earlier raw-string fallback and nested-cell placeholder proposal.

`get` and `list` accept per-call boolean `parse_content`, default false. For note records only,
true parses the stored JSON `content` string into a JSON value in place. JSON objects, arrays,
strings, numbers, booleans, and null are supported. Invalid JSON refuses with `invalid_input`,
naming the note UUID and `content` field, distinct from the existing missing-note refusal.
One invalid returned note refuses its list operation. Omission or false retains byte-identical
existing response behavior and the exact stored content string. Both parameter structs retain
`deny_unknown_fields`; a client probing an older server gets an explicit unknown-field refusal.
No connection state, storage or write contract changes. The option covers get by ID (including
soft-deleted notes) and key, and note lists using substrate/granular kinds, filters, offset,
insertion cursors, or keyed cursors. Non-note get/list responses, including edge annotation
notes, are unchanged.

The option changes only the note's content field, after the existing timestamp/status/label
projection. In opt-in responses, content is opaque user data throughout presentation and output
format preparation: it is not recursively shortened, rounded, compacted, emptied, deduplicated, or
hoisted as record metadata. Parsed null and empty values remain present. The policy comes from the
resolved request option and the actual note result; no response marker or caller-supplied payload
field selects it. Chain substitution observes canonical parsed content, before presentation.

Existing get/list metadata, pagination, request/output envelope keys, per-op presentation/format
precedence, and AlwaysVerbose get behavior remain in force. JSON and AUTO preserve parsed content
values, including on multi-note pages; AUTO uses JSON result text for these opt-in note responses.
TABLE serializes parsed objects and arrays to JSON strings for display only, preserving scalar
values. A get record's parsed content array is never selected as the response's record table.
Existing response depth and size guards still apply.

## Amendment 8 (2026-09-25): the Agent-mode savings figures are unreproduced estimates

**Status: Accepted (2026-09-25).**

### Context

§7 states: "On synthetic 10-item task listings with full timestamps and UUIDs, the
Agent transform reduced response JSON byte length by ~55–60%. On smaller responses (single
record, few fields), savings are proportionally lower (~20–30%). Benchmark in
`tests/presentation_savings.rs` (to be added)." Consequences, Positive, repeats the range as
"Agent token budget cuts ~20–60% on list-heavy responses".

The benchmark was never added. No commit in the repository history touches a path named
`presentation_savings`, and no bench target under `crates/*/benches/` refers to presentation.
The transform lives in `crates/khive-runtime/src/presentation.rs` (`present`,
`present_with_policy`). It has also changed since the figures were written: Amendment 1
(2026-08-08, in §7), Amendment 3, Amendment 4 and Amendment 5 each keep fields that the original
drop rule removed, and ADR-078 added a redundancy-reduction pass that Agent mode also applies
when the output format is prepared (`prepare_format_value`). The figures therefore describe the transform as first designed and cannot be reproduced
from the tree.

### Decision

The percentages in §7 and in Consequences are design-time estimates. They are not a
performance contract and not an acceptance criterion for any change to the Agent transform. The
"(to be added)" note in §7 is not an outstanding obligation of this ADR. A later statement of
Agent-mode savings, in this ADR or elsewhere, cites a committed reproducer and the commit it was
measured at.

### Alternatives considered

- **Keep the note and add `tests/presentation_savings.rs`.** A benchmark is useful, but it is
  implementation work and does not need this ADR to promise it. Leaving the note in force keeps an
  unfulfilled obligation in an accepted ADR, and a benchmark written now would measure the
  amended transform, so it would not confirm the original numbers.
- **Withdraw the figures entirely.** This would remove the only statement of the size motivation
  behind Agent mode. Marking them as estimates keeps that rationale and removes the implied
  guarantee.

### Consequences

- No test or benchmark is required by this ADR for the savings figures.
- Changes to the Agent transform are judged by the transformation rules and the amendments
  above, not by whether they preserve a savings percentage.
- No code change follows from this amendment.

### Refs

- There is no tracking issue for the benchmark.
- Transform changes made after the figures were written: #1995 and #2211 (Amendment 3), #2679
  (Amendment 5).

## Amendment 9 (2026-09-29): exact Agent timestamps

**Status: Accepted (2026-09-29).**
**Related issue:** #1355.

### Context

The §3 timestamp row compacts every ISO-8601-shaped string outside a record's `properties` and the
named exemptions (such as `trigger_at`, `due` and the Amendment 6 receipt paths) to a relative form
(`"3m ago"`) when it is less than 24 hours old, and otherwise to its first 16 characters
(`"2026-05-23T16:18"`). Both forms drop the offset and the seconds, so an Agent reader cannot
compare two instants or hand one back to a verb. They also sit beside values the transform does not
touch: a `comm.inbox` row renders `created_at` as `"29s ago"` while its `properties.sent_at` keeps
`"2026-09-29T19:18:25.132811+00:00"`, so one row carries two dialects.

The shape test also reaches strings that are not timestamps. Any string that begins
`YYYY-MM-DDTHH:` and does not parse as a whole timestamp is cut to 16 characters, so a note or
message body that begins with a date-time loses everything after its minute.

### Decision

**Exact form.** In Agent mode, a string that the §3 transform would compact is rendered as the same
instant in UTC with a `Z` suffix: `YYYY-MM-DDTHH:MM:SS[.fraction]Z`. The fraction keeps the
canonical value's digits exactly, neither padded nor truncated, and a canonical value without a
fraction gains none. A canonical value with a non-zero offset is converted to UTC. The rule never
writes the `+00:00` spelling.

**Only whole timestamps.** The rule applies only to a string that parses in full as a date-time
with an explicit offset (`Z`, `±HH:MM` or `±HHMM`). Any other string passes through byte-for-byte:
free text that begins with a date-time, a date-time without an offset, and a malformed offset. The
transform never supplies an offset the canonical value did not carry, and never shortens a string
it cannot parse.

**Relative form on list rows.** An object that is an immediate member of an array and carries a
`created_at` string rendered by this rule also receives a sibling `created_at_relative`, computed
against the response's single sampled `now` (§Implementation): `Ns ago` under one minute, `Nm ago`
under one hour, `Nh ago` under one day, `Nd ago` otherwise, each `N` rounded down. No sibling is
added when the instant is later than `now`, when `created_at` is not rendered by this rule, or when
the object already has a `created_at_relative` key; the transform never overwrites a canonical
field. A root result object, such as a single `get` record, receives no sibling. Other timestamp
fields, `updated_at` included, receive the exact form and no relative sibling.

### Unchanged

- Verbose output, and Human at the runtime boundary, remain canonical. Canonical handler output is
  not normalized: a producer that emits `+00:00` still does so under Verbose.
- Values under a record's `properties`, the `trigger_at` and `due` payload timestamps, the
  Amendment 6 receipt paths and opaque content protected by Amendment 7 keep their exact canonical
  bytes, whatever their offset spelling. Byte-exact protection takes precedence over this rule, and
  no object inside those values receives a relative sibling.
- AlwaysVerbose verbs, whole-operation error envelopes, structural list and cursor envelopes, the
  `$prev` chain (which sees canonical values) and ADR-078 format rendering keep their existing
  rules. `format=table` and `format=auto` treat `created_at_relative` as an ordinary row field.

The display-timezone setting, the renaming or unit documentation of raw-integer `*_us` fields,
and normalization of canonical handler output stay open on #1355. They are not decided here.

### Consequences

- Agent responses heavy in timestamps grow: a compacted `"3m ago"` or 16-character value becomes an
  exact value of at least 20 characters (27 with microseconds), and each list row gains one relative
  field. Amendment 8 already withdrew the savings figures as a design constraint.
- A value copied from Agent output can be passed back to a verb or compared with another system's
  timestamp without a Verbose round trip.
- Free text that begins with a date-time is no longer truncated.

### Acceptance and mutation witnesses

- **TS-EXACT:** at a fixed `now`, canonical inputs spelled with `Z`, `+00:00`, a non-zero offset and
  the compact `±HHMM` offset, each with six, three and zero fractional digits, render in UTC with
  `Z`. Parsing input and output yields the same instant, and the fraction digits are byte-identical.
  Inputs both younger and older than 24 hours are covered.
- **TS-PASSTHROUGH:** a body that begins with a date-time, a date-time without an offset, and a
  malformed offset are returned byte-for-byte. Each case fails before this change.
- **TS-PAIR:** at the transform, list rows in each unit band (seconds, minutes, hours, days) carry
  the matching `created_at_relative`. Real Agent MCP calls to `comm.inbox`, a note `list` and
  `memory.recall` return list rows whose `created_at` is exact and which carry the sibling. A single
  `get` record, a row with a future `created_at`, and a row whose canonical form already carries
  `created_at_relative` receive no added sibling, and the last keeps its own value.
- **TS-UNCHANGED:** `properties` values, `trigger_at`, `due`, receipt paths, opt-in parsed content,
  an AlwaysVerbose verb, Verbose mode, a whole-operation error and a `$prev` chain keep their
  existing controls and outputs.
- **TS-MUTATIONS:** each of the following must fail a named witness above: restoring minute
  truncation or the relative-only form; emitting `+00:00`; padding or trimming the fraction;
  assuming UTC for an offset-less value; shortening a string that does not parse; adding the
  sibling to a root object or inside `properties`; overwriting an existing `created_at_relative`;
  and adding a sibling for a future instant.

The implementing change freezes its selectors, fixtures and individual mutants before it runs. A
zero-test selection, a compile failure or a fixture that never reaches the presentation boundary is
neither a baseline nor a mutation kill.

## Amendment 10 (2026-09-30): opt-in single-operation acknowledgement views

**Status: Proposed.**
**Related issues:** #1798; #2138 describes a broader JSON-contract change that this amendment does not address.

### Context and current behavior

A successful acknowledgement can return both its outcome and context that the caller already
selected. `comm.read(id=..., body=false)` returns the mark outcome plus message properties;
a real `gtd.transition` returns the state change plus task title, priority, assignee and due
fields. [ADR-019's response contract](ADR-019-gtd-pack.md#wire-shape) already specifies
`transition` as a delta receipt with `id`, `from`, `to`, `transitioned`, `is_terminal` and
`audit_persisted`. At source commit `83a478a8f917e56b7ea7a43cb9206019b4b70e34`, the real-transition
builder also returns `full_id` and the task context above, including `due_timezone`; both
`full_id` and the task context exceed the receipt as written. This amendment records the
task-context drift and does not amend ADR-019 to authorize it. Those context fields are
useful to current record-reading
clients but need not accompany every acknowledgement view. `gtd.complete` already returns an eight-field receipt, including the
completion instant and audit outcome; it does not return the task property bag.

The current MCP response is compact JSON, including for a single operation. It retains the
`results` array, each operation's `ok`, `tool`, `result` and usage metadata, and the outer
`summary` and `status`. `format=json` keeps `result` a JSON value; `auto` and `table` render
that value as text inside the same JSON envelope. A single object uses compact-JSON fallback,
not the withdrawn key-value block of ADR-078 §3(b). The historical line count in #1798 is not
a measurement of this current serializer.

An acknowledgement is not necessarily one bit. Dispatch success does not establish that a
best-effort message mark succeeded: `comm.read` can return inner `status: "failed"` or
`"unknown"` with `read: false` or `null` and `mark_error`. A task transition can commit while
`audit_persisted` is false. Bulk marks need counts and each item's outcome. Removing those
fields would change what the caller can conclude.

This amendment decides a bounded acknowledgement view, not a classification of every public
verb. #2138's request to stop over-delivering in canonical JSON contracts is not addressed:
handler conformance, listing, detail and diagnostic contracts, and the complete verb audit
remain separate work. Amendment 9's exact UTC timestamps remain governing; the local-offset
display proposal in #1798 is not adopted here. Of #1798, this amendment adopts only the
removal of already-selected context from two acknowledgement views that a caller opts into.
Its request to change the default Agent presentation, its collapsed single-operation envelope
and its flat key-value rendering are not adopted, so a caller on the default Agent and JSON
path sees no change. The implementing change does not close #1798.

### Alternatives and recommended default

| Alternative                                                                   | Change and blast radius                                                                                                                                                                                                                                                                                                                                                                        | Disposition                                                                                                                                                                                                 |
| ----------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Presentation only: flatten the entire single-op Agent response                | Default Agent/JSON clients lose `results[0]`, `summary` and possibly transport metadata. Verbose could preserve the old wrapper, but an existing default client would need a new parser or an explicit mode change.                                                                                                                                                                            | Not selected. The wrapper conveys operation and transport outcomes and is consumed by existing clients.                                                                                                     |
| Presentation only: a narrow acknowledgement view within the existing envelope | Only callers selecting effective Agent `auto` or `table` for the two eligible standalone cases below see fewer context fields. Canonical results and the default Agent/JSON envelope remain available.                                                                                                                                                                                         | Selected, with explicit omission markers.                                                                                                                                                                   |
| Per-verb canonical wire conformance or revision                               | `gtd.transition` can conform to the already accepted ADR-019 delta receipt; it does not require a new schema. `comm.read(body=false)` would need a deliberate revision to ADR-040's existing properties promise. Removing handler fields affects JSON, Verbose, runtime/library consumers and tests; Verbose cannot restore them, and a separate detail read can observe a different snapshot. | Independent work. Transition conformance needs a compatibility plan for consumers of the current extra fields, not a new receipt design. The message-read canonical change needs its own contract decision. |
| Both presentation and canonical wire changes                                  | Combines view migration with per-verb migration and must explain which removed fields Verbose can recover. Listing contracts and other acknowledgement candidates would add further decisions.                                                                                                                                                                                                 | Not selected for this bounded change.                                                                                                                                                                       |

**Keep the built-in defaults: Agent presentation and JSON format.** Neither the defaults nor
the outer envelope change. A caller wanting an acknowledgement view opts into `format=auto`
or `format=table` with Agent presentation. Existing environment or configuration format
selection still applies, so a deployment already defaulting to Auto is an affected view
consumer. No new request parameter or presentation/format axis is introduced.

For `gtd.transition`, the view is selected as an interim, opt-in compatibility step: it
reduces displayed context while leaving existing JSON, Verbose and library consumers of the
current handler untouched. Conforming that handler to ADR-019 remains necessary independent
work; acceptance of this view neither fixes nor excuses the drift. If conformance removes the
five context fields first while retaining every field this profile requires, `full_id`
included, this profile removes nothing and adds no `view_omitted` marker. If the receipt also
omits required `full_id`, the profile declines and takes the existing rendering path. The
profile must not require the handler to retain those fields merely to produce a marker.

ADR-078 Amendment 2 closes by identifying per-verb response contracts as the durable fix
for listing verbosity: listings should project selection fields and acknowledgements should
return acknowledgements.
This amendment follows that boundary for its chosen view but does not claim to complete the
canonical response-contract work or supersede that long-term direction.

### Proposed decision

**Eligibility comes from execution context, not payload resemblance.** The response boundary
must know the resolved registered verb, validated request options and parsed execution mode.
Only `ExecutionMode::Single` with one operation, a successful outer entry (`ok: true`),
effective Agent presentation and effective Auto/Table format can select a profile. Per-op
overrides are resolved first and `AlwaysVerbose` takes precedence. A `tool` string, a marker
inside stored data, a similarly shaped JSON object or a custom handler using similar keys
cannot select the profile. The required context is internal; it is not a caller-controlled
result field and does not enter canonical handler output.

A bare JSON-object operation (`{"tool":...,"args":...}`) parses as `ExecutionMode::Single`
and is eligible when the other conditions above hold. A one-element JSON array parses as
a batch and is ineligible.

The two initial profiles are a closed set:

| Registered operation and canonical outcome                                                                                                                                  | Fields removed by this amendment from the rendered root object      | Receipt retained under the existing presentation rules                                                                                                                                                                                                                        |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `comm.read` with the singular `id` form and validated `body=false`; root object has string `id` and `full_id`, `status: "success"`, `read: true` and an object `properties` | Only root `properties`                                              | Identity, inner status, read outcome and every other root field. No success is inferred from outer `ok`.                                                                                                                                                                      |
| `gtd.transition` with a real transition; root object has string `id`, `full_id`, `from` and `to`, `transitioned: true`, and boolean `is_terminal` and `audit_persisted`     | Only root `assignee`, `due`, `due_timezone`, `priority` and `title` | Identity, original and resulting state, transition/terminal/audit outcomes, and every other root field or diagnostic. The eligible real-transition builder at this source emits no `note_recorded`, `note` or `reason`; those no-op fields belong to the excluded path below. |

Title, priority, assignee and due fields are decision columns when choosing among tasks in
an ADR-078 listing. Here the caller has already selected the task and requested the specific
state change; those fields describe task context, while `from`, `to`, `transitioned`,
`is_terminal` and `audit_persisted` establish the requested outcome. Their omission is confined
to this acknowledgement view; task listings keep their existing decision-column rules.

Check canonical outcome fields before presentation. Missing or incorrectly typed required
fields, a preexisting root `view_omitted`, or a value whose ordinary Auto/Table shape selects
a record array declines the new profile and takes the existing rendering path. A profile
never repairs a result, invents a field, converts `false` to success, or changes handler
validation. Unknown additional root fields remain on the ordinary presentation path; only
the closed removal sets above may be removed by this amendment.

For an eligible value, ordinary Agent presentation and ADR-078 View reductions run first.
Remove the named root fields still present, then add `view_omitted`: a sorted array of JSON
Pointer paths for exactly those fields newly removed, such as `["/properties"]` or
`["/assignee","/due","/due_timezone","/priority","/title"]`. Do not add a marker if no field
was newly removed. The marker describes this acknowledgement projection only; it does not
enumerate preexisting Agent or View reductions such as `full_id` suppression. It is view
metadata, not a stored or canonical receipt field. It is the one key an Agent view can carry
that the canonical result does not: the subset example in Consequences (`present(agent).fields
⊆ verbose.fields`) holds for JSON-format results and for every Auto/Table result except these
two acknowledgement views. Serialize the remaining root object with the existing compact-JSON
fallback and place that text in the entry's `result` string. Do not flatten the envelope or
turn a receipt into prose such as "done".

The following remain on their existing paths:

- `comm.read` with omitted or true `body`, any `ids` form, `comm.mark_read`, and failed or
  unknown mark outcomes. In particular, do not remove `mark_error`, reinterpret `read: null`
  or drop per-item results or bulk counts under the new profile.
- `gtd.transition` with `transitioned: false`, including its no-op explanation and any note
  outcome. `gtd.complete` remains its existing receipt, with no new omission marker.
- Explicit batches, including one-element bracket/JSON-array batches, chains, atomic units
  and grouped requests. One successful item does not make a batch a standalone operation.
- Other mutations, including `create`, `link`, `delete` and `restore`. Creation may report
  related writes or deduplication hints; deletion/restoration may carry distinct outcomes
  or degradation evidence. Those are not selected merely because they write. `link` is
  already AlwaysVerbose. `brain.mark_turn` reports best-effort accounting, not a durable
  delivery receipt, and gains no stronger meaning here.
- AlwaysVerbose verbs, stream receipt policies, opt-in parsed-note bodies, list/cursor
  envelopes, and whole-operation errors. Help, plan and `save_to` manifests are not
  acknowledgement results and keep their contracts.

No mutation, authorization, secret screening, persistence, audit, idempotency or delivery
rule changes. Protected payloads that remain present keep Amendments 6 and 7's byte-exact
rules; Amendment 9 continues to govern eligible timestamps. This projection can omit the
whole acknowledged message's property context in the named view, but does not normalize or
reinterpret that context or make it unavailable to JSON/Verbose consumers.

### Modes, formats and envelope invariants

| Effective presentation | JSON                                                                                               | Auto/Table                                                                                                      |
| ---------------------- | -------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| Agent                  | Existing Agent plus Machine reductions; result remains a JSON value; no acknowledgement projection | Existing Agent plus View reductions, then the eligible acknowledgement projection; result remains rendered text |
| Verbose                | Canonical handler result                                                                           | Existing format rendering without this projection                                                               |
| Human                  | Canonical result at the runtime boundary                                                           | Existing format rendering without this projection                                                               |

The outer compact-JSON `results`/`summary`/`status` contract survives in every case. Preserve
entry `ok`, `tool`, usage and other transport metadata, correlation fields and advisories
outside `result`. An inner mark failure or an audit failure is not promoted to transport
failure or erased. Per-op errors stay canonical. Frame fitting, depth limits, response
budgets and explicit omission/refusal behavior retain their existing precedence; this view
is not permission to bypass a response limit. If frame fitting replaces a projected entry
with its pre-render compact form, that fallback entry carries no `view_omitted` marker.

Canonical handler results remain the input to runtime/library callers and `$prev`
substitution. Result sinks retain their existing presentation/export contract and never
apply this projection; `save_to` writes before format rendering. The projection runs only
on the final displayed copy after execution; it never changes a chain's value or writes
`view_omitted` to the database. Local and daemon-backed MCP rendering must carry equivalent
trusted selection context and produce the same view for the same completed operation.

On acceptance, this is a narrow extension of this ADR's Scope and §6 policy mechanism for the
two named views. It supersedes ADR-078 Amendment 2's fallback clause, "beyond those enumerated
reductions the fallback is lossless by construction — no truncation, no elision, no
reformatting." **Only** these eligible Agent Auto/Table acknowledgements gain the additional
closed-set projection. It does not reinstate the withdrawn generic key-value renderer, change
ADR-078 Amendment 3's Agent/JSON Machine contract, or change ADR-078 §8.4's outer envelope.
All other shapes retain Amendment 2's fallback rule. It also qualifies ADR-040's `body=false`
promise only at this opt-in rendered-view boundary: the canonical `comm.read` response still
contains its properties. Acceptance does not authorize a canonical per-verb response
migration. The change that records acceptance of this amendment also adds one line at ADR-078
Amendment 2's fallback clause and one at ADR-040's `body=false` paragraph, each naming this
amendment as the qualification. Until acceptance neither document changes.

### Existing clients and migration boundary

The following client and CLI anchors are pinned to source commit
`ade26d19bf4c42a0d3e348cc4eab996146116071`:

- `tests/smoke_test.py:93-136` (`_call_request_raw`, `call_verb`) requests JSON, parses the
  MCP text as JSON, checks `results` and `summary`, and unwraps `results[0].result`.
  Its one-element batch and default JSON
  behavior remain unchanged.
- `tests/khive-contract/khive_contract/client.py:167-203,352-435` (`KhiveMcpSession.__init__`,
  `request`, `request_batch`, `verb`) defaults to Verbose,
  interprets the same wrapper and unwraps single verbs through one-element batches. That
  default and batching behavior remain unchanged. A caller using its raw request path
  with a standalone Agent Auto/Table operation is a view consumer under the new rule.
- `crates/kkernel/src/exec.rs:1941-1970` (`enforce_strict_batch_result`) derives strict-batch
  failure behavior from the outer summary. Keeping that envelope avoids a parser migration
  or a change to partial
  failure detection. CLI presentation defaults are not changed. Inline CLI execution through
  `run_exec_inline_with_forward`, whose `RequestParams` and dispatch are at `2514-2529`,
  shares the request boundary and is in scope when the parsed request is eligible
  Single/Agent/Auto or Single/Agent/Table. Ops-file dispatch in
  `apply_ops_file_reader_with_response_transform_and_dispatch_mode` at `1411-1433` uses
  the typed batch path and remains ineligible even with one operation: serial scheduling
  does not change the batch execution mode.
- Runtime/library consumers and `$prev` continue to receive canonical values. Existing
  Agent/JSON record clients retain the current Machine path and strict identifiers.
- A client already opting into Agent Auto/Table for a standalone operation may see fewer
  context fields and the new `view_omitted` array inside the rendered result text. Such a
  client must accept the declared acknowledgement view or select `format=json` for the
  existing machine shape; `presentation=verbose, format=json` retains the canonical
  escape hatch. Auto/Table are already text results, so the outer parser does not change.

This is still an observable change for existing opted-in view consumers. The implementing
change must document these exact two cases and the omission marker, and validate those
consumer paths before enabling the profiles. It must not silently turn JSON record clients
into view clients or claim that every acknowledgement is smaller.

### Synthetic fixture sizes, not production measurements

The following UTF-8 byte counts are of **three constructed JSON bodies**, modeled from
source at commit `7f822459c7bb0cf6ff74a70a1491e33b15418113`. They are not captured MCP responses,
a production sample, runtime-renderer verification, token measurements or latency results.
The population measured and the population claimed are the same three fixture bodies below;
no result is extrapolated to live stores or other verbs.

| Synthetic fixture                                    | Canonical fixture body | Current Agent/JSON model | Current Agent/Auto model | Proposed Agent/Auto model |
| ---------------------------------------------------- | ---------------------: | -----------------------: | -----------------------: | ------------------------: |
| `comm.read(body=false)` success                      |                    436 |                      424 |                      409 |                       238 |
| `gtd.transition` real transition with audit failure  |                    423 |                      423 |                      414 |                       362 |
| `gtd.complete` with audit failure, unchanged control |                    343 |                      338 |                      313 |                       313 |

Construction includes one `results` entry, `usage: {}`, and the compact outer summary/status.
It excludes JSON-RPC and MCP content wrapping, daemon frames and optional correlation,
configuration or advisory fields. The inner Auto result is JSON text, so its quotes are
escaped again by the outer JSON serializer. The completion timestamp models Amendment 9's
`+00:00` to `Z` conversion; the message's property timestamp is protected. The only modeled
reductions are those exercised by these fixtures. Python's sorted, compact JSON encoder
makes the fixture byte strings reproducible; it is not a substitute for the Rust renderer.

Source anchors at that commit:

- `crates/khive-pack-comm/src/params.rs:93-105` and `src/handlers.rs:1065-1097,1185-1204,1478-1530`:
  singular/body selection, body inclusion and actual mark outcomes. The fixture's routing
  property names are a six-key subset of the bag assembled at `src/handlers.rs:2840-2860`,
  not a complete production property bag; `comm_schema_version`, `from`, `to`, `subject`
  and optional routing metadata are not included in this synthetic fixture.
- `crates/khive-pack-gtd/src/handlers.rs:1626-1635,1886-1909,1964-1978`: completion,
  no-op and real-transition receipt shapes.
- `crates/khive-runtime/src/presentation.rs:159-207,281-322,374-404`: JSON/View preparation,
  root `full_id`/property dedup reductions and compact-JSON single-object fallback.
- `crates/khive-mcp/src/server.rs:2417-2833,3624-3632,5442-5589,5912-5914`: envelope,
  usage stamping, per-op result rendering and compact serialization. The `save_to`
  branch at `5030-5157` returns its manifest before format rendering.

The reproducer prints both counts and exact constructed bodies:

```python
import copy
import json

BASE = "7f822459c7bb0cf6ff74a70a1491e33b15418113"
UUID = "01234567-89ab-4cde-8012-3456789abcde"
identity = {"id": "01234567", "full_id": UUID}
fixtures = {
    "comm.read(body=false)": ("comm.read", {
        **identity, "status": "success", "read": True,
        "properties": {
            "direction": "inbound", "read": True,
            "from_actor": "fixture-source", "to_actor": "fixture-reader",
            "thread_id": UUID, "sent_at": "2026-09-30T20:00:00+00:00",
        },
    }),
    "gtd.transition(write)": ("gtd.transition", {
        **identity, "transitioned": True, "from": "next", "to": "active",
        "is_terminal": False, "audit_persisted": False,
        "title": "Fixture task", "priority": "p2", "assignee": "fixture-worker",
        "due": "2026-10-01T12:00:00Z", "due_timezone": "UTC",
    }),
    "gtd.complete": ("gtd.complete", {
        **identity, "completed": True, "from": "active", "to": "done",
        "completed_at": "2026-09-30T20:00:00+00:00",
        "is_terminal": True, "audit_persisted": False,
    }),
}

def encode(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))

def envelope(tool, result):
    return {
        "results": [{"ok": True, "tool": tool, "result": result, "usage": {}}],
        "summary": {"total": 1, "succeeded": 1, "failed": 0, "aborted": 0},
        "status": "success",
    }

def byte_length(value):
    return len(encode(value).encode("utf-8"))

rows = []
for label, (tool, canonical) in fixtures.items():
    # Model only these fixtures' source-governed transforms, not the Rust renderer.
    agent = copy.deepcopy(canonical)
    if tool == "gtd.complete":
        agent["completed_at"] = agent["completed_at"].replace("+00:00", "Z")
    if "properties" in agent:
        agent["properties"] = {
            key: value for key, value in agent["properties"].items()
            if key not in agent or agent[key] != value
        }
    auto = copy.deepcopy(agent)
    auto.pop("full_id", None)
    projected = copy.deepcopy(auto)
    drop = ["properties"] if tool == "comm.read" else (
        ["assignee", "due", "due_timezone", "priority", "title"]
        if tool == "gtd.transition" else []
    )
    removed = sorted("/" + key for key in drop if key in projected)
    for key in drop:
        projected.pop(key, None)
    if removed:
        projected["view_omitted"] = removed
    # AUTO result text is a JSON string inside the compact outer JSON envelope.
    values = {
        "canonical_fixture_body": envelope(tool, canonical),
        "current_agent_json_model_body": envelope(tool, agent),
        "current_agent_auto_model_body": envelope(tool, encode(auto)),
        "proposed_agent_auto_model_body": envelope(tool, encode(projected)),
    }
    rows.append({
        "fixture": label, "base": BASE,
        "bytes": {name: byte_length(value) for name, value in values.items()},
        "bodies": {name: encode(value) for name, value in values.items()},
    })
print(json.dumps(rows, indent=2))
```

These counts support only a comparison of the named constructed bodies. Changing property
content, omitted fields, metadata, identifier spelling or renderer behavior changes the
counts. The unchanged completion control demonstrates why no universal savings percentage
or default-format performance guarantee follows. Amendment 8's historical estimates remain
estimates. Native rendering and client-path witnesses are required for an implementation;
none is claimed by this fixture model.

### Acceptance and mutation witnesses for the implementing change

- **ACK-SELECT:** resolved Single/Agent/Auto and Single/Agent/Table requests select each
  registered profile using validated options and canonical outcome fields, through both
  DSL standalone operations and bare JSON-object operations. Default
  `comm.read` body, body true, `ids`, `comm.mark_read`, a lookalike handler, malformed or
  missing required fields, marker collisions and a record-array shape use the prior path.
  Explicit one-element bracket and JSON-array batches, chains, atomic/grouped requests,
  plan, help and sink manifests never select it. Selection must be exercised at the real
  response boundary.
  Production success handlers for the two verbs do not emit a root marker or record array;
  exercise those defensive decline cases by fault-injecting the returned canonical value
  before that same boundary. Register a separate lookalike handler to prove that payload
  resemblance cannot grant eligibility. These are boundary fixtures, not claims that the
  normal handlers produce those shapes.
- **ACK-MATRIX:** cover all nine presentation/format combinations and per-op overrides.
  Agent/JSON retains its prior JSON-value result and identifiers; only eligible effective
  Agent Auto/Table receives the new rendered view. AlwaysVerbose wins. Transport metadata,
  advisories, strict IDs, protected payloads, exact timestamps and budget behavior keep
  their existing witnesses.
- **ACK-OUTCOME:** successful singular reads and real transitions compact the named context
  fields only; assert exact equality with the closed removal set for the fixture's present
  fields. A real transition with `audit_persisted: false` still says false. No-op
  `note_recorded`, `reason` and note outcomes remain on their excluded existing path;
  no eligible real-transition fixture assumes they are produced. Failed/unknown marks keep the existing
  outcome and diagnostics, without treating outer `ok` as mark success. No-op transitions,
  completion receipts and bulk partial outcomes remain on their existing paths.
- **ACK-OMISSION:** `view_omitted` lists only fields actually removed by this projection, in
  sorted pointer order, with no marker if none was removed. Unknown root diagnostics survive
  ordinary rendering; canonical marker collisions are never overwritten. A field-subset check
  over an eligible view treats `view_omitted` as the single allowed extra key and fails on any
  other.
- **ACK-CLIENT:** exercise the current JSON wrapper consumers and strict-batch parser above,
  plus a standalone Auto/Table consumer, with success and partial/degraded outcomes.
  Demonstrate the documented JSON/Verbose escape hatch and that no outer schema changed.
- **ACK-CANONICAL:** handler/library output, `$prev` and existing result sinks never acquire
  the marker or lose fields through this projection. Matched local and daemon-backed
  operations, including eligible inline CLI dispatch, select the same profile; response limits and whole errors remain governing.
- **ACK-MUTATIONS:** named witnesses above must fail when a selected profile is removed;
  selection trusts payload shape rather than registered execution context; JSON is
  projected; an audit/mark outcome is erased or success is invented from outer `ok`;
  the removal set is widened, including removing `from` while listing `/from` in the
  marker; the omission marker is removed or lies about removed fields; a one-element batch
  is projected; the outer wrapper is flattened; or a chain/result sink sees the marker.

The implementing change freezes executable selectors, fixtures and individual reversible
mutants before running them. A zero-test selection, compile failure or fixture that never
reaches the presentation boundary is neither an accepted baseline nor a mutation witness.
This Proposed text supplies a contract and a fixture model, not an executed implementation
or an approval of dependent code.
