# Communication and Email

This guide covers the comm pack: actor-addressed messaging inside khive, and
the optional email channel that bridges that same messaging model to an
external mailbox.

## What messages are

Messages are notes with `kind=message`, managed by the comm pack
(`crates/khive-pack-comm/`). `comm.send` writes both an outbound copy (in the
sender's namespace) and an inbound copy (addressed to the recipient), so a
send always produces two notes and no cross-namespace write occurs even when
`to` names a different actor.

New messages follow the stable
[`properties` v1 contract](../../crates/khive-pack-comm/docs/api/message-properties.md).
Workers may set `KHIVE_PROCESS_REF` to an opaque run or job reference; `comm.send`
and `comm.reply` then persist it verbatim as `sent_by_process` on both copies.
It is provenance only and does not change addressing or authorization.

## Actor addressing

Actors are labeled strings such as `lambda:leo` or `lambda:khive`. `comm.send`
stores the caller's actor label as `from_actor` and the `to` argument as
`to_actor` on both the outbound and inbound copies.

```
request(ops="comm.send(to=\"lambda:leo\", content=\"PR #610 merged\")")
```

`comm.inbox` filters by `to_actor` for the calling actor. Legacy messages
written before actor addressing existed have no `to_actor` field and remain
visible to every actor (an `EqOrMissing` match), so older history is not
hidden by the newer filter.

### Send

| Param       | Type   | Required | Notes                                                                                                                                                                                           |
| ----------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `to`        | string | yes      | Actor label, e.g. `"lambda:leo"`.                                                                                                                                                               |
| `content`   | string | yes      | Message body. Must not be empty.                                                                                                                                                                |
| `subject`   | string | no       | Optional subject line.                                                                                                                                                                          |
| `thread_id` | uuid   | no       | Optional full thread UUID. Prefixes are rejected because a thread root is an explicit stable reference. Accepted complete spellings normalize to canonical lowercase dashed form.               |
| `self_send` | bool   | no       | Default false. Required when `to` matches the configured sender actor; otherwise the send is rejected. The anonymous `local` fallback is exempt. Use true only for an intentional note to self. |

A configured actor that addresses itself must opt in with `self_send=true`. If the
target was meant to be a distinct parent or sub-agent, configure distinct actor identities
instead of opting in; the rejection is intended to expose that identity collapse.

The response includes canonical 36-character `full_id` and `thread_id` handles even in
Agent mode. Reuse the returned `thread_id` unchanged to send another message in that thread.

### Confirm an uncertain internal delivery

Rows, FTS documents, and vectors for a send/reply pair commit atomically.
Ordinary failures leave neither copy. If the writer cannot establish whether
an accepted request committed, the operation instead returns an `ambiguous`
error containing `outbound_id=<full-uuid>`. Confirm the paired inbound write
before retrying:

```
request(ops="comm.delivered(id=\"<full-outbound-uuid>\")")
```

A successful lookup returns `status="delivered"` and `delivered=true` when at
least one live inbound note has `properties.outbound_ref` equal to that UUID.
It returns `status="undelivered"`, `delivered=false`, and `inbound_count=0`
when none does. The lookup does not require the outbound row to remain present
and never compares message bodies. It is scoped to the caller's namespace and
sender actor, so another actor cannot inspect a sender's outcome even if it
learns the correlation UUID. If the lookup itself errors, the outcome is still
uncertain.

This operation confirms khive's internal inbound sibling only. It does not
confirm asynchronous SMTP or another external transport; see
[How outbound delivery works](#how-outbound-delivery-works) for that separate
state machine.

If the entire MCP response is lost, the caller receives neither the result nor
the structured error and therefore does not know the server-generated UUID.
`comm.delivered` cannot resolve that wider response-loss case.

### Inbox

| Param                | Type    | Required | Notes                                                                        |
| -------------------- | ------- | -------- | ---------------------------------------------------------------------------- |
| `limit`              | integer | no       | Default 20, max 200.                                                         |
| `box`                | string  | no       | `inbox` (default) \| `sent`; sent rows are scoped to the caller.             |
| `offset`             | integer | no       | Default 0; offset in the fully-filtered newest-first result set.             |
| `status`             | string  | no       | Inbox-only: `"unread"` (default) \| `"read"` \| `"all"`.                     |
| `wait_ms`            | integer | no       | Long-poll only when the initial page is empty; default 0, max 30,000.        |
| `from_actor`         | string  | no       | Exact sender; mutually exclusive with `from_prefix`.                         |
| `from_prefix`        | string  | no       | Sender prefix; mutually exclusive with `from_actor`.                         |
| `exclude_from_actor` | string  | no       | Exclude an exact sender actor label.                                         |
| `to_actor`           | string  | no       | Sent-only exact recipient actor filter.                                      |
| `since`              | string  | no       | Inclusive RFC 3339 lower bound on response `created_at`.                     |
| `before`             | string  | no       | Exclusive RFC 3339 upper bound on response `created_at`.                     |
| `subject_contains`   | string  | no       | Case-insensitive non-empty subject substring; missing subjects do not match. |
| `content_contains`   | string  | no       | Case-insensitive non-empty body substring.                                   |
| `fields`             | array   | no       | Non-empty message-field projection shared with `comm.thread`.                |

```
request(ops="comm.inbox(limit=10)")
request(ops="comm.inbox(status=\"all\")")
request(ops="comm.inbox(status=\"all\", content_contains=\"timeout\", since=\"2026-07-31T00:00:00Z\")")
request(ops="comm.inbox(box=\"sent\", to_actor=\"lambda:leo\", fields=[\"id\",\"subject\",\"sent_at\"])")
request(ops="comm.inbox(limit=10, wait_ms=30000)")
```

Responses include `offset`, `has_more`, and `next_offset`. Repeat the same call
with `offset=<next_offset>` until `next_offset` is null to enumerate every
matching message without changing its read state. Filters are ANDed and offsets
apply after all filters. Time bounds use the always-present top-level
`created_at`, not optional transport `sent_at` metadata.

Omit `fields` for the full message body and properties. When supplied, it is a
strict whitelist shared with `comm.thread`; unknown fields and an empty list
fail loudly. Stable property aliases such as `from_actor`, `to_actor`, and
`sent_at` can be projected directly.

Long-polling preserves that paginated response shape and every actor/status/
sender/time/text filter, including the requested offset. Existing matches
return immediately. A new committed message wakes the call and causes the full
filtered query to run again; unrelated messages cannot leak through or end the
wait early. `limit=0` remains immediate.

### Mark read

`comm.mark_read` is the named bulk mutation. It marks inbound messages read; it does not return
message content. Use `comm.inbox` or `comm.thread` to retrieve content. Outbound messages cannot be
marked read.

```
request(ops="comm.mark_read(ids=[\"<message_id_1>\", \"<message_id_2>\"])")
request(ops="comm.mark_read(ids=[\"<message_id_1>\", \"<message_id_2>\"], atomic=true)")

# Compatibility surface
request(ops="comm.read(id=\"<message_id_or_prefix>\")")
request(ops="comm.read(ids=[\"<message_id_1>\", \"<message_id_2>\"])")
```

`comm.mark_read` requires `ids` with 1-500 full UUIDs or 8-character hex prefixes. It validates
every target before mutation, deduplicates resolved IDs, and returns ordered results plus
`requested_count`, `unique_count`, `marked_count`, and `failed_count`. The default
`atomic=false` reuses the best-effort bulk behavior: later storage failures appear in each
result's `read=false` and `mark_error` without rolling back an earlier success. With
`atomic=true`, all unique marks are guarded and committed in one transaction; any failed
recheck or storage statement rolls back the full set.

`comm.read` remains compatible with the 0.7.0 surface: exactly one of `id` or `ids` is required,
and its bulk form remains best-effort. Prefer the named verb for new bulk callers.

### Reply

Replies thread against the original message. If the original had no subject,
the reply carries no subject either; otherwise the reply subject is prefixed
`Re:` (and not re-prefixed if it already starts with `Re:`).

```
request(ops="comm.reply(id=\"<message_id_or_prefix>\", content=\"Thanks, following up now\")")
```

### Thread

Retrieves every message in a conversation thread, ordered chronologically,
given the thread root's id.

```
request(ops="comm.thread(id=\"<root_message_id_or_prefix>\", limit=50)")
request(ops="comm.thread(id=\"<root_message_id_or_prefix>\", fields=[\"id\",\"from_actor\",\"sent_at\"])")
```

`limit` defaults to 100 and caps at 500. `fields` uses the same strict
projection vocabulary as `comm.inbox`; omission returns the full thread view.

### Health

`comm.health()` is a read-only, no-argument verb that reports per-channel
polling state, keyed by `(channel_kind, channel_slug)`. It never returns a
computed `healthy` boolean. It does expose the nominal cadence and a narrower,
nullable schedule-staleness advisory; overall health judgment stays with the
caller. Quarantine counts are orthogonal: a successful poll can remain current
while one or more terminally parked messages require operator attention.

```
request(ops="comm.health()")
```

Each entry in the returned `channels` array carries:

| Field                  | Notes                                                                                                                                                            |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `channel_kind`         | e.g. `"email"`.                                                                                                                                                  |
| `channel_slug`         | Per-credential identifier (the configured mailbox address for the email channel), so two accounts of the same `channel_kind` get distinct rows.                  |
| `poll_interval_secs`   | Positive nominal/minimum poll cadence, or `null` for a legacy/malformed heartbeat row.                                                                           |
| `stalled`              | Advisory schedule staleness: `true` after three missed nominal intervals, `false` when current, or `null` when the facts are unknown or backoff is active.       |
| `last_success_at`      | Timestamp of the most recent successful poll attempt, or `null`.                                                                                                 |
| `last_failure_at`      | Timestamp of the most recent failed poll attempt, or `null`.                                                                                                     |
| `last_poll_attempt_at` | Timestamp of the most recent poll attempt regardless of outcome.                                                                                                 |
| `last_error`           | `{class, message, at}` of the most recent failure, or `null` when none was observed. `class` is an open enum (`auth`, `transport`, `config`, or a future value). |
| `consecutive_failures` | Resets to 0 on success, increments on failure, or `null` for a quarantine-only identity with no heartbeat evidence.                                              |
| `quarantined_count`    | Live parked messages carrying this exact channel identity.                                                                                                       |

`last_error` is retained after a later success: a success updates
`last_success_at` and resets `consecutive_failures` to 0 but never clears
`last_error`. Compare `last_error.at` against `last_success_at` to tell a
resolved failure from one that is still live.

`stalled` is computed against the response's single `as_of` timestamp and is
deliberately not a supervisor verdict. The loop polls channels sequentially,
and a transport operation can run longer than the nominal interval, so `true`
means the persisted schedule is overdue; it does not prove the task is dead.
Rows with `consecutive_failures > 0` report `stalled: null` because intentional
exponential backoff can exceed the nominal cadence. ADR-119's component
supervisor remains responsible for authoritative hung-task detection/restart.

`comm.heartbeat` persists under its dispatch-authorized namespace
(`token.namespace()`, khive #917). The shipped local poll loop explicitly
dispatches heartbeat writes to the fixed `local` operational namespace,
regardless of `KHIVE_EMAIL_INGEST_NAMESPACE`; an authorized per-tenant writer
can instead write its own namespace. `comm.health` reads from the caller's
injected namespace, the same `namespace=` escape / `"local"` default every
other comm verb resolves. A scoped read never falls back to `"local"`. The
response carries a `namespace` field naming the namespace actually read. It
also carries namespace-wide `quarantined_count` and
`unattributed_quarantined_count` totals. A quarantine identity with no
heartbeat row in that namespace is included as a channel entry with nullable
heartbeat fields.

The `role` field is `"daemon"` (with `source: "daemon-heartbeat"`) whenever
any persisted heartbeat row exists **in the namespace read**, and `"client"`
otherwise. A client-role response can contain quarantine-only channel entries;
those rows are message evidence, not fabricated daemon ownership. This
distinguishes who owns the channel loops, not which process answered the call.

Inspect parked rows with the full `comm.inbox(status="all")` view and the
`properties.quarantined` marker, then use `get(id=...)` for detail.
`delete(id=...)` removes one from the live parked count; `hard=true`
permanently purges it. No automatic trusted release exists because quarantine
means the attribution gate did not establish a sender identity.
Generic message `create`/`update` cannot set or clear `quarantined`,
`channel_kind`, or `channel_slug`; `comm.ingest` is the only supported writer
for those transport-owned facts.

**Known ambiguity:** an empty `channels` array cannot distinguish "no daemon
has ever run" from "channels are configured but a poll has never completed."
The comm pack has no visibility into channel configuration (that lives in
`khive-mcp` / `khive-channel-email`), so `role: "client"` with an empty
`channels` array means only "no daemon heartbeat state exists in the
namespace read," not "nothing is configured." The `namespace` field
disambiguates which namespace that is. A call scoped to a non-local
`namespace=` returns `role: "client"` until an authorized writer has produced
heartbeat state there, even while the shipped local loop is actively
heartbeating under `"local"`; its `channels` array may still contain
quarantine-only message evidence. Check the response's `namespace` field
before reading that role as "no daemon running."

Results are capped at 200 channels. Heartbeat rows take precedence and retain
their persisted order; quarantine-only identities fill remaining capacity in
lexical `(channel_kind, channel_slug)` order. Later identities are omitted, but
the top-level quarantine totals remain namespace-wide. This ordering prevents
a real heartbeat beyond a full heartbeat page from being presented as a
synthetic unknown-liveness row.

## The email channel

The email channel (`crates/khive-channel-email/`) bridges `comm.send` /
`comm.inbox` to a real mailbox over SMTP and IMAP. It is not part of the
default build; see [Feature gating](#feature-gating) below.

### Addressing an email recipient

Send to an email address by prefixing `to` with `email:` and passing an
explicit `subject`. Because the outbox loop reads `subject` off the stored
note, a mail sent without `subject` goes out with `(no subject)` in the
subject line.

```
request(ops="comm.send(to=\"email:prof.sheng@example.edu\", subject=\"Draft ready for review\", content=\"...\")")
```

### How outbound delivery works

`comm.send` itself only writes the note; it does not talk to SMTP directly.
A background outbox loop polls every 5 seconds for undelivered outbound
notes:

```
list(namespace=<ingest_namespace>, kind="message", direction="outbound", delivered=false, limit=200)
```

For each note returned, the loop keeps only those where `to_actor` starts
with `email:` and the note is not already delivered, then checks the
recipient against the allowlist (`KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS`, or the
channel's maintainer address if that variable is unset). Passing notes are
sent over SMTP, using the note's `subject`, `content`, and any
`thread_id`/`in_reply_to_message_id`/`references_chain` properties to set the
RFC 822 `Message-ID`, `In-Reply-To`, and `References` headers so replies group
correctly in native mail clients.

### How inbound ingestion works

A separate poll loop reads the IMAP mailbox every 5 seconds and, for each new
message, calls the pack-internal `comm.ingest` subhandler (not callable
directly over the MCP wire) with the parsed envelope: `from`, `to`, `content`,
`subject`, `channel_kind`, the exact per-credential `channel_slug`, `external_id`
(an IMAP-derived dedup key of the form `imap:{host}:{uidvalidity}:{uid}`),
`sent_at`, and the wire threading fields `wire_message_id` / `wire_references`.
Every channel poller must supply both `Channel::kind()` and `Channel::slug()`;
kind alone cannot distinguish two accounts using the same adapter. Duplicate
`external_id` values are ignored, making re-delivery idempotent.

### Configuration

`EmailChannelConfig::from_env` reads configuration exclusively from
environment variables; there is no file-based config for this channel. See
[Configuration](../configuration.md) for the full khive-wide environment
variable reference. The email-specific variables are:

Required:

- `KHIVE_EMAIL_SMTP_HOST`
- `KHIVE_EMAIL_IMAP_HOST`
- `KHIVE_EMAIL_USERNAME`
- `KHIVE_EMAIL_MAINTAINER_ADDRESS` (comma-separated; the first entry is
  primary and used for outbound-allowlist defaulting)
- `KHIVE_EMAIL_AUTHSERV_ID` (the trust anchor for validating inbound
  `Authentication-Results` headers; the reserved value
  `!topmost-no-authserv-id` selects trust of the topmost header when the
  receiving boundary emits no `authserv-id` at all, as with Exchange Online's
  internal-hop stamp)

Auth mode (choose one):

- Basic: `KHIVE_EMAIL_PASSWORD`
- OAuth (Exchange Online app-only client-credentials flow):
  `KHIVE_EMAIL_OAUTH_CLIENT_ID`, `KHIVE_EMAIL_OAUTH_TENANT_ID`,
  `KHIVE_EMAIL_OAUTH_CLIENT_SECRET` (all three required together; a partial
  set is a config error, never a silent fallback to Basic)

Optional, with defaults:

- `KHIVE_EMAIL_SMTP_PORT` (default `587`)
- `KHIVE_EMAIL_IMAP_PORT` (default `993`)
- `KHIVE_EMAIL_MAILBOX` (default: same as `KHIVE_EMAIL_USERNAME`)
- `KHIVE_EMAIL_QUARANTINE_STORE` (default `true`; when a message fails the
  sender-authentication or allowlist gate, store it as an unattributed
  quarantine record instead of dropping it)
- `KHIVE_EMAIL_INGEST_NAMESPACE` (default `local`; target namespace for
  ingested messages)
- `KHIVE_EMAIL_DEFAULT_ACTOR` (default `lambda:leo`; inbound actor assigned to
  fresh, uncorrelated email messages)
- `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS` (comma-separated outbound allowlist;
  falls back to the maintainer address when unset)

### Feature gating

`channel-email` is an optional Cargo feature
(`crates/khive-mcp/Cargo.toml`), not compiled into the plain
`cargo build --workspace --release` invocation used by `make build` or by any
release/CI workflow. It is enabled explicitly by `make local`
(`cargo build --release --features channel-email`). A binary built without
this feature has no email channel code at all: `to="email:..."` sends still
write a note (the comm pack has no awareness of channels), but nothing polls
IMAP or drains the outbox, so the message is never delivered.

### Daemon-only channel loops

The email poll loop and the outbox loop are spawned only by the persistent
daemon process (`kkernel mcp --daemon`), never by a plain stdio `kkernel mcp`
client. This is a deliberate role gate (issue #602): before it existed, every
stdio client process spawned its own independent IMAP poll loop against the
same mailbox, and nine concurrent pollers exhausted Exchange Online's
per-mailbox connection slots, taking inbound email down for about 19 hours on
2026-07-04.

The gate logs one line at startup either way, so the decision is observable:

```
email channel loops: spawning (daemon role)
email channel loops: skipped (client role; daemon owns channel loops)
```

If the ingest namespace fails authorization, the loops are not started at all
(fail-closed) and this is logged separately:

```
email channel loops NOT started: ingest namespace authorization failed (fail-closed)
```

If no daemon is running, mail is simply not polled until one starts. That is
the intended behavior, not a silent failure.

## Limitations

Actor addressing (`to_actor` filtering on `comm.send`/`comm.inbox`) is a
view-layer convention for cooperating, co-located actors, not a security
boundary ([ADR-063](../adr/ADR-063-comm-principal-model.md)). Any process
with access to the underlying SQLite store can read every message row
regardless of `to_actor`, and there is no per-principal storage partition on
the local backend. Where authorization is enforced, it lives at a single
seam, the Gate ([ADR-018](../adr/ADR-018-authorization-gate.md)), not at the
comm pack's inbox filter.

## See also

- [Agent Sessions and Data Ingest](sessions-and-ingest.md): a different
  ingestion path, transcript mirroring rather than message channels.
- [Configuration](../configuration.md): the full environment variable
  reference.
