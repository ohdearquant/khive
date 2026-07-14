# Communication and Email

The comm pack provides actor-addressed messaging with an inbox model. Use it
when one actor needs to send another a directed message, follow up on it, and
keep the exchange together as a thread. Messages are not a shared chat stream:
each actor reads the inbound messages addressed to its own actor address.

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

Use `comm.inbox(status="all")` when you need a message ID from older inbox
history. Thread results are chronological by default; the signature reference
documents pagination and ordering options.

## Polling and channel status

`comm.probe` is a read-only poll for newly arrived inbound-message metadata
and the count of stale unread messages. Supply the actor address being watched
and round-trip the returned `cursor_us` unchanged as the next `since_us`.
That cursor is opaque, not a timestamp, and probing never changes read flags.

`comm.health()` is a read-only, per-channel heartbeat snapshot. Its channel
rows report poll timestamps and consecutive failure counts; it deliberately
does not make a health judgment. Decide staleness or alerting in the caller.

## Email

When the optional email channel is configured, it uses the same comm message
model. Address a new email as `email:person@example.com` and provide a
`subject`; the channel delivers stored outbound messages asynchronously. Reply
to an inbound email with `comm.reply` to preserve its conversation linkage.
See [Configuration](../configuration.md) for email-channel setup.

## Gotchas

- A send to your own configured actor address is refused unless you explicitly
  pass `self_send=true`.
- A new email send needs a `subject`.
- An inbox is scoped to the calling actor address. If an expected message is
  absent, check which actor is making the request and the address used in
  `comm.send(to=...)`.

## See also

- [API Reference](api-reference.md): full verb catalog and response shapes.
- [Prompt Cookbook](prompt-cookbook.md): additional `request(ops="...")` patterns.
