# Message lifecycle

Technical reference for the `comm` pack's message write, threading, and read path —
`comm.send` / `comm.delivered` / `comm.inbox` / `comm.read` / `comm.mark_read` / `comm.reply` /
`comm.thread` / `comm.ingest` —
spanning `message.rs`, `handlers.rs`, `params.rs`, and the inbox/thread indexes in
`vocab.rs`.

## `message.rs::resolve_id`

Accepts a 36-char hyphenated UUID or an 8+ hex-char short prefix. The prefix
is resolved via `runtime.resolve_prefix` (namespace-scoped).

## `message.rs::attach_outbound_id_to_ambiguous_write`

Preserves ordinary `create_notes_atomic` failures unchanged. A
`WriterTaskTerminated { request_state: SideEffectsUnknown }` error is different:
the request was accepted, but the caller cannot establish whether the complete
pair committed. The helper turns that case into a structured
`RuntimeError::Khive(KhiveError)` (`kind=Conflict`) carrying the pre-generated
full `outbound_id` as `details.outbound_id`, so an automated caller can read
the correlation id back out of the MCP error object instead of parsing prose,
and use it for an exact `comm.delivered` lookup before retry.

`WriterTaskBusy` is preserved unchanged. In that case the queue accepted the
request but SQLite never entered the transaction and the dual-write operation
never ran, so the failed `comm.send`/`comm.reply` operation is safe to retry.
It carries no `outbound_id` and does not instruct the caller to use
`comm.delivered`. In a chain or parallel batch, retry only that failed per-op
entry; successful sibling operations may already have committed.

## `message.rs::dual_write_message`

Writes an outbound copy (caller namespace) and an inbound copy (recipient
namespace) through `create_notes_atomic`. Both notes, their FTS documents, and
all registered-model vector rows commit in one writer transaction. Ordinary
prepare/plan failures leave neither copy; an ambiguous writer-reply failure can
mean either the complete pair committed or neither did, never a durable
half-pair. The recipient-namespace behavior applies to this generic
cross-namespace path; the public actor-addressed `comm.send` keeps both copies
in the caller namespace (see below).

`subject`, `thread_id` are optional. `sent_at` is the RFC3339 timestamp for
both copies. `from_actor` and `to_actor` are optional actor labels (ADR-057)
stored in properties. Both copies follow the versioned
[`message-properties` v1 contract](message-properties.md); optional
`sent_by_process` provenance is copied unchanged to both copies.

Cross-namespace thread root invariant: when a root message is sent (i.e.,
`thread_id` is `None`), both the outbound and inbound copies must share the
same canonical `thread_id` — the sender's outbound UUID. This ensures that
`comm.thread(id=outbound_id)` can find replies written in any namespace,
because all replies carry the same canonical thread_id regardless of which
copy they were replying to.

The runtime pre-generates that outbound UUID before either note is written, so
the canonical `thread_id` and `comm_schema_version = 1` are already known when
both notes are constructed. `dual_write_message` commits both fully-formed v1
notes through `khive_runtime::create_notes_atomic` in one atomic writer
transaction — a failure on either note rolls back the whole unit, so no
partial or unversioned row can ever be observed.

When `thread_id` is already supplied, the handler first parses it and serializes
the UUID in full-hyphenated form, then forwards that canonical value unchanged
to both copies.

A supplied `thread_id` must also resolve to an existing thread (issue #1673):
at least one live `message` note in the caller's namespace must carry that
`thread_id`, checked against the same spelling set `comm.thread` probes. Shape
validation alone would accept any UUID-shaped value, and a send onto a root no
note carries would succeed silently while no reader could ever reconstruct the
thread — the failure would degrade the shared artifact (a reply missing from
its conversation) rather than the caller's state. An unresolvable `thread_id`
is therefore rejected with an invalid-input error naming the id, and no
message row is persisted. Omit `thread_id` to start a new thread.

`in_reply_to_message_id` is the parent's wire Message-ID (angle-bracketed),
when this write is a reply to a message with a known one (issue #403). It is
stored verbatim on both copies as `in_reply_to_message_id`; the outbox
delivery loop reads it back to set the RFC 822 `In-Reply-To` header for native
MUA conversation grouping. `None` when there is no known parent Message-ID (a
plain send, or a reply whose parent has none).

`references_chain` is the full RFC 5322 `References` value for this reply: the
parent's existing chain (if any) followed by the parent's Message-ID, space-
separated angle-bracketed ids (issue #403 finding: References must preserve
ancestry, not truncate to the immediate parent). Stored verbatim on both
copies as `references_chain`; the outbox delivery loop reads it back to set
the `References` header, and a further reply reads it back (direction-aware,
via `parent_references_chain`) to extend the chain again. `None` when there is
no known parent Message-ID (mirrors `in_reply_to_message_id`).

## `handlers.rs::handle_send`

Creates a message note in the caller's namespace (outbound) AND delivers an
inbound copy addressed to the actor label supplied in `to` (ADR-057).

Both copies land in the caller's namespace; no cross-namespace write occurs.
`from_actor` is set to `token.actor().id`; the caller namespace is carried separately as the routing `from`/`to` values passed to `dual_write_message`. `to_actor` is set to the
`to` argument. `comm.inbox` scopes every caller, including the anonymous
`"local"` fallback, with `to_actor = caller OR to_actor IS NULL`. Anonymous
callers therefore share messages addressed to `"local"` and can still read
legacy rows without `to_actor`, but cannot read messages explicitly addressed
to another actor.

The routing `from` and `to` passed to `dual_write_message` are both set to the
caller's namespace string so that `from == recipient_ns_str` is always true:
this naturally bypasses the cross-namespace allowlist gate in
`dual_write_message` (ADR-057 §"Interaction with ADR-040"). The actor labels
are propagated via the `from_actor`/`to_actor` arguments and stored in message
properties.

Message properties follow the versioned
[`message-properties` v1 contract](message-properties.md). When the request
origin set `KHIVE_PROCESS_REF`, its exact Unicode value is copied to
`sent_by_process` on both delivery copies as attribution-only metadata.

The response includes the canonical full `thread_id` persisted on both copies.
For a root send it is the outbound message's full UUID; for a continuation it
is the canonicalized caller-supplied root. Because `comm.send(thread_id=...)`
requires a full UUID, Agent presentation preserves this field so it can be
submitted to a later send unchanged.

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
explicitly via `self_send=true` (`SendParams::self_send`, khive #820), turning
the collapse loud instead of silent. `to_actor == "local"` is exempted: that
is the anonymous single-tenant party-line default (both sender and recipient
unattributed), not a collapsed distinct-principal address.

### Unattributed-caller warning (#200)

Addressed sends from an unattributed caller stamp `from_actor="local"`, which
causes reply-threading collapse when multiple unconfigured actors interact.
Known limitation pending issue #75 (actor identity per request). A visible
warning is surfaced so operators can diagnose mis-attribution; the send
proceeds rather than hard-erroring, to preserve backward compatibility with
sessions that set `default_namespace` but not `actor_id`. Uses the shared
actor-identity policy (#567) so this warning fires under exactly the same
"unattributed" definition the gate and token minter use.

## `handlers.rs::handle_delivered`

Requires the full outbound UUID. It performs one indexed count of live inbound
`message` notes in the caller's namespace whose `properties.from_actor` is the
caller and whose `properties.outbound_ref` exactly matches that UUID. It
deliberately does not fetch or resolve the outbound row first, because an
undelivered or legacy/injected half-pair may have no outbound row available to
resolve. The response carries
`status` (`delivered` or `undelivered`), a matching boolean, and
`inbound_count`; message content is irrelevant. This is internal paired-copy
confirmation only, not external channel delivery status. If the caller loses
the complete MCP response rather than receiving the structured ambiguous
error, it also loses the server-generated outbound UUID; that case requires a
future caller-supplied idempotency/correlation contract and is out of scope.
Agent presentation keeps this response's `id` canonical, so the returned exact
correlation key can be submitted to `comm.delivered` again unchanged.

## `handlers.rs::handle_inbox`

Lists inbound messages for the caller's actor label by default (ADR-057).
`box="sent"` selects outbound rows authored by that caller instead; no separate
storage format or verb is involved. An attributed caller's sent view requires
an exact `from_actor` match. The anonymous `local` single-actor fallback also
admits legacy outbound rows without `from_actor`. `to_actor` is an optional
exact recipient filter for the sent box. Read `status` and sender filters are
inbox-only and are rejected with `box="sent"`, while `to_actor` is rejected for
the default inbox, so a misplaced filter cannot silently return the wrong box.
The existing envelope fields remain stable. For the default inbox,
`unread_count` is the caller's mailbox-wide unread count — independent of the
page window and of `status` and sender filters — and is exact below
`unread_count_cap` (1,000). `unread_count_saturated=false` means the number is
exact, including when it equals the cap; `true` means the value is the lower
bound "at least 1,000". The addressed and legacy-recipient partitions are
counted through cap-limited subqueries in one storage snapshot. The sent box
reports zero and `unread_count_saturated=false` because outbound rows have no
recipient read state.

Every caller is filtered by `to_actor = caller OR to_actor IS NULL`. A
configured actor therefore sees messages addressed to that actor plus legacy
rows without `to_actor`. The anonymous `"local"` fallback sees messages
addressed to `"local"` plus the same legacy rows; it does not bypass the filter
or expose messages explicitly addressed to another actor.

`from_actor`/`from_prefix` (#493) sender filters are mutually exclusive.
Direction + read-status + `to_actor` filters are pushed into SQL so
`idx_comm_message_direction`/`idx_comm_message_to_actor` are usable; the read
filter uses `json_type` to match the old `as_bool().unwrap_or(false)` semantics —
only JSON boolean `true` counts as read, missing/false/string/integer all count as
unread. Exact `from_actor` and inclusive `since` (`created_at >=`) also stay in
SQL. `from_prefix`, `exclude_from_actor`, exclusive `before`, and case-insensitive
`subject_contains`/`content_contains` have no corresponding `FilterOp`, so they
are applied over an unbounded paged scan in Rust.

`offset` is logical rather than a raw database offset: it skips rows only after
every SQL and Rust filter has matched. The handler collects one extra logical
match to return `has_more` and `next_offset`; following `next_offset` with the
same filters enumerates a backlog larger than the 200-message page cap without
marking anything read. The total order is `(created_at DESC, id ASC)`. `since`
is inclusive and `before` is exclusive, both RFC 3339 and both evaluated against
the top-level note `created_at` exposed in the response, not optional transport
metadata in `properties.sent_at`. Empty substring filters are rejected, and a
missing/non-string subject does not match `subject_contains`.

Each underlying filtered-note window is count-free: storage runs the limited
row query only and declines an exact page total. One lookahead row supplies
`has_more`, so a small inbox page no longer performs a full matching-set
`COUNT(*)` before reading its rows.

`fields` is the same strict, non-empty projection used by `comm.thread`.
Omitting it preserves the full message object. The accepted top-level names are
`id`, `short_id`, `full_id`, `kind`, `from`, `to`, `subject`, `read`,
`direction`, `preview`, `content`, `namespace`, `properties`, `created_at`, and
`updated_at`. Stable property aliases `comm_schema_version`, `from_actor`,
`to_actor`, `thread_id`, `sent_at`, `outbound_ref`, and `sent_by_process` are
also available without returning the full `properties` map; an absent optional
property projects as null, except `from_actor`/`to_actor`, which fall back to
the full view's `from`/`to` values, and `short_id`/`full_id`, which fall back
to the projected `id` value so the identifier aliases stay consistent with the
row's UUID. Unknown names and an empty list are hard errors. Duplicate names
are allowed and collapse to one key.
Authorization, filtering, unread counting, pagination lookahead, and thread
deduplication all operate on the complete internal view before projection.

`wait_ms` (#1499) adds bounded long-polling without changing the response
shape. Omission or `0` returns the first query immediately; values from 1
through 30,000 wait only when that query is empty. `limit=0` remains a
count-only immediate return and never waits. The deadline is established before
the initial storage query, so query time reduces the remaining signal-wait
budget. The timeout-edge final query and response serialization can add ordinary
request-processing time after that deadline.

One process-local `InboxSignal` belongs to each `CommPack` instance. It combines
`tokio::sync::Notify` with a monotonically increasing generation. The handler
captures the generation before every query, preventing a commit between the
empty query and waiter registration from becoming a lost wakeup. A wake always
re-runs the complete namespace, actor, status, and sender-filtered query; an
unrelated message therefore causes only a re-query and the caller keeps waiting
within the original deadline. A final query at deadline expiry observes any commit
visible before that query takes its storage snapshot; a commit that lands after
the snapshot is left to the caller's next request.

`comm.send` and `comm.reply` publish after their dual-write has committed.
`comm.ingest` publishes only after `try_create_note` returns a newly committed
note; the deduplicated path does not publish. The signal carries no message or
identity data and is not a delivery or authorization boundary. It is intentionally
not cross-process pubsub: direct writes through another registry/process become
visible on the timeout-edge final query or a subsequent call, while normal daemon
dispatches share the same pack instance and wake immediately.

## `handlers.rs::handle_read`

Marks a message as read. Rejects `read()` on outbound messages — "read" is a
recipient action; marking an outbound (sent) message as read corrupts the
read/unread invariant and has no semantic meaning to the sender.

Exactly one of `id` or `ids` is required. The single-ID form preserves its
existing response. The bulk form accepts 1-500 IDs, resolves duplicates to one
update, validates every target before the first mutation, and returns ordered
per-target `results` with `requested_count`, `unique_count`, `marked_count`, and
`failed_count`.
Validation includes the same namespace, message-kind, direction, addressee, and
legacy-message rules as the single-ID form. Updates are not a cross-message
transaction: a validation failure rejects the call before any update, while an
item-level storage failure returns `read=false` plus `mark_error` without rolling
back an earlier successful item.

Patches only the `read` key via `NoteStore::try_patch_note_property`, a
storage-side `json_set`, not a caller-side merge-then-overwrite of the whole
`properties` column: the write re-evaluates namespace, message kind, direction,
and addressee against the row's _current_ state in the same `UPDATE`, so a
property written by another caller between validation and this call (the bulk
form's window can span up to 500 targets) survives untouched, and an
eligibility change in that window degrades the mark instead of silently
landing on stale data. This also patches in place via a real `UPDATE`, never
`upsert_note`'s `INSERT OR REPLACE` (the latter silently deletes and
re-inserts the row on a primary-key conflict — #780). The `comm.probe` cursor
is keyed on `notes_seq.seq`, which is fixed at first insert and survives such
churn, so avoiding `upsert_note` here is defensive rather than load-bearing; a
metadata patch should never rewrite the row regardless.

`handle_reply`'s fold-in mark (see below) covers the single-original case and
uses the simpler `NoteStore::set_note_property` — an unconditional atomic
patch with no eligibility recheck — since a reply has only one target and no
validate-then-mark window to race.

The mark-read patch is best-effort: under multi-client burst traffic the
sqlite writer pool can time out (`checkout_timeout`, 5s default), and the
read itself has already succeeded by the time the patch runs, so a failed or
no-op write no longer fails the whole call. This follows the same
high-level best-effort principle as `handle_reply`'s fold-in mark, which has
been best-effort since its introduction. Three outcomes:

- `Ok(true)` — the row was live and updated: `read: true`, `properties` is
  the patched value (including the new `read: true`).
- `Ok(false)` — no live row currently matches (soft-deleted mid-flight, or an
  eligibility property — namespace, kind, direction, addressee — changed
  since this handler's prior validation): `read: false`, `mark_error: "no
  live row updated"`, `properties` is the note's ORIGINAL stored value (a
  stored SQL-NULL properties column round-trips as JSON `null`, never `{}`)
  — the response never claims a write that did not land.
- `Err(e)` — the patch failed (writer timeout, pool exhaustion, etc.):
  logged via `tracing::warn!` with the full error detail, then `read:
  false`, `mark_error` is the error's `Display` string, `properties` is the
  original stored value (again `null` if that is what was stored).

`id`/`full_id` are returned in all three arms — only the mark degrades, not
the read. There is no retry loop; a caller polling unread counts simply
sees the message still unread and can re-issue `comm.read` (self-healing).
Every validation error that runs before the patch (not found, wrong kind,
outbound, wrong addressee) is unaffected and stays a hard error.

## `handlers.rs::handle_mark_read`

`comm.mark_read` is the canonical named bulk mutation; message bodies are retrieved through
`comm.inbox` or `comm.thread`. It requires `ids` (1-500) and accepts optional `atomic` (default
false). Resolution, namespace/message-kind checks, inbound-direction enforcement, addressee
authorization, legacy-row compatibility, deduplication, response ordering, and aggregate counts
are shared with `handle_read` rather than reimplemented.

The default path calls the same best-effort target loop documented above. `atomic=true` instead
passes the unique target UUIDs and the same live eligibility `NoteFilter` to
`NoteStore::patch_note_property_atomic`. The SQLite implementation executes every guarded
`json_set(..., '$.read', true)` inside one writer transaction. Every statement must affect exactly
one row; a missing, soft-deleted, non-object, or newly ineligible target aborts the transaction, so
an earlier mark cannot survive a later failure. On commit, the handler returns the ordinary bulk
summary with `read=true` for every unique target. Both the writer-task and legacy pool-mutex
executors verify that finalization restored autocommit mode. An unverified rollback, an indeterminate
commit, or any other poisoned-connection state returns `side_effects_unknown`. Any transaction-body
panic also retires its writer (reporting `transaction_rolled_back` only when rollback was verified),
rather than allowing another request to reuse a terminal connection.

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
- Message properties follow the versioned
  [`message-properties` v1 contract](message-properties.md). An optional
  `KHIVE_PROCESS_REF` is copied verbatim to both reply delivery copies as
  attribution-only `sent_by_process` metadata.
- Replying folds in the addressee's read mark with the same atomic
  `set_note_property("read", true)` operation as `comm.read`; it does not
  re-fetch and replace the properties document. The delivery of the reply is
  already committed, so this mark remains best-effort and reports
  `marked_read: false` on a failed/no-op property set.

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

Legacy compact, braced, URN, and upper-hex stored roots are parsed and
normalized for the response. The indexed read queries the deduplicated set of
lower- and upper-hex formatter spellings accepted before v1, plus the selected
row's exact spelling. A mixed legacy/v1 thread therefore stays whole whether
lookup starts from its canonical root, a v1 child, or a pre-v1 child; existing
rows are not rewritten.

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

`order` (#494, `ThreadParams::order`) is a closed set: `"asc"` (default) |
`"desc"`. `after` (#494, `ThreadParams::after`) is either a message id (short
prefix or full UUID, resolved the same way `id` is) or an RFC 3339 timestamp.
An id cursor resolves to the full `(created_at, full_id)` tuple of the
referenced note so ties on equal microsecond timestamps are broken
deterministically instead of being skipped or duplicated. A timestamp cursor
is parsed to microseconds via chrono (matching the pattern in
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

The optional `fields` projection is identical to `comm.inbox` and is applied
only after visibility filtering, dual-write deduplication, cursor filtering,
ordering, and truncation. Omitting it preserves the full thread response.

## `handlers.rs::handle_ingest`

Writes a single inbound message note from a channel adapter. This is a
`Visibility::Subhandler` verb: not accessible via the MCP wire, only callable
from within the process (e.g. the polling loop in `khive-mcp`). It is the
authoritative write path for all channel-delivered messages; the polling loop
must not bypass it.

The resulting properties follow the versioned
[`message-properties` v1 contract](message-properties.md). Ingested messages
do not receive `sent_by_process`: the adapter process delivered the message but
did not author it.

Issue #479a: a present, non-empty `thread_id` that is not a valid UUID must
fail closed rather than being silently dropped and replaced with a fresh UUID,
which would split the message into the wrong conversation. A blank/absent
value is not an error — it just means "no caller-supplied thread_id". Valid
compact, braced, URN, hyphenated, and upper-hex UUID spellings are accepted and
normalized to the full-hyphenated v1 representation before storage. Roots
recovered from a correlated legacy message are normalized through the same
UUID parse.

An omitted `sent_at` defaults to the current time. A supplied value must parse
as RFC 3339 or ingest fails before writing a note; valid values are normalized
to UTC RFC 3339 before the v1 marker is stamped.

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
The acknowledgement returns the `thread_id` read from the existing row — the
canonical 36-character hyphenated UUID for v1 rows — never the new root
proposed by the duplicate delivery. Exception: a pre-v1 row may store a
non-UUID legacy thread label; the ack echoes that stored value verbatim
(fabricating the duplicate's note UUID instead would route a caller echoing
the ack into a DIFFERENT thread) and flags it with `thread_id_canonical:
false` so a strict caller can detect the non-canonical shape without
re-parsing the string. A row with NO stored `thread_id` falls back to the
duplicate's note UUID as the thread root (#479b, ADR-040) and is flagged with
`thread_id_warning` instead. See ADR-056 §Amendment 2026-08-04.

Generic transport-layer metadata passthrough (issue #448, `IngestParams::metadata`):
merged additively so it can never clobber a key already present. Names in the
stable [`message-properties` v1 contract](message-properties.md) are reserved
even when an optional field is absent, so metadata cannot fabricate a subject,
an outbound twin, or originating-process provenance. Other metadata is generic
and channel-agnostic; the email channel happens to use it for quarantine
markers. `deny_unknown_fields` is intentionally absent on `IngestParams` (and
`HeartbeatParams`) — the
polling loop may pass extra fields (including the `namespace` routing key
consumed by the dispatch layer) that future handler versions can extend
without breaking existing deployments; the `namespace` key is consumed by
`VerbRegistry::dispatch` to mint the `NamespaceToken` before the handler is
called, and the handler uses `token` directly rather than reading `namespace`
from the struct.

Channel pollers additionally pass handler-owned `channel_kind` and
`channel_slug` provenance (#1383). Both are trimmed, nonblank transport
identifiers; a slug requires a kind. Free-form metadata cannot override or
fabricate either field. `comm.health` uses the pair to group any generic
`quarantined: true`/`"true"` disposition without depending on an email-only
sender label. Because those three fields become operational health evidence,
generic `create(kind="message", properties=...)` and `update` mutations refuse
caller-supplied `channel_kind`, `channel_slug`, or `quarantined`. The internal
`comm.ingest` subhandler remains their only supported writer; ordinary custom
message metadata is unaffected.

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
  all.
- `parent_references_chain`: direction-aware — an inbound parent's chain (as
  received over the wire) lives in `wire_references`; an outbound parent's
  chain is whatever was persisted on it as `references_chain` when _it_ was
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

`idx_comm_message_outbound_ref` covers the exact `comm.delivered` lookup by
namespace, note kind, direction, sender actor, and `properties.outbound_ref`.

The `idx_comm_message_external_id` UNIQUE index is NOT listed here; it is
created by the V5 schema migration (`005-unique-comm-external-id.sql`), which
is the sole durable authority for that index.
