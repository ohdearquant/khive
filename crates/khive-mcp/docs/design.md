# khive-mcp Design

## ADR Compliance

### ADR-016: Request DSL — Single `request` Tool
- The MCP server exposes exactly one tool named `request`. All verbs are dispatched
  through it using the function-call DSL or JSON form.
- The DSL supports three execution modes: Single, Parallel (batch), and Chain
  (`|`-separated with `$prev` substitution).
- `run_parsed` in `server.rs` is intentionally kept as a single match expression
  over the three execution modes. Splitting the branches would scatter the
  contract invariants (summary shape, aborted semantics, `$prev` substitution
  ordering) across files, making them harder to review as a unit.
- Response envelope shape: `{"results": [...], "summary": {"total": N, "succeeded": K, "failed": M, "aborted": A}}`.
- Per-op failures do not abort siblings in Parallel mode; they do abort remaining
  ops in Chain mode (reported as `{"ok": false, "aborted": true}`).
- Invalid DSL (parse/lex failure) returns an RPC-level `invalid_params` error.
  Per-verb validation failure returns a per-op `{ok: false, error: "..."}` entry.

### ADR-017: Pack Standard — Vocabulary, Visibility, and Schema Plans
- Subhandler verbs are operator-only and are blocked at the MCP wire boundary.
  Exception: `help=true` is short-circuited in `VerbRegistry::dispatch` before
  reaching the pack, so introspection passes through.
- Pack-auxiliary schema plans are applied at server startup (before any handler
  runs) so that pack tables are present. Errors are logged but not propagated
  to avoid a single pack's schema failure aborting the whole server boot.
- `register_embedders` is called on every pack after the registry is built so
  custom embedding providers are available before the first `remember`/`recall`.

### ADR-027: Dynamic Pack Loading
- `builtin_pack_names()` is sourced from `PackRegistry::discovered_names()` so
  the list always reflects whichever pack crates are linked into the binary.
- Pack registration fails fast on unknown names or unsatisfied dependencies —
  a misconfigured `KHIVE_PACKS` is a boot error, not a silent degradation.
- `pack.rs` force-references one public symbol per pack crate so the linker
  includes their `inventory::submit!` constructors in the final binary.

### ADR-031: Edge Endpoint Rules and Embedder Registration
- After the registry is built, `install_edge_rules` aggregates pack-declared
  edge endpoint rules into the runtime so `validate_edge_relation_endpoints`
  can consult the combined ruleset.
- `call_register_embedders` is invoked after registry construction, before any
  verb dispatch, to wire custom embedding providers from each pack.

### ADR-035: Authorization Gate and Audit Persistence
- The authorization gate from `runtime.config().gate` is threaded into the
  registry. Gate decisions are hard-enforcing — a `Deny` result blocks pack
  dispatch and returns `PermissionDenied`.
- The `EventStore` is wired into the registry via `builder.with_event_store` for
  audit persistence of all dispatched operations.

### ADR-038: Write-Key Conflict Detection
- Before parallel/single dispatch, operations targeting the same write key in
  the same batch are detected and receive per-op error entries.
- Non-conflicting ops in the same batch execute normally.
- `results.length == summary.total` is preserved (the response envelope contract
  is never violated by conflict detection).

### ADR-045: Presentation Transforms
- Presentation transforms are applied per-op AFTER dispatch, at the response
  envelope boundary. Chain `$prev` substitution uses canonical (verbose) handler
  output — the transform runs only on the final result, not on the intermediate
  value passed to the next op.
- `AlwaysVerbose` verbs (as declared by the verb registry policy) override the
  caller's requested presentation mode.
- Error envelopes are never transformed — only successful `result` fields.
- Known presentation mode strings: `"agent"` (default, token-efficient),
  `"verbose"` (full canonical shape), `"human"` (same as verbose at runtime).

### ADR-049: Daemon — Warm Pack Registry
- The `daemon.rs` module provides the client side: `forward_or_spawn` connects
  to a warm daemon, auto-spawns it on first use, and maps responses to MCP
  error types. Any failure falls back to `None` so the caller dispatches locally.
- The daemon is bound to `~/.khive/khived.sock`. Namespace mismatches trigger
  local-dispatch fallback.
- `warm_all` is called in a background task after the daemon socket is bound so
  ANN indexes and pack in-memory state are warmed before serving requests.
- `DaemonDispatch` is implemented on `KhiveMcpServer` so the runtime daemon
  server can call back into the MCP server's local dispatch path.

### ADR-014: Fail-Fast Pack Validation
- Pack registration is fail-fast: unknown names or unsatisfied dependencies
  abort construction and return the original runtime so callers can recover.
  The `PackRegError` type carries the runtime for this reason.

## Consistency Notes
- The `schemars` description on `RequestParams.ops` references "ADR-016" inline;
  this is user-facing schema documentation and has been left as-is to avoid
  breaking schema consumers. The description text is surfaced in MCP client
  tool discovery.
- The `get_info` server instructions string and the `request` tool description
  both contain references visible to MCP clients. These are runtime-generated
  strings, not compile-time doc comments, and serve as discoverability hints
  for agent consumers of the MCP server.
- `adr-dsl-packs H3` referenced in `server.rs` is an internal tracking label
  for a specific UX improvement (field hints in `$prev` substitution errors);
  it maps to the DSL hardening work associated with the pack DSL design.
