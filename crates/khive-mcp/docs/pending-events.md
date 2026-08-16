# Pending events — design notes

Design rationale for `src/pending_events.rs`, the scheduled-event drain that
backs `kkernel exec --pending-events` (one-shot) and the daemon-resident tick
(ADR-106, `schedule_tick_loop`). See the module-level rustdoc for the
callable contract (invocation modes, namespace isolation, repeat
advancement, missed-event policy); this document carries the narrative and
history behind those contracts. The drain's API-level contract rationale
(the `rt`/`server` pair, claim/finalize semantics) lives in the sibling
[`api/pending-events.md`](api/pending-events.md).

## Why this module lives in `khive-mcp`, not `kkernel`

The module originated in `kkernel` but was moved to `khive-mcp` because the
daemon tick loop needs to call it in-process from `khive-mcp::serve`, and
`khive-runtime` (where the daemon's socket/accept loop lives) cannot depend
back on `khive-mcp` — `khive-mcp` already depends on `khive-runtime`, so a
dependency the other way would cycle. `kkernel` already depends on
`khive-mcp`, so its `exec --pending-events` entry point simply calls
`run_pending_events` here instead of keeping a local module.

## Invocation modes

- **One-shot** (`kkernel exec --pending-events`, cron-friendly): calls
  `run_pending_events` directly. Suitable for `* * * * * kkernel exec
  --pending-events` to achieve minute-granularity delivery.
- **Daemon-resident tick** (ADR-106): `schedule_tick_loop` calls
  `run_pending_events_on` against the daemon's own resolved `KhiveRuntime`
  handle on a fixed interval for the lifetime of the warm `khived` daemon
  process. It runs as the ADR-119 `schedule-tick` component only in daemon
  role, never from a short-lived stdio client, with tracked cancellation,
  bounded restart, and component health.

Running both an external cron entry and the daemon tick at once is safe: the
drain's `pending -> firing` CAS claim (`claim_pending_event`) makes
concurrent or overlapping invocations harmless by construction — at most one
caller ever wins a given row.

## Namespace isolation

Each event fires in its own namespace: the action is dispatched through the
MCP server's registry with the event's namespace injected as the
`namespace=` parameter, so all writes land in the event's namespace. Replay
derives its actor from an immutable, target-bound provenance event written
by the schedule handler; `created_by_actor` note metadata is never an
authorization source. Executable `scheduled_event` state is
schedule-managed and rejects generic KG update/merge, so provenance cannot
authorize rewritten intent. A generic legacy row without provenance fails
closed instead of inheriting daemon authority.

## Repeat advancement

Named aliases are advanced as follows:
- `"daily"`   → `trigger_at + 1 day`
- `"weekly"`  → `trigger_at + 7 days`
- `"monthly"` → `trigger_at + 1 calendar month`

Five-field cron is rejected by schedule creation because the executor cannot
advance it. A legacy row carrying any unsupported repeat fails closed before
invocation instead of silently degrading to one-shot delivery.

## Missed-event policy (ADR-106 amendment)

An event is "missed" when it is discovered overdue by more than
`KHIVE_FIRE_GRACE_SECS` (default 300s / 5 minutes). A missed event is
**never dispatched** — it is marked `status="missed"` with `missed_at`
stamped (epoch µs) and `fired_at` left null. A missed *repeating* event is
skipped for this occurrence and re-armed at the next occurrence strictly
after now (looping past every accumulated occurrence) — it never fires a
catch-up burst. This means a daemon that was offline for a long stretch (or
a first boot against a store with a large stale backlog) marks the entire
overdue backlog missed on its first tick and dispatches zero of them. The
creator-identity fence runs first for generic actions: an unattributed
legacy row becomes `failed`, not `missed`, even when stale.

## Server construction: explicit namespace, implicit actor

`run_pending_events_with_config` resolves its server through the same
multi-backend-aware construction the daemon boot path uses
(`khive-mcp::serve::build_server_with_explicit_namespace`), rather than a
throwaway `RuntimeConfig::default()`. `RuntimeConfig::default()` is env-only
— it never consults `khive.toml` (`[[backends]]`, `[actor] id`,
`[packs.*].backend`) at all, so a project with a declared multi-backend
config or a config-file actor identity would be silently invisible to this
one-shot CLI path even though `kkernel mcp --daemon` (and, for ordinary
ops, `kkernel exec`'s own `resolve_runtime_config` call) both resolve it.
The wrapper also returns the fully-wired `KhiveMcpServer` for the resolved
pack set (single- or multi-backend), so replayed actions route through the
correct per-pack backend exactly like the daemon tick does — not a single
runtime standing in for every pack. An explicit `kkernel exec --config`
path is forwarded unchanged; otherwise `config: None` still triggers
`khive.toml`'s standard cwd/home search order inside
`resolve_runtime_config`.

This does **not** call `crate::serve::build_server` directly: `build_server`
derives both `namespace_explicit` and `actor_explicit` from
`resolve_cli_namespace`, which treats "a namespace value is present" and
"the operator typed `--actor`/`--namespace`" as the same fact — true for a
real CLI parse, where there is no other way a namespace value could appear.
This wrapper's `namespace` argument is not a CLI flag the operator typed;
it is a plain default this function was called with (`"local"` unless the
caller passed something else), and `resolve_runtime_config` (`serve.rs`)
treats a genuine explicit actor override as authoritative — it clears any
configured `[actor] id` for the resolved-to-`"local"` case rather than
falling through to it. Routing this default namespace through
`build_server` would therefore silently discard a project's configured
`[actor] id`, and — under strict actor mode with the comm pack — could make
server construction itself fail despite a valid config.
`build_server_with_explicit_namespace` is the seam that lets this caller
assert the narrower, correct semantic instead: the namespace *is* a real
default (`namespace_explicit: true`, so it still becomes `default_namespace`
and fills `actor_id` when non-`"local"`), but it is **not** an actor
override (`actor_explicit: false`), so a `"local"` resolution keeps falling
through to the project/db/env actor tiers — exactly the shape `kkernel
exec`/`kkernel reindex` already use via their own direct
`resolve_runtime_config` calls.

## Keyset pagination and due-ness comparison

The per-namespace drain loop uses bounded, mutation-immune keyset
pagination. An earlier version snapshotted every `status="pending"` row for
the namespace into one `Vec` before any mutation, which fixed a
`LIMIT/OFFSET` skip bug (mutating a row out of the `status="pending"`
predicate mid-page shifted every subsequent page) but introduced a new
failure mode: the snapshot filter checked only `status`, not `trigger_at`,
so a namespace with one due event buried in a large future schedule pulled
the entire future backlog into memory every tick. The current approach
instead:

1. Pushes the due-ness predicate (`trigger_at <= now`) into the SQL `WHERE`
   clause directly, via a raw statement (bypassing `NoteFilter`, whose
   `order_by`/property-filter surface can only express JSON-path
   predicates, not compare a JSON path against a bind parameter with `<=`)
   — future events are never fetched at all, so the working set is bounded
   by the due backlog, not the namespace's total schedule size.
2. Pages via a `(created_at, id)` keyset cursor instead of `LIMIT/OFFSET`.
   Both columns are immutable — this drain never rewrites `created_at` or
   `id` — so a row's claim/dispatch/finalize mutation between pages can
   never shift a later page's boundary, and at most `PAGE_SIZE` (200) rows
   are held in memory at once.

The due-ness predicate itself compares via SQLite's `datetime(...)`, not a
raw string `<=`: stored `trigger_at` values are **not** normalized to UTC —
`khive-pack-schedule`'s `handle_remind`/`handle_schedule` deliberately
round-trip the caller's original string (offset included), and
`validate_at` accepts any RFC 3339 offset. A raw lexicographic `<=` against
a UTC `now`-string therefore mis-ranks any non-UTC-offset `trigger_at`:
e.g. `"2026-07-10T02:00:00+04:00"` (chronologically `2026-07-09T22:00:00Z`,
overdue) sorts *after* a UTC `now` string like
`"2026-07-10T00:47:00.123+00:00"` as raw text, so it would never be
fetched — never fire, never get marked missed, forever. `datetime(...)`
normalizes both sides to UTC before comparing, so the predicate is
chronological regardless of the stored string's offset; storage itself is
unchanged, only this fetch-bound comparison is normalized. `datetime()`
returns NULL for a value it cannot parse, and `NULL <= anything` is NULL
(never true) — the `OR ... IS NULL` clause keeps an unparseable
`trigger_at` row in the candidate set instead of silently dropping it, so
the Rust-side unparseable-`trigger_at` branch (which logs and advances the
cursor past it) still sees it. `validate_at` rejects unparseable
`trigger_at` at write time, so this only matters for a hand-written or
pre-validation row.

`discover_pending_namespaces`'s namespace-level pre-filter is held to the
same correctness bar: it is a pre-filter gate for the per-namespace
candidate scan, not the final due-ness decision, but a namespace excluded
there never reaches that scan at all, so a raw-text comparison would
silently exclude an entire namespace from every future pass — not just skip
one row.

Regression coverage: a due event stored with a positive `trigger_at` offset
(sorting lexicographically *after* a UTC "now" string) must still fire, and
a future event stored with a negative offset (sorting lexicographically
*before* a UTC "now" string, a false positive under the old raw-text
predicate) must not — the Rust-side `trigger_at > now` re-check is the
backstop for the latter direction even before the SQL fix. A backlog larger
than the drain's page size (201 rows, one more than `PAGE_SIZE`) must be
fully processed in one drain pass, not silently truncated at the page
boundary the way the old `LIMIT/OFFSET` implementation could be.

## Offset preservation and relaxed RFC 3339 parsing

`trigger_at` is parsed as `DateTime<FixedOffset>`, not straight to
`DateTime<Utc>`, so the caller's original UTC offset is retained alongside
the UTC instant: `khive-pack-schedule` round-trips the caller's original
`trigger_at` string verbatim, offset included, and that offset must survive
repeat advancement — rendering the advanced `trigger_at` via a bare
`DateTime<Utc>::to_rfc3339` always stamps `+00:00`, silently rewriting a
non-UTC schedule to UTC on its first advance. Regression coverage asserts
that a `+04:00` schedule that fires and advances still carries `+04:00`
(and the same local wall-clock hour) on its next occurrence.

Parsing uses the same relaxed grammar as the write boundary
(`khive-pack-schedule`'s `at.parse::<DateTime<Utc>>()`), not the strict
`DateTime::parse_from_rfc3339`: already-persisted `trigger_at` strings can
use the relaxed RFC 3339 form (space instead of `T`, offset without a
colon), and the strict parser would silently skip them forever. A legacy
stored timestamp such as `2026-07-14 09:00:00+0400` must still be
recognized as due and advanced.

## Writer-pool checkout contention under CI

`replayable_action_dispatches_without_failure_at_trigger_time` asserts that
a canonical `schedule.schedule` payload dispatches with zero failures on its
first trigger-time replay. A single drain pass can legitimately report
`failed >= 1` for this exact payload with no logic bug involved:
`claim_pending_event` checks out the pool's single writer connection via
`WriterPool::writer()`, which is `parking_lot::Mutex::try_lock_for(
checkout_timeout)` (default 5s, `khive-db/src/pool.rs`) — a bounded wait,
not a logic gate. On a CPU-oversubscribed CI runner (`cargo test
--workspace` runs dozens of test binaries, each further parallelized,
against 2-4 physical cores), a task can be scheduled off-CPU for longer than
the checkout timeout while queued for that mutex, so the checkout times out
*before* the claim's SQL `UPDATE` ever runs: the drain loop counts
`summary.failed += 1` and the row stays `status="pending"`, retryable on the
next cron drain — but the zero-failure contract this test asserts still
requires the first invocation to succeed.

This was confirmed live: the test passed 100/100 serial runs, 8/8
full-suite runs, and 3/3 `cargo llvm-cov` runs on a 12-core box, yet failed
on CI on a commit whose source did not change from a passing run — the
signature of scheduler contention, not a deterministic dispatch defect.
Rather than weakening the assertion (which could mask a genuine first-drain
dispatch regression), the test removes the contention boundary
deterministically: it runs serially (`#[serial_test::serial]`) and raises
the writer-pool checkout timeout for its own duration, keeping the original
single-drain zero-failure contract intact.
