# ADR-109: Sandboxed kkernel Gateway for Untrusted Execution (Phase C)

**Status**: Proposed
**Date**: 2026-07-11
**Authors**: khive maintainers
**Depends on**: [ADR-007](./ADR-007-namespace.md), [ADR-016](./ADR-016-request-dsl.md),
[ADR-017](./ADR-017-pack-standard.md), [ADR-018](./ADR-018-authorization-gate.md)

## Context

The ordinary MCP request surface assumes a caller that may reach the registered verb
catalog, subject to the configured authorization Gate. Some integrations instead execute
untrusted or externally influenced input and need a narrower contract. Giving those callers
the full request surface, or relying on operator discipline alone, does not provide a
fail-closed boundary.

The required boundary has six properties:

- a closed verb allowlist;
- a namespace fixed by the gateway rather than by request arguments;
- enforced rate and resource budgets;
- no operator-only command surface;
- no caller-controlled host filesystem paths; and
- denial on unknown input, policy failure, or enforcement failure.

This ADR defines that boundary without adding a second verb implementation path.

## Decision

Introduce a **gateway contract**: a closed set of canonical verb identifiers and permitted
argument shapes, a fixed namespace, and enforced request budgets. A dedicated gateway
process validates the contract before forwarding an accepted request to the normal
`VerbRegistry::dispatch` path. The existing Gate remains authoritative for authorization;
the gateway is an additional, stricter precondition.

The gateway process is structurally distinct from the unconstrained MCP entry point. It may
proxy accepted requests to the warm daemon, but the untrusted caller cannot reach that
daemon directly. A launch flag on the ordinary MCP process is insufficient because omitting
the flag would silently restore the full surface.

### Hard rules

1. **Canonical verb allowlist.** The gateway checks the canonical `pack.verb` identifier.
   Aliases are canonicalized before comparison. An absent verb is denied before dispatch.
2. **Pinned namespace.** The contract supplies the namespace. A caller-provided namespace is
   rejected; it is never allowed to override the contract.
3. **No operator command surface.** CLI administration commands are not gateway
   capabilities. A future verb that wraps administrative behavior is denied unless a new
   architectural decision explicitly admits it.
4. **No host-path arguments.** A capability either excludes a path-bearing verb or declares
   an argument constraint that rejects absolute paths, `file:` URLs, path traversal, and
   platform path separators as appropriate for that field.
5. **Enforced caps.** Request-count and resource budgets are checked before dispatch. A
   declared but unenforced policy obligation does not satisfy this rule.
6. **Fail closed.** Unknown verbs, invalid argument shapes, namespace overrides, Gate
   errors, budget-store errors, exhausted budgets, and malformed contracts all deny.

### Caller identity

The gateway accepts only an identity established by its transport. That identity is passed
to the existing Gate as an `ActorRef`; callers cannot submit or replace it in request
arguments. This ADR does not define a credential issuance protocol. Deployments that cannot
establish a trustworthy transport identity must not expose the gateway.

### Capability declaration

Contracts use the existing policy engine with a restricted, validated policy profile:

- default decision is deny;
- the canonical verb allowlist is finite and explicit;
- every permitted verb has an argument schema;
- the namespace is exactly one configured value; and
- numeric caps are finite and non-negative.

Startup validation rejects a policy that lacks any of these declarations. Runtime policy
evaluation cannot widen the validated allowlist.

### Dispatch order

For each request, the gateway:

1. parses the request using the standard request grammar;
2. canonicalizes the verb identifier;
3. validates the verb, argument shape, and pinned namespace;
4. authenticates the transport identity;
5. reserves capacity in the rate and resource counters;
6. invokes the existing Gate;
7. dispatches the request through the ordinary registry; and
8. finalizes accounting and returns the ordinary response envelope.

Failure before step 7 produces a denial response and no handler invocation. Capacity
reservation and release must be atomic with respect to concurrent requests.

## Threat model

**Instruction injection.** Attacker-controlled content may steer a caller toward verbs or
arguments outside the intended capability. The closed allowlist, argument schemas, and
fail-closed behavior bound the reachable surface.

**Data exfiltration.** A permitted read verb may otherwise traverse beyond its intended
scope. Namespace pinning is mandatory, and per-verb schemas may further cap result count,
depth, or fan-out.

**Resource exhaustion.** A caller may issue expensive requests repeatedly or concurrently.
The gateway enforces request and resource budgets before shared daemon work begins.

**Policy degradation.** Missing policy data, evaluation errors, stale budget state, or a
malformed response from an enforcement dependency deny. There is no permissive fallback.

The gateway does not replace operating-system isolation. Process sandboxing and transport
access control remain complementary controls.

## Implementation requirements

- Build a dedicated thin gateway binary or equivalent structurally separate entry point.
- Keep parsing and handler execution on the existing request and registry implementations.
- Add startup validation for the restricted policy profile.
- Add an atomic counter service for request and resource caps.
- Pin caller identity and namespace in gateway-owned request context.
- Emit audit information for allow, deny, counter reservation, and policy failure without
  including secrets or raw untrusted content.

## Verification

Tests must prove:

- every unlisted or aliased-to-unlisted verb is denied;
- caller-supplied namespace values cannot escape the pinned namespace;
- malformed and path-bearing arguments are rejected before handler execution;
- policy-engine and counter-store errors deny;
- caps remain correct under concurrent requests;
- the untrusted endpoint cannot reach the unconstrained dispatch entry point;
- accepted requests preserve the ordinary response envelope; and
- gateway restarts do not reset durable caps when the configured window spans the restart.

## Alternatives considered

| Alternative                                                 | Reason rejected                                                                                                  |
| ----------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Rely on operator discipline                                 | Documentation does not enforce a narrow capability boundary.                                                     |
| Make the ordinary Gate globally fail closed                 | This addresses infrastructure failure but not namespace pinning, argument constraints, or structural separation. |
| Use only operating-system sandboxing                        | Host isolation does not constrain which application verbs a reachable MCP endpoint accepts.                      |
| Use a mode flag on the ordinary MCP process                 | An omitted flag would silently expose the unconstrained surface.                                                 |
| Demultiplex trusted and untrusted callers inside one daemon | A routing defect would affect both trust classes.                                                                |

## Consequences

### Positive

- Untrusted integrations receive a small, inspectable capability surface.
- Namespace, argument, and budget rules are enforced before handler execution.
- Accepted requests still use the public request grammar, Gate, registry, and daemon.

### Negative

- The gateway, policy validator, and counter service add security-sensitive code.
- A separate process adds deployment and integration-test surface.
- Capability changes require policy validation and typically a gateway reload.

## References

- [ADR-007](./ADR-007-namespace.md): namespace semantics constrained by the gateway
- [ADR-016](./ADR-016-request-dsl.md): request grammar
- [ADR-017](./ADR-017-pack-standard.md): registry and handler declarations
- [ADR-018](./ADR-018-authorization-gate.md): authorization Gate
- [ADR-049](./ADR-049-khived-daemon.md): warm daemon transport
- [ADR-096](./ADR-096-warm-daemon-per-request-identity.md): per-request daemon identity
- [ADR-103](./ADR-103-resource-attribution-model.md): optional resource-accounting input
