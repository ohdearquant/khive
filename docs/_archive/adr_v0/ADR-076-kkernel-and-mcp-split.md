# ADR-076: Kernel/MCP Split — `kkernel` as Management Binary

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-025 (Pack Standard), ADR-027 (Single-tool MCP Surface), ADR-035 (Gate Hard Enforcement)

## Context

`khive-mcp` started as a single-purpose binary: an MCP server speaking JSON-RPC
over stdio (ADR-027). It now needs to do more than that:

- **Sync** — build a SQLite working DB from NDJSON sources (issue #174). The Deno
  CLI's `khive kg sync` shells out to a Rust binary because the indexing layer
  (sqlite-vec, FTS5 trigram, embeddings) lives in Rust.
- **Pack introspection** — humans and tooling need to query what packs are
  registered, what verbs each exposes, what their schemas are. Today this is
  only visible by spawning the MCP server and calling `tools/list`.
- **Future admin ops** — pack install/remove (#236), migrations, audit log
  inspection, etc.

The name `khive-mcp` is becoming misleading: an MCP server is one of its modes,
not its identity. Continuing to overload one binary with both agent-facing and
admin surfaces also conflates trust boundaries — MCP clients should only see
the agent-allowed subset; admin tools need full kernel access.

## Decision

### Two binaries, clear separation of concerns

**`kkernel`** (new, this ADR) — the Rust management binary. Owns:

- Full pack capability surface (all registered packs, all handlers, all verbs)
- Sync, migration, and admin operations against a khive DB
- Pack introspection (`kkernel pack list`, `kkernel pack handler <pack>`)
- No opinion on access control — assumes operator context

**`khive-mcp`** (existing, refactored in a follow-up) — the agent-facing MCP
adapter. Owns:

- JSON-RPC stdio surface (ADR-027 `request` tool unchanged)
- Gate enforcement (ADR-035) — decides which kernel capabilities are agent-safe
- Curated subset of kernel verbs, filtered by policy

Both binaries link the same Rust crates (`khive-runtime`, `khive-pack-*`). They
differ in what they expose, not in what they can do.

### Subcommand layout for kkernel

```
kkernel sync --repo <dir> --db <path> [--namespace <ns>]
    Read .khive/kg/{entities,edges}.ndjson into a fresh SQLite DB.
    Atomic via tmp+rename.

kkernel pack list
    Print all registered packs (name, verbs, note_kinds, entity_kinds).

kkernel pack handler <pack> [<verb>]
    Print the full data for a pack (or specific verb): schema, description,
    requires, edge rules. Machine-readable JSON by default; --human for table.

kkernel db migrate <path>
    Run pending migrations against the given DB.

kkernel db check <path>
    Verify schema version, run integrity checks.
```

`khive-mcp` retains its current invocation (no subcommands; flags only) for
backward compatibility. Existing scripts continue to work.

### Pack registration: kernel-owned (follow-up PR)

Today packs self-register via the `inventory!` macro at link time. Both binaries
collect the same set because they link the same crates. This works but has
limitations:

- No way to enable/disable packs at runtime per binary
- No way for tools to introspect registrations without spawning a runtime
- Pack registration timing is implicit (link-order dependent in pathological
  cases)

The follow-up refactor moves registration into a kernel-managed registry:

- Each pack ships a `PackManifest` declaring its surface
- `kkernel` reads manifests from a known location at startup
- Both `kkernel` and `khive-mcp` consult the kernel's registry
- The MCP adapter applies an "exposure policy" to filter the registry down to
  agent-safe verbs

This ADR does NOT make that change. It establishes the binary split that
enables it.

### Gating mechanism: kernel-owned (follow-up PR)

Today gating lives in `khive-runtime::pack::dispatch` — every dispatch consults
`self.gate.check(&gate_req)` (ADR-035). Both binaries currently install the
default `AllowAllGate` and let policy crates extend it.

In the target architecture:

- `kkernel` runs verbs without gating (operator context, full trust)
- `khive-mcp` installs the full gate stack (agent context, policy-enforced)
- The gating trait moves out of `khive-runtime` into a `khive-gate-engine` or
  similar that both binaries depend on

This ADR establishes the direction; the runtime refactor is a follow-up.

## Consequences

- **New crate**: `crates/kkernel` with `[[bin]]` target.
- **Distribution**: both binaries ship together (see ADR-077 for packaging).
- **No back-compat break**: `khive-mcp` invocation unchanged; existing MCP
  clients see no difference.
- **Deno CLI integration**: `khive kg sync` calls `kkernel sync` via
  `Deno.Command`.
- **Documentation**: README, AGENTS.md, and CLAUDE.md updated to describe the
  two-binary model.

## Alternatives considered

1. **Subcommand `khive-mcp sync`** — keep the historical name, add subcommands
   underneath. Rejected: locks in the misleading name and conflates trust
   boundaries (one binary doing both admin and agent-facing work).

2. **Drop `khive-mcp`, single `kkernel` binary with MCP as subcommand** —
   `kkernel mcp` for the server, `kkernel sync` for sync. Rejected for v0.1
   compatibility: existing scripts and MCP configs reference `khive-mcp` by
   name. The split happens here; a future ADR may collapse them.

3. **Two separate Rust kernels (per-pack runtime)** — each pack ships its own
   binary, kkernel only coordinates. Rejected: explodes the install matrix
   and breaks the "single SQLite DB shared across packs" model.

4. **WASM kernel** — see ADR-077; rejected for performance.

## Alternatives considered but explicitly NOT in this ADR

- The pack registration refactor (move from `inventory!` to manifest-driven)
- The gating engine extraction
- The eventual collapse of `khive-mcp` into `kkernel`

These are tracked as follow-up issues and depend on this ADR landing first.

## Planned follow-up: collapse khive-mcp into `kkernel mcp`

This ADR ships kkernel alongside khive-mcp for back-compat. The intended
end-state is a single binary:

- `kkernel mcp` — long-lived stdio MCP server (current khive-mcp behavior)
- `kkernel sync`, `kkernel pack list`, etc. — one-shot admin commands

Reasoning: the work that an MCP server does (SQLite queries, FTS5 indexing,
embedding inference) lives in Rust crates. A separate `khive-mcp` binary
holding the same `khive-runtime` is duplicate plumbing — one binary with
multiple modes is cleaner.

**Sequencing** (three PRs, after this one):

1. **Add `kkernel mcp` subcommand.** Move `khive-mcp/src/{server,tools,pack}.rs`
   into `kkernel/src/mcp/`. `kkernel mcp [flags]` runs the MCP server with
   identical behavior. `khive-mcp` continues to exist unchanged.
2. **Make `khive-mcp` a thin shim.** Replace its `main.rs` with code that
   exec's `kkernel mcp` passing all flags through, emitting a one-line
   stderr deprecation notice on every invocation.
3. **Remove `khive-mcp`** after one deprecation cycle (v1.0). The
   `khive-mcp` crate is deleted; documentation, marketplace plugin configs,
   and the Deno CLI all point users at `kkernel mcp`.

This three-PR sequence keeps each step small and reversible. Existing Claude
Code MCP configurations referencing `khive-mcp` continue to work through
step 2; step 3 happens on a published major-version boundary with a
migration guide.
