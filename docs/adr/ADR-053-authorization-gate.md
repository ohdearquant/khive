# ADR-053: Authorization Gate Dispatch Contract

**Status**: Proposed\
**Date**: 2026-06-13\
**Depends on**: [ADR-016](./ADR-016-request-dsl.md),
[ADR-017](./ADR-017-pack-standard.md),
[ADR-018](./ADR-018-authorization-gate.md)

---

## Context

ADR-018 defines a mandatory authorization check at the verb-dispatch boundary. The public
distribution supports local execution and exposes the gate types needed to evaluate a request.
Network authentication and external identity resolution are outside this repository's contract.

This ADR narrows the integration contract between dispatch, the gate, namespace capabilities, and
handlers. It does not define credentials or a transport-specific identity protocol.

## Decision

### 1. Dispatch performs exactly one authoritative gate check

Every public verb invocation is converted into a `GateRequest` containing the caller attribution,
requested namespace, canonical verb name, arguments, and gate context. Dispatch calls
`Gate::check` before invoking a handler.

```rust
pub trait Gate: Send + Sync + std::fmt::Debug {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError>;
}
```

A denial or gate error stops dispatch. No handler side effect may occur before an allow decision.

### 2. Namespace access is capability-based

After an allow decision, dispatch mints a `NamespaceToken` scoped to the authorized namespace and
passes it to the handler. Handlers use the token when accessing runtime stores and do not accept an
independent namespace string for the same operation.

The token is an in-process capability, not a serialized credential. It must not be accepted from a
request payload or reconstructed by pack handlers.

### 3. Canonical verbs are authorized

Aliases are resolved before the gate check. The gate receives the canonical verb identifier, so a
policy cannot be bypassed by choosing another spelling for the same operation. Invalid or ambiguous
aliases fail before handler invocation.

### 4. Nested calls retain or reduce authority

A handler that invokes another registered verb uses the runtime's nested-dispatch path. The nested
request retains the parent attribution and namespace capability unless it is explicitly narrowed.
It cannot select a broader namespace or replace the parent attribution.

Each nested public verb is checked by the gate under its canonical identifier. Direct helper calls
inside one handler are implementation details and remain within that handler's original decision.

### 5. Local attribution is configuration-derived

The local command and daemon surfaces resolve attribution from the effective runtime configuration
before dispatch. This value is used consistently for gate input and audit output. Request arguments
cannot override it.

Any future network transport must define its own authenticated attribution mechanism in a separate
security specification before it can expose this dispatch surface.

## Security invariants

1. Gate evaluation completes before any handler side effect.
2. Gate errors fail closed.
3. Alias resolution cannot change the operation after authorization.
4. Pack handlers cannot mint or widen namespace capabilities.
5. Nested dispatch cannot widen attribution or namespace authority.
6. Request payloads cannot choose the caller attribution.
7. Audit output records the same canonical verb and attribution evaluated by the gate.

## Failure behavior

| Condition                                  | Result                                              |
| ------------------------------------------ | --------------------------------------------------- |
| Gate returns deny                          | Stable authorization error; handler is not called   |
| Gate returns error                         | Stable authorization error; handler is not called   |
| Alias is invalid or ambiguous              | Parse or resolution error before authorization      |
| Handler lacks a valid namespace capability | Runtime authorization error                         |
| Nested call requests broader authority     | Authorization error before nested handler execution |

Errors returned to callers must not include policy source, secrets, or backend connection details.
Detailed diagnostics may be emitted to local audit logs subject to the configured logging policy.

## Consequences

### Positive

- Authorization remains centralized at one dispatch seam.
- Handlers receive a capability rather than interpreting namespace strings independently.
- Alias and nested-call behavior are explicit and testable.
- The public contract does not imply an unauthenticated network boundary.

### Tradeoffs

- New public verbs must register canonical gate metadata.
- Nested verb calls incur another gate evaluation.
- Network transports require an additional, separately reviewed identity specification.

## Testing requirements

- Deny and error decisions produce no handler side effects.
- Every alias is resolved to the same canonical verb before policy evaluation.
- Tokens cannot be supplied through request JSON.
- Nested dispatch preserves or narrows the parent capability.
- Audit records match the gate request for both allowed and denied calls.
- Local configuration attribution cannot be overridden by verb parameters.

## References

- [ADR-016](./ADR-016-request-dsl.md): request parsing and canonicalization
- [ADR-017](./ADR-017-pack-standard.md): pack and verb registration
- [ADR-018](./ADR-018-authorization-gate.md): gate types and enforcement point
- [ADR-050](./ADR-050-kg-token-namespace-contract.md): namespace-token use by the KG pack
