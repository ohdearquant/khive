---
description: Cancel a pending scheduled event — sets status to cancelled with timestamp.
---

# Cancel

Cancel a scheduled event. Sets `status: "cancelled"` and records `cancelled_at`. The event remains
in storage for audit purposes but no longer appears in `agenda`.

## Workflow

### 1. Cancel an event

```
request(ops="schedule.cancel(id=\"a1b2c3d4-e5f6-7890-abcd-ef1234567890\")")
```

Response:

```json
{
  "id": "a1b2c3d4",
  "full_id": "a1b2c3d4-...",
  "status": "cancelled",
  "cancelled_at": "2026-05-30T..."
}
```

### 2. Verify cancellation

After cancelling, confirm it no longer appears in agenda:

```
request(ops="schedule.agenda()")
```

## Parameters

| Parameter | Type   | Required | Description                          |
| --------- | ------ | -------- | ------------------------------------ |
| `id`      | string | yes      | Full UUID of the scheduled event.    |

## Anti-patterns

- **Using a short UUID prefix.** `cancel` requires a full UUID — short prefixes are rejected.
- **Cancelling a non-scheduled-event note.** `cancel` only works on notes with
  `kind="scheduled_event"`. Cancelling a task or observation returns an error.
- **Cancelling a nonexistent ID.** Returns "not found".
- **Cancelling an already-cancelled event.** Returns an error — use `agenda` first to confirm
  the event is still pending before cancelling.
