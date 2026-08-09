# Coordinator dispatch — fail-closed contract

`CoordinatorService` (`src/coordinator.rs`) is the cross-backend dispatch seam
a multi-backend server routes `link`/`search` through instead of the plain
`VerbRegistry`. This document records the validation and degradation contracts
the `server.rs` intercept (`dispatch_via_coordinator_inner`) must never
reintroduce. The boundary constructs the KG pack's public
`ValidatedSearchRequest` once, before fan-out, rather than parsing an ad-hoc
subset of the search JSON.

## Complete search-filter contract (#1377)

The validated request represents `kind`, `query`, `limit`, `entity_kind`,
`entity_type`, `note_kind`, `include_superseded`, `properties`, `tags`, and
`min_score`. The coordinator receives that type, forwards every applicable
storage filter to every backend, and the MCP seam applies `min_score` to the
merged ranking. `namespace` remains a transport/authentication field: it is
removed before KG parameter validation and resolved fail-closed by the registry
gate seam.

`entity_kind` and `note_kind` are compatibility spellings for callers that use
the substrate-level `kind="entity"` or `kind="note"`. A granular `kind` may be
combined with its matching compatibility spelling; contradictory values are
rejected. Entity-only fields (`entity_kind`, `entity_type`) on a note search and
note-only fields (`note_kind`, `include_superseded`) on an entity search are
rejected with a substrate-specific validation error. `properties` must be an
object and `tags` must be an array of strings. No accepted filter is silently
dropped.

## Partial-result advisory (#1370)

Backend failures do not erase successful sibling results. A degraded search
operation is successful but its operation envelope also carries:

```json
{
  "ok": true,
  "tool": "search",
  "result": [{ "id": "..." }],
  "status": "partial",
  "partial": true,
  "missing_backends": ["archive"],
  "backend_errors": {
    "archive": {
      "kind": "backend_error",
      "message": "backend search timed out after 5000ms"
    }
  }
}
```

The advisory is part of a typed intercepted-dispatch outcome, not an optional
mutex slot. The same value flows through single, batch, and chain execution;
presentation transforms only `result`, and daemon frame-budget omission keeps
`status`, `partial`, `missing_backends`, and `backend_errors` even if the result
itself must be omitted. If no result survives filtering, the operation instead
returns `ok: false` with `error.kind="search_incomplete"`; the same
`missing_backends` and `backend_errors` live inside that error object. Complete
searches omit both degradation fields.

## `t6d` — malformed `tags` must reject, not silently drop the filter

A multi-backend `search` with a malformed `tags` value must return a per-op
error (`ok: false`) rather than silently returning unfiltered results.
Single-backend rejects malformed tags via the shared validated request
(`RuntimeError::InvalidInput` → `ok: false`); multi-backend must match that
contract: the server rejects before reaching the coordinator, not by
collapsing the filter to an empty `Vec` via `filter_map(as_str)`.

## `t6e-namespace` — malformed `namespace` must fail closed (RUNTIME-AUD-002 / #433)

A multi-backend `search` (T6e) or UUID-form `link` (T6f) with a
present-but-malformed `namespace` (null/number/bool/array/object) must fail
closed — `ok: false`, an error naming the namespace — and the coordinator
must NEVER be invoked under the server's default namespace.

Before the fix, `dispatch_via_coordinator_inner` never inspected
`args_value["namespace"]` at all: it always parsed the server's
`default_namespace` and called `coord.fan_out_search`/`coord.link` under it,
silently substituting the default for a caller value that failed to parse.
The fix shares `resolve_explicit_namespace` between the coordinator intercept
and `VerbRegistry::dispatch` so both paths reject the same way.

## `t6e-limit` — out-of-range `limit` must reject, not wrap (MCP-AUD-003)

A multi-backend `search` limit beyond `u32::MAX` must be rejected with a
per-op error, not silently wrapped by `as u32` and passed through. Before the
fix, `limit=4294967297` (`u32::MAX as u64 + 2`) was parsed as `u64`, cast
with `as u32` (wrapping to `1`), then `.min(100)` left `1` — the coordinator
was called with a near-empty limit instead of rejecting the out-of-range
input. A valid-but-huge `u32` limit (`u32::MAX` itself) is a distinct case:
it is in-range and must still reach the coordinator, capped at 100.
