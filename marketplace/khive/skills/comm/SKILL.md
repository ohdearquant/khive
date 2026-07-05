---
description: Coordinate with other agents and lambdas over khive comm — be attributable (set KHIVE_ACTOR), address by actor (to="lambda:X") with a subject, triage your inbox by sender + subject, and reply to thread. Use whenever you send a message, check your inbox, follow up in a conversation, or read a thread.
---

# Coordinate over comm

khive comm is how agents and lambdas message each other. The surface is four verbs —
`comm.send`, `comm.inbox`, `comm.reply`, `comm.thread` (plus `comm.read` to clear a message) —
but the thing worth learning is the _coordination pattern_, not the verbs. Per-verb param
detail is one call away: `request(ops="comm.send(help=true)")`.

## The pattern

### 1. Be attributable before you send

Every message is stamped with **who sent it** (`from_actor`). That identity comes from
`KHIVE_ACTOR` (env) or `--actor` (flag); if both are unset it silently defaults to `"local"`.

Two things break when you are `"local"`:

- **Recipients can't tell who sent it** — every unattributed sender looks identical, and the
  reader has to guess from the content.
- **Your inbox becomes a party line** — `comm.inbox` as `"local"` returns _every_ local
  message, not just yours, because there is no actor to scope on.

So set `KHIVE_ACTOR=lambda:<you>` in the MCP server env. The server logs a startup warning when
the comm pack is loaded and the actor is still `"local"`. Attribution is the price of admission
to coordination.

### 2. Send addressed, with a subject

```
request(ops="comm.send(to=\"lambda:leo\", subject=\"CI status\", content=\"all 72 smoke tests pass\")")
```

- **`to="lambda:<name>"`** — address by actor. Delivery is actor-routed (ADR-057): the message
  lands in the recipient's inbox regardless of namespace. The older "sender and recipient must
  share a namespace" rule no longer holds — address the actor, not a namespace.
- **Always set `subject`** — it is the one field a busy recipient scans first. An un-subjected
  send is harder to triage and easier to miss.

### 3. Triage your inbox by sender + subject

```
request(ops="comm.inbox(limit=10)")
```

The fields you triage on are surfaced at the **top level** — no digging into `properties`:

```json
{
  "from": "lambda:lattice",
  "subject": "blocked on embed config",
  "preview": "the engine_config resolver returns None when…",
  "read": false,
  "direction": "inbound",
  "content": "…full body…"
}
```

Scan `from` + `subject` + `preview`, open `content` for the ones that matter, then
`comm.read(id="<full_id>")` to clear them. Always pass a `limit` — active inboxes are large.

### 4. Reply to thread, don't start a new one

```
request(ops="comm.reply(id=\"<message-full-id>\", content=\"ack, fix landing in #198\")")
```

`comm.reply` auto-threads, prepends `Re:` (once), and routes back to the other party — you don't
re-specify `to`. Reconstruct context before replying with:

```
request(ops="comm.thread(id=\"<any-message-in-thread>\")")
```

Any message id in the thread resolves to the same canonical thread.

## Anti-patterns

- **Sending as `"local"`.** Unattributed and unscoped. Set `KHIVE_ACTOR` first.
- **No subject.** The recipient can't triage. Always set one.
- **Believing cross-namespace is denied.** It is not — delivery is actor-routed (ADR-057).
  Address `to="lambda:<name>"` directly.
- **Reading `properties` to find the sender.** `from` / `subject` / `preview` are top-level.
- **`comm.send` with a `thread_id` for a follow-up.** Use `comm.reply` — it threads, prefixes,
  and routes for you.
