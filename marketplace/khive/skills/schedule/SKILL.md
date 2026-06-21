---
description: Schedule time-triggered reminders and deferred verb dispatches, review upcoming events with agenda, and remove pending entries with cancel. Use whenever you want to remind yourself at a future time, automate a verb to run later, check what is on the calendar, or cancel a scheduled item.
---

# Schedule time-triggered reminders and actions

khive schedule is four verbs: `schedule.remind`, `schedule.schedule`, `schedule.agenda`, and
`schedule.cancel`. The distinction worth internalizing is `remind` vs `schedule`: remind stores
a human-readable prompt ("tell me at time T"), schedule stores a DSL verb string to dispatch
("run this verb at time T"). Per-verb param detail is one call away:
`request(ops="schedule.remind(help=true)")`.

## The pattern

### 1. Remind: tell me at time T

Use `remind` when you want the runtime to surface a text note at a future moment.

```
request(ops="schedule.remind(content=\"check CI results\", at=\"2026-06-22T10:00:00Z\")")
```

The `at` param is an RFC 3339 timestamp. It is named `at`, not `due`. Add `repeat` to make it
recurring: `"daily"`, `"weekly"`, `"monthly"`, or a 5-field cron expression (`"0 9 * * 1"` for
Mondays at 09:00).

```
request(ops="schedule.remind(content=\"weekly standup prep\", at=\"2026-06-23T09:00:00Z\", repeat=\"weekly\")")
```

### 2. Schedule: run a verb at time T

Use `schedule` to defer any DSL verb call. The `action` param must be a valid khive verb
expression — it is validated at write time. Plain English prose is rejected.

```
request(ops="schedule.schedule(action=\"gtd.assign(title=\\\"weekly review\\\", priority=\\\"p1\\\")\", at=\"2026-06-23T09:00:00Z\")")
```

Any expression accepted by `request` is valid as an action:

```
schedule.remind(content="check status")
comm.send(to="lambda:leo", content="heartbeat")
[memory.recall(query="recent work"), comm.inbox()]
```

### 3. Agenda: review what is pending

`schedule.agenda()` returns pending events sorted soonest-first. Cancelled or already-fired
events are excluded. Use a time window to focus.

```
request(ops="schedule.agenda()")
request(ops="schedule.agenda(from=\"2026-06-22T00:00:00Z\", to=\"2026-06-28T23:59:59Z\")")
```

Batch with inbox and tasks for a session-start snapshot:

```
request(ops="[schedule.agenda(limit=10), comm.inbox(limit=10), gtd.next(limit=5)]")
```

### 4. Cancel: remove a pending entry

Pass the full UUID from the response (or an unambiguous 8+ hex prefix). The event's `status` is
set to `cancelled` (with a `cancelled_at` timestamp) — it stays in storage for audit but vanishes
from `agenda`.

```
request(ops="schedule.cancel(id=\"a1b2c3d4-e5f6-7890-abcd-ef1234567890\")")
```

Run `schedule.agenda()` first to confirm the event is still pending before cancelling.

## Anti-patterns

- **`at` named wrong.** The param is `at`, not `due`. Using `due=` is silently wrong or rejected.
- **`action` as prose.** `action="send a heartbeat"` is rejected. The value must be a valid DSL verb call.
- **Natural-language timestamps.** `"tomorrow"` or `"next week"` are rejected by both `remind` and `schedule`. Compute the RFC 3339 string.
- **Cancelling by ambiguous prefix.** If the short prefix matches more than one event, the call is rejected. Use a longer prefix or the full UUID.
- **Expecting `agenda` to show cancelled events.** It only returns `status: "pending"`. Use `list(kind="scheduled_event")` via the KG pack to see all states.
