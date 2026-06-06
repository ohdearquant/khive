# khive-pack-schedule Design

## ADR Compliance

### ADR-040: Communication and Schedule Packs

This crate implements the schedule half of ADR-040.

**Core constraint: intent storage only.** The pack creates and queries
`scheduled_event` notes. Trigger evaluation — reading pending events, checking
`trigger_at` against the current time, and dispatching the stored payload — is
not performed by this pack in process. Two supported execution modes exist:

1. `kkernel scheduler` daemon mode (future): polls pending events and dispatches
   them via the internal verb registry.
2. External scheduler integration: an operator configures OS cron or a cloud
   scheduler to call `kkernel exec --pending-events` at an appropriate polling
   interval (minimum 1 minute).

**Pack identity:**

- Name: `schedule`
- Note kinds: `["scheduled_event"]`
- Entity kinds: `[]`
- Requires: `["kg"]`
- Verbs: `schedule.remind`, `schedule.schedule`, `schedule.agenda`, `schedule.cancel`

**Notes-as-scheduled-events.** A `scheduled_event` is a note stored with the
following `properties` shape:

```json
{
  "trigger_at": "2026-05-23T14:00:00Z",
  "repeat": "daily",
  "status": "pending",
  "event_type": "remind",
  "payload": null,
  "fired_at": null,
  "cancelled_at": null
}
```

`event_type` distinguishes `remind` (no action payload; fires a notification)
from `schedule` (stores a serialized verb+args payload for replay). `payload`
is null for reminders and a JSON-encoded verb call string for scheduled dispatch.

**Four verbs:**

| Verb | Speech act | Args | What it does |
|------|-----------|------|-------------|
| `schedule.remind` | commissive | `content`, `at`, `repeat?` | Create a `scheduled_event` with `event_type="remind"` |
| `schedule.schedule` | commissive | `action`, `at`, `repeat?` | Create a `scheduled_event` with `event_type="schedule"`; `action` is a DSL verb string |
| `schedule.agenda` | assertive | `from?`, `to?`, `limit?` | List pending `scheduled_event` notes ordered by `trigger_at` ascending |
| `schedule.cancel` | declaration | `id` | Set `properties.status = "cancelled"`, record `cancelled_at` |

**Recurrence specification.** `repeat` accepts:

| Value | Semantics |
|-------|-----------|
| `"daily"` | Repeat every 24 hours from `trigger_at` |
| `"weekly"` | Repeat every 7 days |
| `"monthly"` | Repeat on the same day-of-month each month |
| 5-field cron expression | Standard cron: `"0 9 * * 1"` (Monday 09:00) |

Cron field ranges:
- $\text{MIN} \in [0, 59]$
- $\text{HOUR} \in [0, 23]$
- $\text{DOM} \in [1, 31]$
- $\text{MON} \in [1, 12]$
- $\text{DOW} \in [0, 7]$

**`action` payload security.** The `action` string accepted by `schedule` is
validated at write time by the request DSL parser. Garbage inputs are rejected
before entering storage. At dispatch time, the payload runs with the permissions
of the namespace that created the event — no privilege escalation is possible
via stored payloads.

**Pack-auxiliary index.** The `idx_schedule_trigger` index is declared via
`SchemaPlan` as idempotent DDL (`CREATE INDEX IF NOT EXISTS`) outside the core
versioned migration chain. It uses `WHERE deleted_at IS NULL` rather than
`WHERE kind = 'scheduled_event'` so that the parameterized `kind = ?N` predicate
in `build_note_filter_where` can use this index. A literal-value partial condition
on `kind` is invisible to the SQLite planner when the query uses a bound parameter.

**Disambiguation from substrate `Event`.** The substrate `Event` type is a
read-only audit observable emitted by the runtime on state changes. It is not
user-authored. The `scheduled_event` note kind is user-authored future intent.
The two concepts are deliberately named to avoid confusion.

### ADR-015: Schema Migrations

Pack-auxiliary DDL (the `idx_schedule_trigger` index) uses idempotent
`CREATE INDEX IF NOT EXISTS` and is NOT part of the core versioned migration
chain. It is declared via `schema_plan()` on `PackRuntime`.

### ADR-016: Request DSL

The `action` parameter of `schedule.schedule` is validated at write time using
`khive_request::parse_request`. This catches malformed DSL before it enters
storage, rather than at trigger time when no observer is present.

### ADR-017: Pack Standard

The pack self-registers via `inventory::submit!`. It declares `REQUIRES = ["kg"]`,
ensuring the kg pack is loaded before this pack during boot-time dependency
resolution.

### ADR-025: Verb Speech Acts

- `schedule.remind` and `schedule.schedule` are commissive (create future intent).
- `schedule.agenda` is assertive (query state without side effects).
- `schedule.cancel` is declarative (changes the status of an existing record).

## Consistency Notes

- The `agenda` handler uses a bounded top-k scan with a `BinaryHeap` rather than
  fetching all pending events and sorting. This is correct behavior (efficiency
  at large scale) but is not explicitly specified in ADR-040 — it is an
  implementation detail.
- The `idx_schedule_trigger` index in ADR-040's `schema_plan` example uses
  `WHERE kind = 'scheduled_event'` as the partial condition. The implementation
  uses `WHERE deleted_at IS NULL` instead for SQLite planner compatibility with
  parameterized queries. The ADR example is illustrative; the implementation is
  correct.
- `cancel` enforces a namespace check (`note.namespace != token.namespace()`)
  before modifying the event. This is the standard namespace isolation gate and
  is consistent with the pattern in other packs.
- Pagination in `agenda` uses `u64` offsets (not `u32`) to prevent overflow on
  very large stores. This is a defensive coding choice beyond ADR-040's spec.
