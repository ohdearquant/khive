# khive-pack-comm

The communication pack for khive — inter-agent messaging over a dedicated
`message` note kind, with dual-write, actor-addressed delivery, sender-side
delivery confirmation, and channel polling observability.

## Verbs

| Verb             | What it does                                                                                                                                                  |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `comm.send`      | Send a message, optionally threaded                                                                                                                           |
| `comm.delivered` | Confirm the internal inbound sibling for an outbound UUID                                                                                                     |
| `comm.inbox`     | Page and filter the caller's inbound inbox or sent-message history, optionally waiting up to 30 seconds for a new matching message                            |
| `comm.read`      | Mark one or up to 500 inbound messages as read (best-effort: inspect each result's `read`/`mark_error`)                                                       |
| `comm.mark_read` | Named bulk mark-read for 1-500 inbound messages; `atomic=true` makes the cross-message mutation all-or-nothing                                                |
| `comm.unread`    | Count the caller's unread inbound messages without message payloads                                                                                           |
| `comm.reply`     | Reply to a message, preserving thread linkage                                                                                                                 |
| `comm.thread`    | Retrieve all messages in a conversation thread, chronologically                                                                                               |
| `comm.health`    | Read a bounded heartbeat-first channel snapshot, quarantine backlog counts, nominal poll cadence, and nullable advisory schedule staleness                    |
| `comm.probe`     | Read-only poll for new inbound message metadata and a stale unread count (takes an explicit `actor`; unlike `comm.inbox`, it is not inferred from the caller) |

The internal `comm.ingest` handler is `Visibility::Subhandler` — it lets an
out-of-band channel adapter (email, Telegram, etc.) write an inbound message
directly, deduplicated by `external_id`, but it is not callable on the MCP wire.

## `comm.inbox` — optional long poll

`wait_ms` is optional and defaults to `0`, preserving the immediate snapshot
behavior. When it is positive and the initial filtered query is empty, the
call waits for a newly committed message and re-runs the same actor, status,
and sender-filtered query. The maximum is 30,000 ms. Existing messages still
return immediately, and `limit=0` never waits.

The wake signal is process-local and advisory; the database remains the source
of truth. Successful `comm.send`/`comm.reply` deliveries and successful,
non-deduplicated `comm.ingest` writes publish after commit. An unrelated wake
only causes a filtered re-query, so it cannot leak another actor's message or
make a filtered call return early with an empty page.

## `comm.probe` — polling contract

`comm.probe` is a strictly read-only verb built for frequent polling (e.g.
every 30s by many monitors): it never mutates the `read` flag or writes any
row. It runs a single indexed query (`INDEXED BY idx_comm_message_to_actor`)
over inbound messages addressed to the given `actor`.

Args:

- `actor` (required) — actor label whose inbound queue is probed, e.g.
  `"lambda:leo"`.
- `since_us` (optional): cursor from a previous `comm.probe` response's
  `cursor_us`; only messages committed after that cursor are returned.
- `stale_minutes` (optional, default 20) — unread age threshold in minutes.

Returns:

- `cursor_us`: an opaque, monotonically increasing token (currently backed
  by the durable `notes_seq.seq` commit-order sequence), or `0` if no
  inbound messages exist for the actor.
  Round-trip it as the next call's `since_us`; do not treat it as a
  timestamp or compute elapsed time from it (#780).
- `new_messages` — up to 100 newest matching rows, each `{id, created_at_us,
  from_actor, subject?}`, ordered ascending (newest-last) by `created_at`.
  `created_at_us` is a real display timestamp, useful for "how long ago did
  this arrive", but it is not the cursor and carries no ordering guarantee
  relative to `cursor_us`.
- `stale_unread_count` — count of inbound unread messages older than
  `stale_minutes`.

The response shape is frozen: it is a public polling contract and must stay
minimal and stable. `cursor_us`/`since_us` keep their `_us`-suffixed wire
names for backward compatibility even though the value is no longer a
microsecond timestamp (issue #780); the representation is deliberately
opaque so it can change again without a breaking rename.

## Dual-write delivery

Every `comm.send` writes two `message` notes via `dual_write_message`
(`src/message.rs`): an **outbound** copy (`direction=outbound`) and an
**inbound** copy (`direction=inbound`), linked by `outbound_ref`. Rows, FTS
documents, and vector rows for both copies commit in one atomic unit. An
ordinary prepare or plan failure therefore leaves neither copy. If the writer
seam reports `side_effects_unknown`, however, the caller cannot tell whether
the complete pair committed. That error is surfaced as `ambiguous` with the
pre-generated full `outbound_id`; pass the UUID to `comm.delivered` before
deciding whether to retry.

`comm.delivered(id=<full-outbound-uuid>)` performs one indexed, read-only
lookup for a live inbound message whose `outbound_ref` is that UUID and whose
`from_actor` is the caller. Its
response is `{id, status, delivered, inbound_count}`, where `status` is
`"delivered"` when at least one inbound sibling exists and `"undelivered"`
otherwise. It does not require the outbound row to exist and never compares
message bodies, so it also works for legacy/injected half-pairs and identical
templated content. An operation error means the lookup itself is uncertain.
This confirms only khive's internal inbound copy; it does not report later
SMTP or other external-transport delivery. Loss of the entire MCP response is
outside this contract: without the structured error, the caller never receives
the server-generated UUID needed for confirmation.

New comm-authored messages use the versioned
[`properties` v1 contract](docs/api/message-properties.md). If `KHIVE_PROCESS_REF` is set for a
`comm.send` or `comm.reply`, its opaque value is copied to `sent_by_process` on both delivery
copies without affecting routing or authorization.

Two addressing modes govern where the inbound copy lands:

- **Actor-addressed** ([ADR-057](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-057-comm-actor-addressed-delivery.md)) —
  `to` carries an actor label (e.g. `"lambda:leo"`) stamped into
  `to_actor`/`from_actor` properties. Both copies land in the caller's
  namespace; recall is actor-filtered, not namespace-filtered. This is the
  common case.
- **Cross-namespace** (original [ADR-040](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-040-communication-and-schedule-packs.md) model) —
  when `to` is a bare namespace different from the sender's and no actor label
  is supplied, delivery is gated by the sender's
  `actor.allowed_outbound_namespaces` allowlist in `khive.toml`; an unlisted
  namespace returns `RuntimeError::PermissionDenied`.

A root message (`thread_id` absent) gets a canonical `thread_id` equal to the
outbound note's own UUID, generated before either note is written and set on
both copies, so `comm.thread` finds every reply regardless of which copy it
answered.

## Usage

`CommPack` requires the `kg` pack (`REQUIRES = ["kg"]`) for the notes substrate:

```rust
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

let mut builder = VerbRegistryBuilder::new();
builder.register(KgPack::new(runtime.clone()));
builder.register(CommPack::new(runtime));
let registry = builder.build()?;

registry
    .dispatch("comm.send", json!({"to": "lambda:leo", "content": "PR #372 is ready for review"}))
    .await?;

let inbox = registry.dispatch("comm.inbox", json!({"limit": 20})).await?;
let next = registry.dispatch("comm.inbox", json!({"limit": 20, "wait_ms": 30_000})).await?;
```

`comm.inbox` defaults to inbound messages. Pass `box="sent"` to list only the
caller's outbound rows; `to_actor` filters that sent history by recipient, and
the existing `limit`/`offset`/`since`/`before`/subject/content filters still
apply. The default inbox behavior is unchanged. Responses return
`has_more`/`next_offset`; pass the latter back as `offset` with otherwise-identical
filters to enumerate the complete read-only result. Time bounds apply to
top-level `created_at`.

Both `comm.inbox` and `comm.thread` accept the same non-empty `fields=[...]`
projection. Unknown names fail loudly; omitting `fields` preserves the complete
message view. Stable property aliases such as `from_actor`, `to_actor`, and
`sent_at` can be requested without returning the full body or `properties` map.

`comm.read(id=...)` keeps the single-message response. The additive
`comm.read(ids=[...])` form validates 1-500 supplied IDs and returns per-item
outcomes with marked/failed counts; inspect `read`/`mark_error` because bulk
updates are not one cross-message transaction. `comm.read` remains available
for compatibility, but its name describes neither retrieval nor mutation
clearly; retrieve message content through `comm.inbox` or `comm.thread`.

Use `comm.mark_read(ids=[...])` as the canonical bulk mutation. It reuses the
same best-effort behavior by default. Pass `atomic=true` when every unique
validated target must be marked inside one transaction or none may change.

At the MCP request layer, a parallel batch makes every operation independent.
Putting `comm.send` beside `comm.read` or `comm.mark_read` does not condition the
read mark on delivery: the acknowledgement may commit even if the send fails.
Use a `send | mark_read` chain when that dependency is required. For replying to
one inbound message, prefer `comm.reply`; it commits delivery before attempting
the original message's best-effort read mark, so a delivery failure cannot mark
the original read.

This is the residual scope of #1387 after the 0.7.0 release: #1572 already
shipped bulk best-effort `comm.read(ids=[...])`, and ADR-057 superseded the
issue's original namespace and legacy-recipient acceptance assumptions with
actor-addressed eligibility, attribution-only namespaces, and the accepted
fail-open rule for rows without `to_actor`. The additive work here is the
canonical `comm.mark_read` name and its `atomic=true` mode; the released
validation and compatibility contracts remain unchanged.

Over MCP: `request(ops="comm.send(to=\"lambda:leo\", content=\"PR #372 is ready for review\")")`.

## Where this sits

`khive-pack-comm` sits alongside `khive-pack-gtd`, `khive-pack-memory`, and
`khive-pack-schedule` in the pack layer, depending on `khive-pack-kg` for the
note substrate. The schedule pack's `schedule.remind` verb requires the registered `comm.send`
delivery capability at creation time; the schedule pack itself requires only `kg`.
Both register into `khive-runtime`'s `VerbRegistry`, consumed by `khive-mcp`.
Governing ADRs:
[ADR-040](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-040-communication-and-schedule-packs.md) (communication and schedule packs),
[ADR-057](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-057-comm-actor-addressed-delivery.md) (actor-addressed delivery),
built on [ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md) (pack standard).

## License

Apache-2.0.
