# khive-mcp — misc guide

Long-form rationale extracted from doc-comments in small/medium khive-mcp
source files (coordinator.rs, save_sink.rs, and others). Each section links
back to the source item; the source doc-comment keeps a short summary and a
one-line pointer here.

## `t6d` — coordinator.rs test regression

T6d: a multi-backend search with a malformed `tags` value must return a
per-op error (`ok: false`) rather than silently returning unfiltered
results. Single-backend rejects malformed tags via `SearchParams`
deserialisation (`RuntimeError::InvalidInput` → `ok: false`). Multi-backend
must match that contract: the server must reject before reaching the
coordinator, not silently collapse the filter to an empty `Vec`.

This test FAILS against the old `filter_map(as_str)` code (which would call
the coordinator with an empty tags `Vec` and return `ok: true, result: []`),
and PASSES after the strict `serde_json::from_value::<Vec<String>>` fix.

## `t6e-namespace` — coordinator.rs test regression (T6e/T6f, PR #549 blocker)

A multi-backend `search` (T6e) or UUID-form `link` (T6f) with a
present-but-malformed `namespace` (null/number/bool/array/object) must fail
closed — `ok: false`, an error naming the namespace — and the coordinator
must NEVER be invoked under the server's default namespace.

Before the fix, `dispatch_via_coordinator_inner` never inspected
`args_value["namespace"]` at all: it always parsed the server's
`default_namespace` and called `coord.fan_out_search`/`coord.link` under it,
silently substituting the default for a caller value that failed to parse.
This test FAILS against that code (coordinator IS called, `ok: true`) and
PASSES once the coordinator intercept shares `resolve_explicit_namespace`
with `VerbRegistry::dispatch` (RUNTIME-AUD-002 / #433).

## `t6e-limit` — coordinator.rs test regression (MCP-AUD-003)

A multi-backend `search` limit beyond `u32::MAX` must be rejected with a
per-op error, not silently wrapped by `as u32` and passed through.

Before the fix, `limit=4294967297` (`u32::MAX as u64 + 2`) was parsed as
`u64`, cast with `as u32` (wrapping to `1`), then `.min(100)` left `1` — the
coordinator was called with a near-empty limit instead of rejecting the
out-of-range input.

## `save-sink-rationale` (save_sink.rs)

Why the manifest matters: a sink that self-reports null counts catches bulk
export corruption (e.g. `content=null` across 10 000 rows) in one second
rather than after a downstream agent fleet has graded blind.

Why the destination policy matters: `save_to` is a client-supplied string
reaching the filesystem. Without a root + traversal + symlink check, a
client could request `../../etc/cron.d/x` or overwrite an existing
symlinked file outside any sandbox.

`export_root` defaults to `~/.khive/exports`, overridable via
`KHIVE_SAVE_TO_ROOT` (used by tests to scope each case to its own temp
directory). Every `save_to` request must resolve to a path inside this
root — see `validate_destination`.

## `write-atomic-rationale` (save_sink.rs)

`write_atomic` uses `tempfile::Builder::tempfile_in` instead of a
predictable `path.with_extension("tmp")` sibling. This closes the
symlink-following / predictable-path race the previous sibling-tmp approach
was open to, and the temp file always lives in the same directory as
`path` so the final rename is same-filesystem and atomic.

## `hash-embed-rationale` (bench_embedder.rs)

Lexically similar texts share tokens and therefore accumulate signal in the
same dimensions with the same sign, producing correlated vectors. This lets
the gate exercise the vector/ANN/fusion legs rather than treating them as
pure noise (as the previous whole-text FNV avalanche did).
