# khive-channel Design

## Purpose

`khive-channel` is the transport-neutral boundary between khive's communication runtime and
concrete message adapters. It defines how outbound messages are delivered, how inbound messages
are polled, and how configured adapters are registered. Persistence, authorization policy, and
`comm.ingest` dispatch remain outside this crate.

## Key types

- `Channel` is the asynchronous adapter contract. Implementations expose a stable transport
  `kind`, an optional per-credential `slug`, outbound `send`, and inbound `poll`/`poll_page`.
- `ChannelEnvelope` carries normalized addresses, content, transport metadata, deduplication and
  thread-correlation identifiers, and email reply headers across the adapter/runtime boundary.
- `QuarantineReplay` retains byte-exact inbound transport content in memory until durable handling
  decides whether it must be quarantined.
- `ChannelCheckpoint`, `StoredChannelCheckpoint`, and `ChannelPollPage` represent durable,
  transport-neutral high-water progress for checkpoint-aware polling.
- `ChannelRegistry` owns adapters by their `(kind, slug)` identity and exposes iteration and exact
  composite lookup.
- `ChannelError` is the shared configuration, transport, authentication, authorization, and
  envelope-validation error taxonomy.

## Invariants

- A channel's durable identity is the composite `(kind, slug)`. Distinct credentials of the same
  transport kind must coexist; registering the same composite identity replaces the old adapter.
- `ChannelRegistry::get(kind)` is only deterministic for the common one-credential-per-kind case.
  Code that selects among multiple credentials must use `get_by_slug`.
- The `Channel` trait intentionally does not require `Debug`: concrete adapters may contain
  credentials that must not become log output through a derived implementation.
- Adapters provide stable external deduplication keys when their transport can do so. Atomic
  deduplication belongs to `comm.ingest`, not to an adapter's polling loop.
- A checkpoint is advanced only after every envelope represented by its page has been durably
  handled. The default `poll_page` is stateless and preserves compatibility with adapters that
  implement only `poll`.
- Exact quarantine bytes never travel through serialized `ChannelEnvelope` metadata. The replay
  field is skipped by serde, and its `Debug` output reveals only the byte count and notification
  target.
- `new_thread_correlation_id` returns a canonical hyphenated UUID suitable for external message
  headers; it does not itself create or resolve an internal thread.

## Runtime relationship

The MCP server owns an `Arc<ChannelRegistry>`, polls its adapters, and forwards accepted envelopes
to the communication pack. Concrete transports such as `khive-channel-email` and
`khive-channel-telegram` depend on this crate; this crate does not depend on any one transport.
