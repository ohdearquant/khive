# ADR-176: Server-owned storage config — client processes stop carrying engines

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
opening comment (`crates/kkernel/src/exec.rs:1-6`) — the caller's config
fingerprint is checked against the daemon's, and _"a mismatch falls back to
local"_. Under the shipped default, a mismatch on a request with a daemon
rendezvous on disk is refused instead (corrected by Amendment 1;
`crates/khive-mcp/src/daemon/fallback.rs:116-125,428-449`). The daemon wire enforces the same coupling:
`DaemonRequestFrame.config_id` is compatibility-checked against the daemon's
own id (`crates/khive-runtime/src/daemon/connection.rs:283-299`), so a client
is served only when its config satisfies the compatibility rules, which are
not always byte-for-byte equality (corrected by Amendment 1).

The consequences follow mechanically:

1. **N processes × N config copies.** Every directory a host process may
   start from needs a config, and every storage-topology fact (which pack
   writes which backend, `no_embed` flags, events-db paths) must be
   replicated into all of them. The deployed configs carry comments to the
   effect of "identical block in every spawn-path config" — a manual
   lockstep convention standing in for an architectural guarantee.
2. **Divergence fails toward a second writer.** When a copy drifts, the
   fingerprint mismatch fails the request under the shipped default when a
   daemon rendezvous is on disk. Without a rendezvous, or when
   `KHIVE_DAEMON_STRICT` is explicitly false, it can activate the caller's
   embedded engine (corrected by Amendment 1;
   `crates/khive-mcp/src/daemon/fallback.rs:116-125,428-449`).
   A backend split performed by editing config copies is one missed copy away from a process quietly
   writing a table back into the store it was just split out of. This is the
   single-writer guarantee's structural hole, and historical writer-contention
   incidents trace to exactly this shape.
3. **Config compatibility blocks legitimate clients.** A pure client (the Python
   socket client under `python/`) has no engine and therefore no config of
   its own; it can only adopt the daemon's `served_config_id` from the
   handshake. The compatibility check exists to protect the shared runtime
   and local fallback paths; legacy identifiers permit a superset of extra
   embedders, while peer-fingerprinted identifiers require matching extra
   embedder sets (`python/khive/transport.py:18-22`;
   `crates/khive-runtime/src/daemon/config_id.rs:131-151`; corrected by Amendment 1).

(Amendment 1) Forwarding returns a daemon response before local construction;
local construction remains available after forwarding declines and for
non-atomic `--ops-file` dispatch. Cold-start spawn and strict fallback
rejection with a rendezvous are shipped, while Decision 2 and Forward (3)
remain future work (`crates/kkernel/src/exec/run.rs:842-871,1149-1155`;
`crates/khive-mcp/src/daemon/forward_or_spawn.rs:523-551,633-640`).

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
   cold-start story. This cold-start behavior is already shipped (corrected by Amendment 1).)
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
still present, default off) (superseded by Amendment 1, Decision 1); (2) remove the fallback engine from the exec
path and the `config_id` equality reject from the daemon; (3) split the
config schema into a server document and a client stub, with the daemon
rejecting storage sections found in client-position configs so stale
copies fail loudly instead of lingering.

## Amendment 1 (2026-10-23) -- Align daemon mode with shipped behavior

**Status**: Proposed 2026-10-23. This draft requires specification approval before dependent implementation lands. It
does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#5591](https://github.com/ohdearquant/khive/issues/5591).

**Trigger.** The earlier context said every `kkernel exec` invocation was a full engine and that any fingerprint
mismatch silently activated a local engine. The current forwarding path returns before local construction when it
serves a request, and strict mode rejects fallback when a daemon rendezvous exists
(`crates/kkernel/src/exec/run.rs:842-871`; `crates/khive-mcp/src/daemon/fallback.rs:116-125,428-449`). The earlier
Forward (1) also proposed a second switch with a default that conflicts with ADR-049 Amendment 14
(`docs/adr/ADR-049-khived-daemon.md:1758-1760`).

### Decision 1 -- Use the shipped strict-mode switch

`KHIVE_DAEMON_STRICT` is the single switch. When explicitly false, fallback is permitted. When explicitly true, all
fallback reasons are rejected. When unset or empty, fallback is rejected if a daemon rendezvous was present at
request admission or is present at the fallback decision. If neither observation finds a rendezvous, the quiet
`no_socket` fallback remains in effect under ADR-049 Amendment 2. Removing that fallback requires an explicit
supersession of ADR-049 Amendment 2. This preserves the established no-daemon behavior while avoiding a second,
conflicting strict-mode switch (`crates/khive-mcp/src/daemon/fallback.rs:116-125,147-152,428-449`;
`crates/khive-mcp/docs/api/daemon-lifecycle.md:276-284`; `docs/adr/ADR-049-khived-daemon.md:213-236,1758-1760`).

The ADR-176 Forward (1) proposal for a daemon-required switch with a default off is superseded. The alternative
decision to introduce a new switch is rejected because the shipped switch already defines strict behavior and its
default (`crates/khive-mcp/src/daemon/fallback.rs:116-125`). The opening comment in
`crates/kkernel/src/exec.rs:1-6` still says a mismatch falls back to local; that comment should be corrected to
describe rejection under the shipped default when a rendezvous is on disk. This amendment records the correction
needed and does not change code.

### Decision 2 -- Correct the current-state description and retain future work

Cold-start spawn is shipped, and spawn failures are loud. Local fallback construction remains available in
`kkernel exec` after forwarding declines and in the non-atomic `--ops-file` path. These statements follow the
forwarding branch, local construction path, bulk dispatch, and spawn handling
(`crates/kkernel/src/exec/run.rs:842-871,1149-1155`;
`crates/khive-mcp/src/daemon/forward_or_spawn.rs:523-551,617-640`).

Decision 2 and Forward (3), which describe a client configuration stub and daemon rejection of storage sections in
client-position configs, remain future work. The runtime configuration is currently discovered from configuration
files, and the client continues to read backend configuration when constructing a spawn request
(`crates/khive-mcp/src/args.rs:78-88`; `crates/kkernel/src/exec/run.rs:817-846`).

### Decision 3 -- Correct the issue's current-state premise

Issue #5591 tracks strict mode, cold-start spawn, and the client stub. The referenced `crates/kkernel/src/exec.rs:954`
location does not exist in the checkout: `crates/kkernel/src/exec.rs` has 144 lines
(`crates/kkernel/src/exec.rs:1-144`). The strict-mode behavior and
cold-start spawn are already present in code; the issue's acceptance must be rerun on a built binary by the
implementation work. This amendment records the state and does not claim that acceptance was rerun. The behavior
and existing spawn seam test are in `crates/khive-mcp/src/daemon/fallback.rs:116-125`,
`crates/khive-mcp/src/daemon/forward_or_spawn.rs:523-551`, and
`crates/kkernel/src/exec_tests/daemon_spawn_config_tests.rs:59-80`.

### Acceptance

| ID | Statement                                                                                                                                                        | Evidence                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| -- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1 | Unset strict follows rendezvous; explicit false overrides it; other values are strict; no-rendezvous fallback stays quiet; refusing socket and stale PID reject. | `crates/khive-mcp/src/daemon/strict_default_tests.rs::unset_strict_follows_the_rendezvous_on_disk`, `crates/khive-mcp/src/daemon/strict_default_tests.rs::explicit_false_spellings_override_a_present_rendezvous`, `crates/khive-mcp/src/daemon/strict_default_tests.rs::any_other_value_is_strict_without_a_rendezvous`, `crates/khive-mcp/src/daemon/strict_default_tests.rs::refusing_socket_with_strict_unset_rejects_with_no_socket`, `crates/khive-mcp/src/daemon/strict_default_tests.rs::no_rendezvous_with_strict_unset_falls_back_quietly`, `crates/khive-mcp/src/daemon/strict_default_tests.rs::stale_pid_file_with_strict_unset_rejects_with_no_socket`. |
| A2 | `KHIVE_DAEMON_STRICT` is the sole daemon-required switch read in `crates/`.                                                                                      | Search for `daemon_required`, `require_daemon`, and `KHIVE_DAEMON_REQUIRED` under `crates/` returns no matches; `crates/khive-mcp/src/daemon/fallback.rs:116-125,154-165` reads `KHIVE_DAEMON_STRICT`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| A3 | Forwarding, local fallback, and non-atomic `--ops-file` dispatch follow the recorded paths.                                                                      | `crates/kkernel/src/exec/run.rs:842-871,1149-1155`; `crates/khive-mcp/src/daemon/forward_or_spawn.rs:523-551,617-640`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| A4 | Decision 2 and Forward (3) remain unbuilt; no client-stub section exists in `crates/`.                                                                           | Search for `client_stub`, `ClientConfig`, and `client stub` under `crates/` returns no matches; `crates/kkernel/src/exec/run.rs:817-846` reads and forwards config.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| A5 | The implementation change for #5591 reruns that issue's acceptance on a built binary.                                                                            | Owed by that change; not executed by this amendment.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
