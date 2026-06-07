---
description: Schedule a future verb dispatch — the action DSL is validated at write time.
---

# Schedule

Schedule a khive verb to execute at a future time. Unlike `remind` (which stores a text prompt),
`schedule` stores a parseable DSL action string that can be dispatched by the trigger engine.

## Workflow

### 1. Schedule a future verb dispatch

```
request(ops="schedule.schedule(action=\"remind(content=\\\"weekly review\\\")\", at=\"2026-06-02T09:00:00Z\")")
```

The `action` string is validated as parseable DSL at write time — invalid verb calls are rejected
before they enter storage.

Response:

```json
{
  "id": "b2c3d4e5",
  "full_id": "b2c3d4e5-...",
  "event_type": "schedule",
  "trigger_at": "2026-06-02T09:00:00Z",
  "repeat": null,
  "status": "pending"
}
```

### 2. Schedule with repeat

```
request(ops="schedule.schedule(action=\"comm.send(to=\\\"local\\\", content=\\\"heartbeat\\\")\", at=\"2026-06-01T00:00:00Z\", repeat=\"daily\")")
```

### 3. Valid action examples

```
remind(content="check status")
comm.send(to="local", content="automated ping")
gtd.assign(title="weekly review", priority="p1")
[memory.recall(query="recent work"), comm.inbox()]
```

Any DSL expression accepted by `request` is valid as an action.

## Parameters

| Parameter | Type   | Required | Description                                          |
| --------- | ------ | -------- | ---------------------------------------------------- |
| `action`  | string | yes      | Valid khive DSL verb call. Validated at write time.  |
| `at`      | string | yes      | RFC 3339 timestamp. Must be in the future.           |
| `repeat`  | string | no       | `"daily"`, `"weekly"`, `"monthly"`, or 5-field cron. |

## Anti-patterns

- **Invalid DSL.** `action="do something"` is rejected — must be a valid verb call.
- **Empty action.** Rejected.
- **Past timestamps.** Rejected.
- **Confusing remind and schedule.** Use `remind` for human-readable prompts. Use `schedule` for
  automated verb dispatch.
