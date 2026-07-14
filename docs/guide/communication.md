# Communication and Email

The comm pack provides actor-addressed messaging with an inbox model. Use it
when one actor needs to send another a directed message, follow up on it, and
keep the exchange together as a thread. Messages are not a shared chat stream:
each actor reads the inbound messages addressed to its own actor address.
Actor addressing routes and filters inbox views in the shared local store; it is
not a security boundary or a substitute for principal-isolated transport.

Full parameter signatures live in the [comm pack rustdoc](../../crates/khive-pack-comm/src/vocab.rs).
This page describes the working flow rather than duplicating that reference.

## Send, receive, and act

Start with `comm.send`. A send is a dual-write: the sender gets an outbound
copy and the addressed actor gets an inbound copy. That makes delivery
cross-actor while keeping the recipient's inbox actor-filtered.

```text
request(ops="comm.send(to=\"another-actor\", subject=\"Review needed\", content=\"Please review the change set.\")")
```

The recipient checks its inbox. It returns inbound messages addressed to the
calling actor; unread is the default view.
For compatibility, legacy messages without `to_actor` remain visible.

```text
request(ops="comm.inbox(status=\"unread\", limit=10)")
```

After acting on a message, reply and then mark that same inbound message as
read. Repeat the inbox message ID in both operations: `comm.reply` returns an
outbound message ID, which is not valid input to `comm.read`.

```text
request(ops="comm.reply(id=\"<inbound-message-id>\", content=\"I will review it today.\") | comm.read(id=\"<inbound-message-id>\")")
```

`comm.read` is for received (inbound) messages only. Do not mark a message
read before you have handled it; an outbound copy cannot be marked read.

## Follow a conversation

Replies retain the conversation's thread automatically. To inspect it, call
`comm.thread` with an `id` for a message in the conversation. Both
`comm.reply` and `comm.thread` take a message ID in the parameter named `id`,
not a `thread_id`; the thread operation resolves the conversation root.

Use `comm.inbox(status="all", limit=200)` for recent inbox history when you
need a message ID; the maximum limit is 200. Thread results are chronological
by default; the signature reference documents pagination and ordering options.

## Polling and channel status

`comm.probe` is a read-only poll for newly arrived inbound-message metadata
and the count of stale unread messages. Supply the required actor address and
optionally set `stale_minutes` (default 20), then round-trip the returned
`cursor_us` unchanged as the next `since_us`. That cursor is opaque, not a
timestamp, and probing never changes read flags. A response contains at most
the 100 newest matching messages, so use `comm.inbox` or the appropriate
history workflow for complete or backlog history rather than treating probe as
a paginated feed.

`comm.health()` is a read-only, per-channel heartbeat snapshot. Its channel
rows report poll timestamps and consecutive failure counts; it deliberately
does not make a health judgment. Decide staleness or alerting in the caller.
Inspect the response `namespace`: a non-local scoped call can validly return
`role: "client"` and empty `channels` while the local daemon is active.

## Email

When the optional email channel is configured, it uses the same comm message
model. Address a new email as `email:person@example.com` and provide a
`subject` conventionally, but it is optional; when omitted, the channel
delivers `(no subject)`. The daemon asynchronously delivers eligible stored
outbound messages. Recipients must be in its configured outbound allowlist,
which defaults to the maintainer address. Reply to an inbound email with
`comm.reply` to preserve its conversation linkage.
See [Configuration](../configuration.md) for email-channel setup.

## Gotchas

- A send to your own configured actor address is refused unless you explicitly
  pass `self_send=true`; the anonymous `local` fallback is exempt.
- An email `subject` is conventional but optional; an omitted subject is
  delivered as `(no subject)`.
- An inbox is scoped to the calling actor address. If an expected message is
  absent, check which actor is making the request and the address used in
  `comm.send(to=...)`.

## See also

- [API Reference](api-reference.md): full verb catalog and response shapes.
- [Prompt Cookbook](prompt-cookbook.md): additional `request(ops="...")` patterns.
