# khive-pack-schedule

The schedule pack for khive — time-triggered intent storage (`remind`,
`schedule`, `agenda`, `cancel`) over a dedicated `scheduled_event` note kind.

## Verbs

| Verb                | What it does                                                            |
| ------------------- | ----------------------------------------------------------------------- |
| `schedule.remind`   | Deliver a time-triggered reminder to your inbox                         |
| `schedule.schedule` | Schedule a future verb dispatch (a DSL string, validated at write time) |
| `schedule.agenda`   | List upcoming scheduled events, optionally within a time window         |
| `schedule.cancel`   | Cancel a scheduled event                                                |

`at` is an RFC 3339 timestamp; `repeat` accepts `daily` / `weekly` / `monthly`
or a limited 5-field form where each field is `*` or one in-range integer
(e.g. `"0 9 * * 1"`). Cron operators such as steps (`*/15`), ranges (`9-17`),
and lists (`0,30`) are not accepted, and `kkernel`'s pending-events runner
currently fires the 5-field form one-shot rather than advancing it to its
next occurrence.

## Semantics

This pack creates and queries `scheduled_event` notes; the daemon or pending-event
runner evaluates their triggers. At fire time, `schedule.remind` delivers its
content to the creating actor's inbox through the same dual-write path as
`comm.send`. Use `schedule.schedule(action="comm.send(...)")` when the recipient
is a different actor. `schedule.schedule`'s `action` parameter
is a full verb-dispatch string (e.g.
`"schedule.remind(content=\"hello\", at=\"2099-06-01T09:00:00Z\")"`) that must
satisfy a stricter *replayable* contract, validated at write time (issue
\#461): a single call (no chains, no `$prev` references) against an
exactly-registered, pack-prefixed verb name, with only literal argument
values and every one of that verb's own required arguments present. This is
stricter than plain `khive_request::parse_request`-level parseability — the
inner call must itself be independently valid, because `kkernel`'s
pending-events runner re-parses and re-dispatches the stored string
unmodified at trigger time. An `action` that fails any of these checks is
rejected before the event is stored, not at trigger time. Reading pending
events and dispatching at `trigger_at` is the execution environment's
responsibility (the daemon tick or an external cron / cloud scheduler invoking
the pending-event runner).

## Usage

`SchedulePack` requires only the `kg` pack (`REQUIRES = ["kg"]`) for the notes
substrate. `schedule.remind` additionally requires the `comm.send` delivery
capability at creation time; without it, the call fails before any
`scheduled_event` note is persisted. Include `CommPack` when creating reminders:

```rust
use khive_pack_kg::KgPack;
use khive_pack_comm::CommPack;
use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

let mut builder = VerbRegistryBuilder::new();
builder.register(KgPack::new(runtime.clone()));
builder.register(CommPack::new(runtime.clone()));
builder.register(SchedulePack::new(runtime));
let registry = builder.build()?;

registry
    .dispatch(
        "schedule.remind",
        json!({"content": "Ship the 0.4.0 release", "at": "2026-07-05T09:00:00Z"}),
    )
    .await?;
```

Over MCP: `request(ops="schedule.remind(content=\"Ship the 0.4.0 release\", at=\"2026-07-05T09:00:00Z\")")`.

## Where this sits

`khive-pack-schedule` sits alongside `khive-pack-gtd`, `khive-pack-memory`,
and `khive-pack-comm` in the pack layer, depending on `khive-pack-kg` for the
note substrate and on `khive-request` to validate `schedule.schedule`'s
DSL payload, registering into `khive-runtime`'s `VerbRegistry`, consumed by
`khive-mcp`. The pack can load without `khive-pack-comm`; only
`schedule.remind` requires a registered `comm.send` delivery verb. Governing ADR:
[ADR-040](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-040-communication-and-schedule-packs.md) (communication and schedule packs),
built on [ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md) (pack standard).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
