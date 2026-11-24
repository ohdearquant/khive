# ADR-220: Routing Command Verbs to the Caller's Computer

- **Status:** Accepted 2026-11-22 (#6816)
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
- `[actor]` is a closed table: a misspelled key fails startup. It has been closed since #2902
  (2026-09-17); unknown top-level tables are accepted. Source:
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
[actor]
id = "lambda:example"
computer = "build-1"

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
- `[actor].computer`: required when the table is present, and equal to its `name`. The table
  without the field, and the field without the table, both fail startup. The field sits in the
  closed `[actor]` table so that a build which does not know `[computer]` refuses the file instead
  of starting with the table ignored (Consequences).

One caller has at most one computer.

### 3. The computer key

- Only the process that resolved the caller's configuration reads the key, from `key_file`, when it
  first needs it and again after a reconnect. The file must be a regular file owned by the process's
  user with no group or other permission bits, and must hold one nonempty value of visible ASCII
  (0x21 to 0x7e), at most 4096 bytes, one trailing LF or CRLF ignored; a symbolic link or a FIFO
  at that path is refused. Any other state refuses routed operations with
  `computer_key_unavailable`; store operations are unaffected.
- The bridge reads the key into a buffer cleared on drop and gives the HTTP client a copy for each
  session, used only for the `Authorization` header of requests to `url`; the HTTP client keeps
  that copy for the session and frees it without clearing it. The HTTP client follows no redirect,
  so the header is sent only to the host and port of `url`. The key never appears in a result, a
  refusal, a log or tracing field, a daemon frame, `bridge.diagnostics`, the store, or the catalog.
  A refusal names the table key (`[computer].key_file`), not the path and never the contents.
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
- A `tools/call` the computer accepted is never sent again: re-running a write is a second write.
  A request answered HTTP 404 for a lost session did not run and is sent once on the new session
  (section 10).

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
  before that operation is forwarded. So no request from a caller with a `[computer]` table runs
  a verb of a command-running pack on the store's machine, whatever `packs` lists.
- **Scheduled actions:** `schedule.schedule` stores a verb string that the store dispatches later
  on its own machine, as the creating actor, from the daemon's tick or from
  `kkernel exec --pending-events`. For a caller with a `[computer]` table, the routing process
  refuses `schedule.schedule` with `runs_on_computer` when its action, or an action nested in it,
  names a command-running pack or a routed pack. An action that is not a literal string, does not
  parse, or nests more than eight deep cannot be read before it is stored and is refused the same
  way. A scheduled-event note written with a generic `create` does not fire: the drain replays only
  a row whose creator provenance event the schedule pack appended.
- Other dispatch that starts on the store's machine is listed under Not covered, with what a
  deployment does about it.

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
- Every read of the computer's catalog, at start and at call time, is bounded by the smaller of
  `call_timeout_s` and 15 seconds, session open included. A computer that accepts the connection
  and does not answer within that bound is unreachable for the catalog: the tool description lists
  its routed packs as unavailable, and `verbs()` fails with `computer_unreachable` and
  `not_committed`. The store's verbs are never held behind it.
- A caller with no `[computer]` table sees the catalog it sees today.

### 9. What this repository carries

- In khive: the `[computer]` table and its validation; an MCP HTTP client for any server that
  serves khive's `request` tool and envelope (section 10); the routing in the bridge and in
  `kkernel exec`; the catalog composition; the refusal kinds named above; and the test computer that
  the acceptance arms run against.
- Outside it: the computer itself (sandbox, trees, grants, receipts, its verbs and their schemas),
  how it is hosted, and how its keys are issued, rotated and revoked.
- The kernel depends on no computer-specific verb. It uses only the `request` tool, `verbs`,
  `whoami` and `help=true`, which khive itself serves, and the pack names in section 2.

### 10. Wire contract

- MCP over streamable HTTP to `url`, with `Authorization: Bearer <key>`. One MCP session per
  bridge process. The computer must answer HTTP 404 to a request on a session it no longer holds,
  and must answer before it handles that request. The bridge relies on this: it reads such a 404
  as a request that did not run, initializes once more (`initialize` writes nothing), repeats the
  `whoami` check of section 3, and sends that request once on the new session. rmcp's client can
  re-initialize and re-send by itself, but it would skip the `whoami` check, so the bridge turns
  that off and does it itself (arm 12). A `tools/call` that the computer accepted is never sent
  again.
- The rule that a routed `tools/call` runs at most once rests on that requirement. A computer that
  handles a request and then answers 404 for its session breaks it, and the bridge cannot detect
  that: it sends the request again and the computer runs it twice. The computer is specified
  outside this repository; meeting this requirement is its part of the contract.
- A routed request is one `tools/call` of `request` with `ops` and the caller's presentation and
  format options. The bridge's request id is passed through so the computer's receipts can be
  matched to the caller's request.

## Acceptance

Each arm is stated before code exists. The test computer is khive's own `request` handler, carrying
the routed packs, served over streamable HTTP. Its `exec` is a recording stand-in that writes a
receipt row (actor and operations) to the test computer's own store instead of starting a sandboxed
process; a sentinel command is one that stand-in turns into a file. The client reaches the test
computer through a connector seam: in production it connects to `url` over TCP or TLS, in tests
over an in-process pipe with no socket, so every arm except the redirect case of arm 5 runs on a
machine that allows no socket of any kind and no nested sandbox. The pipe carries real HTTP/1.1
between the client and the test computer, so the arms still see real status codes, the 404 for a
lost session included. A spy daemon records every frame it receives, as the existing actor
forwarding tests do. Arms 3, 4 and 8 also run with the real `exec` pack, and arm 1 also runs over
loopback TCP. The redirect case of arm 5 runs only over loopback TCP, because the property it
checks belongs to the production HTTP client. Those variants need a host that allows the real
`exec` pack and TCP, and run in the hosted checks. A green local run does not cover them: arms 3,
4 and 8 with the real `exec` pack pass only on hosted macOS.

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
   success, `computer_unreachable`, `computer_identity_mismatch` and a computer refusal. A routed
   request that the test computer answers with a redirect to a second listener is refused, and the
   second listener receives no request.
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
11. **Configuration rules.** An `http` URL to a non-loopback host fails startup; a `[computer]`
    table without `[actor].computer`, the field without the table, and the two naming different
    computers each fail startup; a key file with group or other permission bits, a symbolic link
    and a FIFO at `key_file` each refuse routed operations with `computer_key_unavailable`.
12. **No repeated call.** Across a session expiry reported by the test computer, one `exec.run`
    executes exactly once on the test computer: its run list for the actor gains one receipt, and
    the test computer answers `whoami` on the new session before that run.
13. **Scheduled command refused.** For a caller with a `[computer]` table, `schedule.schedule`
    whose action is `exec.run(...)`, a nested `schedule.schedule` of one, or a `$prev` value is
    refused with `runs_on_computer`, and the store holds no new scheduled event; a
    `schedule.schedule` of `memory.remember(...)` is stored as today.

Not run: a key file owned by another user. The reader refuses it, but creating such a file needs
root or a second account, and the test suites run as neither, so the arm is stated and not
exercised.

## Consequences

- A caller's configuration gains one table with five keys and one `[actor]` key. Whatever writes
  those files writes `name`, `url`, `key_file`, `packs`, `call_timeout_s` and `[actor].computer`.
- A build that predates the `[computer]` table ignores it, since unknown top-level tables are
  accepted, and would dispatch that caller's command verbs on the store's machine.
  `[actor].computer` makes such a build fail closed: every build from #2902 (2026-09-17) on
  refuses a file that carries it and does not start. A build older than #2902 also ignores unknown
  `[actor]` keys; it starts and runs that caller's command verbs on the store's machine, so a
  deployment replaces such a build before it writes a `[computer]` table.
- A caller that passes arms 1 to 13 can drop its second tool.
- The bridge process holds one HTTP client and one key. The key already lives with the process
  that starts the session today; this record moves where it is read, not who holds it.
- A long routed call holds one of the bridge's outstanding request slots for up to
  `call_timeout_s`.
- A mixed chain costs one round trip per segment.
- The HTTP client and the test computer use rmcp's streamable HTTP features, which add the
  `sse-stream` crate to the lock file.

## Not covered

- A computer that also serves the store packs (option c).
- Named tools that write only inside the calling caller's own folder (a session log, notes,
  lessons). These would be store verbs scoped by the caller's actor, not routed operations, and
  belong in their own record. The route named for them is the store's blob pack with a sync, and
  section 2 makes a routed `blob` hide the store's blob verbs from the caller, so a caller that
  writes its files that way does not list `blob` in `packs` until that record decides how the two
  meet.
- Scheduled events stored before the caller's configuration gained a `[computer]` table. They fire
  on the store's machine as stored. Before writing the table, a deployment lists them with
  `schedule.agenda` as that caller and cancels with `schedule.cancel` each one whose action names
  a command-running pack.
- Mounted tool sources (`[[mounts]]`). A mount is a process the store's operator configures; the
  store starts it on its own machine and serves its tools as verbs named `<mount>.<tool>` to every
  caller, whatever that caller's table says. A deployment that serves callers with a `[computer]`
  table configures no mount whose tools run commands.
- Dispatch that no caller's request starts: the channel loops (email, telegram, polling and
  ingest), which dispatch only `comm.*` and `blob.put`; the daemon's own processes (the events
  daemon, its launch and self-heal, its liveness probe); and operator subcommands such as
  `kkernel kg init` and `kkernel kg commit`, which run `git` and `/bin/sh` on the machine where
  they are run. None of these takes a command from a caller.
- Builds older than #2902 (Consequences).
- Issuing, rotating and revoking computer keys.
- More than one computer per caller.
- Streaming a long run's output while it runs.

## Amendment 1 (2026-11-24) - Sharing Named Files into a Computer

**Status**: Proposed. The ADR's Accepted status stands unchanged.

**Grounding commit:** `621f0a01827e30aa7cb79b5089e41696696630b9`

### Context

A caller on a computer reads only what that computer's store holds as trees. A file that lives on
another machine, such as a handoff note, a report or a draft that its owner or another caller wrote
there, can reach it today only as text pasted into a message or a store note. That copies the
text, loses the file's bytes and name, and has no size rule. The computer side of the fix is a node
verb, `computer.share`, specified with the computer outside this repository: called with a tree,
a receiving actor and the files it holds, it records who shared which files with whom and returns
the share's id. This amendment decides the sending side: how one khive tool turns named local
files into a tree on a target computer and makes that call.

Facts at the grounding commit, and of the computer's verbs as the computer serves them at the same
date, that this amendment builds on:

- `blob.import` reads a file only under a configured import root: no `..` component, no symbolic
  link at any component (a link whose target is inside the root included), canonical containment
  in the root, a regular file opened without following links, and at most 64 MiB. Source:
  `crates/khive-runtime/src/file_policy.rs` lines 173-246;
  `crates/khive-pack-blob/tests/file_transfers_tests/transfer_and_root_policy_tests.rs`.
- `bridge.diagnostics` is answered by the stdio bridge itself and must be the only operation in
  its request. Source: `crates/khive-mcp/src/server/bridge.rs` lines 195-240.
- A computer names a blob, and a tree entry's content, by the lowercase hex BLAKE3 of its bytes:
  a file holding `abc` is entered as `6437b3ac...9d85`, the published BLAKE3 test vector.
  `blob.begin(size, content_ref?)` takes at most 64 MiB and answers an existing reference without
  staging; `blob.put_part` appends a part; `blob.commit` checks the declared size and reference.
  `exec.tree(entries)` builds a tree from `{path, ref, mode}` entries; `exec.tree_put` needs an
  existing tree to edit.
- Section 2 gives a caller at most one computer, and that table also routes the caller's command
  packs. The caller that shares a file is usually not a caller of the receiving computer in that
  sense: it may have no computer, or a computer of its own.

### Decision

1. **A bridge verb, `bridge.share`.** It is answered by the process that resolved the caller's
   configuration, the stdio bridge and `kkernel exec`, like `bridge.diagnostics`: it is never
   sent to the daemon and never routed by section 1. It must be the only operation in an
   unbatched request. Arguments: `target`, the name of a share target (item 2); `receiver`, the
   actor id the share is addressed to; `paths`, a nonempty array of distinct paths relative to
   the share root; `note`, an optional string of at most 1024 bytes passed to the computer. The
   reply is `{share_id, target, receiver, tree, files: [{path, digest, size}]}`, where `share_id`
   is the computer's answer and `digest` is the BLAKE3 reference of the bytes sent.

   Options considered:

   - **(a) A verb of the store's `blob` pack.** Rejected: the daemon would read callers' files and
     hold computer keys, which section 1 option (d) rules out.
   - **(b) A verb the computer serves.** Impossible: the computer cannot reach the sending
     machine's files.
   - **(c) A verb the caller's own process answers.** Chosen. The name sits under `bridge.`
     because a `computer.*` name would be routed by section 1.

2. **Where a share goes: `[share]` targets, separate from `[computer]`.** Example:

   ```toml
   [share]
   root = "/home/example/share"
   max_file_bytes = 67108864
   max_total_bytes = 268435456
   max_files = 256

   [[share.targets]]
   name = "build-1"
   url = "https://build-1.example/mcp"
   key_file = "/home/example/.secrets/share-build-1.key"
   call_timeout_s = 600
   ```

   - The table and each target are closed: an unknown key fails startup. A `[share]` table with
     no target fails startup.
   - `root`: required; an absolute path to an existing directory, canonicalized when a share
     runs. Everything under it can be shared, and nothing else.
   - `max_file_bytes`: optional, default and maximum 67108864 (64 MiB, the computer's per-blob
     limit). `max_total_bytes`: optional, default 268435456, at most 1073741824. `max_files`:
     optional, default 256, at most 4096.
   - Each target's `name`, `url`, `key_file` and `call_timeout_s` follow the rules section 2 and
     section 3 give the `[computer]` keys of the same names, the loopback rule for plain `http`,
     the key file rules and the `whoami` check included. Target names are distinct.
   - A target's key is one the target computer's owner issues to this caller, holding the grants a
     share uses: the blob upload verbs, `exec.tree` and `computer.share`. Issuing it is outside this
     repository, as section 9 says of every computer key.
   - A target may be the caller's own computer; it is still reached through its target entry.

   Options considered:

   - **(a) Share only into the caller's own `[computer]`.** Rejected: the usual sender has no
     computer, or shares into a computer that is not its own, and adding a `[computer]` table to
     reach the receiver's computer would route the sender's command packs there.
   - **(b) Named targets in their own table.** Chosen. Routing (sections 1 to 8) is unchanged by
     it, and the rule of one computer per caller still holds for routing.

3. **What it reads.** Each path is checked with the `blob.import` rules against `root`: a
   relative path, no `..` component, no symbolic link at any component, a regular file, at most
   `max_file_bytes`. Every path is checked, and the count and total size are checked against
   `max_files` and `max_total_bytes`, before anything is sent; a failure refuses the whole share,
   names the first failing path and its rule, and sends nothing. Nothing is truncated. Each file
   is then opened without following links, its bytes are read once, and those bytes are both
   hashed and uploaded, so the digest names exactly what was sent. A file whose size differs
   from the size checked is refused with `share_file_changed`. In the tree a file's path is its
   path relative to `root`, with `/` separators, and its mode is 644.

4. **Order on the computer, and what each failure leaves.** After the `whoami` check the bridge
   uploads each file (`blob.begin` with its size and digest, which skips a blob the computer
   already holds, then `blob.put_part` and `blob.commit`), builds the tree with `exec.tree`, and
   calls `computer.share` last, with the tree, the receiver, each path with its digest, and the
   note.

   - A failure before `computer.share` is sent leaves no share and records nothing on the
     computer; blobs already committed and a tree already built stay in its store and count
     against its budget. The refusal carries `domain_disposition: not_committed` and names the
     tree if one was built.
   - A refusal by `computer.share` reaches the caller unchanged (section 4).
   - A failure after `computer.share` was sent is `computer_unreachable` with
     `domain_disposition: unknown` (section 5), names the tree, and is never sent again
     (section 4). Whether the share was recorded is the computer's to answer.

5. **The record is the computer's.** As in section 4, the store writes no receipt and no dispatch
   row for a share: the computer's share record, which names the actor its key belongs to as the
   sharer, is the record. `bridge.diagnostics` counts shares per target in memory.

6. **What the bridge never does.** It reads no path outside `root` and follows no link; it puts
   no file byte in a result, a refusal, a log or tracing field, `bridge.diagnostics` or a daemon
   frame (a result carries paths, digests and sizes); it sends a target's key only to that
   target's `url`; and without a `[share]` table it reads no file and refuses `bridge.share` with
   `share_not_configured`, naming the configuration file. The tool description lists
   `bridge.share` only when the caller's configuration has a `[share]` table.

7. **Refusal kinds.** `share_not_configured`; `share_target_unknown`; `share_path_refused`
   (naming the path and the rule it broke); `share_too_large` (a file over `max_file_bytes`, or
   the count or total over its limit, naming which); `share_file_changed`; `invalid_input` (an
   empty `paths`, a repeated path, a `note` over 1024 bytes, or another operation in the same
   request). The section 3 and section 5 kinds apply to a target as they do to a computer.

### Acceptance

Each arm is stated before code exists. They run against the test computer of the base Acceptance,
which gains a stand-in `computer.share` that derives each digest from the tree's own entries,
refuses a mismatch, and records the share in its own store; its request log shows every operation
it received.

- **S1. One file.** A file under `root` shared to a named receiver yields a tree on the test
  computer whose entry at that path holds the file's bytes; the stand-in's record names the
  receiver, the digest and the actor of the target key; the reply's `share_id` is that record's.
- **S2. The digest.** The reply's digest equals the BLAKE3 of the file computed by the test and
  equals the tree entry's reference.
- **S3. Bounds refuse before anything is sent.** An absolute path, a `..` component, a symbolic
  link inside `root` to a file inside `root`, a path through a linked directory, a directory, a
  FIFO, a file of `max_file_bytes` plus one byte, a total over `max_total_bytes`, more than
  `max_files` paths, a repeated path and an empty `paths` are each refused with the kind item 7
  names, and the test computer's request log gains no operation.
- **S4. S3 can fail.** With the link check removed, the inside-link case of S3 reaches the test
  computer.
- **S5. Unknown target.** `share_target_unknown`, and no connection is opened.
- **S6. Wrong key.** A target key of another actor is refused with `computer_identity_mismatch`
  before any upload.
- **S7. A computer refusal passes through.** With the stand-in refusing every share as a digest
  mismatch, the caller receives that refusal unchanged, the tree exists, and the stand-in records
  nothing.
- **S8. Unknown outcome, one call.** With the connection cut after `computer.share` is sent, the
  reply is `computer_unreachable` with `domain_disposition: unknown` and names the tree, and the
  stand-in received `computer.share` exactly once.
- **S9. A file that changes.** Through a test seam between the size check and the read, a file
  that grows is refused with `share_file_changed` and is not uploaded.
- **S10. No bytes and no key surface.** With a fixture key and a fixture file of known bytes, no
  reply, refusal, log or tracing output, `bridge.diagnostics` snapshot or daemon frame holds the
  key or the file's bytes, across S1, S3, S6, S7 and S8.
- **S11. Configuration rules.** A relative `root`, a target with a plain `http` URL to a
  non-loopback host, two targets with one name, a limit over its maximum, and a `[share]` table
  with no target each fail startup. Without a `[share]` table, `bridge.share` is refused with
  `share_not_configured` and no file is opened.
- **S12. Alone in its request.** `bridge.share` beside another operation, in a batch or a chain,
  is refused with `invalid_input` and nothing is sent.

### Consequences

- A configuration gains one optional table. A build without this amendment ignores the table,
  since unknown top-level tables are accepted, and does not know `bridge.share`, so a share fails
  there before any file is read.
- Each target needs its own key from that computer's owner, and the receiving computer must know
  the sending actor to issue it.
- A share that fails before its last call leaves blobs and possibly a tree in the target's store.
- The whole of `root` is shareable by any request the bridge serves, so `root` is chosen as a
  directory that holds only what may leave the machine.

### Not covered

- Sharing a directory or a pattern: paths name files one by one.
- Revoking a share or removing a shared tree; the computer keeps its share records append-only.
- The receiving side: the receiver reads the tree with the file verbs its computer already grants.
- Sharing from one computer to another: the files are read on the sending caller's machine.
