# khive-runtime Protocol

## Purpose

The runtime protocol defines how verb dispatches are routed from the MCP `request` surface
through the `VerbRegistry` to individual pack handlers, and how security, auditing, and
namespace attribution are enforced at each step (ADR-007 Rev 6: namespace is gate-policy
input, not a storage access boundary).

## ADR Links

- ADR-017 — Pack trait, verb surface, and boot-time collision checks
- ADR-023 — Declarative pack format and verb visibility
- ADR-027 — Dynamic pack loading via self-registration
- ADR-028 — Pack-scoped backends and schema declaration
- ADR-007 — Namespace attribution and read visibility
- ADR-050 — NamespaceToken authority contract

## Dispatch Flow

```
MCP request(ops=...) → khive_request::parse_request → Vec<ParsedOp>
  for each ParsedOp:
    VerbRegistry::dispatch(verb, params)
      → help=true? → describe_verb() [short-circuit, no gate]
      → Gate::check(GateRequest) → Allow|Deny
          Deny → RuntimeError::PermissionDenied [pack not invoked]
          Allow → first matching pack.dispatch(verb, params)
                  → RuntimeResult<Value>
      → EventStore::append(audit_event) [if configured]
      → DispatchHook::on_dispatch(event) [if configured]
```

## Verb Visibility Contract

- `Visibility::Verb` — callable via MCP `request` surface, advertised in `help=true` envelopes.
- `Visibility::Subhandler` — internal / operator-only. `help=true` returns an envelope with
  `callable_via_mcp: false`.
- Subhandler blocking is enforced at the **MCP wire boundary** (`khive-mcp`'s request
  handling rejects non-help `Visibility::Subhandler` calls before they reach the runtime).
  `VerbRegistry::dispatch` itself does not block on visibility — direct/operator dispatch
  (e.g. `khived` local calls, tests) may invoke internal subhandlers.

## Request Schema

The `describe_verb` response shape (issue #287):

```json
{
  "verb": "<name>",
  "pack": "<pack-name>",
  "description": "...",
  "category": "<VerbCategory>",
  "params": [
    { "name": "...", "type": "...", "required": true, "description": "..." }
  ]
}
```

For subhandlers, the envelope additionally carries `"visibility": "internal"` and
`"callable_via_mcp": false`.

## Invariants

- One pack per verb at boot: duplicate verb names across packs produce `RuntimeError::VerbCollision`.
- Gate is consulted before every dispatch. Gate infrastructure errors are fail-open by design.
- Namespace is attribution and gate-policy input (ADR-007 Rev 6, ADR-050): it is minted into
  the dispatch `NamespaceToken`'s read/write scope, not re-checked per record. By-ID
  operations (get, delete, update) resolve globally unique UUIDs without a namespace
  equality check; `merge_entity`/`merge_note` are the exception and still require a
  namespace match.
- A present but non-string `namespace` request param (`null`, number, boolean, array,
  object) is rejected with `RuntimeError::InvalidInput` before the gate is consulted —
  it is never coerced to the default namespace (RUNTIME-AUD-002 / #433 fail-closed contract).

## Failure Modes

| Condition                    | Error                                                                       |
| ---------------------------- | --------------------------------------------------------------------------- |
| Unknown verb                 | `RuntimeError::InvalidInput("unknown verb ...")`                            |
| Gate deny                    | `RuntimeError::PermissionDenied { verb, reason }`                           |
| Pack not loaded              | `RuntimeError::InvalidInput` (unknown verb path)                            |
| Malformed explicit namespace | `RuntimeError::InvalidInput` (non-string `namespace`, rejected before gate) |

`RuntimeError::NamespaceMismatch` is a historical/rejected variant from a pre-Rev-6
design where by-ID lookups compared `record.namespace == caller_namespace`; it is not
part of the current by-ID contract described above.

## Extension Points

- Add a new pack: implement `Pack + PackRuntime`, call `VerbRegistryBuilder::pack()`.
- Add a gate: implement `Gate`, call `VerbRegistryBuilder::with_gate()`.
- Add an audit sink: implement `EventStore`, call `VerbRegistryBuilder::with_event_store()`.
- Add a post-dispatch hook: implement `DispatchHook`, call `VerbRegistryBuilder::with_dispatch_hook()`.
