# Response wire contract

This document describes the native JSON shapes currently emitted for substrate
`list` operations and per-operation failures. It records source behavior; it does
not add server guarantees or describe every transport-level error. Examples omit
record bodies and unrelated envelope fields. Message wording is diagnostic text,
not a stable error discriminator.

## Pages

Offset-mode `list` results for entities, edges, and notes use `items` (R1):

```json
{
  "items": [],
  "requested_limit": 50,
  "effective_limit": 50,
  "limit_clamped": false
}
```

This renderer does not emit `total` or `next_offset`. A client can preserve those
fields when another response supplies them, but must not manufacture a count or
an offset continuation and attribute it to this server response. An absent count
means not counted, not zero.

Supplying `after=""` selects cursor mode from the beginning. Subsequent calls
supply the full UUID returned as `next_after`. The collection key is `entities`,
`edges`, or `notes`, according to the substrate; the limit metadata is still
present (R2). The Python facade exposes these rows as `Page.items`.

```json
{
  "notes": [],
  "next_after": "00000000-0000-0000-0000-000000000001",
  "requested_limit": 1,
  "effective_limit": 1,
  "limit_clamped": false,
  "scan_incomplete": true
}
```

This is a possible filtered-note result after exhausting the bounded scan before
finding enough matches. The cursor can refer to the last scanned row rather than
a returned row. Continue using `next_after` even when `items` is empty or shorter
than the effective limit. `next_after: null` is the normal cursor exhaustion
marker; `scan_incomplete: true` explicitly prevents an exhaustion conclusion.
Filtered offset-mode notes can also report `scan_incomplete: true`, without a
cursor. That response reports incomplete scanning, not a resumable scan token
(R3).

The current requested limit caps are 500 entities, 200 notes, and 1,000 edges.
For example, requesting 201 notes yields `requested_limit: 201`,
`effective_limit: 200`, and `limit_clamped: true`. These are response metadata,
not instructions to infer EOF from the caller's original requested limit (R4).

## Per-operation results

A request envelope carries `results` plus an aggregate `summary` with `total`,
`succeeded`, `failed`, and `aborted`. Aggregate `status` is `success` when neither
failed nor aborted operations exist, and `partial` otherwise. The result entries
remain in input order even when execution is parallel. A batch failure does not
roll back earlier or concurrent successes; callers must inspect individual
entries (R5).

```json
{
  "results": [
    {"ok": true, "tool": "create", "result": {"id": "example-id"}},
    {"ok": false, "tool": "get", "error": {
      "kind": "runtime_error", "message": "not found: example-id",
      "domain_disposition": "unknown"
    }}
  ],
  "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
  "status": "partial"
}
```

Per-operation errors are objects. `OpError` preserves their fields, including
unfamiliar fields. `OpResult.error` also accepts legacy strings and absent/null
errors; an old string error is not upgraded to a fabricated structured error.
A missing-ID `get` returns `RuntimeError::NotFound` from its handler, which the
server renders as a `runtime_error` object with unknown domain disposition (R6).

Every current error object carries `domain_disposition` (R14):

| Value | What this operation's dispatch boundary establishes |
| --- | --- |
| `committed` | Domain dispatch succeeded before later processing failed. |
| `not_committed` | The operation was refused before dispatch or explicitly asserts no write. |
| `unknown` | The domain outcome cannot be established from this failure. |

Post-dispatch obligation failures retain the successful value in `domain_result`:

```json
{
  "kind": "obligation",
  "code": "store_failure",
  "message": "audit failed",
  "domain_disposition": "committed",
  "domain_result": {"id": "persisted-row"}
}
```

Depth or byte limits can omit a committed result; `code` names the limit.
An absent result requires separate outcome resolution. A disposition other than
`not_committed` never grants permission to replay. A legacy missing field supplies
no evidence of non-commit. Success entries gain no disposition field. Python
exposes these keys without adding retries or changing when per-op failures raise.

Writer-task failures carry this object shape (R7):

```json
{
  "kind": "storage",
  "code": "writer_task_terminated",
  "stage": "writer_task_terminated",
  "message": "writer task terminated",
  "retryable": false,
  "request_state": "side_effects_unknown",
  "task_terminated": true,
  "domain_disposition": "unknown"
}
```

The exact current `request_state` strings are (R8):

| Value | What the writer can establish |
| --- | --- |
| `not_started` | The operation closure was never invoked. |
| `transaction_rolled_back` | The wrapping SQLite transaction was successfully rolled back. |
| `side_effects_unknown` | The exact outcome cannot be established; side effects may exist. |

Preserve these strings exactly. Preserve an unfamiliar value such as `unknown`
exactly too, without converting it to a claim of failure or rollback.
`task_terminated` describes writer-task liveness, separately from request finality.
`retryable` describes the source failure's retry policy; rollback alone does not
imply that policy is true. Do not infer known absence of writes from `ok: false`
or replay an ambiguous write automatically.

Retryable runtime failures use an `unavailable` object. The optional context
values are emitted as JSON null when unavailable (R9):

```json
{
  "kind": "unavailable",
  "code": "writer_queue_saturated",
  "stage": "writer_queue_saturated",
  "message": "write queue full",
  "retryable": true,
  "timeout_ms": 100,
  "capability": null,
  "operation": null,
  "scope": "writer_admission",
  "retry_after_ms": 100,
  "domain_disposition": "unknown"
}
```

`scope: "writer_admission"` identifies the queue-admission case. Other
`unavailable` failures can have `scope: null`; do not infer that every retryable
failure occurred before queue acceptance. The model preserves `kind`, `code`,
`stage`, `message`, `retryable`, `timeout_ms`, `capability`, `operation`, `scope`,
and `retry_after_ms` when present.

The `RuntimeError::Khive` arm serializes `KhiveError`, then adds the disposition
from its dispatch boundary; a keyed-memory `key_conflict` reads `not_committed` and a
`key_holder_unresolved` reads `unknown` whatever the boundary says, because those
outcomes carry their own domain proof. It has `kind` and `message`, with nullable `code` and
`details`. A populated code is a string such
as `runtime:10`; populated details are a string-to-string map. Retry hints are
not serialized as a field on this type (R10).

Two named keyed-memory outcomes additionally carry `domain_disposition`:
`conflict` with `details.reason == "key_conflict"` carries `"not_committed"`,
and `unavailable` with `details.reason == "key_holder_unresolved"` carries
`"unknown"`. This does not classify other errors by their kind. A key conflict
terminates reconciliation at `details.existing_id`; `not_committed` describes
the replay attempt, not the earlier holder's write. The client preserves these
fields and does not automatically retry the operation.

```json
{
  "kind": "not_found",
  "message": "entity not found: example-id",
  "code": null,
  "details": null,
  "domain_disposition": "unknown"
}
```

These error families are not exhaustive. For example, post-dispatch depth refusal
emits `kind` and `code` as `result_too_deep`, `message`, and a committed disposition
without `domain_result`. Preserving
extra fields and admitting a message-only object avoids imposing a closed
client-side taxonomy on evolving server responses (R11).

## Daemon errors

Protocol v4 keeps the top-level frame `error` as text for unchanged peers. The
optional additive `error_detail` carries the structured object. The MCP adapter
preserves it as error data; native Python frame failures expose it as
`exception.error_detail`, an `OpError` model. A legacy frame without detail keeps
that attribute `None` (R15).

Version mismatches are always unknown, including responses from an older peer.
A frame-cap replacement can cover mixed per-op outcomes and therefore reports
unknown with no `domain_result`. Neither error initiates a new Python retry.
Existing config-mismatch recovery remains a separate handshake path.

## Aborted entries and correlation

After a chain operation fails, later operations are not executed. The ordinary
chain arm emits `ok: false`, `tool`, `aborted: true`,
`domain_disposition: "not_committed"`, and a top-level `message`,
without an `error` field. The strict-fallback arm omits that message as well (R12):

```json
{"ok": false, "tool": "get", "aborted": true, "domain_disposition": "not_committed"}
```

The existing Python envelope compatibility rule also admits the older minimal
shape `{"ok": false, "aborted": true}` and supplies an empty `tool` string.
That admission does not mean the current ordinary server arm omits `tool`.
An aborted entry must remain distinguishable from an attempted write whose
effects are unknown.

`request_id` correlates a request group with daemon responses and audit records.
Every operation in a batch or chain shares it. It is neither an operation-unique
identifier nor a cross-attempt idempotency key. Reusing it does not deduplicate a
write. Correlate per-operation results by their response position and inspect
their actual outcome; a partial batch is not permission to replay every operation
(R13).

## Source citations

Each quoted fragment below is a verbatim substring of one source line. The test
in `python/tests/test_response_wire_contract.py` checks citation presence in the
named file, not semantics, branch identity, completeness, or a live deployment.
A quote can recur at several sites. A passing presence check does not establish
that the associated explanation remains correct after surrounding code changes;
review those changes and use behavioral fixtures for the client obligations.

| Rule | Source-line citations |
| --- | --- |
| R1 | crates/khive-pack-kg/src/handlers/list.rs -- "fn render_list_response(items: Value, requested: u32, effective: u32) -> Value {"; crates/khive-pack-kg/src/handlers/list.rs -- "\"items\": items,"; crates/khive-pack-kg/src/handlers/list.rs -- "\"requested_limit\": requested,"; crates/khive-pack-kg/src/handlers/list.rs -- "\"effective_limit\": effective,"; crates/khive-pack-kg/src/handlers/list.rs -- "\"limit_clamped\": requested > effective," |
| R2 | crates/khive-pack-kg/src/handlers/list.rs -- "if raw.is_empty() {"; crates/khive-pack-kg/src/handlers/list.rs -- "uuid::Uuid::parse_str(raw).map(Some)"; crates/khive-pack-kg/src/handlers/list.rs -- "\"entities\": normalize_entity_timestamps_array(to_json(&entities)?),"; crates/khive-pack-kg/src/handlers/list.rs -- "\"edges\": to_json(&edges)?,"; crates/khive-pack-kg/src/handlers/list.rs -- "\"notes\": remapped,"; crates/khive-pack-kg/src/handlers/list.rs -- "\"next_after\": next_after,"; crates/khive-pack-kg/src/handlers/list.rs -- "add_list_limit_metadata(&mut response, requested, limit);" |
| R3 | crates/khive-pack-kg/src/handlers/list.rs -- "} else if raw_more && scanned >= MAX_SCAN_TOTAL {"; crates/khive-pack-kg/src/handlers/list.rs -- "!has_more_match && raw_more && scanned >= MAX_SCAN_TOTAL,"; crates/khive-pack-kg/src/handlers/list.rs -- "response[\"scan_incomplete\"] = Value::Bool(true);"; crates/khive-pack-kg/src/handlers/list.rs -- "let mut response = render_list_response(to_json(&remapped)?, requested, limit);" |
| R4 | crates/khive-pack-kg/src/handlers/list.rs -- "const ENTITY_LIST_CAP: u32 = 500;"; crates/khive-pack-kg/src/handlers/list.rs -- "const NOTE_LIST_CAP: u32 = 200;"; crates/khive-runtime/src/operations.rs -- "pub const EDGE_LIST_MAX_LIMIT: u32 = 1000;" |
| R5 | crates/khive-mcp/src/server.rs -- "results[index] = Some(entry);"; crates/khive-mcp/src/server.rs -- "\"summary\": { \"total\": total, \"succeeded\": succeeded, \"failed\": failed, \"aborted\": 0 },"; crates/khive-mcp/src/server.rs -- "if failed == 0 && aborted == 0 {"; crates/khive-mcp/src/server.rs -- "ops (reported as {\"ok\": false, \"aborted\": true}). Committed ops are not rolled back." |
| R6 | crates/khive-pack-kg/src/handlers/get.rs -- "Err(RuntimeError::NotFound(format!(\"not found: {id_ref}\")))"; crates/khive-mcp/src/server.rs -- "fn runtime_error_value("; crates/khive-mcp/src/server.rs -- "\"runtime_error\""; crates/khive-mcp/src/server.rs -- "error_with_disposition(payload, disposition)" |
| R7 | crates/khive-mcp/src/server.rs -- "other.writer_task_failure_context()"; crates/khive-mcp/src/server.rs -- "\"storage\""; crates/khive-mcp/src/server.rs -- "context.stage"; crates/khive-mcp/src/server.rs -- "other.to_string()"; crates/khive-mcp/src/server.rs -- "context.retryable"; crates/khive-mcp/src/server.rs -- "context.request_state.to_string()"; crates/khive-mcp/src/server.rs -- "context.task_terminated" |
| R8 | crates/khive-storage/src/error.rs -- "Self::NotStarted => \"not_started\","; crates/khive-storage/src/error.rs -- "Self::TransactionRolledBack => \"transaction_rolled_back\","; crates/khive-storage/src/error.rs -- "Self::SideEffectsUnknown => \"side_effects_unknown\","; crates/khive-runtime/src/error.rs -- "/// not be inferred from rollback finality alone." |
| R9 | crates/khive-mcp/src/server.rs -- "other.retryable_failure_context()"; crates/khive-mcp/src/server.rs -- "\"unavailable\""; crates/khive-mcp/src/server.rs -- "context.timeout.as_millis()"; crates/khive-mcp/src/server.rs -- "context.capability.map(storage_capability_wire_name)"; crates/khive-mcp/src/server.rs -- "context.operation"; crates/khive-mcp/src/server.rs -- "context.scope"; crates/khive-mcp/src/server.rs -- "context.retry_after_ms"; crates/khive-runtime/src/error.rs -- "pub const WRITER_ADMISSION_SCOPE: &str = \"writer_admission\";" |
| R10 | crates/khive-mcp/src/server.rs -- "RuntimeError::Khive(k) => serde_json::to_value(&k)"; crates/khive-types/src/khive_error.rs -- "pub struct KhiveError {"; crates/khive-types/src/khive_error.rs -- "code: Option<ErrorCode>,"; crates/khive-types/src/khive_error.rs -- "details: Option<Details>,"; crates/khive-types/src/khive_error.rs -- "s.serialize_str(&self.to_string())"; crates/khive-types/src/khive_error.rs -- "map.serialize_entry(k.as_ref(), v.as_ref())?;" |
| R11 | crates/khive-mcp/src/server.rs -- "fn depth_error_payload("; crates/khive-mcp/src/server.rs -- "\"kind\": \"result_too_deep\""; crates/khive-mcp/src/server.rs -- "\"code\": \"result_too_deep\""; crates/khive-mcp/src/server.rs -- "fn error_with_disposition(" |
| R12 | crates/khive-mcp/src/server.rs -- "fn aborted_entry("; crates/khive-mcp/src/server.rs -- "entry.insert(\"aborted\".into(), Value::Bool(true));"; crates/khive-mcp/src/server.rs -- "DomainDisposition::NotCommitted.as_str()"; crates/khive-mcp/src/server.rs -- "aborted_entry(op.tool.clone(), None)" |
| R13 | crates/khive-mcp/src/tools/request.rs -- "/// operation-unique id or a cross-attempt idempotency key."; crates/khive-mcp/src/tools/request.rs -- "pub request_id: Option<u64>," |
| R14 | crates/khive-runtime/src/error.rs -- "Self::Committed => \"committed\","; crates/khive-runtime/src/error.rs -- "Self::NotCommitted => \"not_committed\","; crates/khive-runtime/src/error.rs -- "Self::Unknown => \"unknown\","; crates/khive-mcp/src/server.rs -- "RuntimeError::AuditObligation"; crates/khive-mcp/src/server.rs -- "error.insert(\"domain_result\".into(), domain_result);"; python/khive/models.py -- "Literal[\"committed\", \"not_committed\", \"unknown\"]" |
| R15 | crates/khive-runtime/src/daemon.rs -- "pub error: Option<String>,"; crates/khive-runtime/src/daemon.rs -- "pub error_detail: Option<serde_json::Value>,"; crates/khive-runtime/src/daemon.rs -- "pub const PROTOCOL_VERSION: u32 = 6;"; crates/khive-mcp/src/daemon.rs -- "fn daemon_mcp_error("; crates/khive-mcp/src/daemon.rs -- "fn protocol_mismatch_error("; python/khive/transport.py -- "error_detail=_validate_frame_error_detail(response, \"daemon\")" |
