# ADR-119: Host-Supervised Daemon Components Beside the Verb Plane

**Status**: Accepted
**Date**: 2026-07-21
**Authors**: khive maintainers
**Depends on**: [ADR-016](./ADR-016-request-dsl.md),
[ADR-017](./ADR-017-pack-standard.md),
[ADR-022](./ADR-022-events-query-surface.md),
[ADR-023](./ADR-023-declarative-pack-format.md)

## Context

The daemon hosts long-running internal work that is not initiated by a request. Such work
needs consistent startup ordering, cancellation, restart policy, health reporting, and
shutdown behavior. Treating each loop as an independent spawned task lets lifecycle rules
drift and makes daemon shutdown incomplete.

This work is not a verb: it has no caller, request envelope, or bounded request lifetime.
It is also not a new persistent substrate. Durable state remains governed by the existing
storage and event contracts.

## Decision

khive recognizes two implemented capability classes and reserves a third:

1. **Verb** — a bounded caller-invoked operation dispatched through the request DSL.
2. **Daemon component** — host-constructed, daemon-only, long-running work supervised by a
   host-local component registry.
3. **Event topic** — a reserved term for a possible future process-local wake-up channel
   over already durable state. This ADR does not implement or authorize one.

Daemon components are not declared as verbs and do not extend the public request catalog.
This ADR does not add an MCP resource, subscription, or notification surface.

### Component registration

The first implementation uses a daemon-host-local registry with this conceptual shape:

```rust
struct DaemonComponentRegistration {
    name: &'static str,
    restart: RestartClass,
    restart_budget: RestartBudget,
    backoff: BackoffPolicy,
    shutdown_timeout: Duration,
    start: ComponentFactory,
}

trait DaemonComponent: Send + 'static {
    async fn run(self, host: ComponentHost) -> Result<(), ComponentError>;
}

struct ComponentHost {
    cancellation: CancellationToken,
    health: HealthReporter,
}
```

Names are illustrative; the behavioral contract is normative.

Registrations are constructed only after configuration, request identity defaults,
backend routing, and pack selection are resolved. A component factory may capture its own
narrow resolved handles. `ComponentHost` contains lifecycle services only and is not a
general service locator.

### Host responsibilities

The supervisor, not the component loop, owns:

- daemon-role gating and startup order;
- task naming and structured logging;
- cooperative cancellation and shutdown timeout;
- panic and join-error observation;
- retryability classification;
- bounded restart budget;
- exponential backoff with jitter; and
- last start, heartbeat, error, restart count, and terminal health state.

Restart classes include at least `Never` and `OnFailure`. Clean completion and
cooperative cancellation do not consume failure budget. A component that exhausts its
budget becomes terminally unhealthy and cannot hot-loop. Budgets and backoff state are per
component.

### Fault boundary

In-process task supervision is not process isolation. Blocking work uses a bounded
blocking pool or a subprocess; it must not occupy an asynchronous runtime worker
indefinitely.

A cooperative shutdown timeout may end by aborting the component task. If fault tests show
that a hang can starve unrelated daemon work or violate the shutdown bound, that component
must move behind a watchdog subprocess before adoption.

### Shutdown handoff

The registry joins the daemon's existing tracked-task shutdown path:

1. The supervisor task is registered with the runtime's tracked-background-task mechanism.
2. When the unified daemon shutdown signal resolves, the runtime cancels every component.
3. Each component is joined up to its configured `shutdown_timeout`; a task still running
   at expiry is aborted.
4. The supervisor completes inside the runtime drain.
5. Transport and process-marker cleanup occur only after the drain returns.

No component may install a parallel shutdown path outside this handoff. Total component
shutdown latency is bounded by the longest component timeout plus the existing drain
overhead.

### Error and restart mapping

Component errors carry a retryability class:

| Result                                  | Supervisor action                                                          |
| --------------------------------------- | -------------------------------------------------------------------------- |
| Cooperative cancellation                | Join without restart or budget charge.                                     |
| Clean completion for a finite component | Mark complete without restart.                                             |
| Retryable error                         | Restart within budget after backoff and jitter.                            |
| Permanent configuration or schema error | Mark terminally unhealthy.                                                 |
| Panic or join error                     | Apply the registration's explicit restart class.                           |
| Shutdown timeout                        | Abort and mark terminally unhealthy unless a subprocess policy takes over. |

An item-level or operation-level error handled inside a component is not automatically a
`ComponentError`. Components must define which failures end the loop; supervisors must
not turn local data errors into unbounded whole-component restart.

### Health

The first implementation reports component status through operator-local structured logs
and metrics. It includes registration name, state, last start, last heartbeat, last error,
restart count, and remaining budget. Any public inspection verb or resource requires a
separate decision.

### Public compatibility

For an unchanged pack and configuration set:

- the request parser and schema are unchanged;
- the verb catalog is unchanged;
- response bytes are unchanged; and
- existing pack implementations compile without new trait methods.

The registry is local to the daemon host until at least two independent component types
demonstrate that a narrower pack-level declaration boundary is stable.

### Future event-topic invariant

This ADR does not authorize an event bus. Any future decision must preserve:

> Commit durable state first; publish second. A dropped notification may delay detection
> but cannot lose the underlying fact.

Slow or reconnecting consumers must resynchronize from durable state using the event-query
and cursor contracts. A process-local topic cannot become a second durable log or cursor
authority.

## Verification

Tests must cover:

- startup after configuration and backend routing are complete;
- clean cancellation without budget charge;
- retryable failure with bounded exponential backoff and jitter;
- permanent failure entering terminal health;
- panic isolation at the task boundary;
- restart-budget exhaustion without a hot loop;
- a hung task bounded by its shutdown timeout;
- daemon drain waiting for the supervisor;
- component-local errors that do not restart the component; and
- byte-identical public verb discovery and responses for unchanged configuration.

Fault tests must distinguish task panic, cooperative hang, blocking starvation, process
abort, and process crash. The registry claims containment only for failure classes its
tests demonstrate.

## Alternatives considered

| Alternative                                     | Reason rejected                                                                                       |
| ----------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Add component methods to every pack immediately | Freezes a dependency boundary before multiple component types establish a common narrow host context. |
| Keep independent spawned loops                  | Preserves inconsistent retry, health, and shutdown behavior.                                          |
| Run every component as a subprocess             | Adds IPC and deployment contracts before fault evidence shows they are required.                      |
| Add an event bus with the registry              | Notification semantics are a separate decision and must preserve durable replay authority.            |

## Rationale

Without a registry, daemon loops remain untracked and each can invent lifecycle behavior.
A host-local registry provides one enforceable contract at the place where resolved
dependencies already exist. Promotion to a pack-level descriptor remains additive after
the boundary is proven.

## Consequences

### Positive

- Daemon-resident work receives consistent lifecycle and health behavior.
- Shutdown waits for all registered work within explicit bounds.
- The public verb and pack contracts remain unchanged.

### Negative

- The daemon host gains a registry and supervisor implementation.
- In-process supervision cannot contain every process-level failure.
- Local registration can become a special case if the promotion criteria are never met.

## References

- [ADR-016](./ADR-016-request-dsl.md): public request plane
- [ADR-017](./ADR-017-pack-standard.md): pack and event-consumer contracts
- [ADR-022](./ADR-022-events-query-surface.md): durable event query and cursor authority
- [ADR-023](./ADR-023-declarative-pack-format.md): verb visibility and composition
- [ADR-079](./ADR-079-ann-persistence-warm-path-integration.md): public daemon warm-path integration
