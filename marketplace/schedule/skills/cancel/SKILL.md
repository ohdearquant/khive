---
description: Cancel a pending scheduled event — sets status to cancelled with timestamp.
---

# Cancel

Cancel a scheduled event. Sets `status: "cancelled"` and records `cancelled_at`. The event remains
in storage for audit purposes but no longer appears in `agenda`.

## Workflow

### 1. Cancel an event

```
request(ops="schedule.cancel(id=\"<event-full-id>\")")
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

### 2. Cancel by short prefix

```
request(ops="schedule.cancel(id=\"a1b2c3d4\")")
```

The 8-char prefix is resolved to the full UUID.

### 3. Verify cancellation

After cancelling, confirm it no longer appears in agenda:

```
request(ops="schedule.agenda()")
```

## Parameters

| Parameter | Type   | Required | Description                                        |
| --------- | ------ | -------- | -------------------------------------------------- |
| `id`      | string | yes      | Full UUID or 8-char prefix of the scheduled event. |

## Anti-patterns

- **Cancelling a non-scheduled-event note.** `cancel` only works on notes with
  `kind="scheduled_event"`. Cancelling a task or observation returns an error.
- **Cancelling a nonexistent ID.** Returns "not found".
- **Note:** Cancelling an already-cancelled event is currently idempotent (no error, overwrites
  `cancelled_at`). See issue #544.
