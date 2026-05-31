---
description: View upcoming scheduled events — filter by time window, sorted ascending by trigger time.
---

# Agenda

List pending scheduled events in the caller's namespace. Events are sorted ascending by trigger time
(soonest first). Only `status: "pending"` events are shown — cancelled or fired events are excluded.

## Workflow

### 1. View all pending events

```
request(ops="schedule.agenda()")
```

Response:

```json
{
  "events": [
    {
      "id": "a1b2c3d4",
      "full_id": "a1b2c3d4-...",
      "kind": "scheduled_event",
      "content": "check CI results",
      "properties": { "trigger_at": "2026-06-01T10:00:00Z", "status": "pending", "event_type": "remind", ... }
    }
  ],
  "count": 1
}
```

### 2. Filter by time window

Show events in a specific range:

```
request(ops="schedule.agenda(from=\"2026-06-01T00:00:00Z\", to=\"2026-06-07T23:59:59Z\")")
```

Both `from` and `to` must be RFC 3339 timestamps. Either can be omitted for an open-ended range.

### 3. Limit results

```
request(ops="schedule.agenda(limit=5)")
```

Default limit is 20, max 200.

### 4. Batch: agenda + inbox in one call

```
request(ops="[schedule.agenda(limit=5), comm.inbox(limit=5)]")
```

## Patterns

### Morning dashboard

```
request(ops="[schedule.agenda(limit=10), comm.inbox(limit=10), gtd.next(limit=5)]")
```

Combine agenda with inbox and tasks for a session-start overview.

### Weekly planning

```
request(ops="schedule.agenda(from=\"2026-06-02T00:00:00Z\", to=\"2026-06-08T23:59:59Z\")")
```

## Anti-patterns

- **Non-RFC-3339 filter values.** `from="next week"` is rejected.
- **Expecting cancelled events.** Agenda only returns `status: "pending"`. Use
  `list(kind="scheduled_event")` via the KG pack to see all events including cancelled ones.
