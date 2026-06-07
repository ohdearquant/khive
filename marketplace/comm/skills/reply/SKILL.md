---
description: Reply to a message with automatic threading, subject prefixing, and recipient routing.
---

# Reply

Reply to an existing message. The reply is automatically threaded to the original conversation, the
subject gets "Re: " prepended (if not already present), and the recipient is routed to the "other
party" — if you are the original sender, the reply goes to the original recipient, and vice versa.

## Workflow

### 1. Reply to a message

```
request(ops="comm.reply(id=\"<message-full-id>\", content=\"acknowledged, will deploy tomorrow\")")
```

Response:

```json
{
  "id": "b2c3d4e5",
  "full_id": "b2c3d4e5-...",
  "thread_id": "<canonical-thread-root>",
  "from": "local",
  "to": "local",
  "subject": "Re: status update",
  "sent_at": "2026-05-30T..."
}
```

### 2. Reply chains

Subsequent replies to any message in the thread continue the same thread:

```
# First reply
request(ops="comm.reply(id=\"<original-msg-id>\", content=\"got it\")")

# Reply to the reply — same thread
request(ops="comm.reply(id=\"<reply-msg-id>\", content=\"one more thing...\")")
```

Subject does not double-prepend: "Re: status update" stays "Re: status update".

### 3. View the full thread after replying

```
request(ops="comm.thread(id=\"<any-message-in-thread>\")")
```

## Parameters

| Parameter | Type   | Required | Description                                                                      |
| --------- | ------ | -------- | -------------------------------------------------------------------------------- |
| `id`      | string | yes      | Full UUID or 8-char prefix of the message to reply to. Must be kind `"message"`. |
| `content` | string | yes      | Reply body. Must not be empty.                                                   |

## Anti-patterns

- **Replying to a non-message note.** `comm.reply` only works on notes with `kind="message"`.
  Replying to an observation or task returns an error.
- **Empty reply content.** Rejected.
- **Using `comm.send` with `thread_id` instead of `reply`.** `reply` handles subject prefixing and
  recipient routing automatically.
