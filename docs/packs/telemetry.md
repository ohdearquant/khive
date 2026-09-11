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

Loading telemetry requires an explicit `telemetry.default_carrier` declaration. A deployment
that does not load telemetry needs no telemetry configuration. The stream defaults to
`telemetry` and the channel list defaults to empty. An unlisted event uses the declared default
with `classified=false`; matching a channel returns `classified=true`. A `durable` default uses
failure posture `stop`; an `ephemeral` default uses `gap`. Metadata inspection does not activate
the pack and needs no carrier declaration.

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
`run_id` is opaque. The runtime's canonical `kind:id` actor stamp is server-owned and appears
in the response and durable record. A caller-supplied `actor` is only a matching assertion;
it never supplies the stamp. A different assertion is ignored and returns
`actor_argument_ignored=true`. Actor fields inside the generic payload remain payload data and
cannot change the record's attribution.
The response names the chosen carrier, failure posture, classification, and outcome.

| Outcome    | Meaning                                                                          | Receipt                                                                          |
| ---------- | -------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| `recorded` | The durable append succeeded.                                                    | `receipt_id` is the stored row ID; `seq` and `created_at` identify its position. |
| `dropped`  | An ephemeral event was discarded, or the durable append is proven not committed. | `receipt_id=null`; no sequence is claimed.                                       |
| `unknown`  | A durable append failed without proof whether it committed.                      | `receipt_id=null`; no sequence is claimed.                                       |

For a durable channel, `stop` propagates the original append error. `gap` returns `dropped`
only when the append path proves no commit, and otherwise returns `unknown`. Both gap outcomes
include the original structured `error`, preserving its own disposition and details separately
from the append outcome. Neither posture retries. Ephemeral drops have no error. A gap response
does not create a durable gap record or reconnect history. Failures before the handler runs
remain dispatch errors, regardless of the configured append posture.

`telemetry.read` reads durable records from any named stream in the request namespace.
Both `read` and `counts` default to the calling actor. An explicit `actor` must be visible
to the caller. `all_actors=true` requires the calling actor in `brain.fleet_readers` and cannot
be combined with `actor`. Legacy raw actor IDs remain readable when unambiguous.
`since` is an exclusive integer log sequence, not a timestamp. It defaults to zero; reuse the
returned `next_cursor` on the next call. `limit` is the number of rows scanned before optional
kind filtering, from 1 to 1,000, default 100. An empty filtered page can advance the cursor.
`has_more` identifies remaining rows at that read's head. Actor and kind filtering happen after
the scan cursor advances. `coverage` reports the scanned sequence window, visibility, and current
policy. With explicit `kinds`, `coverage.ephemeral` lists the requested kinds currently routed
ephemerally, including when the returned page is empty. Without `kinds`, it is null and
`classification_scope="all_kinds"`; `current_policy` describes the full routing table and default.
That scope requires a present, non-null `current_policy`. An unavailable or incomplete policy
refuses the call, producing no coverage object; it cannot masquerade as a successful unfiltered read.
Coverage describes current configuration, not historical routing or a loss ledger. Historical
durable rows remain visible after their kind becomes ephemeral.

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
