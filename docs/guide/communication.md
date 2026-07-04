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

| Param       | Type   | Required | Notes                                       |
| ----------- | ------ | -------- | ------------------------------------------- |
| `to`        | string | yes      | Actor label, e.g. `"lambda:leo"`.           |
| `content`   | string | yes      | Message body. Must not be empty.            |
| `subject`   | string | no       | Optional subject line.                      |
| `thread_id` | uuid   | no       | Groups the message into an existing thread. |

### Inbox

| Param    | Type    | Required | Notes                                        |
| -------- | ------- | -------- | -------------------------------------------- |
| `limit`  | integer | no       | Default 20, max 200.                         |
| `status` | string  | no       | `"unread"` (default) \| `"read"` \| `"all"`. |

```
request(ops="comm.inbox(limit=10)")
request(ops="comm.inbox(status=\"all\")")
```

### Read

Marks an inbound message read. Outbound messages cannot be marked read.

```
request(ops="comm.read(id=\"<message_id_or_prefix>\")")
```

`id` accepts either a full UUID or a short 8-character hex prefix.

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
```

`limit` defaults to 100 and caps at 500.

### Health

`comm.health()` is a read-only, no-argument verb that reports per-channel
polling state, keyed by `(channel_kind, channel_slug)`. It never returns a
computed `healthy` boolean: staleness and alerting judgment stay with the
caller, not the pack.

```
request(ops="comm.health()")
```

Each entry in the returned `channels` array carries:

| Field                  | Notes                                                                                                                                                    |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `channel_kind`         | e.g. `"email"`.                                                                                                                                          |
| `channel_slug`         | Per-credential identifier (the configured mailbox address for the email channel), so two accounts of the same `channel_kind` get distinct rows.          |
| `last_success_at`      | Timestamp of the most recent successful poll attempt, or `null`.                                                                                         |
| `last_failure_at`      | Timestamp of the most recent failed poll attempt, or `null`.                                                                                             |
| `last_poll_attempt_at` | Timestamp of the most recent poll attempt regardless of outcome.                                                                                         |
| `last_error`           | `{class, message, at}` of the most recent failure. `class` is one of `auth`, `transport`, `config` (an open enum; callers must tolerate unknown values). |
| `consecutive_failures` | Resets to 0 on success, increments on failure.                                                                                                           |

`last_error` is retained after a later success: a success updates
`last_success_at` and resets `consecutive_failures` to 0 but never clears
`last_error`. Compare `last_error.at` against `last_success_at` to tell a
resolved failure from one that is still live.

Heartbeat rows are always persisted to, and read from, the local operational
namespace, regardless of the caller's own namespace or
`KHIVE_EMAIL_INGEST_NAMESPACE`. These rows are an operational surface, not
message data, so they must be visible to a no-arg `comm.health()` call
independent of where the caller's own messages happen to be ingested.

The `role` field is `"daemon"` (with `source: "daemon-heartbeat"`) whenever
any persisted heartbeat row exists, and `"client"` with an empty `channels`
array otherwise. This distinguishes who owns the channel loops, not which
process answered the call: any persisted row means some daemon owns the
loops, even when this particular call was served by a different, non-daemon
process.

**Known ambiguity:** an empty `channels` array cannot distinguish "no daemon
has ever run" from "channels are configured but a poll has never completed."
The comm pack has no visibility into channel configuration (that lives in
`khive-mcp` / `khive-channel-email`), so `role: "client"` with an empty
`channels` array means only "no daemon heartbeat state exists," not "nothing
is configured."

Results are capped at 200 channels. A full page logs a `tracing::debug!`
line noting that results may be silently truncated.

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
`subject`, `channel_kind`, `external_id` (an IMAP-derived dedup key of the
form `imap:{host}:{uidvalidity}:{uid}`), `sent_at`, and the wire threading
fields `wire_message_id` / `wire_references`. Duplicate `external_id` values
are ignored, making re-delivery idempotent.

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
