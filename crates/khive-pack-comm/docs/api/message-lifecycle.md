# Message lifecycle

Technical reference for the `comm` pack's message write, threading, and read path —
`comm.send` / `comm.inbox` / `comm.read` / `comm.reply` / `comm.thread` / `comm.ingest` —
spanning `message.rs`, `handlers.rs`, `params.rs`, and the inbox/thread indexes in
`vocab.rs`.

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

Generic transport-layer metadata passthrough (issue #448, `IngestParams::metadata`):
merged additively so it can never clobber the identity/routing fields (from,
to, from_actor, to_actor, direction, read, thread_id, sent_at, subject,
external_id, wire_message_id, wire_references, channel_kind) — a key already
present always wins. The comm pack does not interpret any metadata key; the
email channel happens to use it for quarantine markers. `deny_unknown_fields`
is intentionally absent on `IngestParams` (and `HeartbeatParams`) — the
polling loop may pass extra fields (including the `namespace` routing key
consumed by the dispatch layer) that future handler versions can extend
without breaking existing deployments; the `namespace` key is consumed by
`VerbRegistry::dispatch` to mint the `NamespaceToken` before the handler is
called, and the handler uses `token` directly rather than reading `namespace`
from the struct.

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
