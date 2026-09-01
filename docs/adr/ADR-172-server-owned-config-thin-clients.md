# ADR-172: Server-owned storage config — client processes stop carrying engines

- Status: Proposed
- Date: 2026-09-01

## Context

khive today has no client processes in the database sense. Every host
process — each MCP server instance, every `kkernel exec` invocation — is a
full engine: it discovers its own config file (cwd-anchored, first match
wins), builds its own backend map from the `[[backends]]` /
`[packs.*].backend` sections, and holds a complete runtime capable of
opening the store files directly. The serving daemon is an optimization,
not an authority: `crates/kkernel/src/exec.rs` states the contract in its
opening comment — the caller's config fingerprint is checked against the
daemon's, and _"a mismatch falls back to local"_. The daemon wire enforces
the same coupling: `DaemonRequestFrame.config_id` is equality-checked
against the daemon's own id (`crates/khive-runtime/src/daemon.rs`), so a
client is only served if its config byte-for-byte agrees with the server's.

The consequences follow mechanically:

1. **N processes × N config copies.** Every directory a host process may
   start from needs a config, and every storage-topology fact (which pack
   writes which backend, `no_embed` flags, events-db paths) must be
   replicated into all of them. The deployed configs carry comments to the
   effect of "identical block in every spawn-path config" — a manual
   lockstep convention standing in for an architectural guarantee.
2. **Divergence fails toward a second writer.** When a copy drifts, the
   fingerprint mismatch does not fail the request — it silently activates
   the caller's embedded engine, which then writes the store files under
   its _own_ backend map. A backend split performed by editing config
   copies is one missed copy away from a process quietly writing a table
   back into the store it was just split out of. This is the single-writer
   guarantee's structural hole, and historical writer-contention incidents
   trace to exactly this shape.
3. **Config equality blocks legitimate clients.** A pure client (the
   Python socket client under `python/`) has no engine and therefore no
   config of its own; it can only adopt the daemon's `served_config_id`
   from the handshake. The equality check exists to protect the fallback
   path — remove the fallback and the check has nothing left to protect.

The comparison that motivates the decision: Postgres clients do not carry
`postgresql.conf`, and cannot degrade into opening the data directory
themselves when the server is unreachable. Storage topology is the
server's; clients bring a connection target and an identity.

## Decision

**The serving daemon exclusively owns storage configuration. Host
processes become thin clients: connection endpoint + identity, no engine,
no fallback.**

1. **One config, at the daemon.** `[[backends]]`, `[packs.*]` routing,
   embedder configuration, and the events-db path are read by the daemon
   process only. No other process interprets these sections.
2. **Client-side config shrinks to identity.** A per-directory config
   contributes only attribution (`[actor]`) and, optionally, a socket
   path override. A client missing a config is an anonymous client, not a
   differently-routed one.
3. **No local fallback.** For a socket-capable caller, an unreachable
   daemon is a connection error. The embedded-engine fallback in the exec
   path is removed; the only process that constructs a runtime over the
   configured backends is the daemon itself. (`kkernel` may _start_ the
   daemon when none is running — auto-spawn replaces embed as the
   cold-start story.)
4. **`config_id` demotes from precondition to fact.** The daemon reports
   `served_config_id`; clients adopt it and use it to detect restarts
   (re-handshake on change). The equality reject disappears with the
   fallback it guarded.

## Consequences

- The single-writer property becomes structural: exactly one process can
  hold a backend map, so no configuration state anywhere else can create
  a second writer. The "identical block in every config" conventions —
  and the class of incident they guard against — are deleted rather than
  maintained.
- Storage topology changes (adding a backend, splitting a pack's store)
  become one-file edits followed by one daemon restart, instead of a
  fleet-wide config sweep with silent-divergence risk.
- Offline/embedded use changes shape: `kkernel exec` against a cold
  machine spawns the daemon rather than embedding an engine. Tests keep
  the embedded runtime via an explicit test-only constructor; production
  code paths lose access to it.
- Clients in any language reduce to the wire contract (length-prefixed
  JSON frames over the Unix socket, one metrics handshake). The Python
  client already implements exactly this and needed no config file —
  evidence that the thin-client contract is already sufficient.
- A daemon outage now stops all callers instead of degrading them into
  local writes. This is judged correct: the degraded mode was the defect.
  Availability work (supervised restart, auto-spawn) addresses the same
  concern without a second writer.

## Alternatives considered

- **Status quo + convention.** Keep per-process configs identical by
  discipline. Rejected: the failure mode is silent, biased toward data
  corruption, and already observed; discipline does not compound.
- **Config distribution tooling.** Sync one canonical config to all spawn
  paths. Rejected: N copies remain the hazard; distribution narrows the
  divergence window without closing it, and adds machinery whose failure
  reproduces the original problem.
- **Keep fallback behind an opt-in flag.** Rejected: the flag would exist
  to re-enable the second writer; any caller that sets it reintroduces
  the full hazard. Cold-start convenience is served by auto-spawn.

## Forward

Phasing: (1) daemon-required mode for MCP hosts and `exec` (fallback
still present, default off); (2) remove the fallback engine from the exec
path and the `config_id` equality reject from the daemon; (3) split the
config schema into a server document and a client stub, with the daemon
rejecting storage sections found in client-position configs so stale
copies fail loudly instead of lingering.
