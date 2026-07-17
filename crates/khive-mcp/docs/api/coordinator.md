# Coordinator dispatch — fail-closed contract

`CoordinatorService` (`src/coordinator.rs`) is the cross-backend dispatch seam
a multi-backend server routes `link`/`search` through instead of the plain
`VerbRegistry`. This document records three validation regressions the
`server.rs` intercept (`dispatch_via_coordinator_inner`) must never
reintroduce — each is pinned by a `coordinator.rs` test named for it.

## `t6d` — malformed `tags` must reject, not silently drop the filter

A multi-backend `search` with a malformed `tags` value must return a per-op
error (`ok: false`) rather than silently returning unfiltered results.
Single-backend rejects malformed tags via `SearchParams` deserialization
(`RuntimeError::InvalidInput` → `ok: false`); multi-backend must match that
contract: the server rejects before reaching the coordinator, not by
collapsing the filter to an empty `Vec` via `filter_map(as_str)`. The
regression test fails against the old `filter_map` code (which called the
coordinator with an empty tags `Vec` and returned `ok: true, result: []`) and
passes once the multi-backend path uses a strict
`serde_json::from_value::<Vec<String>>`.

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
