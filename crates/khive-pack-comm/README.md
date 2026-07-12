# khive-pack-comm

The communication pack for khive — inter-agent messaging (`send`, `inbox`,
`read`, `reply`, `thread`) over a dedicated `message` note kind, with
dual-write, actor-addressed delivery.

## Verbs

| Verb          | What it does                                                       |
| ------------- | ------------------------------------------------------------------ |
| `comm.send`   | Send a message, optionally threaded                                |
| `comm.inbox`  | List inbound messages for the caller (filter: unread / read / all) |
| `comm.read`   | Mark an inbound message as read                                    |
| `comm.reply`  | Reply to a message, preserving thread linkage                      |
| `comm.thread` | Retrieve all messages in a conversation thread, chronologically    |
| `comm.probe`  | Read-only poll for new inbound message metadata and a stale unread count |

A sixth handler, `comm.ingest`, is `Visibility::Subhandler` — it lets an
out-of-band channel adapter (email, Telegram, etc.) write an inbound message
directly, deduplicated by `external_id`, but it is not callable on the MCP wire.

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
**inbound** copy (`direction=inbound`), linked by `outbound_ref`. If the
inbound write fails, the outbound note is deleted before the error is
returned — the pair is atomic.

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
outbound note's own UUID, patched into both copies, so `comm.thread` finds
every reply regardless of which copy it answered.

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
```

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
