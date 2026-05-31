---
description: View a full conversation thread in chronological order.
---

# Thread

Retrieve all messages in a conversation thread, ordered chronologically (earliest first). Pass any
message ID from the thread — the handler resolves to the canonical thread root automatically.

## Workflow

### 1. View a thread

```
request(ops="comm.thread(id=\"<any-message-id-in-thread>\")")
```

Response:

```json
{
  "thread_id": "<canonical-root-uuid>",
  "count": 3,
  "messages": [
    { "id": "...", "content": "original message", "created_at": "2026-05-30T10:00:00..." },
    { "id": "...", "content": "first reply", "created_at": "2026-05-30T10:05:00..." },
    { "id": "...", "content": "second reply", "created_at": "2026-05-30T10:10:00..." }
  ]
}
```

### 2. Limit thread length

For long threads, cap the result:

```
request(ops="comm.thread(id=\"<message-id>\", limit=10)")
```

Default limit is 100, max 500.

### 3. Use any message as entry point

Both the original message ID and any reply ID resolve to the same thread:

```
request(ops="comm.thread(id=\"<original-msg-id>\")")
request(ops="comm.thread(id=\"<reply-msg-id>\")")
```

Both return the same thread.

## Parameters

| Parameter | Type   | Required | Description                                              |
| --------- | ------ | -------- | -------------------------------------------------------- |
| `id`      | string | yes      | Full UUID or 8-char prefix of any message in the thread. |
| `limit`   | int    | no       | Max messages to return (default 100, max 500).           |

## Patterns

### Reconstruct context before replying

```
request(ops="comm.thread(id=\"<message-id>\")")
```

Read the thread before composing a reply to avoid repeating information already covered.

## Anti-patterns

- **Threading on a non-message note.** Only `kind="message"` notes are valid. An observation or task
  note returns an error.
- **Using a nonexistent ID.** Returns "not found".
