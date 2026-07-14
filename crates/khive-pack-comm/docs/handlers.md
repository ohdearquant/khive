# comm pack — internal handler notes

Rationale, incident history, and algorithm detail relocated from `src/*.rs` doc
comments during the rustdoc condense pass. These items are all `pub(crate)` or
private — they never render on docs.rs — so their `.rs` doc comments now carry
only a 1-3 line summary; this file is the durable home for the "why".

## `lib.rs::CHANNEL_HEALTH_NAMESPACE` — rationale

Channel heartbeat rows are an OPERATIONAL surface, not message data: the write
must not follow `KHIVE_EMAIL_INGEST_NAMESPACE` (or any other caller-chosen
namespace) — `handle_heartbeat` is the ONLY comm handler pinned to this
constant (khive #606 design review Blocker fix, example actor 2026-07-04).

`comm.health` no longer reads this constant unconditionally (khive #877): it
resolves its read namespace from the dispatch token (`token.namespace()`), the
same explicit `namespace=` escape / `"local"` default every other comm verb
uses. An unscoped `comm.health()` call still defaults to `"local"` and so
still observes rows this constant wrote — but a call with an explicit
non-local `namespace=` reads that namespace instead, and must not fall back to
this constant to find heartbeat state a single-namespace daemon wrote
elsewhere.

## `message.rs::resolve_id`

Accepts a 36-char hyphenated UUID or an 8+ hex-char short prefix. The prefix
is resolved via `runtime.resolve_prefix` (namespace-scoped).

## `message.rs::rollback_outbound`

Rolls back a partially-written outbound note after a later `dual_write_message`
step fails (issue #460). Uses a row-first compensating delete so that a
cleanup failure cannot leave the outbound row (and thus the failed send's live
message) behind. Returns `original` unchanged when rollback fully succeeds;
returns a composite `RuntimeError::Internal` naming both the original failure
and the rollback cleanup failure when the row was removed but cleanup did not
complete.

## `message.rs::dual_write_message`

Writes an outbound copy (caller namespace) and an inbound copy (recipient
namespace), rolling back the outbound note if the inbound write fails
(atomicity guarantee).

`subject`, `thread_id` are optional. `sent_at` is the RFC3339 timestamp for
both copies. `from_actor` and `to_actor` are optional actor labels (ADR-057)
stored in properties.

Cross-namespace thread root invariant: when a root message is sent (i.e.,
`thread_id` is `None`), both the outbound and inbound copies must share the
same canonical `thread_id` — the sender's outbound UUID. This ensures that
`comm.thread(id=outbound_id)` can find replies written in any namespace,
because all replies carry the same canonical thread_id regardless of which
copy they were replying to.

When `thread_id` is already supplied (reply path), it is forwarded unchanged
to both copies.

`in_reply_to_message_id` is the parent's wire Message-ID (angle-bracketed),
when this write is a reply to a message with a known one (issue #403). It is
stored verbatim on both copies as `in_reply_to_message_id`; the outbox
delivery loop reads it back to set the RFC 822 `In-Reply-To` header for native
MUA conversation grouping. `None` when there is no known parent Message-ID (a
plain send, or a reply whose parent has none).

`references_chain` is the full RFC 5322 `References` value for this reply: the
parent's existing chain (if any) followed by the parent's Message-ID,
space-separated angle-bracketed ids (issue #403 finding: References must
preserve ancestry, not truncate to the immediate parent). Stored verbatim on
both copies as `references_chain`; the outbox delivery loop reads it back to
set the `References` header, and a further reply reads it back (direction-aware,
via `parent_references_chain`) to extend the chain again. `None` when there is
no known parent Message-ID (mirrors `in_reply_to_message_id`).

## `handlers.rs::handle_send`

Creates a message note in the caller's namespace (outbound) AND delivers an
inbound copy addressed to the actor label supplied in `to` (ADR-057).

Both copies land in the caller's namespace; no cross-namespace write occurs.
`from_actor` is set to `token.namespace().as_str()`. `to_actor` is set to the
`to` argument. When the caller's actor label is `"local"` (single-actor
fallback), `comm.inbox` does not apply an actor filter, preserving backward
compatibility.

The routing `from` and `to` passed to `dual_write_message` are both set to the
caller's namespace string so that `from == recipient_ns_str` is always true:
this naturally bypasses the cross-namespace allowlist gate in
`dual_write_message` (ADR-057 §"Interaction with ADR-040"). The actor labels
are propagated via the `from_actor`/`to_actor` arguments and stored in message
properties.

### Self-send collapse guard (#820)

A resolved target that equals the sender's own actor identity is, outside the
anonymous single-tenant fallback ("local"), almost always a mis-resolution
rather than intent — most commonly a sub-agent session spawned in the same
project scope trying to reach a distinct parent orchestrator actor. Both
processes resolve `[actor] id` from the same worktree-scoped `.khive/config.toml`
(ADR-096 Fork 2's project-local `[actor]` injection tier is per-project, not
per-session), so the sub-agent's `from_actor` and the parent label it names
collapse onto the identical string with no error, no warning, and no distinct
inbox: the message silently "delivers" to the sender's own attributed identity
instead of a genuinely different principal. Rejected by default; a caller that
truly means to message its own inbox (e.g. a personal reminder) must say so
explicitly via `self_send=true`, turning the collapse loud instead of silent.
`to_actor == "local"` is exempted: that is the anonymous single-tenant
party-line default (both sender and recipient unattributed), not a collapsed
distinct-principal address.

### Unattributed-caller warning (#200)

Addressed sends from an unattributed caller stamp `from_actor="local"`, which
causes reply-threading collapse when multiple unconfigured actors interact.
Known limitation pending issue #75 (actor identity per request). A visible
warning is surfaced so operators can diagnose mis-attribution; the send
proceeds rather than hard-erroring, to preserve backward compatibility with
sessions that set `default_namespace` but not `actor_id`. Uses the shared
actor-identity policy (#567) so this warning fires under exactly the same
"unattributed" definition the gate and token minter use.

## `handlers.rs::handle_inbox`

Lists inbound messages for the caller's actor label (ADR-057).

When the caller's actor label is `"local"` (single-actor fallback), no
`to_actor` filter is applied and the inbox behaves as before (party-line).
When the caller has a non-`"local"` actor label, only messages addressed to
that actor are returned. Legacy messages without a `to_actor` field are
visible regardless (Q3: OR IS NULL).

`from_actor`/`from_prefix` (#493) sender filters are mutually exclusive.
Direction + read-status + `to_actor` filters are pushed into SQL so
`idx_comm_message_direction`/`idx_comm_message_to_actor` are usable; the read
filter uses `json_type` to match the old `as_bool().unwrap_or(false)`
semantics — only JSON boolean `true` counts as read, missing/false/string/
integer all count as unread. `from_prefix` has no SQL `FilterOp`, so when a
sender filter is supplied, pages are scanned in Rust (same unbounded-page-loop
shape `handle_thread` uses) until `limit` matches are collected or the store is
exhausted.

## `handlers.rs::handle_read`

Marks a message as read. Rejects `read()` on outbound messages — "read" is a
recipient action; marking an outbound (sent) message as read corrupts the
read/unread invariant and has no semantic meaning to the sender.

Merges `read: true` into properties and patches in place via a real `UPDATE`
(not `upsert_note`'s `INSERT OR REPLACE`): the latter silently deletes and
re-inserts the row on a primary-key conflict (#780). The `comm.probe` cursor
is keyed on `notes_seq.seq`, which is fixed at first insert and survives such
churn, so this is defensive rather than load-bearing; a metadata patch should
never rewrite the row regardless.

## `handlers.rs::handle_reply`

Replies to a message, threading linkage.

- Issue #403: captures the parent's wire Message-ID so native mail clients
  (not khive's own X-Khive-Thread-ID/external_id correlation) can group this
  reply into the same conversation via In-Reply-To/References. `None` when
  the parent has no wire Message-ID — the reply then sends without those
  headers, exactly as before this feature. References must carry the FULL
  ancestor chain per RFC 5322, not just the immediate parent: the parent's
  existing chain (if any) followed by the parent's own Message-ID. Malformed
  tokens in the parent's stored chain are individually skipped rather than
  corrupting the header.
- UE6-H2: `thread_id` must always be a full 36-char hyphenated UUID. If the
  stored `thread_id` is a valid full UUID, use it; otherwise fall back to the
  original message's own full UUID as the thread root.
- ADR-057: prefer `from_actor`/`to_actor` fields when present (actor-addressed
  messages); fall back to `from`/`to` namespace strings for legacy messages.
- UE6-H1: routes the reply to the "other party" — not always to the original
  sender. If the reply caller is the original sender (`from_actor` or `from`),
  route to the original recipient; if the reply caller is the original
  recipient, route back to the original sender.
- ADR-057: always sets `from_actor`/`to_actor` on replies (fail-closed on
  cross-namespace write). Both copies land in the caller's namespace
  regardless of whether the original message carried actor labels. No legacy
  code path can cause `dual_write_message` to mint a token in a foreign
  namespace.

## `handlers.rs::handle_thread`

Retrieves all messages in a conversation thread, ordered chronologically:
the originating message (the one whose `id` matches the `thread_id` root)
plus all messages whose `properties.thread_id` equals the root UUID.

Cross-namespace thread resolution: when the resolved note carries a
`thread_id` in its properties that differs from its own UUID, that stored
`thread_id` IS the canonical root (e.g. this is an inbound copy of the root,
or a non-root message). `comm.thread` resolves to that canonical root so that
`thread(id=id_A)` and `thread(id=id_B)` both return the full conversation
regardless of which copy UUID the caller holds.

The root ID is validated: it must exist in the caller namespace and its
`kind` must be `"message"`.

Missing/invalid `thread_id` (issue #479b — e.g. a legacy/imported root written
before the canonical field existed) falls back to the passed note's own UUID,
matching ADR-040: a target with no `thread_id` becomes the root for its chain.
The SQL filter only matches `properties.thread_id == canonical_thread_id`,
which misses a root note lacking a `thread_id` property at all, so the
already-validated root note is explicitly appended when the query didn't
already return it — `comm.thread(id=root)` never reports an empty/incomplete
thread for a root that predates the canonical `thread_id` field.

`order` (#494) is a closed set: `"asc"` (default) | `"desc"`. `after` (#494) is
either a message id (short prefix or full UUID, resolved the same way `id` is)
or an RFC 3339 timestamp. An id cursor resolves to the full `(created_at,
full_id)` tuple of the referenced note so ties on equal microsecond timestamps
are broken deterministically instead of being skipped or duplicated. A
timestamp cursor is parsed to microseconds via chrono (matching the pattern in
`khive-pack-brain/src/handlers.rs` and `khive-vcs/src/sync.rs`) rather than
compared as a raw string, so non-canonical but valid RFC 3339 forms
(whole-second `Z`, `+00:00` offsets, ...) compare correctly against khive's
canonical microsecond timestamps. An `after` value that is neither a
resolvable id nor a parseable RFC 3339 timestamp is a hard error — never
silently coerced or treated as "no cursor". Two rows sharing a microsecond
`created_at` (e.g. ADR-057 dual-write self-send copies) are ordered
deterministically by `full_id`. Sorting on the `(created_at, full_id)` tuple
(rather than timestamp alone) keeps ties stable across pages/backends.

`ThreadRow` carries the sort/cursor key `(created_at, full_id)` alongside the
already-rendered message JSON, so the total-order sort and cursor filter
compare exact `(i64, Uuid)` tuples instead of re-parsing the ISO string
embedded in the JSON. `AfterCursor::Id` carries the full tuple for
tie-breaking; `AfterCursor::Timestamp` carries only the parsed microsecond
value since there is no specific row to break ties against.

## `handlers.rs::handle_ingest`

Writes a single inbound message note from a channel adapter. This is a
`Visibility::Subhandler` verb: not accessible via the MCP wire, only callable
from within the process (e.g. the polling loop in `khive-mcp`). It is the
authoritative write path for all channel-delivered messages; the polling loop
must not bypass it.

Issue #479a: a present, non-empty `thread_id` that is not a valid UUID must
fail closed rather than being silently dropped and replaced with a fresh UUID,
which would split the message into the wrong conversation. A blank/absent
value is not an error — it just means "no caller-supplied thread_id".

Thread resolution: when `correlation_external_id` is supplied, the handler
queries for an existing message note whose `external_id` matches that value,
reads its `thread_id`, and attaches the new note to the same thread, so
replies route back to the actor who sent the original, not to the raw email
address. Two-query fallback: `corr` may be either a Message-ID (matched via
`$.external_id`) from a human webmail In-Reply-To header, OR a thread UUID
(matched via `$.thread_id`) from a preserved X-Khive-Thread-ID header on our
own outbound emails. External_id is tried first (preserves the In-Reply-To
path); if that misses, thread_id is tried. Our own outbound mail stores its
Message-ID in wire form `<id@domain>` (angle brackets included), while
`mail_parser` strips the brackets from an inbound `In-Reply-To`, yielding
`id@domain`. The correlation key is matched as received and in its
bracket-toggled form so `<id>` and `id` correlate either way. Both passes are
restricted to outbound notes so an inbound note's own external_id can never be
matched as a threading parent. When a match is found but carries no valid
`thread_id` (issue #479b, e.g. a legacy/imported outbound row), the matched
note's own UUID becomes the canonical root per ADR-040.

`thread_id` priority: caller-supplied > resolved from correlation > new root.
`to_actor` priority: (1) `from_actor` of the correlated original (route reply
back to the sending actor), (2) caller-supplied `default_inbound_actor` (fresh
email landing actor), (3) `p.to.trim()` (back-compat: raw recipient address).

Deduplication: when `external_id` is supplied, `try_create_note` uses a
verify-after-insert check on the durable unique index on `external_id`. A
confirmed duplicate returns `Ok(None)` without error; only an external_id
collision is treated as dedup, other constraint violations surface as errors.

Generic transport-layer metadata passthrough (issue #448): merged additively
so it can never clobber the identity/routing fields (from, to, from_actor,
to_actor, direction, read, thread_id, sent_at, subject, external_id,
wire_message_id, wire_references, channel_kind) — a key already present
always wins. The comm pack does not interpret any metadata key; the email
channel happens to use it for quarantine markers.

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
#606). Subhandler — only the daemon's channel poll loop
(`crates/khive-mcp/src/serve.rs::channel_poll_loop`) calls this.

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

Heartbeat rows are an OPERATIONAL surface, not message data (#606). Persists to
`crate::CHANNEL_HEALTH_NAMESPACE` ALWAYS — never `token.namespace()` — so a
poll loop configured with a non-local `KHIVE_EMAIL_INGEST_NAMESPACE` cannot
cause heartbeat rows to land anywhere but this one fixed namespace. This is
enforced here (not just at the serve.rs call site) so the guarantee holds even
if a future caller passes a different `namespace` dispatch param.

`handle_health` (khive #877) no longer mirrors this fixed pin: it reads from
`token.namespace()`, which only resolves to this same constant for an
unscoped (default-namespace) caller. An explicitly-scoped `comm.health` caller
reads its own namespace, not wherever this handler wrote — do not reintroduce
a `handle_health` read of this constant to "fix" that; it is the
cross-namespace leak #877 closed.

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
since heartbeat rows still land under `crate::CHANNEL_HEALTH_NAMESPACE`
(`"local"`) and an unscoped token also resolves to `"local"`. A caller that
passes an explicit non-local `namespace=` now reads that namespace's rows
only — never `"local"`'s — closing the cross-namespace operational-surface
leak that held this verb off the cloud data plane (#877).

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
call in the shipped OSS build (`comm.heartbeat` only ever persists under
`crate::CHANNEL_HEALTH_NAMESPACE`, and there is no OSS producer for
tenant-scoped heartbeat rows yet — that is cloud-side follow-up). A caller
reading `namespace: "tenant-a"` alongside `role: "client"` can tell "no daemon
anywhere" (unscoped call, `namespace: "local"`) apart from "no rows written
under my scope yet" (scoped call, `namespace: "tenant-a"`) without khive
silently falling back to `"local"` to paper over the difference.

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

## `handlers.rs::PROBE_SQL`

The single indexed read powering `comm.probe` (ADR-D5). `INDEXED BY
idx_comm_message_to_actor` is a regression fence: if a custom bootstrap skips
comm schema-plan application, this query fails loudly instead of silently
degrading to a table scan.

`cursor_us`/`since_us` are keyed on `notes_seq.seq`, not SQLite `rowid` and
not `created_at` (#780, #827):

- `created_at` is an application-clock read taken before a note's write
  acquires the writer critical section, so two concurrent writers can commit
  out of stamp order; a `created_at`-keyed cursor can then advance past a row
  that committed *after* it, permanently hiding that row from every later
  probe.
- `notes.rowid` looked monotonic with commit order, but `notes` has a TEXT
  PRIMARY KEY, so that rowid is *implicit*: SQLite may renumber it on
  `VACUUM` (khive exposes `memory.vacuum`), and reuses the highest rowid once
  that row is hard-deleted (khive exposes a public hard delete), either of
  which can permanently exclude a later message whose rowid lands at or below
  an already-issued cursor.

`notes_seq.seq` fixes both: it is assigned once, inside the same writer
transaction as the note's insert, from a dedicated `INTEGER PRIMARY KEY
AUTOINCREMENT` sequence that VACUUM never renumbers and SQLite never reuses
(see `sql/007-notes-seq.sql`). The wire field names keep the `_us` suffix
(frozen contract, ADR-D5) but the value is an opaque monotonic token, not a
microsecond timestamp; do not revert this to `created_at` or `rowid`.
`created_at_us` on each `new_messages` entry is unaffected: it stays a real
display timestamp, still ordered ascending by `created_at` for readability,
and carries no cursor guarantee of its own.

## `handlers.rs::notes_seq_high_water_mark`

A caller-supplied `since_us` above `notes_seq`'s durable high-water mark
(`sqlite_sequence.seq` for the `notes_seq` table) cannot be a genuine cursor —
`notes_seq` starts at 1 and grows by exactly one per note ever inserted, so no
value this store ever handed out can exceed the highest value it has ever
assigned. Such a `since_us` is a pre-upgrade persisted-timestamp cursor
(#827): a real Unix-microsecond timestamp from after 1970-01-12 already
exceeds any realistic note count by orders of magnitude. Comparing against the
actual high-water mark, instead of a fixed ceiling, keeps this correct forever
as `notes_seq` grows — a fixed ceiling would eventually reset a legitimate
high sequence value to baseline, contradicting `comm.probe`'s opaque
round-trip contract.

`query_probe`'s cursor clamp: never let the returned cursor regress below what
the caller already holds (#827) — if the message that previously held the
highest `notes_seq.seq` was hard-deleted since the last probe, `MAX(seq)`
over the remaining rows can be smaller than a cursor already handed out.
Clamping in Rust, rather than in SQL, keeps the single indexed query a pure
aggregate with no extra branch.

## `handlers.rs::handle_cursor_get` / `handle_cursor_commit`

Read/persist the durable channel poll checkpoint for `(channel_kind,
channel_slug)` (issue #449). Subhandlers — only the daemon's channel poll loop
calls these, and `cursor_commit` only after every envelope in the page has
returned `Ok` from `comm.ingest`. Both run the pack-owned
`comm_channel_cursor` schema statement before the query/write so an
in-memory/test runtime that never applied the boot-time schema plan still
works (matches the repository's lazy pack-schema bootstrap convention).
`cursor_get` returns JSON `null` when no row exists yet (first-run
compatibility mode). `cursor_commit` replaces any prior row for that identity.

## Message-ID / References header helpers (#403)

- `message_id_match_candidates`: outbound mail stores its Message-ID in wire
  form `<id@domain>` (angle brackets included); `mail_parser` strips those
  brackets from an inbound `In-Reply-To`, yielding `id@domain`. To correlate a
  reply back to the sending actor, both representations must be tried, so
  this returns the key as received plus its bracket-toggled variant, exact
  form first.
- `wrap_message_id`: normalizes a stored Message-ID into RFC 5322 wire form
  (angle-bracketed). Stored values may already be bracketed (an outbound
  note's self-minted `external_id`, e.g. `<uuid@domain>`) or bracket-free (an
  inbound note's `wire_message_id`, since `mail_parser` strips brackets when
  parsing). This is the single place that normalizes to the wire form the
  `In-Reply-To`/`References` headers require.
- `parent_wire_message_id`: direction-aware — an outbound parent's own
  Message-ID is self-minted into `external_id` at send time; an inbound
  parent's Message-ID lives in `wire_message_id` instead, since an inbound
  note's `external_id` is the IMAP UIDVALIDITY/UID dedup key, never a
  Message-ID. Returns `None` when the parent carries no wire Message-ID at
  all (e.g. a khive-internal parent, or an email parent the channel never
  captured one for).
- `parent_references_chain`: direction-aware — an inbound parent's chain (as
  received over the wire) lives in `wire_references`; an outbound parent's
  chain is whatever was persisted on it as `references_chain` when *it* was
  sent (an outbound note that was a fresh send, not a reply, carries no
  `references_chain`). Returns `None` when the parent has no chain to extend;
  the caller then falls back to the parent's Message-ID alone, matching RFC
  5322 (References = prior chain, if any, + parent Message-ID).
- `sanitize_reference_token`: rejects anything containing CR or LF (header
  injection guard) or without an `@` (not a plausible message id), then
  normalizes to wire form. Returns `None` for a malformed token so the caller
  can skip it rather than emit a corrupt header.
- `bare_reference_id`: strips angle brackets and surrounding whitespace, for
  use as a de-duplication comparison key only — callers keep pushing each
  token's original serialization into the emitted header, never this bare
  form.
- `build_references_header`: builds the full `References` header value for a
  reply — the parent's existing chain (each token individually sanitized;
  malformed tokens skipped) followed by the parent's own Message-ID. Tokens
  are whitespace-separated per RFC 5322. A stored chain can already contain an
  equivalent of the parent's own id (e.g. tainted or legacy data); tokens are
  de-duplicated by their bracket-stripped form, keeping first-seen order, so
  the parent id is skipped rather than appended a second time when an
  equivalent token is already present.

## `vocab.rs::COMM_SCHEMA_PLAN_STMTS`

Pack-auxiliary indexes for comm inbox and thread queries. Indexes use `WHERE
deleted_at IS NULL` (not `WHERE kind = 'message'`) so that SQLite's index
planner can match them when queries contain the parameterized `kind = ?N`
predicate emitted by `build_note_filter_where`. A literal-value partial index
(`WHERE kind = 'message'`) cannot be used for a parameterized comparison — the
planner sees different predicates and falls back to a table scan.
`deleted_at IS NULL` is always present in filtered queries, so the partial
condition is always satisfied and the index is eligible. `kind` is included
as an indexed column so the `kind = ?N` predicate is covered. Statements are
idempotent (`CREATE INDEX IF NOT EXISTS`).

The `idx_comm_message_external_id` UNIQUE index is NOT listed here; it is
created by the V5 schema migration (`005-unique-comm-external-id.sql`), which
is the sole durable authority for that index.

## `vocab.rs::COMM_CHANNEL_CURSOR_SCHEMA_STMT`

Pack-owned auxiliary cursor table for durable channel poll progress (issue
#449): one row per `(channel_kind, channel_slug)`, holding the
transport-neutral checkpoint fields from `khive_channel::ChannelCheckpoint`.
For IMAP, `generation` is `UIDVALIDITY` and `high_water` is the greatest
durably handled UID. `source` detects a host/port/mailbox/folder change under
the same registry identity, so a stale checkpoint is never applied to a
different configuration.

Idempotent (`CREATE TABLE IF NOT EXISTS`), applied at boot via `schema_plan`
and shared verbatim with `handle_cursor_get`/`handle_cursor_commit`'s lazy
bootstrap for in-memory/test runtimes that never run the boot-time schema
plan.

## `params.rs` field notes

- `SendParams::tags`: structured provenance tags (e.g. run id, job id, traffic
  class), persisted verbatim to `properties["tags"]` on both the outbound and
  inbound copies. Mirrors the shipped `memory.remember` `tags` precedent
  (issue #495).
- `SendParams::self_send`: explicit acknowledgment that `to` intentionally
  names the sender's own resolved actor identity (khive #820). Required
  whenever the resolved `to_actor` equals `from_actor`; without it such a send
  is rejected rather than silently delivered, since a sub-agent session
  addressing a distinct parent/orchestrator actor that happens to collapse
  onto its own identity (both resolve `[actor] id` from the same
  project-scoped `.khive/config.toml`, ADR-096 Fork 2) is a mis-resolution,
  not intent.
- `IngestParams`/`HeartbeatParams`: `deny_unknown_fields` is intentionally
  absent — the polling loop may pass extra fields (including the `namespace`
  routing key consumed by the dispatch layer) that future handler versions
  can extend without breaking existing deployments. The `namespace` key is
  consumed by `VerbRegistry::dispatch` to mint the `NamespaceToken` before the
  handler is called; the handler uses `token` directly and does not read
  `namespace` from the struct.
- `IngestParams::metadata`: optional transport-layer metadata passthrough,
  merged verbatim into the stored note's properties alongside the named
  fields. Generic and channel-agnostic: the comm pack does not interpret any
  key in this map, it only persists it. A channel adapter that needs to
  attach adapter-specific markers (e.g. the email channel's quarantine flags,
  ADR-056 Amendment 2026-07-02) sets `ChannelEnvelope.metadata`; the MCP poll
  loop forwards it here unchanged. Absent metadata is today's behavior exactly
  (issue #448).
- `ProbeParams`: public polling contract (khive #667 daemon hardening slice) —
  shape is frozen, see the comm pack README.
