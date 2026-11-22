# ADR-220: Routing Command Verbs to the Caller's Computer

- **Status:** Proposed 2026-11-22
- **Date:** 2026-11-22
- **Grounding commit:** `f700206f543444d50b8fcd178c93977440a014c8`
- **Depends on:** [ADR-016](ADR-016-request-dsl.md), [ADR-096](ADR-096-warm-daemon-per-request-identity.md),
  [ADR-108](ADR-108-git-write-surface.md)
- **Relates to:** [ADR-029](ADR-029-substrate-coordinator.md), [ADR-137](ADR-137-tailnet-wire-transport.md)

It does not change the status of any other record.

## Direction this record serves

The owner's direction, rendered in English:

- 2026-11-21: the only tool used directly on the local machine is khive; a caller's khive access
  includes a khive computer, and all development is done on that computer.
- 2026-11-22: it should still be one tool; access to the computer belongs inside khive.

## Context and scope

A khive computer is a node that holds a caller's repositories as trees, runs commands in a
sandbox, keeps a receipt for every run, and decides per actor which tools and remote acts are
granted. It serves MCP over HTTP with the same `request` tool and the same response envelope as
khive (`results`, `summary`, `status`), and it answers `verbs`, `whoami` and `help=true`.

A caller that uses a computer today holds two MCP tools: khive for its store (memory, comm, gtd,
kg and the rest) and the computer's own. That gives the model two catalogs and two connections, and
it leaves a gap the store cannot see: the store's own command-running packs (`exec`, `git`, `code`,
`workspace`, `tool`) still dispatch on the store's machine when a caller names them, because the
store has no way to know that this caller's commands belong on its computer.

Facts at the grounding commit that this record builds on:

- The stdio bridge (`kkernel mcp`) resolves the caller's configuration (the first file found among
  the discovery tiers, no merge; an explicit `--config` file is the only one read) and forwards each
  request as a frame to a warm daemon that serves many actors (ADR-096). The daemon never reads a
  caller's credentials. Source: `crates/khive-runtime/src/engine_config.rs` lines 1745-1990;
  `crates/khive-mcp/src/server/bridge.rs`.
- The bridge already answers one verb itself, `bridge.diagnostics`, and refuses to place it in a
  request beside any other operation. Source: `crates/khive-mcp/src/server/bridge.rs` lines
  200-240 and 640-641.
- Single and parallel requests and chains take different dispatch paths after parsing, so a rule
  inserted in only one of them does not constrain the other (ADR-137 Context). Source:
  `crates/khive-mcp/src/server/execution.rs` lines 448-580.
- The git write surface already resolves a credential by reference at call time and never stores
  or returns it: `[git_write] actors.<actor>.credential_ref`. Source:
  `crates/khive-runtime/src/engine_config.rs` lines 825-889.
- `[actor]` is a closed table: a misspelled key fails startup. Source:
  `crates/khive-runtime/src/engine_config.rs` lines 325-399.

This record decides how one khive tool reaches the caller's computer. It does not define the
computer: its sandbox, trees, grants, receipts and verbs are the node's, specified outside this
repository.

## Decision

### 1. Shape: the bridge routes by pack

When the caller's configuration has a `[computer]` table, the process that resolved that
configuration sends every operation whose pack is listed in `[computer].packs` to the computer, and
every other operation to the store as today. The model writes the same verb names in one tool. A
routed verb's parameters, results and refusals are the computer's.

The routing sits in every entry point that resolves the caller's configuration before dispatch: the
stdio bridge in daemon mode and in in-process mode, and `kkernel exec`. It runs on the parsed
request before the request is split into its single, parallel or chain path, so no path bypasses
it. The daemon carries no routing rule and never receives a routed operation.

Options considered:

- **(a) A pass-through verb, `computer.request(ops=...)`, carrying an ops string to the node.**
  Runner-up. It needs no catalog work, which is its one merit. It loses because it nests one
  request language inside a string of another (every quote escaped twice), a `$prev` reference
  cannot cross from a store operation into the nested string, a node result arrives wrapped inside
  a store result with two summaries and two statuses, and the tool carries two catalogs that the
  model must keep apart by hand.
- **(b) Route by pack in the process that holds the caller's configuration.** Chosen.
- **(c) The computer also serves the store packs, and the caller's one tool points at the
  computer.** Not now: the store holds every caller's mailbox and memory in one place, and moving
  one caller's store onto its computer splits that store. That is a migration with its own record.
  Nothing here forecloses it.
- **(d) Route in the shared daemon.** Rejected: the daemon serves many actors, so it would have to
  hold every caller's computer key, and a key would leave the process of the caller it belongs to.

### 2. Which computer: the `[computer]` table

The table is read from the same configuration file as `[actor]`, so the two always come from one
file. Example:

```toml
[computer]
name = "build-1"
url = "http://127.0.0.1:8700/mcp"
key_file = "/home/example/.secrets/computer-build-1.key"
packs = ["computer", "exec", "git", "blob", "tool"]
call_timeout_s = 3700
```

- The table is closed: an unknown key fails startup, as `[actor]` does.
- `name`: required; 1 to 64 bytes of lowercase ASCII letters, digits and `-`. It labels the
  computer in the catalog and in refusals.
- `url`: required; an absolute `http` or `https` URL with a path. Plain `http` is accepted only
  for a loopback host (`localhost`, `127.0.0.0/8`, `::1`), because the key travels in a request
  header; any other host requires `https`. A forwarded port on loopback is the expected `http` case.
- `key_file`: required; an absolute path (section 3).
- `packs`: required, nonempty, no duplicates. Each entry is one of the closed set `computer`,
  `exec`, `git`, `code`, `workspace`, `blob`, `tool`. A routed pack is served only by the computer
  for this caller, including packs the store also has: listing `blob` makes `blob.*` the
  computer's and hides the store's blob verbs from this caller.
- `call_timeout_s`: optional, default 3700, at most 7200. The bridge waits this long for a routed
  request and, for routed requests only, this replaces the bridge's own response deadline, so a
  long run on the computer is not cut off by the bridge first.
- No value in the table is a secret. `Debug` of the parsed table prints `key_file` as a path and
  never reads it.

One caller has at most one computer.

### 3. The computer key

- Only the process that resolved the caller's configuration reads the key, from `key_file`, when it
  first needs it and again after a reconnect. The file must be a regular file owned by the process's
  user with no group or other permission bits, and must hold one nonempty token of at most 4096
  bytes with no inner whitespace (a trailing newline is ignored). Any other state refuses routed
  operations with `computer_key_unavailable`; store operations are unaffected.
- The key is held in a buffer cleared on drop and is placed only in the `Authorization` header of
  requests to `url`. It never appears in a result, a refusal, a log or tracing field, a daemon
  frame, `bridge.diagnostics`, the store, or the catalog. A refusal names the table key
  (`[computer].key_file`), not the path and never the contents.
- On its first routed call, and after every reconnect, the bridge calls `whoami` on the computer.
  If the actor the computer reports differs from the caller's `[actor].id`, every routed operation
  is refused with `computer_identity_mismatch`, naming both actor ids. A key copied from another
  actor's configuration therefore cannot act under that actor's grants unnoticed.

### 4. Grants, receipts and refusals stay the computer's

- The computer decides grants, sandbox limits, time windows, text checks and receipts. The store's
  gate is not consulted for a routed operation and adds no condition of its own.
- The store writes no receipt and no dispatch audit row for a routed operation: the computer's
  receipt is the record. `bridge.diagnostics` counts routed requests in memory.
- Each per-operation entry the computer returns, refusals included, reaches the caller exactly as
  the computer returned it. For a request whose operations are all routed, the computer's whole
  envelope is returned unchanged. For a mixed request (section 7) the entries are passed unchanged
  and only `summary` and `status` are recomputed from them.
- The bridge never retries a routed `tools/call`: re-running a write is a second write.

### 5. Unreachable, misconfigured, or no computer

- A connection that cannot be opened (refused, name resolution, TLS) fails each routed operation of
  the request with `computer_unreachable` and `domain_disposition: not_committed`. A failure after
  the request was sent (timeout, connection reset, a non-success HTTP status) uses the same kind
  with `domain_disposition: unknown`, because the computer may have run it. A response that is not
  the khive envelope uses `computer_protocol`, also `unknown`. Each message names the computer's
  `name` and the URL's host and port.
- None of these falls back to the store or runs anything on the store's machine.
- A caller whose configuration has no `[computer]` table and calls a `computer.*` verb is refused
  with a message that no computer is configured for it, naming its configuration file and the
  missing table.

### 6. Callers with and without a computer

- **No `[computer]` table:** nothing changes. The store's `exec`, `git`, `code`, `workspace` and
  `tool` packs dispatch on the store's machine as they do today.
- **A `[computer]` table:** operations in `packs` go to the computer. An operation in the
  command-running set `exec`, `git`, `code`, `workspace`, `tool` that is not in `packs` is refused
  with `runs_on_computer` (this caller's commands run on its computer, and that pack is not routed)
  before anything is forwarded. So no configuration with a `[computer]` table can run a command on
  the store's machine, whatever `packs` lists.

### 7. Requests that mix store and computer operations

- A request whose operations all have one home is sent whole to that home.
- A parallel batch with both homes is split into two requests, one per home, sent concurrently and
  each whole; the results are restored to input order. The write-key conflict check runs on each
  side as today. In a bracketed batch of chains, each chain follows the chain rule below.
- A chain with both homes runs as consecutive segments of operations with the same home, in order,
  each segment sent as a chain to its home. Inside a segment, `$prev` is resolved by that home as
  today. For the first operation of a segment, `$prev` names the last result of the previous
  segment: the bridge resolves it with the ADR-016 path syntax and sends the resolved value as a
  literal.
- What crosses a segment boundary is a JSON value from a result: a string, number, boolean, array
  or object. An identifier that names something on the computer (a tree, a receipt, a blob) means
  nothing to the store, which receives it as a plain value. The key is never in a result, so it
  cannot cross.
- A failed or refused operation in any segment, including a computer refusal and
  `computer_unreachable`, aborts the rest of the chain: the later operations are reported
  `aborted: true`, as ADR-016 specifies today. Committed operations in earlier segments are not
  rolled back, as today.
- `plan=true` reports each stage's home as `served_by: store` or `served_by: computer` and makes
  no network call.

### 8. The catalog follows the configuration

- When a bridge with a `[computer]` table starts, it reads the computer's catalog once (`verbs`)
  and builds the tool description from the store's packs that are not routed plus the computer's
  packs that are. A routed pack's verbs are the computer's alone, never a union with the store's
  pack of the same name, so the caller sees exactly what it can call. A caller with a computer
  therefore sees the computer's `git` verbs, and does not see store `git` verbs that it cannot
  call.
- `verbs()` and `verbs(pack=...)` answer with the same composition at call time, and each pack row
  carries `served_by` and, for a routed pack, the computer's `name`. `<verb>(help=true)` for a
  routed verb is answered by the computer and returned unchanged.
- If the computer cannot be reached when the bridge starts, the tool description lists each routed
  pack as served by the named computer with its catalog unavailable. Calls are still routed and
  are answered by the computer or refused with `computer_unreachable`; `verbs()` reads the
  computer's catalog again at call time.
- A caller with no `[computer]` table sees the catalog it sees today.

### 9. What this repository carries

- In khive: the `[computer]` table and its validation; an MCP HTTP client for any server that
  serves khive's `request` tool and envelope (section 10); the routing in the bridge and in
  `kkernel exec`; the catalog composition; the refusal kinds named above; and tests that use a stub
  server or a second khive daemon as the computer.
- Outside it: the computer itself (sandbox, trees, grants, receipts, its verbs and their schemas),
  how it is hosted, and how its keys are issued, rotated and revoked.
- The kernel depends on no computer-specific verb. It uses only the `request` tool, `verbs`,
  `whoami` and `help=true`, which khive itself serves, and the pack names in section 2.

### 10. Wire contract

- MCP over streamable HTTP to `url`, with `Authorization: Bearer <key>`. One MCP session per
  bridge process. If the server reports the session expired, the bridge initializes once more;
  `initialize` writes nothing, so repeating it is safe. A `tools/call` is never repeated.
- A routed request is one `tools/call` of `request` with `ops` and the caller's presentation and
  format options. The bridge's request id is passed through so the computer's receipts can be
  matched to the caller's request.

## Acceptance

Each arm is stated before code exists. A stub computer is a test server that serves the `request`
tool and the khive envelope; a spy daemon records every frame it receives, as the existing actor
forwarding tests do.

1. **One tool.** A session whose only tool is khive, with a `[computer]` table naming a test
   computer, runs `exec.run` and reads the run's receipt with a second call. The receipt's actor is
   the configuration's `[actor].id`.
2. **Computer down.** The same call with the computer stopped is refused with
   `computer_unreachable`; after the computer restarts, its run list for that actor has no new
   receipt.
3. **Nothing ran on the store's machine.** During arms 1 and 2 the spy daemon receives no frame
   holding a routed operation; a sentinel command that would create a file on the store's machine
   leaves no file; the store's event plane has no dispatch row for a routed operation.
4. **Arm 3 can fail.** With routing disabled (the table treated as absent), the same sentinel
   creates its file and the spy daemon receives the frame.
5. **The key never surfaces.** With a fixture key of known bytes, no response, refusal, log or
   tracing output, `bridge.diagnostics` snapshot or daemon frame contains those bytes, across a
   success, `computer_unreachable`, `computer_identity_mismatch` and a computer refusal.
6. **Wrong key.** A key belonging to another actor is refused with
   `computer_identity_mismatch` before any routed operation runs; the computer's run list is
   unchanged.
7. **Pack not routed.** With `exec` absent from `packs`, `exec.run` is refused with
   `runs_on_computer` and the spy daemon receives nothing.
8. **No table.** Without a `[computer]` table, `exec.run` runs on the store's machine as today.
9. **Mixed chain.** A chain of a computer operation that fails followed by a `memory.remember` reports
   the second operation aborted and writes no memory row. A chain whose computer operation takes
   `$prev` from a store result receives the resolved value. A computer refusal reaches the caller
   byte-identical to the same call made directly to the computer.
10. **Catalog.** For a caller with a computer, `verbs(pack="git")` lists exactly the computer's git
    verbs; for a caller without one it lists the store's.
11. **Configuration rules.** An `http` URL to a non-loopback host fails startup; a key file with
    group or other permission bits refuses routed operations with `computer_key_unavailable`.

## Consequences

- A caller's configuration gains one table with five keys. Whatever writes those files writes
  `name`, `url`, `key_file`, `packs` and `call_timeout_s`.
- A caller that passes arms 1 to 11 can drop its second tool.
- The bridge process holds one HTTP client and one key. The key already lives with the process
  that starts the session today; this record moves where it is read, not who holds it.
- A long routed call holds one of the bridge's outstanding request slots for up to
  `call_timeout_s`.
- A mixed chain costs one round trip per segment.

## Not covered

- A computer that also serves the store packs (option c).
- Named tools that write only inside the calling caller's own folder (a session log, notes,
  lessons). These would be store verbs scoped by the caller's actor, not routed operations, and
  belong in their own record.
- Issuing, rotating and revoking computer keys.
- More than one computer per caller.
- Streaming a long run's output while it runs.
