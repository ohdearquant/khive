# ADR-073: Communication and Schedule Packs

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive

## Context

khive's pack system (ADR-025) currently ships three packs: kg (knowledge graph), gtd (task
management), and memory (persistent recall). Two additional domains are natural extensions:

1. **Communication** — agents need to send messages, track conversations, and coordinate with
   other agents or humans. Today this is handled outside khive via MCP tools or direct API
   calls, losing the structured persistence that packs provide.

2. **Schedule** — time-triggered actions (reminders, recurring tasks, deadlines) have no native
   representation. GTD tracks _what_ needs doing but not _when_. Agents that need "remind me in
   2 hours" or "check this daily" have no pack-level primitive.

Both domains were part of the original khive internal server but were not included in the OSS
v0.1 release.

## Decision

### Communication pack (`khive-pack-comm`)

**Note kind**: `message`

**Verbs**:

| Verb    | Args                                      | Description                         |
| ------- | ----------------------------------------- | ----------------------------------- |
| `send`  | `to`, `subject?`, `content`, `thread_id?` | Send a message, optionally threaded |
| `inbox` | `limit?`, `status?`                       | List inbound messages               |
| `read`  | `id`                                      | Mark message as read                |
| `reply` | `id`, `content`                           | Reply to a message (creates thread) |

**Design constraints**:

- Messages are notes with `kind=message` and directional metadata (from/to).
- Threading via `thread_id` — a message can reference a parent thread.
- No real-time delivery — this is a mailbox model. Agents poll via `inbox`.
- Cross-namespace messaging requires explicit ACL (deferred to namespace policy layer).

### Schedule pack (`khive-pack-schedule`)

**Note kind**: `event`

**Verbs**:

| Verb       | Args                                  | Description                      |
| ---------- | ------------------------------------- | -------------------------------- |
| `remind`   | `content`, `at` (ISO 8601), `repeat?` | Create a time-triggered reminder |
| `schedule` | `action` (verb+args), `at`, `repeat?` | Schedule a future verb dispatch  |
| `agenda`   | `from?`, `to?`, `limit?`              | List upcoming events             |
| `cancel`   | `id`                                  | Cancel a scheduled event         |

**Design constraints**:

- Events are notes with `kind=event` and temporal metadata.
- `repeat` supports: `daily`, `weekly`, `monthly`, or cron expression.
- `schedule` stores a serialized verb+args payload — the runtime replays it at trigger time.
- Trigger evaluation requires a polling loop or external scheduler (not in-process). The pack
  stores intent; execution is the runtime's responsibility.
- No sub-minute precision — this is for agent-scale scheduling, not real-time.

### Pack interaction

- Schedule + GTD: scheduled tasks can auto-transition (`remind` when a task is due).
- Schedule + Comm: scheduled messages ("send weekly status update").
- Both packs follow the DeclarativePack pattern (ADR-050) when that lands.

## Consequences

- Two new crates: `khive-pack-comm` and `khive-pack-schedule`.
- Two new note kinds: `message` and `event` (require ADR-019 amendment).
- The runtime needs a trigger evaluation mechanism for scheduled events.
- Cross-namespace messaging is explicitly deferred to the ACL layer.

## Alternatives considered

1. **Embed in GTD** — add scheduling to the GTD pack. Rejected: GTD is about task lifecycle,
   not temporal triggers. Mixing them conflates "what to do" with "when to do it."
2. **External scheduler only** — use OS cron or cloud scheduler. Rejected: agents need to
   schedule from within the MCP session. The pack provides the intent storage; external
   schedulers provide the trigger mechanism.
