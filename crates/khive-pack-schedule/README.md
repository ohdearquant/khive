# khive-pack-schedule

The schedule pack for khive — time-triggered intent storage (`remind`,
`schedule`, `agenda`, `cancel`) over a dedicated `scheduled_event` note kind.

## Verbs

| Verb                | What it does                                                            |
| ------------------- | ----------------------------------------------------------------------- |
| `schedule.remind`   | Deliver a time-triggered reminder to your inbox                         |
| `schedule.schedule` | Schedule a future verb dispatch (a DSL string, validated at write time) |
| `schedule.agenda`   | List upcoming events; MCP also reports process-local ticker liveness    |
| `schedule.cancel`   | Cancel a scheduled event                                                |

`at` is an RFC 3339 timestamp; `repeat` accepts `daily` / `weekly` / `monthly`.
Cron expressions are rejected because the pending-events executor cannot advance
them; accepted recurrence never degrades silently to one-shot delivery.

## Semantics

This pack creates and queries `scheduled_event` notes; the daemon or pending-event
runner evaluates their triggers. At fire time, `schedule.remind` delivers its
content to the creating actor's inbox through the same dual-write path as
`comm.send`. Use `schedule.schedule(action="comm.send(...)")` when the recipient
is a different actor. Both creation verbs mirror `created_by_actor` for display and
write an immutable, target-bound creator-provenance event before activating the note.
Generic actions replay under the actor derived from that event for gate checks, audit
attribution, and writes. Existing `scheduled_event` notes are schedule-managed: generic
KG `update` and note `merge` reject them, preventing payload, trigger, cadence, or lifecycle
rewrites from reusing the immutable actor binding. Replay cannot invoke internal
subhandlers. A legacy generic row without provenance fails
closed instead of being dispatched. `schedule.schedule`'s
`action` parameter
is a full verb-dispatch string (e.g.
`"schedule.remind(content=\"hello\", at=\"2099-06-01T09:00:00Z\")"`) that must
satisfy a stricter _replayable_ contract, validated at write time (issue
\#461): a single call (no chains, no `$prev` references) against an
exactly-registered, pack-prefixed verb name, with only literal argument
values and every one of that verb's own required arguments present. This is
stricter than plain `khive_request::parse_request`-level parseability — the
inner call must itself be independently valid, because `kkernel`'s
pending-events runner re-parses and re-dispatches the stored string
unmodified at trigger time. An `action` that fails any of these checks is
rejected before the event is stored, not at trigger time. Reading pending
events and dispatching at `trigger_at` is the execution environment's
responsibility (the ADR-119-supervised daemon component or an external cron / cloud
scheduler invoking the pending-event runner).

The runner records deterministic occurrence identity, a fresh invocation identity,
and a renewable lease before dispatch. Failed one-shots remain pending for recovery;
expired invocations without a durable outcome become indeterminate and are not replayed
automatically. See ADR-106 Amendment F for the receipt and crash-recovery contract.

On the MCP surface, the host decorates `schedule.agenda` with
`ticker.last_tick_at`. It is null until the current server process observes a daemon tick
and then advances on every tick, including empty agendas. The value is process-local and
never stored with schedule intent, so restarting the server resets it instead of exposing
a predecessor's heartbeat as current liveness. Direct `SchedulePack` registry dispatch has
no host loop to report and therefore retains the pack-only `{events, count}` result.

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

Apache-2.0.
