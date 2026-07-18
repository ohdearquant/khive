# Channel health

Technical reference for the `comm` pack's channel-heartbeat write path
(`comm.ingest`'s companion operational surface) and the read-only `comm.health`
verb — how poll-loop outcomes are persisted and reported, spanning `lib.rs` and
`handlers.rs`.

## `lib.rs::CHANNEL_HEALTH_NAMESPACE` — rationale

Channel heartbeat rows are an OPERATIONAL surface, not message data. `#606`
introduced this constant as the fixed namespace every heartbeat write landed
under; `#917` narrowed its role. `handle_heartbeat` no longer pins its write
to this constant: it persists under `token.namespace()`, the same
dispatch-authorized namespace every other comm verb uses (khive #917). This
constant is now the namespace the local single-tenant poll loop (`khive-mcp`'s
`record_channel_heartbeat`) passes explicitly as its own `namespace` dispatch
param, so that loop's writes still land under `"local"` and must not follow
`KHIVE_EMAIL_INGEST_NAMESPACE` (or any other caller-chosen namespace) even
though that env var configures the same daemon's message-ingestion namespace
(khive #606, 2026-07-04).

`comm.health` reads via the dispatch token (`token.namespace()`) too (khive
#877): the same explicit `namespace=` escape / `"local"` default every other
comm verb uses. An unscoped `comm.health()` call still defaults to `"local"`
and so still observes the rows the local poll loop wrote under this constant —
but a call with an explicit non-local `namespace=` reads that namespace
instead, observing the heartbeat rows an authorized per-tenant writer (khive
#917) produced there, and must not fall back to this constant.

## `handlers.rs::heartbeat_note_id`

Deterministic UUID identifying the `channel_health` row for one `(namespace,
channel_kind, channel_slug)` triple (khive #606). Deterministic (not
`Uuid::new_v4`) so `handle_heartbeat` can compute the same id on every poll
tick and `upsert_note`'s `INSERT OR REPLACE` updates the same row instead of
accumulating a new one per tick. Keying by slug in addition to kind is the
point of #606's amendment 2: two accounts of the same kind (e.g. two
mailboxes, both `kind() == "email"`) must not collapse into a single row.

The three components are hashed as a JSON array of strings, NOT joined with a
`:` delimiter. Namespaces may themselves contain `:` (hierarchical namespace
strings are explicitly allowed), so a delimiter-joined
`format!("...:{a}:{b}:{c}")` is not an injective encoding:
`(namespace="a:b", channel_kind="c", channel_slug="d")` and
`(namespace="a", channel_kind="b:c", channel_slug="d")` both produced the
identical string `"khive:channel_health:a:b:c:d"` under the old scheme.
`serde_json::to_vec` of an array of strings is unambiguous — each element is
quoted and internal quotes/backslashes are escaped — so distinct triples
always serialize to distinct byte sequences.

## `handlers.rs::handle_heartbeat`

Persists one poll attempt's outcome into the channel's heartbeat row (khive
#606). Internal subhandler with no MCP wire path: its production local caller
is the daemon's channel poll loop
(`crates/khive-mcp/src/serve.rs::channel_poll_loop`); khive #917 also lets
authorized per-tenant writers reach it via `dispatch_as` (see the persistence
note below).

Read-modify-write against the existing row (if any) so that:

- `created_at` is preserved across updates (first-seen time), not reset every
  tick.
- `last_error` is RETAINED across a subsequent success (design review
  amendment 3): callers compare `last_error.at` against
  `last_success_at`/`last_failure_at` to tell a resolved issue from a live
  one, so a success must never clear it.
- `consecutive_failures` resets to 0 on success and increments on failure,
  read from the prior row rather than any in-process counter, so it is
  correct even across a daemon restart.

Heartbeat rows are an OPERATIONAL surface, not message data (#606). Persists
under `token.namespace()` (khive #917) — the dispatch-authorized namespace
every other comm verb uses — not the fixed `crate::CHANNEL_HEALTH_NAMESPACE`
constant #606 originally pinned it to. `comm.heartbeat` is a `Subhandler`
(never reachable from the MCP wire); the gate check
`VerbRegistry::dispatch_with_identity`, which runs for every dispatch
(subhandlers included), is the sole authorization boundary (ADR-018), so this
handler must not layer a second, handler-local namespace check on top of it.

The local single-tenant poll loop (`khive-mcp`'s `record_channel_heartbeat`)
is unaffected: it always passes `crate::CHANNEL_HEALTH_NAMESPACE` explicitly as
its `namespace` dispatch param, so it keeps writing under `"local"` exactly as
before. An authorized per-tenant writer (#917) instead dispatches via
`VerbRegistry::dispatch_as` with a `VerifiedActor` (an out-of-band
authenticated tenant principal, never a wire-supplied field — this verb has no
wire path at all) and passes that tenant's own namespace, so its heartbeat
rows land under that namespace.

`handle_health` (khive #877) reads from `token.namespace()` the same way, so a
tenant-scoped `comm.health` now observes that tenant's writer state instead of
an empty set by construction. An unscoped read still resolves to `"local"` and
sees the poll loop's rows.

## `handlers.rs::channel_health_to_json`

Projects a persisted `channel_health` note into the `comm.health()` channel
entry shape. Missing fields (a row written before a given property existed)
default to `null`/`0` rather than panicking — forward-compatible with rows
written by an older heartbeat writer.

## `handlers.rs::handle_health`

Read-only per-channel health snapshot (khive #606).

Reads the daemon-persisted `channel_health` rows from `token.namespace()`
(khive #877) — the same injected-namespace resolution every other comm verb
uses (ADR-007 Rev 6 Rule 3: `namespace=` is the caller's explicit escape;
absent that, the token pins to `"local"`). Unscoped callers (single-tenant
local daemon, the common case) see exactly what they saw before this fix,
since the local poll loop's heartbeat rows still land under
`crate::CHANNEL_HEALTH_NAMESPACE` (`"local"`) and an unscoped token also
resolves to `"local"`. A caller that passes an explicit non-local `namespace=`
now reads that namespace's rows only — never `"local"`'s — observing the
heartbeat rows an authorized per-tenant writer (khive #917) produced there, and
closing the cross-namespace operational-surface leak that held this verb off
the cloud data plane (#877).

`role` answers "who owns the loops", not "whose memory answered": any
persisted row means some daemon owns the channel loops, so `role` is reported
as `"daemon"` with `source: "daemon-heartbeat"` regardless of whether THIS
process is that daemon. `role: "client"` with an empty `channels` array is
correct both when no daemon heartbeat state exists at all (fresh install, or a
daemon that has never completed a poll tick) and when the caller's injected
namespace has no heartbeat rows of its own — the comm pack has no visibility
into which channels are configured (that lives in `khive-mcp`/
`khive-channel-email`), so an empty result is the only fact-based response
available at this layer.

`namespace` in the response (khive #877) is the namespace actually read,
echoed back so the shape is self-describing for both the unscoped and the
explicitly-scoped case: `role: "client"` / empty `channels` is now ambiguous on
its own, since it is also the correct, expected shape for a `namespace=`-scoped
call whose namespace has no heartbeat rows yet: `comm.heartbeat` now persists
under `token.namespace()` (khive #917), so an authorized per-tenant writer's
rows are what a scoped read observes, and an empty `channels` array for a
scoped read means no writer has been authorized for that namespace yet, not
that the feature is unreachable. A caller reading `namespace: "tenant-a"`
alongside `role: "client"` can tell "no daemon anywhere" (unscoped call,
`namespace: "local"`) apart from "no rows written under my scope yet" (scoped
call, `namespace: "tenant-a"`) without khive silently falling back to `"local"`
to paper over the difference.

Never returns a computed `healthy: bool` (design review amendment: "report
timestamps only") — staleness/alerting judgment belongs to the caller.

`resource` (ADR-103 Stage 1, issue #723 ask 2): a process-level self-report of
this process's own cumulative CPU time and RSS (via `getrusage`,
`khive_runtime::process_resource_usage`) plus the names of any background
phases (e.g. `ann_warm`) currently in flight in this process
(`khive_runtime::active_phase_names`). "This process" is, in the common case,
the daemon itself: a client-role stdio session without an in-memory poll loop
of its own still forwards `dispatch` calls to the daemon over its socket, so
this handler body executes inside the daemon process, not the thin client.
`cpu_us`/`rss_bytes` are `null` only if the underlying `getrusage` read is
unavailable on this platform; `active_phases` is always present and empty
when nothing is in flight. Raw observations only, per the same "no computed
healthy bool" rule as the rest of this verb — attributing severity to a given
CPU/RSS number is the caller's judgment, not this verb's.
