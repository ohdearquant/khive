# schedule — Time-based Scheduling

Set reminders, schedule future verb dispatches, view upcoming events, and cancel scheduled items.
Events are stored as `scheduled_event` notes with trigger times, repeat rules, and lifecycle status.

## Skills

| Skill      | Description                     |
| ---------- | ------------------------------- |
| `remind`   | Set a time-triggered reminder   |
| `schedule` | Schedule a future verb dispatch |
| `agenda`   | View upcoming scheduled events  |
| `cancel`   | Cancel a pending event          |

## Quick start

```
# Set a reminder
request(ops="schedule.remind(content=\"check CI results\", at=\"2026-06-01T10:00:00Z\")")

# Schedule a verb dispatch
request(ops="schedule.schedule(action=\"remind(content=\\\"weekly review\\\")\", at=\"2026-06-02T09:00:00Z\", repeat=\"weekly\")")

# View agenda
request(ops="schedule.agenda()")

# Cancel an event
request(ops="schedule.cancel(id=\"<event-id>\")")
```

## What's New in 0.2.3

- **Cancel guard**: `schedule.cancel` now rejects attempts to cancel an already-cancelled event with
  a clear error instead of silently succeeding.

## How it works

The schedule pack stores intent only — it does not execute triggers. Events are `scheduled_event`
notes with `status: "pending"` until cancelled. The `at` parameter must be an RFC 3339 timestamp in
the future. The `action` parameter in `schedule.schedule` is validated as parseable DSL at write
time. Repeat accepts `"daily"`, `"weekly"`, `"monthly"`, or a 5-field cron expression.

## Requirements

Requires the `kg` pack (notes infrastructure) and the `schedule` pack.
