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
