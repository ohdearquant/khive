# khive-runtime Protocol

## Purpose

The runtime protocol defines how verb dispatches are routed from the MCP `request` surface
through the `VerbRegistry` to individual pack handlers, and how security, auditing, and
namespace isolation are enforced at each step.

## ADR Links

- [ADR-017](../../docs/adr/ADR-017-pack-standard.md) — Pack trait, verb surface, and boot-time collision checks
- [ADR-023](../../docs/adr/ADR-023-declarative-pack-format.md) — Declarative pack format and verb visibility
- [ADR-027](../../docs/adr/ADR-027-dynamic-pack-loading.md) — Dynamic pack loading via self-registration
- [ADR-028](../../docs/adr/ADR-028-pack-scoped-backends.md) — Pack-scoped backends and schema declaration
- [ADR-007](../../docs/adr/ADR-007-namespace-strategy.md) — Namespace strategy and isolation requirements
- [ADR-050](../../docs/adr/ADR-050-kg-token-namespace-contract.md) — NamespaceToken authority contract

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
- `Visibility::Subhandler` — internal only; `dispatch` returns `PermissionDenied`. `help=true`
  returns an envelope with `callable_via_mcp: false`.

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
- Gate is consulted before every dispatch. Gate infrastructure errors are fail-open (ADR-018).
- Namespace token authority is checked in every ID-based operation at the runtime layer.
  Storage is ID-only; the runtime is the trust boundary (ADR-007, ADR-050).

## Failure Modes

| Condition          | Error                                                              |
| ------------------ | ------------------------------------------------------------------ |
| Unknown verb       | `RuntimeError::InvalidInput("unknown verb ...")`                   |
| Gate deny          | `RuntimeError::PermissionDenied { verb, reason }`                  |
| Pack not loaded    | `RuntimeError::InvalidInput` (unknown verb path)                   |
| Namespace mismatch | `RuntimeError::NamespaceMismatch` (reported as NotFound to caller) |

## Extension Points

- Add a new pack: implement `Pack + PackRuntime`, call `VerbRegistryBuilder::pack()`.
- Add a gate: implement `Gate`, call `VerbRegistryBuilder::with_gate()`.
- Add an audit sink: implement `EventStore`, call `VerbRegistryBuilder::with_event_store()`.
- Add a post-dispatch hook: implement `DispatchHook`, call `VerbRegistryBuilder::with_dispatch_hook()`.
