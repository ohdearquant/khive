---
description: Set a time-triggered reminder with optional repeat schedule.
---

# Remind

Create a reminder that triggers at a future time. The reminder is stored as a `scheduled_event` note
with `status: "pending"`. The schedule pack stores intent only — trigger evaluation is handled by
the runtime.

## Workflow

### 1. Set a one-time reminder

```
request(ops="schedule.remind(content=\"check CI results\", at=\"2026-06-01T10:00:00Z\")")
```

Response:

```json
{
  "id": "a1b2c3d4",
  "full_id": "a1b2c3d4-...",
  "event_type": "remind",
  "trigger_at": "2026-06-01T10:00:00Z",
  "repeat": null,
  "status": "pending"
}
```

### 2. Set a recurring reminder

```
request(ops="schedule.remind(content=\"weekly standup prep\", at=\"2026-06-02T09:00:00Z\", repeat=\"weekly\")")
```

Valid repeat values: `"daily"`, `"weekly"`, `"monthly"`, or a 5-field cron expression (e.g.
`"0 9 * * 1"` for Mondays at 9am).

### 3. Batch reminders

```
request(ops="[
  schedule.remind(content=\"morning pills\", at=\"2026-06-01T08:00:00Z\", repeat=\"daily\"),
  schedule.remind(content=\"review metrics\", at=\"2026-06-01T17:00:00Z\", repeat=\"weekly\")
]")
```

## Parameters

| Parameter | Type   | Required | Description                                          |
| --------- | ------ | -------- | ---------------------------------------------------- |
| `content` | string | yes      | Reminder text. Must not be empty.                    |
| `at`      | string | yes      | RFC 3339 timestamp. Must be in the future.           |
| `repeat`  | string | no       | `"daily"`, `"weekly"`, `"monthly"`, or 5-field cron. |

## Anti-patterns

- **Past timestamps.** Rejected — use a future time.
- **Non-RFC-3339 strings.** `"tomorrow"` or `"next week"` are rejected. Compute the ISO timestamp.
- **Empty content.** Rejected.
- **Invalid cron.** Must be exactly 5 space-separated fields.
