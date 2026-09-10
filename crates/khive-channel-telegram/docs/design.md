# khive-channel-telegram Design

## Purpose

`khive-channel-telegram` adapts the transport-neutral `khive-channel::Channel` contract to the
Telegram Bot API. It uses the `sendMessage` and long-polling `getUpdates` HTTPS/JSON methods
directly, without a Telegram SDK.

## Key types

- `TelegramChannelConfig` is the environment-only configuration: bot token, authorized maintainer
  chat id, routable maintainer slug, and inbound namespace.
- `TelegramChannel` implements `Channel`, converts authenticated text updates to
  `ChannelEnvelope`, and owns the confirmed and pending `getUpdates` offsets.
- The crate-private `TelegramConnector` trait isolates the two Bot API calls. The live
  `reqwest` implementation is replaced by deterministic mocks in unit tests.
- `TelegramUpdate`, `TelegramMessage`, and `TelegramChat` model only the response fields the
  adapter consumes; other Telegram JSON fields are intentionally ignored.

## Configuration

The adapter requires `KHIVE_TELEGRAM_BOT_TOKEN` and a numeric
`KHIVE_TELEGRAM_MAINTAINER_CHAT_ID`. `KHIVE_TELEGRAM_MAINTAINER_SLUG` defaults to `maintainer`, and
`KHIVE_TELEGRAM_INGEST_NAMESPACE` defaults to `local`. No filesystem configuration is read.

## Invariants

- V1 is a single-maintainer channel. Inbound updates from any other chat id and updates without
  text are dropped without creating an envelope. Outbound delivery accepts the configured
  maintainer address in two spellings — `telegram:<maintainer_slug>` or the bare
  `<maintainer_slug>` (the kind prefix is stripped when present, not required) — and every other
  address returns a permanent envelope error and is recorded as a terminal outbound failure,
  never redirected to the maintainer.
- Bot API 408, 429, and 5xx responses are transient delivery failures. Other 4xx responses are
  permanent for the individual outbound note. The shared outbox loop durably backs off transient
  failures and terminally records permanent ones.
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
