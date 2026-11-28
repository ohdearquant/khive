# khive-channel-telegram Design

## Purpose

`khive-channel-telegram` adapts the transport-neutral `khive-channel::Channel` contract to the
Telegram Bot API. It uses the `sendMessage` and long-polling `getUpdates` HTTPS/JSON methods
directly, without a Telegram SDK.

## Key types

- `TelegramChannelConfig` is the environment-only configuration: bot token, maintainer chat id,
  authorized sender account id, routable maintainer slug, and inbound namespace.
- `TelegramChannel` implements `Channel`, converts authenticated text updates to
  `ChannelEnvelope`, and owns the confirmed and pending `getUpdates` offsets.
- The crate-private `TelegramConnector` trait isolates the two Bot API calls. The live
  `reqwest` implementation is replaced by deterministic mocks in unit tests.
- `TelegramUpdate`, `TelegramMessage`, and `TelegramChat` model only the response fields the
  adapter consumes; other Telegram JSON fields are intentionally ignored.

## Configuration

The adapter requires `KHIVE_TELEGRAM_BOT_TOKEN` and a numeric
`KHIVE_TELEGRAM_MAINTAINER_CHAT_ID`. `KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID` identifies the inbound
Telegram account; it defaults to the chat id only for a positive private-chat id and is required
for a non-positive group or channel id. `KHIVE_TELEGRAM_MAINTAINER_SLUG` defaults to `maintainer`,
and `KHIVE_TELEGRAM_INGEST_NAMESPACE` defaults to `local`.
`KHIVE_TELEGRAM_DEFAULT_ACTOR` defaults to the isolated `telegram:bot` recipient.
Set the latter to `local` only when the runtime access policy protects the local
inbox from anonymous readers. No filesystem configuration is read.

## Invariants

- V1 is a single-maintainer channel. Inbound updates must match both the configured chat id and
  the authorized `message.from.id`; updates without a sender or text are dropped without creating
  an envelope. Outbound delivery accepts the configured
  maintainer address in two spellings — `telegram:<maintainer_slug>` or the bare
  `<maintainer_slug>` (the kind prefix is stripped when present, not required) — and every other
  address returns a permanent envelope error and is recorded as a terminal outbound failure,
  never redirected to the maintainer.
- Bot API 408, 429, and 5xx responses are transient delivery failures. Other 4xx responses are
  permanent for the individual outbound note. The shared outbox loop durably backs off transient
  failures and terminally records permanent ones. A 429 carrying `retry_after` also pauses the
  Telegram outbound pass until that interval has elapsed.
- Outbound Telegram delivery is at-least-once. `sendMessage` has no idempotency key or receiver-side
  deduplication: if Telegram accepts a send but its response is lost, or the daemon stops before
  recording delivery, the pending note can be sent again and appear twice in the chat.
- Bot tokens must not appear in diagnostics. `TelegramChannelConfig` has a manual `Debug`
  implementation that masks the token, and connector errors remove request URLs containing it.
- `poll` uses Telegram's offset watermark and ignores its timestamp argument. A fetched batch only
  records a `pending_offset`; `commit_offset` advances the confirmed offset after every authorized
  envelope from that batch has durably passed through `comm.ingest`.
- Until `commit_offset` runs, the next poll repeats the previous offset. This is intentional
  at-least-once delivery, made safe by the stable external id `tg:<chat_id>:<update_id>` and the
  communication store's unique deduplication index.
- The in-memory offset is not restart-durable. Restart recovery relies on Telegram redelivery and
  the durable external-id deduplication boundary.
- Production uses a 25-second Bot API long poll with a longer client timeout. Tests inject a
  connector or test-only base URL and do not make live Telegram calls.
- The daemon selects an in-flight poll against its shutdown token. Cancellation drops the
  request without acknowledging an offset, so the next poll requests the same uncommitted
  updates. Once a fetched batch enters ingestion, the daemon finishes that batch's existing
  ingest/offset discipline before observing shutdown on the next cycle.

## Inbound poll observations

Every completed `Ok` poll is a transport success, including an empty page,
a page containing only filtered updates, and a page whose ingestion is held.
The coordinator captures the poll completion time, handles the selected batch,
and reports a best-effort `comm.heartbeat` with that time and its actual hold and
quarantine counts. The confirmed offset still advances only when every envelope
is durably released. Persisting a heartbeat does not acknowledge a held batch.

A completed transport error records a failure before the existing five-second
retry wait. It retains the last successful poll time. Health reports whole
seconds since that success as `no_success_age_secs`, even through failures; a
channel with no successful poll reports null. Cancellation of a pending poll
records no outcome, while an already selected batch finishes its observation
and existing offset decision. A failed heartbeat write is logged and does not
hold a released batch or introduce a retry, success delay, or lifecycle event.

Heartbeat dispatch uses `khive_pack_comm::CHANNEL_HEALTH_NAMESPACE` on the
backend assigned to the comm pack, independently of the message ingest
namespace. Kind and configured channel slug identify the operational row.
Telegram supplies no `poll_interval_secs`: its five-second error retry and
25-second long-poll window are not a nominal success cadence. New Telegram rows
therefore report `stalled: null`; `no_success_age_secs` is the elapsed-time signal.
This adds no durable offset, notices, or automatic recovery action.

### Configured identity and historical quarantine rows

The real Telegram adapter uses its configured `maintainer_slug` consistently for
inbound deduplication, quarantine, heartbeat and registered outbox routing.
`from_env` defaults a missing, empty, whitespace-only or non-Unicode slug to
`maintainer`. A manually constructed empty or whitespace-only slug instead falls
back to the channel kind, `telegram`; nonblank configured values are preserved.
An existing outbound message without a `channel_slug` still routes through the
sole configured Telegram adapter. Normal comm producers do not stamp the old
kind-only `telegram` slug; manually written backend rows are outside that
producer guarantee.

Normal polling also ages out expired quarantine rows explicitly named
`telegram` from before the configured-slug upgrade. In the original ingest
namespace, each poll retains the configured-slug and historical slugless cleanup
passes, then selects the exact old kind-only slug when it differs from the
configured slug. Each selector keeps the existing maximum of 128 rows and its
expiry rules; the additional named selector removes only expired live rows.
Unexpired rows, named rows without an expiry, other namespaced rows and unrelated
slugs are untouched. A cleanup error holds the poll with the existing retry and
records no completed-poll heartbeat. No manual maintenance step is required.
