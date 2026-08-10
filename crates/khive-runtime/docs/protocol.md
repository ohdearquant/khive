# khive-runtime Protocol

## Purpose

The runtime protocol defines how verb dispatches are routed from the MCP `request` surface
through the `VerbRegistry` to individual pack handlers, and how security, auditing, and
namespace attribution are enforced at each step (ADR-007 Rev 6: namespace is gate-policy
input, not a storage access boundary).

## ADR Links

- [ADR-017](../../../docs/adr/ADR-017-pack-standard.md) — Pack trait, verb surface, and boot-time collision checks
- [ADR-023](../../../docs/adr/ADR-023-declarative-pack-format.md) — Declarative pack format and verb visibility
- [ADR-027](../../../docs/adr/ADR-027-dynamic-pack-loading.md) — Dynamic pack loading via self-registration
- [ADR-028](../../../docs/adr/ADR-028-pack-scoped-backends.md) — Pack-scoped backends and schema declaration
- [ADR-007](../../../docs/adr/ADR-007-namespace.md) — Namespace attribution and read visibility
- [ADR-050](../../../docs/adr/ADR-050-kg-token-namespace-contract.md) — NamespaceToken authority contract

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

When the configured main backend is read-only, the registry intentionally
holds no `EventStore`: a known-failing append is not attempted. Successful
non-help operation entries instead carry an `advisories` array beside their
canonical `result`, with code `audit_persistence_skipped_read_only`. Failed and
aborted entries receive no such advisory because they do not claim a successful
operation whose audit persistence was skipped.

Read-only versus writable main-backend mode is engine-coherence state, not
request identity. It is folded into the warm-daemon `config_id`, so a snapshot
inspection request cannot be served by a daemon that retained a write-capable
handle to the same database path.

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
  "identifier_resolution": {
    "full_uuid": "A complete UUID spelling accepted by the consuming parameter directly names one globally unique record; ...",
    "short_prefix": "A short UUID prefix is at least 8 undashed hexadecimal characters that do not parse as a complete UUID; ...",
    "parameter_rule": "A parameter that requires a full UUID rejects prefixes and explains the resolution consequence; ..."
  },
  "params": [
    { "name": "...", "type": "...", "required": true, "description": "..." }
  ]
}
```

`identifier_resolution` is the shared ID contract for every verb. A complete UUID
directly identifies a record without a namespace search. Alternate complete spellings
are parameter/parser-specific; accepted forms normalize to canonical lowercase dashed
UUIDs in strict responses. Thirty-two undashed hexadecimal characters are complete
compact input, not a prefix. A short prefix has at least eight undashed hexadecimal
characters, does not parse as a complete UUID, and can miss or be ambiguous.
Its scope belongs to the consuming
parameter: operations governed by ADR-007's by-ID contract resolve without a
namespace filter (Rule 2), while other resolvers may search only in the caller's
primary namespace. Full-UUID-only parameters reject prefixes with the consequence
explained, and their response fields remain canonical so callers can submit them
back unchanged.

For subhandlers, the envelope additionally carries `"visibility": "internal"` and
`"callable_via_mcp": false`.

## Invariants

- One pack per verb at boot: duplicate verb names across packs produce `RuntimeError::VerbCollision`.
- Gate is consulted before every dispatch. Gate infrastructure errors are fail-open (ADR-018).
- Namespace is attribution and gate-policy input (ADR-007 Rev 6, ADR-050): it is minted into
  the dispatch `NamespaceToken`'s read/write scope, not re-checked per record. By-ID
  operations (get, delete, update) resolve globally unique UUIDs without a namespace
  equality check; `merge_entity`/`merge_note` are the exception and still require a
  namespace match.
- A present but non-string `namespace` request param (`null`, number, boolean, array,
  object) is rejected with `RuntimeError::InvalidInput` before the gate is consulted —
  it is never coerced to the default namespace (RUNTIME-AUD-002 / #433, ADR-018 fail-closed).

## Failure Modes

| Condition                                             | Error                                                                       |
| ----------------------------------------------------- | --------------------------------------------------------------------------- |
| Unknown verb                                          | `RuntimeError::UnknownVerb("unknown verb ...")`                             |
| Gate deny                                             | `RuntimeError::PermissionDenied { verb, reason }`                           |
| Pack not loaded                                       | `RuntimeError::UnknownVerb` (unknown verb path)                             |
| Malformed explicit namespace                          | `RuntimeError::InvalidInput` (non-string `namespace`, rejected before gate) |
| Read-only audit backend after a successful inspection | Success plus `audit_persistence_skipped_read_only` advisory                 |

`RuntimeError::NamespaceMismatch` is a historical/rejected variant from a pre-Rev-6
design where by-ID lookups compared `record.namespace == caller_namespace`; it is not
part of the current by-ID contract described above.

## Extension Points

- Add a new pack: implement `Pack + PackRuntime`, call `VerbRegistryBuilder::pack()`.
- Add a gate: implement `Gate`, call `VerbRegistryBuilder::with_gate()`.
- Add an audit sink: implement `EventStore`, call `VerbRegistryBuilder::with_event_store()`.
- Add a post-dispatch hook: implement `DispatchHook`, call `VerbRegistryBuilder::with_dispatch_hook()`.
