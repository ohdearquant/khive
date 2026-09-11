# Telemetry pack

The optional `telemetry` pack classifies events using an operator-defined channel table and
counts records in existing durable streams. Load it with `KHIVE_PACKS=kg,telemetry` or
`--pack kg --pack telemetry`. It requires `kg` and is outside the default pack set.

There is no ephemeral ring, subscriber, incremental rollup table, or per-kind payload schema.
Ephemeral events are accepted and dropped immediately. They cannot be read back or recovered
after reconnecting. Durable records use the existing `stream.append` storage path.

## Configuration

```toml
[telemetry]
stream = "telemetry"
default_carrier = "ephemeral"

[[telemetry.channels]]
kinds = ["run.started", "run.completed", "run.failed"]
carrier = "durable"
failure_posture = "stop"

[[telemetry.channels]]
kinds = ["turn.delta", "*.heartbeat"]
carrier = "ephemeral"
failure_posture = "gap"
```

Absent configuration uses stream `telemetry`, no channels, and the fail-cheap `ephemeral`
default. An unlisted event therefore does not silently become durable. An explicitly configured
`durable` default uses failure posture `stop`; the ephemeral default uses `gap`.

Kind matching is case-sensitive. Patterns are exact names or a leading `*.suffix` glob;
the latter matches any name ending in that literal dotted suffix. Empty kind lists, unknown
carriers or postures, unsupported patterns, and overlapping claims across different channels
are refused at configuration load. Refusals identify `telemetry.channels[N]` with a zero-based
entry index. Overlapping patterns inside one channel share one policy and are permitted.
Unknown configuration keys are refused. Stream names follow the existing stream contract:
at most 512 UTF-8 bytes and no U+0000.

## Emit And Read

```text
telemetry.channels()
telemetry.emit(kind="run.completed", payload={"job":"index","count":3}, run_id="run-42")
telemetry.emit(kind="turn.delta", payload="partial text")
telemetry.read(stream="telemetry", since=0, limit=100, kinds=["run.completed"])
```

`telemetry.channels` returns the effective table and `ephemeral_retention="none"`.
Durable carrier cursors are `log`; ephemeral drops have cursor kind `none`.

`telemetry.emit` requires `kind` and an arbitrary JSON `payload`, including null. Optional
`run_id` is opaque. The authenticated actor is stamped into durable records; supplying `actor`
asserts that same identity and cannot impersonate another caller. The response names the chosen
carrier, failure posture, and whether the event was dropped. Durable success includes the stream
sequence, timestamp, and stored note identifier as `receipt_id`. On an ephemeral drop,
`receipt_id` is only a generated correlation identifier, not a stored receipt.
`receipt_persisted` distinguishes these cases explicitly.

For a durable channel, `stop` propagates append failures. `gap` returns an explicit drop only
when the typed storage-admission failure proves the write never started: `accepted=false`,
`dropped=true`, `gap=true`, `domain_disposition="not_committed"`, and
`receipt_persisted=false`. Other errors propagate unchanged under either posture because their
commit outcome is not established; they must not be treated as a confirmed drop or blindly
retried. A gap response does not create a durable gap record or reconnect history.

`telemetry.read` reads durable records from any named stream in the request namespace.
`since` is an exclusive integer log sequence, not a timestamp. It defaults to zero; reuse the
returned `next_cursor` on the next call. `limit` is the number of rows scanned before optional
kind filtering, from 1 to 1,000, default 100. An empty filtered page can advance the cursor.
`has_more` identifies remaining rows at that read's head. No ephemeral gap history is fabricated.

## Windowed Counts

```text
telemetry.counts(stream="telemetry", window={"since":"2026-09-01T00:00:00Z","until":"2026-09-02T00:00:00Z"}, group_by=["kind","payload.job"])
```

The window uses each stream row's storage timestamp, not a timestamp inside its payload.
`since` is inclusive and `until` exclusive. Both use RFC3339 timestamps; omitted `until` is
captured as the current time. The end must be later than the start.

`group_by` accepts 1 to 8 unique dotted paths into the record and defaults to `["kind"]`.
Paths have a 128-byte limit. Values must be scalar or null; missing and null values share a
group. The optional `kinds` filter contains 1 to 100 exact event names. The result contains
`rows` with each grouped `key` and `count`, plus `window`, `group_by`, `total`, and `scanned`.

The scan pins the initial log head, so subsequent appends do not extend the population. It is
a live read, not an atomic multi-page snapshot: concurrent deletion can change what is visible.
`complete=true` means the bounded scan completed, not that it captured an immutable snapshot.
More than 50,000 scanned rows, 1,000 result groups, or 4,096 encoded bytes in one group key
cause a refusal instead of a partial total. Window and kind filtering do not remove that scan
budget. Ephemeral drops have no records and are never counted.
