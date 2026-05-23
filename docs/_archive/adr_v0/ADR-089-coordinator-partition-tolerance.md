# ADR-089: Coordinator Partition Tolerance — Degraded Reads, Hard Writes

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-079 (Pack-Scoped Backends), ADR-086 (Cross-Backend Edge Representation),
ADR-087 (Substrate-Kind Federated Search), ADR-088 (Cross-Backend Traversal and Curation)\
**Part of**: ADR-080 (SubstrateCoordinator umbrella)

## Context

Multi-backend deployments (ADR-079) raise an operational question: what happens when one of the
backends becomes unreachable mid-query? Possibilities range from "hard-fail any operation
that depends on it" to "skip silently and return whatever the reachable backends have."

ADR-080's umbrella commits to graceful degradation for reads and hard failure for writes. This
ADR specifies the **partition tolerance model** in detail: how unreachability is detected, how
operations behave when a backend is unreachable, and how the system recovers.

This decision is structurally orthogonal to ADR-086 (data model), ADR-087 (search dispatch),
and ADR-088 (traversal/curation) — the coordinator could be built without partition tolerance
and would simply hard-fail on backend I/O errors. Adding D12 modifies the behavior of those
ADRs' operations without changing their core decisions. Hence its own ADR.

## Decision

### D12 — Partition-tolerant degraded reads; hard-fail writes

The kernel coordinator (see ADR-080 umbrella) maintains a per-backend health map:

```text
Arc<DashMap<BackendName, BackendHealth>>
where BackendHealth = Healthy | Unreachable { marked_at: Instant }
```

#### Detection — passive, not active

Health is **passive**: a backend becomes `Unreachable` only when an operation against it fails
with an I/O error. There is no active health check / heartbeat in v1. Reasoning:

- An active health check adds operational complexity (timing, false positives, separate
  thread) for a property that is otherwise free (failing operations are the most reliable
  unreachability signal).
- Passive detection has zero cost when all backends are healthy.
- The "unreachable" classification is best-effort, not authoritative — it's a hint for
  subsequent operations to skip a likely-failing backend, not a guarantee.

#### Cooldown and recovery

When a backend is marked `Unreachable { marked_at }`, the coordinator skips it for a
configurable cooldown (`backend_unreachable_cooldown_ms`, default 30 seconds). After the
cooldown elapses, the next operation against that backend re-attempts. Success returns the
backend to `Healthy`; failure restarts the cooldown.

This pattern is the in-process equivalent of RuVector's `ClusterStatus { Healthy, Degraded,
Unreachable, Unknown }`. We omit `Degraded` (a backend in khive is either reachable or not) and
`Unknown` (the post-boot pre-operation transient).

#### Operation behavior under partition

**Substrate-kind search (ADR-087 D4):**

- Skip the unreachable backend; log warning.
- Result carries `partial: true` and `missing_backends: ["lore"]`.
- Successful backends produce their normal contributions; RRF fusion proceeds with the
  remaining ranked lists.
- Caller can decide whether the partial result is acceptable; nothing is silently swallowed.

**Granular-kind operation on an unreachable backend** (e.g., `recall` from `memory` pack when
`memory`'s backend is offline):

- Hard error `PackUnavailable { pack: "memory", backend: "main" }`.
- No degraded fallback — there is no alternative source for granular kinds.

**Cross-backend traversal (ADR-088 D9):**

- BFS proceeds across reachable backends.
- When a frontier neighbor lives on an unreachable backend, that branch terminates with a
  `terminated_at_backend` marker in the path.
- Result carries `partial: true` and the set of unreachable backends.

**Cross-backend cascade on hard-delete (ADR-088 D11):**

- The local hard-delete (entity row + outgoing edges on the entity's own backend) succeeds.
- Incoming-edge cleanup on an unreachable backend is skipped, leaving dangling edges.
- Coordinator returns success but with `partial_cascade: true` and the unreachable backend's
  name; operator can re-run the cleanup later.

**Cross-backend writes (other than the cascade tail above):**

- Always fail hard if the target backend is unreachable. No degraded writes.
- `link(a_on_main, b_on_lore)` with lore unreachable → `BackendUnreachable { backend: "lore" }`
- The user must explicitly migrate the write target or wait.

#### Summary: reads degrade, writes don't

| Op category                         | If a relevant backend is unreachable         | Result shape                 |
| ----------------------------------- | -------------------------------------------- | ---------------------------- |
| Substrate-kind read (search/list)   | Skip backend, proceed                        | `partial: true` flag         |
| Granular-kind read                  | Hard error                                   | `PackUnavailable`            |
| Cross-backend traversal             | Walk reachable, mark terminated branches     | `partial: true` flag         |
| Cross-backend hard-delete cascade   | Cascade reachable backends; skip unreachable | `partial_cascade: true` flag |
| Any write to an unreachable backend | Hard error immediately                       | `BackendUnreachable`         |

The `partial` flag is the only way a caller observes degradation — silent skipping is forbidden.

## Single-backend default behavior

With one backend, there are no "other" backends to be unreachable. The unreachable code path is
effectively dead — if the single backend fails, the operation fails. No `partial: true` flag is
ever raised.

Coordinator is zero-cost on single-backend deployments.

## Alternatives considered

### A. Active health checks (heartbeat)

Background task that pings each backend periodically. Rejected: complexity not justified by the
benefit. Passive detection works on operations the user actually cares about.

### B. Hard-fail any partial query

If any backend is unreachable, all substrate-kind queries fail. Rejected: defeats the purpose
of multi-backend isolation — an `archive` backend being offline should not break searches over
the `main` backend's notes.

### C. Silent skip without `partial` flag

Skip unreachable backends and don't tell the caller. Rejected: hides degradation from the
caller, who may be making decisions on the result. Explicit > silent for partial coverage.

### D. Per-operation cooldown (not global per-backend)

Each operation type maintains its own cooldown for each backend. Rejected: complexity not
justified; if a backend is unreachable for a read, it's overwhelmingly likely unreachable for a
write moments later. Global per-backend cooldown is simpler and good enough.

### E. Configurable cooldown via TOML

`khive.toml` per-backend cooldown override. Acceptable for v1 (no harm done) but not required;
default 30s for all backends is fine until operational evidence suggests otherwise.

### F. Use an existing circuit-breaker library

Adopt `failsafe-rs` or similar. Considered; the coordinator's needs are simple enough (one
`DashMap` + `Instant` comparison) that a library dependency adds more surface than it removes.
Reject for v1; revisit if more sophisticated patterns are needed.

## Consequences

### Positive

- Multi-backend deployments survive single-backend outages on the read path
- Callers see explicit `partial` flags — never silent
- Writes remain consistent (no degraded writes, no eventual reconciliation needed)
- Passive detection has zero cost when all backends are healthy
- Cooldown prevents thrashing on a persistently failing backend

### Negative

- **First-failure latency** — the failing operation itself returns an error before the
  backend is marked Unreachable; subsequent operations within the cooldown skip it.
- **Cooldown delay on recovery** — if a backend comes back during the cooldown window, the
  next operation re-attempts (success returns it to Healthy), but if no operation happens
  during the cooldown, recovery is delayed until the next request.
- **Cascade dangling edges** — hard_delete's incoming-edge cleanup may leave dangling rows if a
  backend was unreachable mid-cascade. ADR-088 D11 documents this; future cleanup admin
  command will reconcile.

### Neutral

- ADR-088's `partial: true` propagation through traversal results adds one field; callers
  ignoring the field see no change.
- ADR-087's substrate search result type gains `missing_backends: Vec<String>`; same
  observation.
- No new error types beyond `BackendUnreachable` and `PackUnavailable` (both already declared
  in the coordinator error enum).

## Open Questions

1. **Global vs per-operation cooldown?** Currently global per backend. Per-operation might let
   read-only failures not poison subsequent writes, but the complexity gain seems unwarranted.
   Defer to operational evidence.
2. **Cooldown configurability via TOML.** Default 30s for all backends. Per-backend override is
   trivial to add (one field on `BackendConfig` per ADR-079); add it if operators ask. v1: not
   added.
3. **Health introspection via admin command.** `kkernel debug backend health` to print the
   current health map. Not in v1 scope.
4. **Active probing as opt-in.** A future `[[backends.main].health_check_interval_ms]` could
   enable a heartbeat for deployments that prefer proactive detection. Not in v1.

## References

- ADR-079 — Backends declared here; this ADR's health map is keyed on backend names from
  there.
- ADR-080 — Umbrella ADR; mentions D12 in §"Decision"; this ADR is the detailed specification.
- ADR-086 — Cross-backend edge representation; the locator's parallel-fetch fallback respects
  the health map (skips Unreachable backends).
- ADR-087 — Substrate-kind search; this ADR specifies its partial-coverage behavior.
- ADR-088 — Traversal and cascade; this ADR specifies their partial-result behavior.
- RuVector `distributed/federation.rs` — `ClusterStatus` enum (pattern adapted for in-process
  backends).
