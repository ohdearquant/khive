# ADR-096: warm daemon per-request identity — serving many attribution identities over one shared backend

**Status**: Accepted
**Date**: 2026-07-05
**Authors**: khive maintainers
**Amends**: [ADR-049](ADR-049-khived-daemon.md) §"Scope boundary" (the "No
multi-namespace daemon" clause, lines 108-111) and §"Socket protocol" (request-frame shape).
**Relates to**: [ADR-007](ADR-007-namespace.md) (namespace =
attribution, Rev 4 Rule 0), [ADR-018](ADR-018-authorization-gate.md) (Gate = the auth seam),
[ADR-057](ADR-057-comm-actor-addressed-delivery.md) (actor-addressed
write-stamp), [ADR-091](ADR-091-wal-snapshot-lifetime.md) (WAL snapshot
lifetime — interaction only, not solved here).

---

## Context

ADR-049 introduced the warm daemon (`khive-mcp --daemon`): a single long-lived process, one
Unix socket per `HOME`, that owns the warm ANN/embedder state so the expensive index restore is
paid once per machine-uptime instead of once per client reconnect. A client either forwards its
request over the socket to the warm daemon or, on any mismatch, silently falls back to local
in-process dispatch.

Two facts about that design, confirmed against the current source, set up this amendment:

1. **The daemon serves exactly one construction-baked namespace, and rejects any other.** The
   request frame carries the client's resolved `namespace`
   (`crates/khive-runtime/src/daemon.rs`, `DaemonRequestFrame`), and `handle_conn` (~line 355)
   rejects the request when `frame.namespace != dispatcher.namespace()` (~line 393), setting
   `namespace_mismatch: true`. The client maps that response to `None` and records a
   local-dispatch fallback (`crates/khive-mcp/src/daemon.rs`, `map_response` →
   `FallbackReason::NamespaceMismatch`, metric `khive_daemon_fallback_total{reason}`). A
   fallback pays the full cold in-process path (index/embedder work on the order of tens of
   CPU-seconds), which is the smoothness regression this line of work targets.

2. **The frame carries a namespace but no actor.** The registry is built with three
   namespace/identity-coupled scalars baked in at construction —
   `default_namespace`, `visible_namespaces`, `actor_id`
   (`crates/khive-mcp/src/server.rs` `with_packs`, ~lines 282-291). The daemon frame threads
   only `namespace` (`wire_daemon_frame`, ~line 1235: `namespace: self.default_namespace`). The
   write-stamp actor (ADR-057) is **not** on the wire. Consequently a request served by the
   warm daemon is stamped with the **daemon's** actor, whatever the calling client's identity.

Separately, a config-discovery change (#651, commit `10d9c92c`) moved tier-3 config resolution
from cwd-anchored to database-directory-anchored, to keep the `config_id` fingerprint
(`{packs, db, embed, extra, backend}`) coherent across processes that share one database. That
change also relocated discovery of the project-local `[actor]` block. Because independent
agent connections share one database under a single `HOME`, the database-anchored path resolves
a shared home config that carries no `[actor]`, so per-connection attribution collapses to the
default identity (`local`). This is the attribution half of the problem.

### The two forks this ADR decides

- **Fork 1 — daemon serving model.** How does one warm daemon correctly serve requests from
  connections whose attribution identity differs from the daemon's construction-baked identity,
  without a cold fallback and without mis-stamping writes?
- **Fork 2 — identity injection.** How does each daemon-spawned MCP process acquire its own
  per-connection attribution identity, given that connections share one root `.mcp.json` and one
  database?

---

## Teardown (refute first)

The driving analysis frames the durable fix as: _restore per-connection attribution AND hold
fallback at zero in one move — therefore build a daemon that serves per-request namespaces._
Source refutes the coupling **as stated**, and the correction changes what the fix must thread.

- **The coupling runs through the actor field, not the namespace field.** Under the standard
  project-local `[actor]` path, ADR-007 Rev 4 Rule 0 holds: `[actor].id` does **not** become
  `default_namespace`. This is enforced in source (`crates/khive-runtime/src/config.rs`
  ~lines 441-445: "the `[actor]` id does NOT become storage namespace") and pinned by tests
  (`crates/khive-runtime/src/runtime.rs:1092`
  `runtime_config_from_khive_config_actor_id_does_not_override_default_namespace`;
  `crates/khive-runtime/src/engine_config.rs:1081`). So a connection that resolves its true
  `[actor]` keeps `default_namespace = local`, its frame namespace stays `local`, and it does
  **not** trip `namespace_mismatch`. Restoring attribution does not, by itself, force a
  namespace fallback. The genuine defect is that the warm frame carries **no actor at all**
  (`server.rs` ~line 1235), so a warm-served write is stamped with the daemon's actor. "Restore
  attribution + zero fallback = one requirement" is **true**, but the thing that must be
  threaded is the **actor**, and namespace-serving is a superset needed for a different reason
  (below), not for per-connection attribution.

- **The observed actor↔namespace coupling is one-directional and env-path-specific.** The only
  place actor and namespace are tied is the _reverse_ direction: an **explicit** `--namespace`
  fills `actor_id` from the namespace for write-stamp parity (`crates/khive-mcp/src/serve.rs`
  ~line 1417, guarded by `inputs.namespace_explicit`; `crates/kkernel/src/exec.rs` ~line 840,
  "namespace_explicit=true must fill actor_id from the namespace (ADR-057)"). Actor never fills
  namespace. `KHIVE_ACTOR` sets `actor_id` only (`config.rs:317`); `KHIVE_NAMESPACE` /
  `--namespace` set the namespace (`config.rs:443`). These are distinct inputs.

- **Corpus check — reconciling the driving analysis's observation with source.**
  That analysis reports `KHIVE_ACTOR=<id>` resolving the namespace to `<id>` and hitting
  `namespace_mismatch`. The observation is real, but not via `config.rs:317` (which sets
  `actor_id` only). The cause is the clap env-alias at `crates/khive-mcp/src/args.rs:29`
  (`#[arg(long, env = "KHIVE_ACTOR")]`): it populates the `--actor` field from `KHIVE_ACTOR`, and
  the serve path treats an explicit `--actor` as a namespace input (`namespace_explicit`,
  `default_namespace`). So on the serve path a bare `KHIVE_ACTOR` flips the namespace, while on
  the `kkernel exec` path it does not — the divergence recorded in the live audit. Fork 2 removes that
  env alias (see Open question 2 resolution), eliminating the divergence: afterward `KHIVE_ACTOR`
  sets `actor_id` only, on every path. (A recorded fleet-operations correction, 2026-07-04,
  independently states the attribution surface is the config `[actor]` block, not the
  `KHIVE_ACTOR` env var — consistent with Fork 2 below.)

- **What survives the teardown.** The multi-namespace serving generalization is still worth
  building, for a reason the driving analysis under-weights: the target operating point is many
  agent connections and multiple tenants against one shared backend on a hosted host. There,
  namespace legitimately routes storage scope and Gate isolation, and a single warm process must
  serve per-request namespaces to avoid N-fold cold state. So Fork 1 lands on per-request
  identity threading (of which per-request namespace is one field) — the same mechanism serves
  both the attribution fix (thread the actor) and the multi-tenant bar (thread the namespace).

---

## Decision

### Fork 1 — thread a per-request identity context through one shared warm registry

**Chosen: per-request identity threading over ONE registry.** Extend the daemon protocol so each
request carries its own identity, and have the single warm registry mint its authorization token
from that per-request identity instead of the construction-baked scalars.

Concretely:

1. **Frame carries an identity context.** Add to the request frame the fields
   `actor_id: Option<String>` and `visible_namespaces: Vec<String>` alongside the existing
   `namespace`. Bump the daemon `PROTOCOL_VERSION` (the response frame already has a
   `version_mismatch` path, so older clients reject-and-fall-back safely during rollout).
2. **Dispatch consumes request identity.** Extend `DaemonDispatch::dispatch(...)` to accept the
   identity context and mint the `NamespaceToken`/actor per request. The runtime already exposes
   a per-request authorization entry point (`authorize_with_visibility(...)`,
   `crates/khive-runtime/src/runtime.rs:475`); the construction-baked scalars are a convenience
   default, not a hard constraint. When no identity context is supplied (pure local dispatch),
   the baked scalars remain the default — back-compatible.
3. **Soften the strict namespace reject.** Replace the hard `frame.namespace !=
   dispatcher.namespace()` reject with accept-and-serve-under-frame-identity: the daemon serves
   the request over the shared physical backend (same database, same warm ANN indexes), applying
   the frame's namespace as the per-request storage filter and the frame's actor as the
   write-stamp. **Keep the `config_id` equality check as a hard reject** — `config_id` governs
   packs/db/embed coherence, which genuinely must match for a shared warm engine to be correct.
4. **One registry, not N.** The warm engine is shared; namespace is applied as a per-request
   filter, not as a key into per-namespace warm state.

**Rejected alternatives**

- **(A) Thread only `frame.namespace` (the minimal "light end").** Clears the
  `namespace_mismatch` reject, but the frame still carries no actor, so every warm-served write
  is stamped with the daemon's actor — silent misattribution, which is _worse_ than today's
  loud, observable fallback. The requirement is to carry the actor; namespace alone does not
  satisfy it.
- **(B) N cached per-namespace registries (map keyed by namespace).** Rejected: with one
  documented exception (the brain pack, below), there is no per-namespace **warm** state to justify
  duplicating the warm engine. The ANN index is corpus-global and namespace-blind (ADR-047
  knowledge pack / ADR-033 Vamana); the only namespace-keyed retrieval data is the `atom_weights`
  table keyed `(namespace, atom_id)` (`crates/khive-retrieval/src/weights/engine_weights.rs`),
  which is a SQL row read per request from the shared backend, not resident warm state; token
  minting is already per request. N registries would replicate the exact memory cost ADR-049 exists
  to eliminate, for zero isolation benefit at the storage layer.

**Known per-namespace warm-state exception — the brain pack (correctness-safe, throughput-bounded).**
The "no per-namespace resident warm state" premise has one exception, called out here on the record
rather than left implicit. The brain pack holds a single un-keyed resident `BrainState` slot
(`Mutex<BrainState>`, `crates/khive-pack-brain/src/pack.rs:53`), swapped per-namespace behind a
global `dispatch_gate` lock: `ensure_loaded` (`crates/khive-pack-brain/src/persist.rs:593`) serves
the fast path when the request namespace is already resident, otherwise clones out the current
state and loads the target namespace (from a `saved_states` cache or a SQLite snapshot + event
replay) into the hot slot. **Correctness is safe under one shared registry** — the cross-namespace
slot-swap race was found and closed, with shipped regression tests
(`dispatch_gate_prevents_cross_namespace_slot_swap`, `ensure_loaded_publication_is_atomic`,
`ensure_loaded_cross_namespace_concurrent_does_not_corrupt_saved_states`), so per-request identity
over one registry stays correct for brain too. The cost is a **throughput ceiling, not a
correctness bug**: under N tenants alternating namespaces, every `brain.*` call serializes on the
global lock and pays a full `BrainState` clone-swap (up to ~10k entity posteriors) on any namespace
change — and this is not niche, because `knowledge.compose` / `knowledge.search` fire an internal
`brain.resolve` per request (`crates/khive-pack-knowledge/src/handlers.rs:401,461`), routing the
money verbs through the same gate + swap. **This does not change the Fork 1 decision** (one registry
is retained). The escalation path — a namespace-keyed `BrainState` or N brain instances — is gated
on **measurement under the load harness at the 20-tenant bar**, and is explicitly **not a Fork 1
blocker**.

- **(C) One daemon process per namespace (socket-per-namespace).** Rejected: the socket is one
  per `HOME` by construction (`socket_path()` has no namespace component). Multiplying daemons
  multiplies resident warm-index memory linearly in the number of namespaces, defeating ADR-049's
  "cold start once per machine-uptime" and exceeding the resident-memory budget at the target
  multi-tenant scale.

**Strongest reason:** per-request identity over one warm registry is the only option that
delivers correct per-connection attribution **and** zero fallback simultaneously, because the
field that actually forces the "fall back or mis-stamp" dilemma is the actor in the token, and
the shared backend already isolates by namespace as a per-request filter — so no warm state
needs duplicating.

#### Light-end → escalation criterion (explicit, evidence-linked)

The chosen design is the light-to-middle end: **one** registry, identity threaded per request.
Escalate to the heavy end (**cached per-namespace registries**) **only when** a pack introduces
per-namespace **warm** state that cannot be re-derived per request from the shared backend —
concretely, any of:

- a namespace-**partitioned** ANN index (today the index is corpus-global: ADR-047, ADR-033), or
- a per-namespace embedder handle held resident (today one embedder serves all), or
- a Gate policy object that must be **constructed** per namespace rather than parameterized per
  request (today authorization mints per request: `runtime.rs:475`).

A current source audit finds **none** of these. The trigger is a _future_ pack or feature that
adds such state; until one exists, one registry is provably sufficient and N registries are
strictly a memory regression.

### Fork 2 — restore project-local `[actor]` as the per-connection injection surface

**Chosen (ratification required): restore project-local (cwd/project-anchored) `[actor]` resolution,
decoupled from the database-anchored `config_id` discovery.**

The database-anchored config discovery (#651) is correct for the fields that define `config_id`
(packs/db/embed/backend) — it protects coherence across processes sharing one database, which is
itself a fallback-prevention guarantee (decoy cwd configs must not produce divergent
`config_id`). The defect is only that it also relocated `[actor]` discovery, and since
connections share one home database, the shared home config carries no `[actor]`.

Fix: resolve `actor_id` from a **per-connection** source (the process's own project/cwd-anchored
config) **independently** of the database-anchored `config_id` resolution. This is provably
compatible with #651's invariant because:

- `actor_id ∉ config_id` — a byte-identical `config_id` is observed while only actor/namespace
  differ; the actor is not part of the fingerprint. So a per-connection actor tier cannot
  reintroduce `config_id` drift.
- Per ADR-007 Rev 4 Rule 0, `[actor].id` does not become `default_namespace` (`config.rs:441-445`,
  tests `runtime.rs:1092`, `engine_config.rs:1081`). So restoring the actor tier changes neither
  the storage namespace nor `config_id` — it only sets the write-stamp actor and folds into
  `visible_namespaces` for read-widening.

The daemon-spawned MCP process inherits its own working directory, so a project/cwd-anchored
`[actor]` reaches each process with **no** per-connection env plumbing.

**Rejected alternatives**

- **(II) Env/CLI (`KHIVE_ACTOR` / `--actor`) as the primary surface.** Rejected as primary on
  two grounds. (a) A shared root `.mcp.json` gives every spawned MCP process the **same** static
  `env` block, so a per-connection env var requires each launcher to export a distinct value
  before spawn — a per-connection env-plumbing layer the project-config path avoids entirely.
  (b) The recorded fleet-operations direction (2026-07-04) is that the attribution surface is the
  config `[actor]` block, not the env var. Env/CLI stays a valid **override** (already wired,
  flag==env parity per `serve.rs` ~line 1799), just not the primary per-connection surface. Note
  that the explicit `--namespace` env/CLI path additionally couples namespace→actor
  (`serve.rs:1417`), which _does_ produce `namespace_mismatch`; the project-config path does not.
- **(III) Revert #651.** Rejected: #651 fixes a genuine `config_id`-coherence defect (decoy cwd
  configs → divergent `config_id` → `ConfigMismatch` fallback). Reverting trades one fallback
  cause for another. The correct move is to split **actor** discovery out of `config_id`
  discovery, preserving #651's coherence guarantee.

**Strongest reason:** project-local `[actor]` restore fixes attribution without touching
`config_id` or `default_namespace` (both provable from source), so it is the only option
compatible with #651 that needs zero per-connection env plumbing.

**Why this fork requires ratification:** the injection surface sets a durable, public config-schema
convention (which resolution tier owns actor identity, and the precedence order among project
config / env / CLI) that many connections depend on. It is reversible but conventional; the
precedence order is the ratification point.

---

## Consequences

**Positive**

- Correct per-connection attribution **and** warm serving at the same time — the smoothness goal
  — over one warm registry.
- Generalizes to the multi-tenant target: per-tenant namespace applied as a per-request
  storage/Gate filter over one warm process, with no N-fold resident memory.
- Fork 2 restores attribution with no new env plumbing and preserves #651's `config_id`
  coherence.
- `khive_daemon_fallback_total{reason="namespace_mismatch"}` drops toward zero for correctly
  configured connections — a direct, measurable verify-by signal.

**Negative / risks**

- **Protocol frame change → version bump.** Old clients must fall back; the existing
  `version_mismatch` path handles this, and a mixed-version host during rollout degrades to
  local dispatch (today's behavior — safe, not a regression).
- **Softening the namespace reject weakens attribution integrity between same-uid connections, not
  data isolation.** On the single-principal host, safety does not rest on the Gate: it rests on the
  socket being `0600` owner-only, all connections being the same uid, and the database being
  already directly accessible to that uid (any same-uid process can open `~/.khive/khive.db` or run
  its own `kkernel` against it). Softening therefore grants **no new data capability** — a same-uid
  process could already read and write that data. What it newly permits is a same-uid connection
  asserting another namespace and having the warm daemon serve and write-stamp under it: an
  **attribution-spoofing** surface between trusted same-uid seats, low severity on this host. The
  earlier framing that the Gate is the isolation seam here does not hold: the shipping Gate is
  `AllowAllGate` (`crates/khive-runtime/src/config.rs:325`), and even a real Gate checks a fixed
  process-level `self.actor_id` (`crates/khive-runtime/src/pack.rs:898-902`), not the connecting
  peer.
- **On a shared/hosted host this is high severity, and closing it is more than a policy binding —
  no connection-identity mechanism exists today.** The daemon captures no peer credentials at
  `accept` (no `SO_PEERCRED`/`UCred` anywhere in `crates/`), the frame's `namespace`/`config_id`
  are self-reported client fields, and `config_id` is a config-compatibility fingerprint that two
  mutually-untrusting same-uid processes compute byte-identically — none of these identifies the
  connecting principal. Hosted enablement therefore requires **building** connection identity, not
  merely adding policy on top of it. **This is the surface the spec-gate owner should scrutinize
  hardest** — see Open question 1 and §Acceptance conditions.

**WAL / #580 / ADR-091 interaction (noted, not solved)**

A single long-lived warm reader pins the WAL snapshot (ADR-091). Serving **more** namespaces from
the **same** warm process adds **no** readers — it is the same single reader — so Fork 1 does not
worsen WAL snapshot lifetime. The rejected N-registry design **would** add readers, which is a
further reason to prefer one registry. This ADR does not change checkpoint behavior and does not
attempt the #580 fix.

---

## Acceptance conditions

This ADR is accepted with the following binding conditions:

1. **Fork 1 is approved as designed** — a per-request identity context over one shared warm
   registry, the `config_id` reject stays hard, and the light-end → escalation criterion stands
   as written.
2. **Softening the namespace reject is accepted for the single-principal `0600` socket only.**
   The single-principal safety rests on the `0600` owner-only socket, all connections being the
   same uid, and the database being already same-uid-accessible — **not** on the Gate (see
   Negative/risks). Hosted / multi-tenant enablement — serving more than one connection principal
   over a shared socket — is **blocked** until a separately gated ADR **builds a connection-identity
   mechanism** (peer-credential capture at `accept`, a connection principal threaded per-request
   into the `GateRequest`, and `frame.namespace` threaded into `dispatch`) and binds that principal
   to its allowed namespaces. This is more than adding a policy on top of existing identity — no
   such identity exists today (Open question 1). Until that ADR is accepted and implemented, the
   softened reject must not be relied on as a tenant-isolation boundary and multi-principal serving
   stays disabled.
3. **Open question 4** (frame-side vs Gate-side `visible_namespaces` for hosted tenants) is
   **deferred to that same future Gate-binding ADR**, not resolved here.
4. **Fork 2 is approved independently** of the hosted conditions above and proceeds first. The
   actor-attribution precedence order is now **ratified**: explicit CLI `--actor` flag → project
   `.khive/config.toml` `[actor]` id → `KHIVE_ACTOR` env → anonymous. Fork 2 implements this full
   chain (the `--actor` flag already exists and feeds `actor_id`; the new tier is the
   cwd/project-anchored `[actor]` restore slotted below CLI and above env). The `KHIVE_ACTOR`
   env tier sets `actor_id` only, never `default_namespace` (see Open question 2 resolution). The
   prior "ratification-pending / project-above-env only" note is superseded by this ratification.
5. **Fork 2 ships a pinning regression test as a hard acceptance-bar item** (same standing as the
   config_id parity tests): a seat-shaped cwd — a project root carrying `.khive/config.toml` with
   an `[actor]` id, with the database anchored elsewhere — must resolve that actor through the full
   tier chain end-to-end, config-discovery step included, asserting `actor_id` equals the project
   `[actor]` id (not `local`/anonymous). The test must exercise the real discovery path so any
   future change to config discovery (the exact class the discovery-relocation regression hit)
   fails loudly instead of silently collapsing the fleet to `local`.
6. **Open question 3** (re-verify the env observation) folds into the Fork 1 implementation test
   plan, not a separate investigation.

---

## Scope / Non-goals

- **Does not implement.** This is a design contract; the code change is a separate, gated task.
- **Does not solve #580 / WAL snapshot lifetime** (ADR-091 owns that). It only establishes
  non-interaction.
- **Does not change** the `config_id` fingerprint definition, the entity/edge/note taxonomy, or
  any pack vocabulary.
- **Does not add** a socket auth/admin plane, an HTTP listener, or a snapshot-format change —
  ADR-049's other scope boundaries are unchanged. Only the "No multi-namespace daemon" clause
  (lines 108-111) and the request-frame shape are amended.

---

## Implementation sketch (for sizing — not a spec to implement verbatim)

- `crates/khive-runtime/src/daemon.rs`: add `actor_id: Option<String>` and
  `visible_namespaces: Vec<String>` to `DaemonRequestFrame`; bump `PROTOCOL_VERSION`; in
  `handle_conn` (~line 355) replace the strict `frame.namespace != dispatcher.namespace()`
  reject (~line 393) with accept-and-serve-under-frame-identity; keep the `config_id` reject
  (~line 404). Extend the `DaemonDispatch::dispatch` signature (~line 252) with the identity
  context.
- `crates/khive-runtime/src/pack.rs`: in `dispatch` (~line 878), when a request identity is
  present, mint the `NamespaceToken`/actor from it (via `authorize_with_visibility`,
  `runtime.rs:475`) instead of the baked `default_namespace` / `visible_namespaces` / `actor_id`
  (~lines 979-1012). Baked scalars remain the default for identity-less local dispatch.
- `crates/khive-mcp/src/server.rs`: in `wire_daemon_frame` (~line 1230) populate the new frame
  fields from the resolved request identity (today sets only `namespace: self.default_namespace`,
  ~line 1235).
- Fork 2 — `crates/khive-mcp/src/serve.rs`, `crates/khive-runtime/src/config.rs`,
  `crates/khive-runtime/src/engine_config.rs`: add a per-connection actor-resolution tier that
  reads project/cwd-anchored `[actor]` independently of the database-anchored `config_id`
  discovery (#651); preserve the env/CLI override and flag==env parity; do not reintroduce
  actor→`config_id` or actor→`default_namespace` coupling (guarded by tests `runtime.rs:1092`,
  `engine_config.rs:1081`).
- Metric: keep `khive_daemon_fallback_total{reason}` as the verify-by signal.

**Verify-by**

1. `khive_daemon_fallback_total{reason="namespace_mismatch"}` → ~0 for correctly configured
   connections.
2. A warm-served write from a connection whose config declares `[actor] id = agent:x` is stamped
   `from = agent:x` — not the daemon's actor and not `local` — asserted via a memory/comm write
   readback routed through the warm daemon.
3. `config_id` is byte-identical across two connections that share one database but declare
   different `[actor]` ids (Fork 2 must not perturb the fingerprint).

---

## Open questions

1. **Gate authorization on a shared socket (hosted bar).** The prerequisite is more fundamental
   than "does the binding live in the Gate": **no connection-identity mechanism exists today.** The
   daemon captures no peer credentials at `accept` (no `SO_PEERCRED`/`UCred` in `crates/`), the
   shipping Gate is `AllowAllGate` and even a real Gate checks a fixed process-level `self.actor_id`
   rather than the connecting peer, and the frame's `namespace`/`config_id` are self-reported client
   fields (`config_id` a config-compatibility fingerprint, not a credential). The future gated ADR
   must therefore **build** connection identity — capture peer credentials at `accept`, thread a
   connection principal per-request into the `GateRequest`, and thread `frame.namespace` into
   `dispatch` — before the connection-principal → allowed-namespaces binding it authorizes can mean
   anything. Security-critical. **Per §Acceptance conditions, this is deferred to that separately
   gated ADR, which must land before hosted/multi-tenant enablement — it is not resolved here.**
2. **Actor precedence order** — **RESOLVED (ratified, §Acceptance conditions cond. 4):** explicit
   CLI `--actor` flag → project-config `[actor]` id → `KHIVE_ACTOR` env → anonymous. The `KHIVE_ACTOR`
   env var sets `actor_id` **only** (tier 3); it does **not** set `default_namespace` — identity is not
   namespace (ADR-007 Rule 0). Fork 2 implements this by removing the `env = "KHIVE_ACTOR"` binding
   from the tier-1 `--actor` clap field (`crates/khive-mcp/src/args.rs:29`), so the command-line flag
   is the only tier-1 actor source, and reading `KHIVE_ACTOR` only at the tier-3 actor fallback below
   the project tier. This also resolves a pre-existing cross-path inconsistency: the `kkernel exec`
   path already leaves `default_namespace=local` when `KHIVE_ACTOR` is set, whereas the serve path
   previously let `KHIVE_ACTOR` flip the namespace — after Fork 2 both paths agree (env sets
   `actor_id`; the namespace stays `local` unless an explicit `--namespace`/`--actor` flag is passed).
3. **Driving analysis's env observation — RESOLVED by Fork 2.** The live audit observed the serve path
   letting `KHIVE_ACTOR` set `default_namespace` (an artificial-probe run), diverging from the
   `kkernel exec` path (which leaves it `local`). Fork 2's actor-only env tier (OQ2) removes that
   divergence: no path sets `default_namespace` from `KHIVE_ACTOR` after this change.
4. **`visible_namespaces` on the frame.** For a single principal, read-widening is derivable from
   the actor; for hosted tenants it may need explicit per-connection scoping. Whether it must be
   threaded on the frame or derived Gate-side from the connection principal is **deferred to the
   future Gate-binding ADR** (§Acceptance conditions), since it only bites at the hosted bar.
