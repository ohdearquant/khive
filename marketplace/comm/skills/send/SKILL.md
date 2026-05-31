---
description: Send a message to a recipient with optional subject and threading.
---

# Send

Send a structured message to another agent, lambda, or namespace. Each send creates an outbound copy
(sender's namespace) and an inbound copy (recipient's namespace).

## Workflow

### 1. Send a simple message

```
request(ops="comm.send(to=\"local\", content=\"deployment complete\")")
```

Response:

```json
{
  "id": "a1b2c3d4",
  "full_id": "a1b2c3d4-...",
  "from": "local",
  "to": "local",
  "subject": null,
  "sent_at": "2026-05-30T..."
}
```

### 2. Add a subject for triage

Subjects help recipients triage in busy inboxes:

```
request(ops="comm.send(to=\"local\", content=\"all 72 smoke tests pass\", subject=\"CI status\")")
```

### 3. Thread a follow-up to an existing conversation

Pass the `thread_id` from a prior message to continue the thread:

```
request(ops="comm.send(to=\"local\", content=\"fixed the flaky test\", thread_id=\"<prior-message-full-id>\")")
```

For most follow-ups, use `comm.reply` instead — it auto-threads and auto-routes.

### 4. Batch multiple sends

```
request(ops="[
  comm.send(to=\"local\", content=\"foundation layer green\", subject=\"CI\"),
  comm.send(to=\"local\", content=\"platform layer green\", subject=\"CI\")
]")
```

## Parameters

| Parameter   | Type   | Required | Description                                                                              |
| ----------- | ------ | -------- | ---------------------------------------------------------------------------------------- |
| `to`        | string | yes      | Recipient namespace. Must equal caller namespace (cross-namespace denied until ADR-018). |
| `content`   | string | yes      | Message body. Must not be empty.                                                         |
| `subject`   | string | no       | Subject line for triage.                                                                 |
| `thread_id` | string | no       | Full UUID of thread root to continue.                                                    |

## Anti-patterns

- **Empty content.** Rejected with an error.
- **Cross-namespace sends.** `to="other-ns"` is denied — use the same namespace as the caller.
- **Using send for replies.** Use `comm.reply` — it handles threading, subject prefixing, and
  routing automatically.
