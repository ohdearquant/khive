# ADR-178: Plan-Only Request — Grammar Check Without Dispatch

- **Status**: Proposed
- **Date**: 2026-09-07
- **Extends**: [ADR-016](ADR-016-request-dsl.md) (Request DSL: the one `request` tool, three
  syntactic forms, the parser this record reuses)
- **Depends on**: [ADR-096](ADR-096-warm-daemon-per-request-identity.md) (per-request identity; a
  plan-only request carries none)
- **Relates to**: [ADR-120](ADR-120-khive-flow-control-flow-envelope.md) (control flow inside the
  request envelope; out of scope here), [ADR-131](ADR-131-batch-write-admission-control.md)
  (admission control; explicitly not what this record provides)

## Context

An agent runtime that lets a model author `request(ops)` strings wants to refuse a malformed string
before it spends a turn on it. Today the only way to learn whether a string parses is to send it: the
daemon parses, then dispatches every stage that parses, and the caller learns the grammar error from
the first failed stage's result row. For a chain that is one round trip and possibly committed
prefix stages before the grammar error surfaces in a later stage's arguments.

Runtimes that need this have started writing their own copy of the grammar (a second parser that
must track ADR-016 and the conformance corpus). A second parser is a fork of the wire format and will
drift. The parser already exists once, in `crates/khive-request` (`parse_request` returning
`ParsedRequest { ops, mode }` or a typed `DslError`); it belongs to no transport and can answer the
question without dispatching anything.

What this record is not. A parse is not admission. The resolved target of a `$prev` reference does
not exist at parse time, a lease that moves between two stages is invisible to a parse, and a verb
that parses may still be refused by the gate at the moment it runs. Admission stays where it is:
one `GateRequest` per dispatched verb, at the effect, with the resolved arguments in hand
(ADR-131 for write batches, the gate for every verb). A runtime may not key any admission decision on
a plan result beyond "parses / does not parse, and how many stages".

## Decision

### D1. `plan` is an envelope field of the `request` tool, and a flag of `kkernel exec`

`request(ops, plan=true)` parses `ops` with the same `parse_request` the dispatch path uses and
returns the plan below. Nothing is dispatched, no identity is minted, no gate runs, no audit event
is appended, no store is touched. The CLI form is `kkernel exec --plan '<ops>'`. The Python client
exposes `Session.plan(ops)` returning the same object.

`plan=true` composes with `ops` only. The other envelope fields of the `request` tool, by their wire
names `presentation`, `presentation_per_op`, `format`, `format_per_op`, `save_to` and `request_id`, are
rejected with `invalid_params` naming the field when present beside `plan=true`, so a plan result can
never be mistaken for a dispatch result by shape.

### D1a. The daemon frame carries the same flag

`kkernel exec --plan` and `Session.plan` do not speak MCP; they send a `DaemonRequestFrame` to the warm
daemon, and the daemon's normal path always dispatches `ops`. The frame therefore gains a boolean `plan`
field (default false, the shape `probe_only` and `metrics_only` already use), and the connection handler
answers it before `dispatch` is reached, on the same `parse_request`, with the plan object in the
response's `result`. `presentation`, `presentation_per_op`, `format`, `format_per_op` and `request_id`
beside `plan` on the frame are rejected the same way. The frame's protocol version is bumped with the
field, so a daemon that predates it refuses the frame with `version_mismatch` instead of dispatching the
ops as an ordinary request; a plan is never silently executed by an older daemon.

### D2. The plan result

```json
{
  "parsed": true,
  "mode": "chain",
  "stage_count": 2,
  "stages": [
    {
      "index": 0,
      "verb": "create",
      "pack": "kg",
      "known": true,
      "args": { "kind": "note", "content": "..." },
      "prev_refs": []
    },
    {
      "index": 1,
      "verb": "memory.remember",
      "pack": "memory",
      "known": true,
      "args": { "source_id": "$prev.id", "memory_type": "episodic" },
      "prev_refs": ["id"]
    }
  ],
  "limits": { "max_ops": 100, "max_depth": 64, "max_input_len": 1048576 }
}
```

- `parsed` is false exactly when `parse_request` returns an error; then `error` carries the same
  `DslError` text the dispatch path would have returned for the same string, and `stages` is absent.
- `mode` is the `ExecutionMode` the parser chose (single, parallel, chain).
- `verb` and `pack` come from the loaded `VerbRegistry` catalog read, the same read `verbs()` does;
  `known` is false for a verb the registry does not have. A catalog read is not admission: it says
  the name exists, nothing about whether this caller may call it.
- `args` are the parser's normalized arguments (JSON literals decoded, strings unescaped); `$prev`
  references stay as the literal `$prev.<path>` string and are listed in `prev_refs`.
- `limits` echoes the parser's three caps, op count, nesting depth and raw input length in bytes, so a
  caller can size a string before sending it.

### D3. Same grammar, same errors, one parser

The plan path and the dispatch path call the same function on the same input. A grammar error
observed through `plan=true` is the error dispatch would have produced, byte for byte, for that
string. The conformance corpus the parser already carries is the test surface for both paths; no
second corpus is introduced.

### D4. What a plan result may be used for

A caller may refuse a string that does not parse, count stages for budgeting, and list the verbs a
string names for a catalog check. A caller may not treat a plan as permission, as a lease check, or
as evidence that a `$prev` target will resolve. The result carries no identity and no admission field
so that nothing downstream can be written to depend on one.

## Consequences

- One parser stays one parser. Runtimes that were porting the grammar delete the port.
- A plan costs a parse and a catalog read; it never takes the writer, never appends an audit row.
- The MCP `request` schema is closed (ADR-016; the schema test rejects unknown envelope fields), so
  adding `plan` is a schema change published in the tool description and in `request(help=true)`.
- Out of scope, deliberately: dry-run of effects, resolved `$prev` targets, gate pre-checks, cost
  estimates. Each of those is admission or execution in disguise.

## Acceptance

1. **Same error, same text.** For every string in the parser's conformance corpus that fails to
   parse, `request(ops, plan=true)` returns `parsed=false` with an `error` equal to the dispatch
   path's error for the same string. Test walks the corpus.
2. **No effect.** A valid chain of two writes sent with `plan=true` leaves `stats()` unchanged,
   appends no audit event (event store count unchanged), and mints no identity (daemon log shows no
   `RequestIdentity` for the request). Control: the same string without `plan` changes all three.
3. **Stage accounting.** A three-stage chain returns `stage_count=3`, `mode="chain"`, and
   `prev_refs` listing exactly the `$prev` paths each stage uses.
4. **Unknown verb is not an error.** A string naming a verb the registry lacks returns
   `parsed=true`, `known=false` for that stage, and no error.
5. **Envelope isolation.** On the MCP envelope, `plan=true` beside any of `presentation`,
   `presentation_per_op`, `format`, `format_per_op`, `save_to` or `request_id` is refused with
   `invalid_params` naming the offending field; on the daemon frame, which carries no `save_to`, the
   same for the other five.
6. **Three surfaces, one result.** The MCP tool, `kkernel exec --plan`, and `Session.plan` return
   structurally equal results for the same string. The two daemon-frame surfaces send `plan=true` on
   the frame; a frame with `plan=true` sent to a daemon at the previous protocol version is answered
   with `version_mismatch` and nothing is dispatched (test against a stub daemon at the old version).
7. **No admission leak.** A grep of the plan handler's call graph reaches no gate, no store and no
   identity constructor; the test asserts the handler's dependencies by module.

## Implementation notes

- Handler in `crates/khive-mcp/src/tools/request.rs` next to the `help` short-circuit: `plan`
  is read before dispatch, the parser is called, and the result is returned on the same path `help`
  uses. `kkernel exec` gains `--plan` in `crates/kkernel/src/exec.rs`.
- Catalog read through the registry's existing verb listing; no new registry API.
- `crates/khive-runtime/src/daemon.rs`: `DaemonRequestFrame` gains `plan: bool` (serde default), the
  connection handler takes the plan arm beside `probe_only`, before `dispatch`, and `PROTOCOL_VERSION`
  is bumped with the field.
- `python/khive/transport.py`: `Session.plan(ops)` sends the daemon frame with `plan=true` and
  returns the decoded object; `envelope.py` gains the plan shape beside the results envelope.
