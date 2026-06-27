# ADR-078: Output Format and Shape-Aware Rendering

**Status**: proposed
**Date**: 2026-06-27
**Authors**: Ocean, lambda:khive
**Depends on**:

- ADR-016 (Request DSL — wire envelope shape, `$prev` chain semantics)
- ADR-035 (CLI Config and Auto-Embed — `khive.toml` `[runtime]` operator config, `RuntimeSectionConfig`)
- ADR-045 (Verb Response Presentation Modes — orthogonally extended and partially revised by this ADR)

## Context

khive's existing presentation layer (ADR-045) handles field-level compaction in Agent mode: 8-char
UUIDs, relative timestamps, 3-significant-figure scores, and empty-field dropping. These
transformations reduce payload size meaningfully on fields that change per record. Two categories of
cost that ADR-045 does not address remain dominant after those transforms are applied.

**Pretty-print whitespace.** The single serialization point in `crates/khive-mcp/src/server.rs`
calls `serde_json::to_string_pretty`, which emits indented, newline-separated JSON for all callers.
Measured against the three heaviest verbs (cl100k_base tokenizer, agent path):

| Verb               | pretty-json (current) | compact-json | reduction |
| ------------------ | --------------------- | ------------ | --------- |
| `gtd.tasks(10)`    | 2,061 tok             | 1,484 tok    | −28%      |
| `list(entity, 10)` | 2,051 tok             | 1,619 tok    | −21%      |
| `memory.recall(5)` | 1,161 tok             | 1,006 tok    | −13%      |

**Repeated keys per record.** On record-array verbs, every key name repeats once per record. For
`gtd.tasks`, the `properties` child object echoes `assignee`, `priority`, and `status` already
present as top-level fields, and that echo alone accounts for 31–36% of total bytes per response.
Across the typical session-start triple `[gtd.next, gtd.tasks, comm.inbox]`, the combined output
is on the order of 4,000 tokens before any work is done, with `gtd.tasks` alone accounting for
roughly 2,061 of them.

A second design consideration is rendering mode. An agent reading a 10-row record list gains
comprehensibility from a labelled table layout, not a multi-thousand-token JSON blob, provided the
table is a view that does not alter the underlying canonical data or disrupt programmatic consumers.
Tool chains that parse verb output (scripts, `$prev` chains, `tests/smoke_test.py`) require a
lossless compact form.

This ADR introduces a new `format` axis that selects the serialization and rendering strategy,
layered on top of the existing `PresentationMode`. The two axes compose independently:
`PresentationMode` governs field-level compaction; `format` governs how the resulting
`serde_json::Value` is serialized or rendered to the output string.

### Scope

This ADR specifies:

- A new `format` parameter with three values: `json`, `auto`, `table` (a fourth, `yaml`, was
  evaluated and deferred — §5)
- Surface defaults and override precedence for `format`
- Shape-dispatch rules for the `auto` format
- Compact vs. pretty-printed JSON behavior
- Redundancy-reduction rules applied in `auto` and `table` modes: `full_id` suppression,
  `properties` child-key deduplication, and `namespace` elision when "local"
- The single implementation seam where format branching occurs
- Amendments to ADR-045's `full_id` default behavior (P-C1 code rule)

This ADR does NOT introduce per-verb custom renderers as a normative requirement (they remain an
allowed extension point). It does NOT change any canonical handler return shape, any schema, or any
operational storage behavior.

## Decision

### 1. Format axis — orthogonal to PresentationMode

A new `format` parameter is added to the request surface:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Compact JSON (serde_json::to_string). Lossless machine contract. Default on every surface.
    #[default]
    Json,
    /// Shape-aware rendering: markdown table for homogeneous record arrays,
    /// flat key-value block for single records, compact JSON fallback. Opt-in.
    Auto,
    /// Force the markdown-table renderer regardless of detected shape.
    Table,
}
```

The shipped enum has three variants. A `Yaml` variant was evaluated and **deferred** (§5):
minimal-YAML measured 11–17% _more_ tokens than compact JSON for khive's record-array shapes, so it
never beats the default. It can be added as a fourth variant if a readability-first surface needs it;
nothing in this design precludes it.

`PresentationMode` (Agent / Verbose / Human, ADR-045) is unchanged. The two axes compose
independently:

- `PresentationMode` controls field-level transforms: UUID shortening, timestamp formatting,
  empty-field dropping, score truncation.
- `OutputFormat` controls how the resulting `serde_json::Value` is serialized or rendered.

Example composition: `PresentationMode::Agent` compacts the value (8-char UUIDs, relative
timestamps), then `OutputFormat::Auto` renders the compacted result as a markdown table for a
10-record homogeneous array.

### 2. Defaults and precedence

| Caller surface                                         | Default format          | Rationale                                                                                                                                |
| ------------------------------------------------------ | ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| MCP `request` tool                                     | `json` (compact)        | Lossless, shape-stable, parseable; the pure-whitespace removal alone is a −28%/−21%/−13% win on the heavy verbs with zero migration risk |
| `kkernel exec` / CLI / CI / scripts                    | `json` (compact)        | Lossless, parseable by default                                                                                                           |
| Test harnesses (`tests/smoke_test.py` and equivalents) | `json` (pin explicitly) | Prevents test breakage from format changes                                                                                               |

`json` is compact (`serde_json::to_string`, no indentation) on every surface — the current
`to_string_pretty` whitespace is removed, and the canonical shape is otherwise unchanged. `auto`
and `table` are opt-in via the `format` parameter or the operator config below.

Compact `json` is the default rather than `auto` for three reasons. (1) It is lossless **and
shape-stable**, so it cannot silently break a `$prev` chain, a script, or a parser that keys into
the response — the only change from today is the removal of indentation whitespace. (2) The
shape-changing redundancy-drop (§7: `properties` dedup, `full_id`/`namespace` elision) and the
markdown-table render are a _view_, not a serialization; folding them into the machine contract
would change what a parser sees. They are therefore scoped to `auto`/`table` (§7), where the caller
has opted into a view. (3) A table renderer truncates long cells and collapses newlines — token-lean
and legible for an agent _reading_ output, lossy for one _parsing_ it.

A fleet that wants the aggressive savings everywhere sets `default_output_format = "auto"` in its
`khive.toml` (or `KHIVE_OUTPUT_FORMAT=auto`), trading shape-stability for the −62%/−55% reduction.
The product/cloud default stays `json` so that callers who parse responses are never surprised.

Configurable via a new `default_output_format` field on `RuntimeSectionConfig` in
`crates/khive-runtime/src/engine_config.rs`:

```toml
[runtime]
default_output_format = "json"   # json | auto | table
```

**Precedence** (highest to lowest): CLI flag `--output-format` > environment variable
`KHIVE_OUTPUT_FORMAT` > `[runtime] default_output_format` in `khive.toml` > built-in surface
default.

The environment variable is applied after TOML loading, mirroring the `apply_env_brain_profile`
pattern at `serve.rs`. The `Default` impl on `RuntimeSectionConfig` does NOT read environment
variables; env resolution happens in a dedicated post-load pass. This keeps `Default::default()`
deterministic and testable in isolation.

A per-request `format=` argument on `RequestParams` (and the corresponding field on
`DaemonRequestFrame`) overrides the configured default for that request. A `format_per_op` array
with the same shape as `presentation_per_op` (ADR-045 §"Wire shape") overrides per-op within a
batch.

### 3. `auto` format — shape-aware render dispatch

`auto` dispatches rendering based on the detected shape of the post-presentation `serde_json::Value`.
Three shape categories are recognized:

**(a) Homogeneous record array.** Condition: the value contains a key whose value is a JSON array of
two or more objects sharing a mostly-scalar key set, meaning no key has a deeply-nested object value
for the majority of records. Rendered as a **markdown table**:

- One header row of field names.
- One separator row of dashes.
- One data row per record.
- Long-text column values are truncated to approximately 120 characters with a trailing ellipsis
  (`...`). `PresentationMode::Verbose` or an explicit `full=true` request parameter disables
  truncation.
- Cell values escape any literal `|` character with a backslash and collapse embedded newlines
  to a single space.

**(b) Single record or heterogeneous object.** Condition: the value is a single JSON object, or an
array that does not meet the homogeneous-record threshold. Rendered as a **flat key-value block**:

- One `key: value` line per field.
- Nested objects are indented by two spaces per level.
- No JSON braces, commas, or quotes around plain string values.

**(c) Fallback.** Shapes that match neither category above (deeply nested objects, mixed-type
arrays, scalar roots) are rendered as compact JSON, identical to `format=json`. An unrecognized
shape is safe-to-fallback, not an error.

Per-shape bespoke renderers (for example, a recall hit-block renderer that shows score bars) are an
**allowed extension point** rather than a normative requirement. Packs may declare a shape hint on
their verb to trigger a named renderer. The generic shape dispatcher described above is the
normative baseline.

### 4. `json` format — compact, lossless

`json` produces `serde_json::to_string` output: compact serialization with no indentation or
newlines. This is the lossless machine contract.

Any caller that programmatically parses verb output — including `tests/smoke_test.py`, shell
pipelines driven by `kkernel exec`, and the `$prev` chain consumer — **must use `format=json`**,
either through the surface default or an explicit override.

The current production behavior is `serde_json::to_string_pretty`. This ADR changes the MCP surface
default from pretty-printed to compact `json`, defines `json` as compact (not pretty-printed) on
every surface, and makes `auto`/`table` opt-in. CLI and `kkernel exec` surfaces likewise
default to compact `json`, not pretty.

### 5. `yaml` format — evaluated and deferred

A `yaml` format was evaluated as minimal-YAML output (block style, plain unquoted scalars where
safe, block-literal `|` for multiline strings, no YAML aliases, empty fields pruned — matching the
`minimal_yaml` configuration used by lionagi: `lionagi/libs/schema/minimal_yaml.py`, SafeDumper,
`default_flow_style=False`, `allow_unicode=True`, empty-prune pass).

It is **not shipped** in the initial implementation. Measurement confirms that minimal-YAML costs
approximately 11–17% more tokens than compact JSON for khive's dominant record-array shapes, because
per-field indentation and `-` list markers add more bytes than the JSON structural characters
(`{}[]"":,`) that YAML eliminates. YAML has a readability advantage for humans inspecting output
interactively; it does not reduce token cost relative to compact JSON on the agent path, so it loses
to both the `json` default and the `auto`/`table` views on the metric this ADR optimizes. It remains
a clean future addition (a fourth enum variant) if a readability-first surface ever needs it.

Measured comparison (cl100k_base, agent path):

| Verb               | compact-json | minimal-yaml | delta |
| ------------------ | ------------ | ------------ | ----- |
| `gtd.tasks(10)`    | 1,484 tok    | 1,730 tok    | +17%  |
| `list(entity, 10)` | 1,619 tok    | 1,838 tok    | +14%  |
| `memory.recall(5)` | 1,006 tok    | 1,122 tok    | +12%  |

The measured performance gap holds because khive verb output is record-arrays nested 2–3 levels
deep, where YAML pays per-line indentation overhead on every field. YAML wins on deep configuration
trees or long-multiline-text-heavy data, not on these shapes.

### 6. `table` format — force markdown table

`table` forces the markdown-table renderer (§3a) regardless of detected shape. Callers assert that
their result is tabular and skip shape detection. The same truncation and pipe-escaping rules as
`auto` (§3a) apply.

`table` is a lossy view. Any caller that needs full field values, including long descriptions or
nested objects, must use `format=json` or pass an expand parameter.

### 7. Redundancy reduction

The following reductions are applied in `format=auto` and `format=table` only — they are a _view_
transform, not a serialization. `format=json` (the default, on every surface) and
`PresentationMode::Verbose` always emit the canonical shape without reduction, so the machine
contract is shape-stable. A fleet that wants these reductions on every call opts in with
`default_output_format = "auto"`.

**7.1. `full_id` suppression**

In `format=auto` and `format=table`, the `full_id` field (the 36-character canonical UUID emitted
alongside the 8-character `id` shortcode) is omitted from the output. `full_id` is retained in
`format=json`, in `PresentationMode::Verbose`, and when the caller passes the existing
`include_full_id=true` envelope override (ADR-045 §"`include_full_id` override").

**This partially supersedes the P-C1 code rule in `crates/khive-runtime/src/presentation.rs`**, which
treated `full_id` as a stable chaining handle kept unconditionally in all modes. That rule was
introduced as an implementation decision and is in tension with ADR-045 §3, which states that
`full_id` is "NOT included by default" in Agent mode. ADR-078 resolves the discrepancy by making
suppression explicit for `auto` and `table`, while preserving `full_id` in `format=json` and
`Verbose` for any caller that requires the full UUID.

The suppression is safe because ADR-016 short-UUID-prefix resolution handles `$prev.id` chains
using 8-char shortcodes without requiring the 36-char form. The `format=json` escape hatch remains
unconditionally available for callers that chain on the full UUID.

The measured cost of `full_id` is approximately 5% of bytes per GTD task record (490 bytes across a
10-record `gtd.tasks` response).

**7.2. `properties` child-key deduplication**

In `format=auto` and `format=table`, any key-value pair in a `properties` child object whose key and
value are both identical to a top-level sibling field in the same record is dropped from
`properties`. Keys in `properties` that are genuinely additive (no matching top-level sibling) are
retained.

This is a pure view transform at the presentation layer. The canonical stored record is unchanged;
`format=json` and `Verbose` mode reproduce the full `properties` object. On `gtd.tasks` output, the
deduplication removes the `assignee`, `priority`, and `status` echoes from `properties`, retaining
`tags` and `transition_note`. On the measured sample this reduction accounts for 31–36% of total
response bytes.

**7.3. `namespace` elision when "local"**

In `format=auto` and `format=table`, the `namespace` field is omitted when its value is `"local"`
(the default namespace, ADR-007). When `namespace` carries a non-default value, it is included.
`format=json` always includes `namespace`.

### 8. Invariants

**8.1. Format rendering runs after `$prev` resolution**

`OutputFormat` rendering is applied at the wire boundary, after all `$prev` substitutions for the
entire request batch have been resolved. The format transform never runs on intermediate chain
results. Intermediate values in a chain carry the canonical `serde_json::Value` throughout; only the
final batch response is formatted. This extends the "Chain `$prev` invariant" of ADR-045 §"Chain
`$prev` invariant" to cover the format axis.

**8.2. Error envelopes are never reformatted**

Error envelopes (`{ok: false, tool: "...", error: "..."}`) are never passed through the `auto`
or `table` renderers. They are always serialized as compact JSON regardless of the requested
`format`. This preserves full UUIDs and structured fields in error messages for debugging,
consistent with ADR-045 §3.5.

**8.3. Canonical value is always recoverable**

`format=json` produces the canonical compact-JSON representation of the post-presentation-mode
transformed value. Every other format is a lossy or rendered view. Callers that need the lossless
form always have `format=json` available.

**8.4. Compounded requests render per-op (part-by-part), not whole-envelope**

A compounded request — a parallel batch `[v1(...), v2(...)]` or a chain `v1(...) | v2(...)` — returns
the envelope `{results: [{ok, tool, result}, ...], summary: {...}}`. Op results are heterogeneous:
one op may return a homogeneous record array, the next a single entity, the next a scalar count.
Rendering is therefore applied **per-op, to each op's `result` payload independently**, by that
payload's own detected shape. It is never applied to the whole envelope as one undifferentiated blob.

The envelope skeleton itself — the `results` array structure, the `ok`/`tool` keys on each entry,
and the `summary` object — is always compact JSON. Only the inner `result` value of each successful
op is handed to the format renderer. Under the default `format=json` this distinction is invisible
(the entire envelope is compact JSON). Under `format=auto`/`table`, a 3-op batch yields three
independently rendered payloads (for example a markdown table, then a key-value block, then a scalar)
each nested under its `results[i].result`, with the surrounding envelope still compact JSON so the
batch remains machine-walkable.

`format_per_op` (§2) sets the format for each op position independently, mirroring
`presentation_per_op` (ADR-045); a single `format` applies uniformly to every op's payload. Error
entries follow §8.2 (never reformatted) regardless of the per-op setting.

### 9. Single serialization seam

All format branching is implemented at the single `Value`-to-string point in
`crates/khive-mcp/src/server.rs`, currently the `serde_json::to_string_pretty` call at
approximately line 1199. Pack handlers return `Result<serde_json::Value, _>`; no pack handler
requires changes. The ADR-045 presentation layer transforms the `Value` before reaching this seam;
the format branch then serializes or renders the resulting value.

No additional serialization points are introduced. The number of shape strategies is bounded: two
generic strategies (markdown table, flat key-value block) plus a compact-JSON fallback, with an
extension point for per-shape bespoke renderers.

## Rationale

### Why compact JSON, not pretty-printed, as the baseline machine contract

The dominant token cost on record-array verbs is not JSON structural characters but indentation
whitespace. On a 10-record `gtd.tasks` response, `to_string_pretty` adds approximately 577 tokens
of indentation and newlines carrying no information. Switching to `to_string` eliminates them with
zero semantic change and zero risk to parsers. A 28% reduction on the heaviest verb from a
single-call change at a single seam is the clearest available return on implementation effort.

### Why markdown table over TSV for `auto` shape rendering

Measured cl100k costs for a 10-record `gtd.tasks` result, tracing the reduction pipeline:

| Representation                         | Tokens | vs. current pretty-json |
| -------------------------------------- | ------ | ----------------------- |
| pretty-json (current)                  | 2,061  | baseline                |
| compact-json                           | 1,484  | −28%                    |
| compact-json + redundancy drop         | 1,026  | −50%                    |
| TSV (after redundancy drop)            | 815    | −61%                    |
| markdown table (after redundancy drop) | 774    | −62%                    |

On these shapes the markdown table is both leaner and more legible than TSV: 774 vs. 815 tokens on
`gtd.tasks(10)` and 932 vs. 1,407 on `list(entity, 10)`. The table shares a single header row across
all records and truncates long cells to approximately 120 characters (a lossy view; `format=json`
returns the full record), whereas TSV repeats a wide union-of-keys layout, renders nested objects as
compact JSON inside a cell, and requires the caller to know column order out-of-band. Header labels
and cell alignment make the table comprehensible without consulting a schema. The markdown table is
therefore chosen for both token cost and readability.

`format=json` provides a lossless form at approximately 1,484 tokens (compact, no redundancy drop)
for any caller that needs minimum tokens with round-trip fidelity.

### Why YAML is not shipped

YAML was evaluated as a potential default to reduce JSON structural noise. Measurement showed the
opposite result for khive's dominant output shapes: minimal-YAML costs 11–17% more tokens than
compact JSON on record-array verbs (§5 table). YAML wins on deep configuration trees or
long-multiline-text-dominated data; it does not win on 2–3-level-deep record arrays. Because it
beats neither the `json` default nor the `auto`/`table` views on token cost, it was deferred rather
than shipped — it remains a clean future variant for a readability-first surface.

### Why shape-dispatch over per-verb hardcoded renderers

Per-verb renderers would require every pack author to declare a renderer and maintain tests for three
format variants. Shape dispatch at the single serialization seam operates on the structure of the
`serde_json::Value` itself, independent of which verb produced it. New packs and new verbs receive
sensible default rendering without any per-verb work. The generic shape strategies cover the large
majority of actual verb output shapes observed in measurement. Per-shape bespoke renderers are
retained as an extension point for cases where the generic rules produce a poor result.

## Alternatives Considered

| Alternative                                                | Why rejected                                                                                                                                                                                                                                                                                                                                                                                                                  |
| ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Single global YAML default                                 | Costs 11–17% more tokens than compact JSON for record arrays (§5). YAML beats neither the `json` default nor the `auto`/`table` views on token cost, so it was deferred rather than shipped as a variant.                                                                                                                                                                                                                     |
| Field-compaction only, no format axis                      | ADR-045's Agent-mode compaction handles per-field trimming but leaves pretty-print whitespace and repeated key names intact. Those two factors remain the dominant cost on heavy verbs and require a separate format axis to address.                                                                                                                                                                                         |
| Per-verb hardcoded renderers                               | Forces every pack author into a rendering concern, couples renderer tests to handler tests, and requires new verbs to declare renderers. Shape dispatch at the seam requires no per-pack work and is testable independently of pack logic. Per-verb renderers are retained as an extension point.                                                                                                                             |
| Using `format=auto` (shape-aware table) as the MCP default | A table is a lossy view (cells truncate at ~120 chars, newlines collapse) and the redundancy-drop reshapes `properties`, so an `auto` default would silently change the parse contract for any agent or script reading MCP output. Compact `json` is lossless and shape-stable; `auto`/`table` are opt-in for callers reading rather than parsing, and a fleet can still adopt `auto` everywhere via `default_output_format`. |
| Using `format=table` as the MCP default                    | Forcing markdown-table rendering before validating that all verb shapes are table-renderable risks rendering errors on complex or single-record shapes. `format=auto` with a compact-JSON fallback is the safer of the two view formats.                                                                                                                                                                                      |

## Consequences

### Positive

- The **default** (compact `json`) reduces the heaviest verbs by approximately 28% / 21% / 13%
  (`gtd.tasks(10)` 2,061 → 1,484; `list(entity, 10)` 2,051 → 1,619; `memory.recall(5)` 1,161 →
  1,006) from removing pretty-print whitespace alone, with **zero shape change and zero caller
  migration**. The session-start triple `[gtd.next, gtd.tasks, comm.inbox]` falls from on the order
  of 4,000 tokens to roughly 3,000 on the default path.
- **Opting into `auto`** (per-request, or fleet-wide via `default_output_format = "auto"`) adds the
  redundancy-drop and markdown-table view for approximately 55–62% off the pretty-print baseline:
  `gtd.tasks(10)` falls to 774 tokens (−62%) and `list(entity, 10)` to 932 (−55%), taking the
  session-start triple to roughly 2,000 tokens. For `gtd.tasks` the `properties` dedup is the
  dominant lever (the echoed `assignee`/`priority`/`status` are 31–36% of bytes); for `list(entity)`,
  whose records carry little duplicated structure, the markdown table itself is the dominant lever
  (1,619 → 932). This path is lossy (cells truncate, `properties` reshapes), which is why it is
  opt-in rather than the default.
- All format branching is bounded to the single serialization seam in `khive-mcp`. Handlers, packs,
  and storage layers require no changes.
- `format=json` provides an unconditional lossless escape hatch that any parser can rely on
  regardless of future format changes.

### Negative

- `format=auto` and `format=table` are lossy views. Because `json` is the default on every surface,
  a parser gets the lossless form for free; the risk is one-directional — a caller that opts into
  `auto`/`table` (per-request or via `default_output_format`) and also parses output must keep
  parsing paths on `json`. `tests/smoke_test.py` should pin `format=json` so a future config change
  cannot affect it. The shape-stable default means there is no forced migration.
- The renderer is more complex than a single `serde_json::to_string_pretty` call. Two generic shape
  strategies, a shape-detection pass, and a redundancy-reduction pre-pass add bounded code relative
  to the single seam constraint.
- Table truncation and `properties` dedup create output that looks complete but omits information
  present in the canonical form. The invariant that `format=json` is always available must be
  documented prominently.

### Neutral

- `PresentationMode` and its Agent-mode transformation rules are unchanged. Callers that set
  `presentation=verbose` receive the canonical shape regardless of `format`, because `Verbose` mode
  bypasses the redundancy-reduction pass.
- The existing `include_full_id=true` envelope override (ADR-045 §"`include_full_id` override")
  continues to work and takes effect before the `full_id` suppression rule in §7.1.
- A future `yaml` variant would need a YAML emitter. `serde_yaml` is NOT currently a workspace
  dependency (verified 2026-06-27), so adding `yaml` later means either taking on `serde_yaml` as a
  new optional dependency or writing a small in-tree minimal-YAML emitter over the pruned `Value`.
  Because `yaml` is deferred (§5) and never a default, this dependency decision is left open; it does
  not affect the shipped `json` default or the `auto`/`table` view paths.
- The `format_per_op` array introduced in §2 uses the same shape as `presentation_per_op` from
  ADR-045, keeping the per-op override pattern consistent across both axes.

## Implementation

### Crate placement

- `OutputFormat` enum: `khive-runtime::presentation` (alongside `PresentationMode`)
- `default_output_format` config field: `khive-runtime::engine_config::RuntimeSectionConfig`
- Environment variable resolution: post-TOML env-apply pass (alongside `KHIVE_BRAIN_PROFILE`)
- Format branching (single serialization seam): `khive-mcp::server` at the `serde_json::to_string_pretty` call site (~line 1199)
- `format` and `format_per_op` fields on wire envelope: `khive-mcp::tools::request` (`RequestParams` struct) and `khive-mcp::daemon` (`DaemonRequestFrame`)
- Shape detection, renderers, and redundancy-reduction pass: `khive-runtime::presentation::render` (new module) or `khive-mcp::format`

### Config field

```rust
pub struct RuntimeSectionConfig {
    /// Brain profile ID for feedback routing (ADR-035 §Brain profile configuration).
    #[serde(default)]
    pub brain_profile: Option<String>,
    /// Default output format. None = use surface built-in default (json on every surface).
    #[serde(default)]
    pub default_output_format: Option<OutputFormat>,
}
```

The built-in surface default is compact `json` on every surface; it is applied after config
resolution, when `default_output_format` is still `None`.

### Shape detection sequence

The shape detector operates on the final post-presentation `serde_json::Value`, before the
redundancy-reduction pass runs. Detection precedence:

1. If the pack has registered a bespoke renderer for this verb and this format, invoke it.
2. If the value contains a key whose value is a JSON array of two or more objects sharing a
   mostly-scalar key set, classify as homogeneous record array and apply the markdown table
   renderer.
3. If the value is a single object (no record array), classify as single record and apply the
   flat key-value block renderer.
4. Otherwise, apply compact-JSON fallback.

### Redundancy-reduction pass

The §7 reductions (full_id suppression, properties dedup, namespace elision) are applied as a
pre-format pass on the `serde_json::Value` after the ADR-045 `present_response` transform and
before shape detection. The pass is skipped entirely when `format=json` or
`PresentationMode::Verbose` is active.

### Migration

- `tests/smoke_test.py` and any script that parses `kkernel exec` output should either rely on the
  CLI/exec surface default of `json` (compact) or pass `format=json` explicitly.
- The MCP surface switches from pretty-print to compact `format=json` — same shape, whitespace
  removed — so callers that parse raw MCP JSON are unaffected beyond the whitespace change. Adopting
  `format=auto` (per-request or via `default_output_format`) is the opt-in step that changes shape;
  callers that parse must stay on `json` if they do.
- Pack handlers require no changes. The canonical `serde_json::Value` return shape is unchanged.

## References

- ADR-016 (Request DSL) — short-UUID-prefix resolution; `$prev` chain semantics; `RequestParams`
  wire shape
- ADR-035 (CLI Config and Auto-Embed) — `khive.toml` `[runtime]` operator config; `RuntimeSectionConfig` shape
- ADR-045 (Verb Response Presentation Modes) — `PresentationMode`; `include_full_id` override;
  §3.5 error envelope invariant; Chain `$prev` invariant; partially superseded on `full_id` default
  behavior by §7.1 of this ADR
- Measurement workspace: `.khive/workspaces/20260627/output-verbosity/05-table-flavor-measurement.md`
  (grounded cl100k_base integer token counts, agent path, 2026-06-27), with `SYNTHESIS.md` and
  `04-measurement.md` as supporting context
- Ocean directive 2026-06-27 — output verbosity reduction, format axis as the design vehicle
