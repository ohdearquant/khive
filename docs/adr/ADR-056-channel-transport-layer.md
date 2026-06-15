# ADR-056: Channel Transport Layer -- `khive-channel` and External Messaging Adapters

**Status**: Proposed\
**Date**: 2026-06-14\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-017 (Pack Standard), ADR-018 (Authorization Gate), ADR-040 (Communication
and Schedule Packs), ADR-053 (ActorStore / SessionStore -- extends ADR-018's actor model)\
**Related issues**: #112 (khive-channel umbrella), #113 (Telegram adapter)

## Context

The autonomous build loop blocks regularly on the maintainer. HC-7 merge approvals and ADR
decisions require a human judgment before work continues. The only current path to unblock is the
maintainer opening a new session.

`khive-channel` adds a bidirectional external messaging transport. The agent sends to the
maintainer on a real channel (Telegram first). The maintainer's reply arrives in `comm.inbox`,
surfaced like any local message. Agents read and reply through the existing verbs with no API
changes.

### Source-code constraints that shape this design

**`dual_write_message` is `pub(crate)`** (`khive-pack-comm/src/message.rs:71`). It is
unreachable from any crate outside `khive-pack-comm`. It always writes both an outbound and an
inbound copy on every call (lines 104, 161). There is no `direction` parameter. No public path
exists for a sibling crate to write a single inbound note via this function.

**The auth gate fires at exactly one site**: `VerbRegistry::dispatch`
(`khive-runtime/src/pack.rs:678`). Any write path that calls `KhiveRuntime::create_note`
directly, without routing through `dispatch`, bypasses the gate. This reintroduces the gap
ADR-053 was written to close.

**`VerbRegistry::dispatch` takes no external token and mints its own.** The signature is
`pub async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError>`
(pack.rs:657). The registry extracts the namespace from `params["namespace"]` or falls back to
`self.default_namespace` (pack.rs:664-668), then mints a `NamespaceToken` internally
(pack.rs:750). After minting, it strips `"namespace"` from `params` before forwarding to the
pack handler -- UNLESS the handler's own `HandlerDef.params` list declares `"namespace"` as a
named `ParamDef` (pack.rs:767-778; the brain pack uses this for targeting a different namespace
as a business argument).

**`VerbRegistry::dispatch` namespace determines which inbox the note lands in.** `comm.inbox`
filters strictly by `token.namespace().as_str()` (handlers.rs:131), which is derived from the
namespace the registry extracted from `params["namespace"]` at dispatch time. The server's
`default_namespace` is `"local"` (server.rs:251, config.rs:228). If the ingest dispatch omits
`"namespace"` or passes the wrong namespace, the inbound note is written into `"local"` (or
whatever the server default is) and is silently invisible in the agent's inbox. This is the
critical namespace alignment requirement (see §5a).

**`KhiveRuntime::authorize` is public** (`runtime.rs:308`, exported via `lib.rs:62`). It calls
the configured gate and, on success, calls `NamespaceToken::mint_authorized` internally (which
is `pub(crate)`). Its only useful role for the channel layer is as a startup pre-flight check
(fail fast if the gate denies the ingest namespace before any polling begins).

**`CommPack` cannot gain a new field post-construction.** `PackFactory::create(runtime)`
(`khive-pack-comm/src/pack.rs`, single-arg factory) accepts only a `KhiveRuntime`. Packs are
stored as `Box<dyn PackRuntime>` with no injection point after construction
(`khive-runtime/src/pack.rs:267, :576`).

**`thread_id` must be a 36-character hyphenated UUID everywhere.** `comm.send` rejects
non-UUID thread_ids (handlers.rs:42-48). `comm.reply` enforces the same (handlers.rs:246-251).
`comm.thread` filters on `len == 36 && parse::<Uuid>()` (handlers.rs:383-390). A Telegram
external message id such as `"tg:12345678:99"` cannot be used as a `thread_id`.

## Decision

### 1. Crate structure: a plain platform crate, not a Pack

`khive-channel` is a plain Rust crate at the platform layer. It does not implement the `Pack`
trait and contributes no verbs, note kinds, entity kinds, or edge endpoint rules to the
`VerbRegistry`. The comm-pack verb surface is entirely unchanged.

`khive-channel-telegram` is a sibling crate at the same layer. Each adapter crate implements
the `Channel` trait from `khive-channel`.

Monorepo placement:

```
crates/
  khive-channel/           -- Channel trait, ChannelEnvelope, ChannelRegistry
  khive-channel-telegram/  -- TelegramChannel: implements Channel
```

`khive-channel` depends on `khive-runtime` for `KhiveRuntime` and `VerbRegistry`. It does not
depend on `khive-pack-comm`.

`CommPack` construction and `PackFactory` wiring are unchanged.

### 2. The `Channel` trait

```rust
// crates/khive-channel/src/lib.rs

#[async_trait::async_trait]
pub trait Channel: Send + Sync + std::fmt::Debug {
    /// Stable short identifier: "telegram", "email".
    fn kind(&self) -> &'static str;

    /// Returns false when required env vars are absent; adapter is silently bypassed.
    fn is_configured(&self) -> bool;

    /// Send an outbound envelope. Returns the external message id on success.
    async fn send(&self, envelope: &ChannelEnvelope) -> Result<String, ChannelError>;

    /// Return new inbound envelopes since the last successful call.
    /// Adapters advance their own offset/cursor; repeated calls are idempotent.
    async fn poll(&self) -> Result<Vec<ChannelEnvelope>, ChannelError>;
}

pub enum ChannelError {
    NotConfigured,
    Transport { kind: &'static str, message: String },
    UnauthorizedSender { external_id: String },
    ApiError { code: u32, message: String },
}
```

### 3. The normalized envelope

```rust
pub struct ChannelEnvelope {
    /// Logical sender address. For inbound, the maintainer's logical address (see §8).
    pub from: String,
    /// Logical recipient namespace.
    pub to: String,
    /// Message body, plain text.
    pub body: String,
    /// "telegram", "email". Stored in properties.channel_kind on the note.
    pub channel_kind: String,
    /// Transport-assigned id. For Telegram: "tg:{chat_id}:{message_id}".
    /// Stored in properties.external_id. Used for dedup and thread resolution.
    pub external_id: String,
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// External id of the message this is replying to (NOT a UUID).
    /// The ingest loop resolves this to a UUID thread_id via DB lookup before writing.
    /// See §5b for the two-step resolution algorithm.
    pub correlation_external_id: Option<String>,
}
```

Note property mapping:

| ChannelEnvelope field     | Note property / column                            |
| ------------------------- | ------------------------------------------------- |
| `from`                    | `properties.from`                                 |
| `to`                      | `properties.to`                                   |
| `body`                    | `content`                                         |
| `channel_kind`            | `properties.channel_kind` (new)                   |
| `external_id`             | `properties.external_id` (new)                    |
| `timestamp`               | `properties.sent_at`                              |
| `correlation_external_id` | resolved to UUID `properties.thread_id` (see §5b) |

`channel_kind` and `external_id` are new optional keys written into the notes table's free-form
`TEXT` JSON properties column (`notes-ddl.sql:14`). No schema migration is required; the column
is untyped and existing notes without these fields are unaffected.

### 4. Channel registry

```rust
pub struct ChannelRegistry {
    channels: Vec<Arc<dyn Channel>>,
}

impl ChannelRegistry {
    /// Send to the first configured channel that accepts the envelope.
    /// Returns Ok(None) when no channel is configured; does not error.
    pub async fn send_outbound(
        &self,
        envelope: &ChannelEnvelope,
    ) -> Result<Option<String>, ChannelError>;

    /// Poll all configured channels. Each channel's errors are logged individually.
    pub async fn poll_all(&self) -> Vec<ChannelEnvelope>;
}
```

### 5. Comm-pack integration: the `comm.ingest` verb

The channel layer integrates with the comm store through the single public gated dispatch path:
`VerbRegistry::dispatch`. It does not call `dual_write_message` (unreachable from outside
`khive-pack-comm`) and does not call `create_note` directly (bypasses the gate).

The mechanism is a new `Visibility::Subhandler` verb `comm.ingest` added to `khive-pack-comm`.
`CommPack` construction is unchanged. The verb is registered in `COMM_HANDLERS` in `vocab.rs`
(array length 5 to 6) and a dispatch match arm is added at `khive-pack-comm/src/pack.rs:87`
alongside the existing five arms.

#### 5a. `comm.ingest` verb definition

`Visibility::Subhandler` verbs are not exposed on the MCP wire and do not appear in
`tools/list`. They are callable only through `VerbRegistry::dispatch`. The gate at `pack.rs:678`
fires for every call, satisfying the ADR-018 single-dispatch-site invariant.

**Namespace alignment (critical).** `VerbRegistry::dispatch` extracts the target namespace from
`params["namespace"]` or falls back to `self.default_namespace` (`"local"` in the MCP server,
config.rs:228, server.rs:251). After minting the token it strips `"namespace"` from `params`
before forwarding to the handler -- UNLESS the handler's `HandlerDef.params` list contains a
`ParamDef` with `name == "namespace"` (pack.rs:767-778). `comm.inbox` filters messages by
`token.namespace()` (handlers.rs:131). If the namespace written at ingest differs from the
namespace checked at inbox time, the note is written but never appears. Therefore:

1. `comm.ingest` MUST declare `"namespace"` as a named `ParamDef` in its `HandlerDef`. This
   makes dispatch forward it to the handler rather than strip it.
2. The ingest loop MUST always pass `"namespace": "<target_agent_namespace>"` (e.g.,
   `"lambda:khive"`) explicitly in every `comm.ingest` dispatch call.

Params:

| Param          | Type   | Required | Description                                                                                                                                  |
| -------------- | ------ | -------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `namespace`    | string | yes      | Target agent namespace (e.g., `"lambda:khive"`). Declared as `ParamDef` so dispatch forwards rather than strips it. Must match `comm.inbox`. |
| `from`         | string | yes      | Sender address. Preserved as channel-prefixed form (see open question §OQ-1).                                                                |
| `to`           | string | yes      | Recipient logical address.                                                                                                                   |
| `content`      | string | yes      | Message body.                                                                                                                                |
| `subject`      | string | no       | Optional subject line.                                                                                                                       |
| `thread_id`    | string | no       | 36-char UUID; supplied after thread resolution (§5b).                                                                                        |
| `channel_kind` | string | no       | `"telegram"`, `"email"`. Stored in `properties.channel_kind`.                                                                                |
| `external_id`  | string | no       | Transport id. Stored in `properties.external_id`; primary dedup key.                                                                         |
| `sent_at`      | string | no       | RFC 3339; defaults to now.                                                                                                                   |

The handler writes exactly one `message` note with `direction=inbound` into the namespace from
the `namespace` param. Before writing, the handler queries
`json_extract(properties,'$.external_id')` for a match; if found, it returns early with no
write (mandatory primary dedup; see §10).

#### 5b. The ingest loop and thread_id resolution

The background polling loop runs in `kkernel`. For each returned `ChannelEnvelope`:

**Step 1 -- Dedup check (mandatory primary)**: query notes where `kind = 'message'` and
`json_extract(properties, '$.external_id') = envelope.external_id`. If a match exists, skip
this envelope and move to the next. Do not dispatch.

**Step 2 -- Thread resolution**: if `correlation_external_id` is `Some(ext_id)`, query notes
where `kind = 'message'` and `json_extract(properties, '$.external_id') = ext_id`. If found,
read that note's `properties.thread_id` (a 36-char UUID written by the original outbound
dispatch). This UUID is the `thread_id` param for `comm.ingest`. If not found, omit `thread_id`
(the ingest note becomes a new thread root).

**Step 3 -- Dispatch**: call `verb_registry.dispatch("comm.ingest", params_json)`. The gate
fires at `pack.rs:678`. The handler's internal dedup check fires again before the write (step 1
is an optimization; the handler check is the authoritative guard).

Steps 1 and 2 use the `idx_comm_message_external_id` index (§11).

#### 5c. Outbound path and `external_id` write-back

The binary calls `channel_registry.send_outbound` after a successful
`VerbRegistry::dispatch("comm.send", ...)` for messages directed to a configured external
target. The external id returned by `send_outbound` is written back to the outbound note's
`properties.external_id` via `VerbRegistry::dispatch("update", ...)` with a properties patch.

The `update` verb's `update_note` implementation uses `merge_properties` with `PreferFrom`
policy (curation.rs:503-507; confirmed by test
`update_entity_properties_merges_preserving_existing_keys` at curation.rs:1858). Patching
`{"external_id": "tg:...", "channel_kind": "telegram"}` into the outbound note's properties
preserves the existing `direction`, `thread_id`, `read`, `sent_at`, `from`, `to` keys -- it
does not replace the whole properties column. The write-back is safe.

The mapping from a logical `to` address to a channel adapter is via configuration (DECISIONS #3,
v1: implicit from env var presence).

#### 5d. Reply routing

When the agent calls `comm.reply(id=<uuid>)`, `handle_reply` writes the reply note locally as
today. The binary observes the dispatch result, reads `properties.channel_kind` from the
original inbound note, and calls `channel_registry.send_outbound` for the reply. The comm-pack
handler is unchanged.

### 6. The polling loop lives in the binary, not in a pack

The polling loop is a `tokio::task::spawn` inside `kkernel`'s startup sequence, after the
`VerbRegistry` is built and before the MCP server begins accepting connections.

**What the loop holds:**

- `Arc<ChannelRegistry>` for polling.
- `Arc<VerbRegistry>` for dispatching `comm.ingest`.
- A `tokio::CancellationToken` for clean shutdown.

The loop does NOT hold a `NamespaceToken`. `VerbRegistry::dispatch` takes no external token
(pack.rs:657 signature: `pub async fn dispatch(&self, verb: &str, params: Value)`). It extracts
the namespace from `params["namespace"]` and mints its own token internally (pack.rs:750). A
token obtained from `KhiveRuntime::authorize` is never consumed by `dispatch` and cannot serve
as a per-dispatch credential.

**Startup pre-flight (gate check only):** At startup, before the polling task is spawned, the
binary calls `KhiveRuntime::authorize(ingest_namespace)` once as a pre-flight check. If the
configured gate denies the ingest namespace, the binary logs an error and does not start the
polling loop (fail-fast before any polling begins). With the default `AllowAllGate`, this always
succeeds. The token returned by `authorize` is discarded after the check.

**Per-dispatch namespace:** Every `comm.ingest` dispatch call includes `"namespace":
"<target_agent_namespace>"` in its params. The registry extracts this at dispatch time
(pack.rs:664-668), uses it to mint a fresh token internally, and -- because `comm.ingest`
declares `"namespace"` as a `ParamDef` -- forwards the field to the handler (pack.rs:767-778),
which writes the note into the correct namespace. This is what makes the inbound note visible in
`comm.inbox` for the right agent.

The loop sleeps a configurable interval (default 5 seconds) between `poll_all` calls.

### 7. Inbound polling vs webhook

Long-poll is the default. The embedded OSS deployment runs with no routable public URL.
Webhooks require one. Long-poll requires only an outbound HTTPS connection to the Bot API.

`Channel::poll()` is adapter-defined. A webhook adapter can buffer received updates and drain
the buffer on each `poll()` call. Webhook support is deferred until a deployment with a public
URL exists.

### 8. Inbound authentication

Each adapter validates the external sender identity on every update before returning an
envelope. Updates from unauthorized senders return `ChannelError::UnauthorizedSender` and are
dropped. No note is written. This mirrors the `isSenderAllowed` pattern from the openclaw
reference (`bot-access.ts:46-66`), simplified for the single-maintainer case.

For Telegram, authentication is by numeric `chat.id` (stable across username changes),
configured via env var. Username matching is a fallback only, as usernames can be reassigned.
The maintainer identity is never stored in the KG.

The exact identity model (which identity claim is authoritative and how it ties to the ADR-053
actor model) is an open question requiring maintainer sign-off before implementation (see
§OQ-2).

### 9. No secrets in the store

All credentials are loaded from env vars at adapter construction. They are never written to any
note property, KG entity, or content verb. `ChannelEnvelope` carries no credential fields.
Credential values in DEBUG logs are masked as `{first6}...[N chars]`.

### 10. Dedup and idempotency

Two layers, applied in order:

**Primary (mandatory, durable)**: before each `comm.ingest` dispatch, the ingest loop (and the
handler itself) queries `json_extract(properties,'$.external_id')` for a match. A match skips
the write. This check is DB-backed and survives restarts. Telegram re-delivers un-acknowledged
updates after a crash; the DB check catches every duplicate regardless of how long the process
was down.

**Secondary (optimization, in-memory)**: the adapter maintains a dedup cache keyed by transport
update id. Parameters from the openclaw reference (`bot-updates.ts:3-5`): TTL 5 minutes, max
2000 entries. This avoids the DB round-trip for the common case of duplicate delivery within
the same process lifetime. The cache starts empty on restart; the DB check covers all cases
the cache misses.

### 11. Expression index on `external_id`

The dedup check (step 1) and thread resolution (step 2) in §5b both query
`json_extract(properties,'$.external_id')`. Without an index, both are full scans of the notes
table.

`COMM_SCHEMA_PLAN_STMTS` in `vocab.rs` gains a third entry:

```sql
CREATE INDEX IF NOT EXISTS idx_comm_message_external_id
    ON notes(namespace, kind, json_extract(properties, '$.external_id'))
    WHERE deleted_at IS NULL
```

This follows the exact pattern of the existing `idx_comm_message_direction` and
`idx_comm_message_thread` (vocab.rs:16-24). It applies via `CREATE INDEX IF NOT EXISTS` at
boot, idempotently, requiring no migration file.

### 12. Rate limiting

The ingest loop enforces a configurable minimum inter-poll interval (default 5 seconds) via
`tokio::time::sleep` between `poll_all` calls.

### 13. Alternatives considered

| Alternative                                           | Why rejected                                                                                                                                                                          |
| ----------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Make `khive-channel` a Pack                           | Packs contribute verbs, kinds, and edge rules. Channels contribute none. Transport lifecycle does not belong in the VerbRegistry.                                                     |
| Write inbound notes via direct `create_note`          | Bypasses the auth gate (ADR-018 invariant, pack.rs:678).                                                                                                                              |
| Modify `CommPack` to hold `Arc<ChannelRegistry>`      | `PackFactory::create` takes only `KhiveRuntime`; packs are `Box<dyn PackRuntime>` with no injection point post-construction.                                                          |
| Route inbound through existing `comm.send`            | `handle_send` derives `from` from `token.namespace()` (handlers.rs:50). External sender address cannot be supplied. `dual_write_message` is `pub(crate)` and always writes two notes. |
| Use `correlation_external_id` directly as `thread_id` | `thread_id` must be a 36-char UUID (handlers.rs:42-48, :246-251, :383-390). Telegram message ids are integers. Two-step DB resolution is required.                                    |
| Skip the expression index for v1                      | The dedup check is on the mandatory critical path for every inbound message. Full-scan risk under load.                                                                               |
| In-memory LRU as primary dedup                        | LRU starts empty on restart, exactly when Telegram re-delivers un-acked updates. DB-first is required.                                                                                |

## Open Questions (pending maintainer ruling)

Both items below block the Telegram adapter implementation (#113). Neither can be resolved
unilaterally; they require Ocean's explicit sign-off before an implementation PR is opened.

### OQ-1: Inbound `from` field format

**Source**: DECISIONS #8 (critic finding M2).

The ingest loop sets the `from` field on each inbound note. Two options:

- **Option A** (current design position): preserve the channel-prefixed external form, e.g.,
  `"tg:12345678"`. Store `properties.channel_kind` on the note so the agent can see the origin.
  `comm.inbox` shows the external id, not a logical name.

- **Option B**: resolve `from` to the logical address (`"ocean"`) before writing. The raw
  external id is visible only via `properties.external_id`. `comm.inbox` shows `"ocean"` as the
  sender. Simpler for agents; hides the channel origin in the displayed sender field.

The current implementation uses Option A. Option B is lossy; the channel-prefixed form is not
recoverable from the note alone once collapsed.

**Maintainer decision needed**: confirm Option A or choose Option B.

### OQ-2: Inbound auth identity model

**Source**: DECISIONS CLAIM #5.

§8 specifies that Telegram inbound auth is by numeric `chat.id` configured via env var. The
open question is how this identity claim ties to the ADR-053 actor model: specifically, whether
the ingest loop should present an authenticated `ActorRef` to `VerbRegistry::dispatch` (rather
than the anonymous default), and if so what `actor.kind` and `actor.id` values are canonical
for the maintainer's Telegram identity.

This is a security boundary question: the wrong answer could allow an unauthorized sender's
message to reach `comm.inbox` if the auth check in the adapter is bypassed.

**Maintainer decision needed**: specify the authoritative identity claim and how it maps to
`ActorRef` fields, or confirm that numeric `chat.id` env-var check in the adapter is sufficient
and that the anonymous actor default for `dispatch` is acceptable for v1.

## Consequences

- `comm.inbox`, `comm.read`, `comm.reply`, and `comm.thread` are unchanged for agents.
- A new `comm.ingest` `Visibility::Subhandler` verb is added to `khive-pack-comm` (`vocab.rs`
  - handler dispatch). It is not visible on the MCP wire.
- `COMM_SCHEMA_PLAN_STMTS` gains a third index (`idx_comm_message_external_id`). Applied
  idempotently at boot via `CREATE INDEX IF NOT EXISTS`. No migration file required.
- The polling loop runs in `kkernel` as a `tokio::task`, holding `Arc<VerbRegistry>` and
  `Arc<ChannelRegistry>` (no `NamespaceToken` -- `dispatch` mints its own per call). It is
  cancelled on shutdown.
- Every inbound write passes through `VerbRegistry::dispatch:678`, satisfying the ADR-018
  single-dispatch-site invariant. The registry mints the `NamespaceToken` internally from the
  `"namespace"` param on each call.
- No credentials appear in any note, entity, or KG store.
- `CommPack` construction and `PackFactory` wiring are unchanged.
- `khive-channel` and `khive-channel-telegram` are new crates at the platform layer (blocked on
  OQ-1 and OQ-2 above).

## Related ADRs

- ADR-017: Pack Standard -- the Pack trait this ADR explicitly decides not to implement.
- ADR-018: Authorization Gate (original) -- defines the single-dispatch-site invariant and the
  `Gate` trait. Every ingest write passes through `VerbRegistry::dispatch:678`.
- ADR-040: Communication and Schedule Packs -- the `comm.*` verb surface and `message` note kind.
- ADR-053: Authorization Gate (ActorStore / SessionStore extension) -- extends ADR-018's actor
  model. `KhiveRuntime::authorize` (the public gate-checked door) and
  `NamespaceToken::mint_authorized` (`pub(crate)`, unreachable externally) are introduced here.
  OQ-2 above requires a ruling on how the ingest loop's identity interacts with this model.
- ADR-028: Pack-Scoped Backends -- offset/cursor persistence pattern for channel adapters.
