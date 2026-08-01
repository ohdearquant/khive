---
description: Coordinate with other agents and lambdas over khive comm — be attributable (set KHIVE_ACTOR), address by actor (to="lambda:X") with a subject, triage your inbox by sender + subject, reply to thread, and inspect channel polling freshness. Use whenever you send a message, check your inbox, follow up in a conversation, read a thread, or diagnose channel delivery.
---

# Coordinate over comm

khive comm is how agents and lambdas message each other. The surface is nine verbs —
`comm.send`, `comm.delivered`, `comm.inbox`, `comm.unread`, `comm.read`, `comm.reply`,
`comm.thread`, `comm.health`, and `comm.probe` —
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
- **Treat a self-address rejection as an identity check.** When `to` matches the configured
  sender actor, `comm.send` rejects by default; the anonymous `local` fallback is exempt. If the
  message is genuinely a note to yourself, resend with `self_send=true`. If you meant to reach a
  distinct parent or sub-agent, configure distinct actor identities instead of opting in.
- **Confirm an ambiguous atomic-write error before retrying.** Ordinary
  failures roll back the pair. If an error is marked `ambiguous`, extract its
  full `outbound_id` and call
  `comm.delivered(id="<full-outbound-uuid>")`. `status="delivered"` means the
  internal inbound sibling exists; `status="undelivered"` means it does not.
  This is sender-scoped exact UUID correlation, not a body search, and it does
  not claim SMTP or other external-transport delivery. It also cannot resolve
  complete MCP response loss, because that loses the server-generated UUID.

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
`comm.read(id="<full_id>")` to clear one or `comm.read(ids=[...])` to clear up to 500 in one
operation. Always pass a `limit` — active inboxes are large. If `next_offset` is non-null, repeat
the same inbox filters with `offset=<next_offset>` until it is null; pagination itself never marks
messages read. Use `content_contains` when automated notifications omit `subject`; sender
exact/prefix/exclusion, RFC3339 `since`/`before`, and subject/content substring filters can be
combined. Mark writes are best-effort and cross-message updates are not atomic: inspect every
result's `read`/`mark_error`, and re-issue failures later.

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

### 5. Check channel polling freshness

```
request(ops="comm.health()")
```

Each channel reports `poll_interval_secs` (its nominal/minimum cadence) and nullable
`stalled`. A `true` value means the persisted poll schedule is more than three nominal
intervals overdue; `null` means the row is legacy/malformed or the channel is in a known
failure/backoff episode. Treat `stalled` as an advisory, not proof that the task is dead:
sequential or slow in-flight polls can also make a row overdue. Inspect
`consecutive_failures`, `last_error`, and `last_poll_attempt_at` together, and use the daemon
component supervisor for authoritative task-liveness/restart decisions.

## Anti-patterns

- **Sending as `"local"`.** Unattributed and unscoped. Set `KHIVE_ACTOR` first.
- **No subject.** The recipient can't triage. Always set one.
- **Using `self_send=true` to mask an identity collapse.** It is only for an intentional note to
  yourself; distinct agents need distinct configured actor identities.
- **Believing cross-namespace is denied.** It is not — delivery is actor-routed (ADR-057).
  Address `to="lambda:<name>"` directly.
- **Reading `properties` to find the sender.** `from` / `subject` / `preview` are top-level.
- **`comm.send` with a `thread_id` for a follow-up.** Use `comm.reply` — it threads, prefixes,
  and routes for you.
- **Blindly retrying an ambiguous send/reply.** Call `comm.delivered` with the
  surfaced full `outbound_id` first; otherwise a committed inbound sibling can
  become a duplicate.
- **Treating `comm.health.channels[].stalled` as a supervisor verdict.** It is a persisted
  schedule heuristic; correlate it with failure and timestamp fields.
