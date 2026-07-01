# khive-channel

Transport abstraction for outbound and inbound message delivery — the `Channel`
trait, `ChannelEnvelope`, and `ChannelRegistry` that concrete adapters (email,
Telegram, ...) implement against.

## Usage

```rust
use khive_channel::{Channel, ChannelEnvelope, ChannelRegistry};

let envelope = ChannelEnvelope::new("email:alice@example.com", "email:bob@example.com", "hello")
    .with_subject("Test")
    .with_external_id("<msg1@example.com>");

let mut registry = ChannelRegistry::new();
// registry.register(Arc::new(my_channel_impl)); // my_channel_impl: impl Channel
let email_channel = registry.get("email");
```

`ChannelEnvelope` carries `from`/`to`/`content`/`subject` plus dedup and
threading metadata: `external_id` (adapter-derived dedup key, e.g.
`imap:{host}:{uidvalidity}:{uid}` for email), `correlation_external_id` (thread
correlation, e.g. an `In-Reply-To` header), and `message_id` (RFC 822
Message-ID to set on outbound mail).

## The `Channel` trait

```rust
#[async_trait::async_trait]
pub trait Channel: Send + Sync + 'static {
    fn kind(&self) -> &'static str;
    fn is_configured(&self) -> bool { true }
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), khive_channel::ChannelError>;
    async fn poll(&self, since: chrono::DateTime<chrono::Utc>) -> Result<Vec<ChannelEnvelope>, khive_channel::ChannelError>;
}
```

`Channel` deliberately does not require `Debug` — concrete adapters hold
credentials, and a derived `Debug` impl would risk leaking them into logs.
Deduplication of polled envelopes is the caller's responsibility: the MCP
server's `comm.ingest` verb performs an `INSERT OR IGNORE` against a unique
index on `external_id`, so adapters do not need to dedup themselves. Adapters
should apply a best-effort server-side filter on `since` to avoid fetching
large backlogs.

## Where this sits

`khive-channel` has no `khive-*` dependencies — it is a leaf crate that concrete
transport adapters (e.g. `khive-channel-email`) depend on to implement
`Channel`. The MCP server holds an `Arc<ChannelRegistry>`, polls every
registered channel in a background loop, and forwards results to `comm.ingest`.

Governing ADR:
[ADR-056](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-056-channel-transport-layer.md)
(channel transport layer and external messaging adapters).

## License

Apache-2.0.
