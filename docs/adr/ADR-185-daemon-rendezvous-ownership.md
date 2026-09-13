# ADR-185: Daemon Rendezvous Ownership — a client must not take the socket a supervisor is for

- **Status**: Proposed
- **Date**: 2026-09-13
- **Depends on**: [ADR-049](ADR-049-khived-daemon.md) (the warm-state daemon, the thin client, and
  client auto-spawn)
- **Relates to**: [ADR-096](ADR-096-warm-daemon-per-request-identity.md) (the daemon's configuration decides
  the identity every call through it resolves against)

## Context

ADR-049 §2 gives the thin client one job beyond dispatch: if no responsive socket exists on the first
request, the client spawns the daemon itself. That was written for a single-user workstation where
the daemon has no other way to start, and there it is exactly right — the first call after a reboot
pays a startup cost and everything after it is warm.

A deployment that puts the daemon under a process supervisor has a second starter, and ADR-049 does
not say what happens when both are live. What happens is a race for the rendezvous, and the client
wins it structurally:

- The client spawns **on demand**, at the instant of a call, from whatever process happens to call
  first.
- The supervisor starts **on a schedule**: at load, and after a failed start on a throttle interval.
- The daemon's own startup contract (ADR-049, and the refusal path in the runtime's daemon module)
  is _first writer wins_: once the PID file names a live process that answers on the socket, every
  later starter is refused with "a khived instance is already serving this socket" and exits
  non-zero.

That refusal is correct in isolation and is what makes the race one-sided. The supervisor's job exits
non-zero against the incumbent, is rescheduled, exits non-zero again, and never owns the daemon it
exists to supervise.

### What was measured

On a supervised restart of a deployment carrying both starters:

1. Every client process known at the time was enumerated and frozen, the incumbent daemon was
   stopped, the new binary installed, and the supervisor kickstarted **first**, with the clients
   still frozen. This ordering is the obvious remedy and it is not sufficient.
2. 69 seconds later the supervisor's job still had no pid, and the socket was already held by a
   daemon carrying the configuration of a _different_ client — one that started after the
   enumeration and was therefore never frozen.
3. The supervised job's last exit code was 1, its state "spawn scheduled": it had tried, been
   refused by the incumbent, and gone back to waiting.
4. Stopping that daemon by hand did not help. Another client won the free socket within 45 seconds,
   and the supervisor lost again.

The freeze can only cover the clients that exist when it enumerates them. In a deployment where
clients are started by anything asynchronous, the set is open, so no amount of ordering closes it.

### What it costs

- **The fleet daemon's configuration is whichever client called first.** Every call routed through
  that daemon resolves identity and backend paths against that configuration (ADR-096). A client
  whose own configuration disagrees is served a mismatch and falls back to running in-process,
  silently, which is the failure mode hardest to notice because everything still answers.
- **There is no supervision.** No restart policy, no keep-alive, no operator control of the process
  that holds the warm state for every client on the machine.
- **An operator has no way to say which configuration the daemon should hold.** The supervisor's
  declaration is the natural place to say it, and the supervisor never gets to run.

## Decision

**Where a supervisor for the daemon is declared, the client must not auto-spawn.** Auto-spawn stays
the default and stays unchanged for the single-starter case ADR-049 describes.

This is the recommended shape rather than socket activation, for one reason: it is the only one that
works on a deployment the platform does not help. Socket activation removes the race where the
platform offers it and should be taken there, but a declaration the client reads is what makes the
answer the same on every platform, and it is also the only shape that gives an operator somewhere to
say which configuration the daemon should hold.

Concretely:

1. The runtime gains an explicit way to declare that daemon startup is owned elsewhere. It is read at
   the point the client decides to spawn, not at process start, so a deployment can turn it on
   without restarting every client.
2. With that declaration present and no responsive socket, the client **refuses the request** rather
   than spawning, and the refusal names the owner and says the daemon is expected to be started by
   it. A refusal is correct here: a request that would otherwise be served by a daemon anchored on
   the caller's own configuration is a request whose result cannot be trusted by anyone else.
3. The refusal is distinguishable in kind from "the daemon is starting", so a caller can retry the
   second and must not retry the first in a loop.

### Alternatives considered

- **Socket activation**: the supervisor binds the socket and hands it to the daemon, so no client can
  take it. This is the strongest form and removes the race rather than refusing it, but it binds the
  design to platforms that offer it and changes the daemon's own bind path. Worth doing where
  available; it does not remove the need for (1), because a deployment without socket activation
  still needs a way to say "not yours to start".
- **The auto-spawned daemon adopts a canonical configuration** rather than the caller's. This fixes
  the configuration half and leaves the supervision half broken: the daemon still runs unsupervised,
  and the supervisor still exits non-zero forever.
- **Leave it to operators**: document that clients must be started with auto-spawn disabled. This is
  what a deployment can do today with an environment variable per client, and it is exactly the kind
  of invariant that holds until one client is started without it — which is the state that produced
  the measurement above.

## Consequences

- A deployment that declares an owner gets a deterministic answer to "whose configuration is the
  daemon holding": the owner's.
- A client in such a deployment can fail where it used to succeed. That is the point, and it is why
  the refusal must name the owner: the alternative is succeeding against a daemon nobody chose.
- The single-starter case is untouched. No configuration, no declaration, no change.
- The existing first-writer-wins refusal stays as it is. It is not the defect; it is the mechanism
  that made the defect one-sided, and it is still what protects a running daemon from a second one.

## Acceptance

The arm that decides whether this record is implemented, stated before anything is built, because it
is the one the obvious remedy already failed:

- **A client started while no daemon is serving and an owner is declared must not take the
  rendezvous.** The test starts from no socket and no PID file, declares an owner, starts a client,
  and asserts the client refused: no socket appeared, no PID file appeared, no daemon process exists,
  and the refusal names the owner. Ordering is not part of the arm — the client is started at the
  moment that would have won under today's behaviour.
- The same arm with no owner declared is the control, and it must produce the opposite result: the
  client spawns and serves, exactly as ADR-049 specifies. Without that control the first assertion is
  satisfied by any broken client.
- A third arm covers the incumbent case that already works and must keep working: with a daemon
  already serving, a client neither spawns nor refuses, it connects.
- The refusal's kind is asserted, not just its text, since a caller has to tell "not yours to start"
  from "starting, try again" without parsing prose.

## Open questions

- Whether the declaration belongs in the configuration file, the environment, or both, and what a
  client should do when the two disagree.
- Whether the refusal should carry a bounded wait for the owner to come up, so the first call after a
  supervised start succeeds instead of refusing once.
