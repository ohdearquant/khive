# ADR-056: Channel Transport Layer -- `khive-channel` and External Messaging Adapters

**Status**: Accepted (amended 2026-07-02 -- inbound authentication hardening; see
[§Amendment 2026-07-02](#amendment-2026-07-02----inbound-authentication-hardening))\
**Date**: 2026-06-14 (amended 2026-07-02)\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-017 (Pack Standard), ADR-018 (Authorization Gate), ADR-040 (Communication
and Schedule Packs), ADR-053 (ActorStore / SessionStore -- extends ADR-018's actor model)\
**Related issues**: #112 (khive-channel umbrella), #113 (Telegram adapter), #114 (email adapter),
#448 (inbound header spoofing -- resolved by this amendment)

## Amendment 2026-07-02 -- Inbound authentication hardening

This amendment supersedes the original OQ-2 resolution (env-var `From:` addr-spec comparison as
the authoritative inbound check). That resolution is retained below, marked superseded, as the
historical decision.

### Motivation

The original OQ-2 model authenticates a sender by comparing the `From:` (and `Sender:`) addr-spec
against `KHIVE_EMAIL_MAINTAINER_ADDRESS`. RFC 5322 originator headers (`From:`, `Sender:`,
`Return-Path:`) are asserted by the sending client and are not authenticated by SMTP transport.
Any party that can reach the receiving mailbox can set `From:` to the maintainer address and pass
the allowlist. The addr-spec comparison therefore authenticates nothing on its own.

This is not a low-severity gap. An inbound `message` note attributed to the maintainer lands in a
lambda's `comm.inbox` and is surfaced by the inbox wake-up monitor with the highest trust tier a
lambda has: maintainer-directed, principal-priority instructions. A spoofed `From:` is a direct
prompt-injection vector into the autonomous loop -- an attacker who can email the ingest mailbox
can inject instructions that a lambda treats as coming from its principal. Attribution, not
delivery, is the trust boundary that must be earned.

### Decision

Inbound email attribution requires cryptographic path authentication in addition to the sender
allowlist. Attribution is granted only when both hold:

1. **Domain authentication with alignment.** The message carries an `Authentication-Results`
   header, inserted by the adapter's own trusted receiving boundary (matched by a configured
   `authserv-id`), that shows `dmarc=pass`, or equivalently at least one of SPF-pass with
   RFC 7208 envelope-from alignment to the `From:` domain or DKIM-pass with an RFC 6376 `d=`
   signing domain aligned to the `From:` domain. Alignment is mandatory: an unaligned SPF or
   DKIM pass (a pass for a domain other than the `From:` domain) does not satisfy this check.
2. **Sender allowlist.** The single `From:` addr-spec matches the maintainer allowlist, under the
   existing OQ-2 addr-spec rules (single From, `Sender:` must also match if present).

A message satisfying both is attributed to the maintainer actor identity. A message failing
either is **quarantined**: it is never attributed, never surfaced as a trusted actor, and never
triggers principal-priority handling.

### Trusted-header selection (the load-bearing detail)

`Authentication-Results` is an ordinary header and can appear multiple times, including copies
forged by the sender before the message reached the receiving server. The adapter MUST NOT trust
an arbitrary `Authentication-Results` header. It MUST:

- select only `Authentication-Results` headers whose `authserv-id` equals the configured
  `KHIVE_EMAIL_AUTHSERV_ID` (the receiving MTA's own identifier, e.g. the Exchange Online host
  that delivers to the ingest mailbox);
- when several such headers are present, use the topmost (most recently prepended by the trusted
  boundary), and ignore all `Authentication-Results` headers bearing any other `authserv-id`;
- treat the absence of any trusted-authserv-id `Authentication-Results` header as a failed
  authentication (quarantine), not as a pass;
- never derive, learn, or select the trusted `authserv-id` from message content. It comes from
  configuration only.

**Operational precondition (RFC 8601 §1.6, §5).** `Authentication-Results` carries no integrity
mechanism; the topmost-wins rule is sound only when the receiving boundary itself makes it so.
Configuring `KHIVE_EMAIL_AUTHSERV_ID` is therefore only valid for a receiving boundary that is
verified to prepend its own `Authentication-Results` on every delivered message and to remove or
rename inbound headers claiming that same `authserv-id` (the ADMD-entry scrubbing RFC 8601
requires). A deployment MUST verify this behavior against the live mailbox before enabling
attribution: send a probe message carrying a forged `Authentication-Results` with the trusted
`authserv-id` and confirm the boundary strips it or that the boundary's genuine stamp lands above
it. If the boundary does neither, attribution MUST NOT be enabled for that mailbox; all inbound
mail is quarantined until it is. The observed `authserv-id` string from a real delivered message,
not provider documentation, is the source of the configured value.

Re-verifying SPF/DKIM inside the adapter is out of scope for v1. The adapter is a downstream IMAP
consumer; the receiving MTA has already performed SPF/DKIM/DMARC at delivery and recorded the
result. The adapter's job is to trust that record only from its own boundary and to enforce
alignment, not to recompute it.

### Quarantine semantics (quarantine is itself an injection surface)

Quarantined mail may be stored so the maintainer can review what arrived, but storage must not
reintroduce the injection it prevents:

- A quarantined message is written with no `from_actor` (or an explicit anonymous/quarantine
  actor). It carries a property marking it quarantined and the reason (auth-absent,
  dmarc-fail, unaligned, or off-allowlist). It is never stamped with the maintainer actor.
- Quarantined notes MUST NOT trigger the inbox wake-up monitor's trusted-sender path and MUST be
  distinguishable by agents so their content is never treated as principal instructions. Content
  of a quarantined message is data to be shown, never a command to be followed -- the same
  instruction-source boundary the loop applies to all untrusted observed content.
- Storing quarantined mail is a configurable behavior (`KHIVE_EMAIL_QUARANTINE_STORE`, default
  on so that legitimate mail failing a transient auth check is not silently lost). When off, a
  failed message is dropped with only the IMAP UID logged, matching the prior behavior.

### Configuration additions

| Variable                       | Required | Default | Description                                                                                                   |
| ------------------------------ | -------- | ------- | ------------------------------------------------------------------------------------------------------------- |
| `KHIVE_EMAIL_AUTHSERV_ID`      | yes      | --      | The trusted receiving MTA's `authserv-id`. Only `Authentication-Results` headers bearing this id are trusted. |
| `KHIVE_EMAIL_QUARANTINE_STORE` | no       | `on`    | When `on`, unauthenticated mail is stored as an unattributed quarantined note; when `off`, it is dropped.     |

`KHIVE_EMAIL_MAINTAINER_ADDRESS` remains the sender allowlist. It stays a single addr-spec for
v1; a multi-entry allowlist is a compatible later extension.

### Scope

This amendment governs the email adapter (#114, #448). The Telegram adapter's numeric `chat.id`
authentication (§8) is a stable transport-authenticated identifier and is unaffected. The
`comm.ingest` dispatch path, the single-dispatch-site gate invariant, and the dedup model are
unchanged; the amendment adds an attribution gate in front of them and a quarantine disposition
beside them. Implementation (#448) follows this accepted revision.

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

// Note: Debug is intentionally NOT required. Concrete adapters hold credentials;
// a derived Debug impl would leak passwords in logs.
#[async_trait::async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Stable short identifier: "telegram", "email".
    fn kind(&self) -> &'static str;

    /// Returns true when this adapter has sufficient configuration to operate.
    ///
    /// The default returns true. Adapters with optional configuration may
    /// override this to report readiness without returning errors on every call.
    fn is_configured(&self) -> bool {
        true
    }

    /// Send a single outbound message.
    ///
    /// Outbound write-back (§5c) and reply routing (§5d) are deferred to a
    /// future release; this method exists so the trait surface is complete.
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError>;

    /// Poll for new inbound messages received since `since`.
    ///
    /// Returns envelopes ready to be ingested. Per-message validation errors
    /// must be logged and skipped; one bad message must not abort the batch.
    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError>;
}

pub enum ChannelError {
    Config(String),
    Transport(String),
    Auth(String),
    UnauthorizedSender(String),
    InvalidEnvelope(String),
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
the `namespace` param. Deduplication is atomic: the handler calls `try_create_note`, which uses
`INSERT OR IGNORE` against the durable unique index `idx_comm_message_external_id` (see §11).
A duplicate `external_id` results in zero rows written and a `{"ok": true, "deduplicated": true}`
response; no error is returned.

Thread resolution: when `correlation_external_id` is supplied, the handler queries for an
existing message note whose `external_id` matches that value, reads its `thread_id`, and
attaches the new note to the same thread. When no match is found, the new note becomes a new
thread root.

#### 5b. The ingest loop

The background polling loop runs in `kkernel`. For each returned `ChannelEnvelope`, it
forwards `external_id` and `correlation_external_id` from the envelope directly to
`comm.ingest` without pre-screening or pre-resolution:

**Dispatch**: call `verb_registry.dispatch("comm.ingest", params_json)` with `external_id` and
`correlation_external_id` included when present. The gate fires at the dispatch seam.
`comm.ingest` performs both dedup (via `INSERT OR IGNORE`) and thread resolution internally.

There is no pre-dispatch dedup or thread-resolution step in the loop itself; those operations
belong entirely to the `comm.ingest` handler.

#### 5c. Outbound path and `external_id` write-back (DEFERRED -- not implemented in v1)

This section describes the design intent for a future release. It is not implemented in the
current PR. No outbound polling or external_id write-back logic exists in the shipping code.

The intended design: the binary calls `channel_registry.send_outbound` after a successful
`VerbRegistry::dispatch("comm.send", ...)` for messages directed to a configured external
target. The external id returned by `send_outbound` is written back to the outbound note's
`properties.external_id` via `VerbRegistry::dispatch("update", ...)` with a properties patch.
`update_note` uses `merge_properties` with `PreferFrom` policy, preserving existing properties.

#### 5d. Reply routing (DEFERRED -- not implemented in v1)

This section describes the design intent for a future release. It is not implemented in the
current PR. When implemented: the binary observes a `comm.reply` dispatch result, reads
`properties.channel_kind` from the original inbound note, and calls
`channel_registry.send_outbound` for the reply. The comm-pack handler is unchanged.

### 6. The polling loop lives in the binary, not in a pack

The polling loop is a `tokio::task::spawn` inside `kkernel`'s startup sequence, after the
`VerbRegistry` is built and before the MCP server begins accepting connections.

**What the loop holds (v1 shipped state):**

- `Arc<ChannelRegistry>` for polling.
- `Arc<VerbRegistry>` for dispatching `comm.ingest`.

The loop does NOT hold a `NamespaceToken`. `VerbRegistry::dispatch` takes no external token
(pack.rs:657 signature: `pub async fn dispatch(&self, verb: &str, params: Value)`). It extracts
the namespace from `params["namespace"]` and mints its own token internally (pack.rs:750). A
token obtained from `KhiveRuntime::authorize` is never consumed by `dispatch` and cannot serve
as a per-dispatch credential.

The v1 loop does not hold a cancellation token; it runs until the process exits. A configurable
interval and explicit cancellation support are possible future additions.

**Startup pre-flight (gate check only):** At startup, before the polling task is spawned, the
binary calls `VerbRegistry::authorize_namespace(ingest_namespace)` once as a pre-flight check.
If the configured gate denies the ingest namespace, the binary logs an error and does not start
the polling loop (fail-closed before any polling begins). With the default `AllowAllGate`, this
always succeeds.

**Per-dispatch namespace:** Every `comm.ingest` dispatch call includes `"namespace":
"<target_agent_namespace>"` in its params. The registry extracts this at dispatch time
(pack.rs:664-668), uses it to mint a fresh token internally, and -- because `comm.ingest`
declares `"namespace"` as a `ParamDef` -- forwards the field to the handler (pack.rs:767-778),
which writes the note into the correct namespace. This is what makes the inbound note visible in
`comm.inbox` for the right agent.

The loop sleeps a fixed 5-second interval between `poll_all` calls.

### 7. Inbound polling vs webhook

Long-poll is the default. The embedded deployment runs with no routable public URL.
Webhooks require one. Long-poll requires only an outbound HTTPS connection to the Bot API.

`Channel::poll()` is adapter-defined. A webhook adapter can buffer received updates and drain
the buffer on each `poll()` call. Webhook support is deferred until a deployment with a public
URL exists.

### 8. Inbound authentication

Each adapter validates the external sender identity on every update before returning an
envelope. For Telegram, updates from unauthorized senders return
`ChannelError::UnauthorizedSender` and are dropped with no note written. This mirrors the
`isSenderAllowed` pattern from the openclaw reference (`bot-access.ts:46-66`), simplified for
the single-maintainer case.

> **Email disposition amended 2026-07-02.** For the email adapter, drop-with-no-note is
> superseded: mail that fails authentication or the allowlist is quarantined (stored
> unattributed by default) rather than dropped. See
> [§Amendment 2026-07-02](#amendment-2026-07-02----inbound-authentication-hardening). The
> Telegram drop behavior is unchanged.

For Telegram, authentication is by numeric `chat.id` (stable across username changes),
configured via env var. Username matching is a fallback only, as usernames can be reassigned.
The maintainer identity is never stored in the KG.

The exact identity model (which identity claim is authoritative and how it ties to the ADR-053
actor model) was an open question at the original Proposed status; it is resolved by OQ-2 as
hardened by the 2026-07-02 amendment (domain authentication with alignment plus allowlist
before attribution; quarantined mail carries no trusted actor identity).

### 9. No secrets in the store

All credentials are loaded from env vars at adapter construction. They are never written to any
note property, KG entity, or content verb. `ChannelEnvelope` carries no credential fields.
Credential values in DEBUG logs are masked as `{first6}...[N chars]`.

### 10. Dedup and idempotency

Two layers, applied in order:

**Primary (mandatory, durable)**: the `idx_comm_message_external_id` PARTIAL UNIQUE index
(§11) enforces uniqueness at insert time. `comm.ingest` inserts with `INSERT OR IGNORE`; when
zero rows are written, the storage layer verifies whether the suppressed insert was caused by
an `external_id` collision (a live note in the same namespace and kind with the same non-empty
`external_id` already exists). If confirmed, the handler returns
`{"ok": true, "deduplicated": true}` without error. Any other ignored constraint surfaces as a
`StorageError` so it is not misreported as deduplication. No note body is ever lost. This check
is DB-backed and survives restarts; any re-delivered message is rejected at the storage layer.

**Secondary (optimization, in-memory)**: the adapter maintains a dedup cache keyed by transport
update id. Parameters from the openclaw reference (`bot-updates.ts:3-5`): TTL 5 minutes, max
2000 entries. This avoids the DB round-trip for the common case of duplicate delivery within
the same process lifetime. The cache starts empty on restart; the DB UNIQUE constraint covers
all cases the cache misses.

### 11. Expression index on `external_id`

The dedup check (step 1) and thread resolution (step 2) in §5b both query
`json_extract(properties,'$.external_id')`. Without an index, both are full scans of the notes
table.

The index is created by schema migration V5 (`005-unique-comm-external-id.sql`). V5 first
reconciles any duplicate rows: for each group of notes sharing the same
`(namespace, kind, external_id)`, the earliest row (lowest rowid) is kept unchanged; all later
duplicates have their `external_id` key removed from the properties JSON so they fall outside
the partial-index WHERE clause and are excluded from the uniqueness constraint. Message bodies
are preserved; only the redundant dedup key is cleared. V5 then drops any pre-existing
non-unique index of the same name and creates the new UNIQUE variant unconditionally. Using
`IF NOT EXISTS` at boot time would silently leave a pre-existing non-unique index in place;
the versioned migration approach avoids that pitfall.

```sql
DROP INDEX IF EXISTS idx_comm_message_external_id;
CREATE UNIQUE INDEX idx_comm_message_external_id
    ON notes(namespace, kind, json_extract(properties, '$.external_id'))
    WHERE deleted_at IS NULL
      AND json_extract(properties, '$.external_id') IS NOT NULL
      AND json_extract(properties, '$.external_id') != ''
```

The index is PARTIAL UNIQUE: the `WHERE` clause excludes rows with a null or empty
`external_id`, so notes without an external id (ordinary local messages) are unaffected.

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

### 14. Email adapter (`khive-channel-email`)

`khive-channel-email` implements the `Channel` trait for SMTP/IMAP. It is a sibling crate to
`khive-channel-telegram` at the same platform layer. See related issue #114.

#### Configuration (env-only)

All configuration is read from environment variables at adapter construction. No filesystem
config files are consulted. Credentials are never defaulted or logged.

| Variable                         | Required | Default | Description                                                                            |
| -------------------------------- | -------- | ------- | -------------------------------------------------------------------------------------- |
| `KHIVE_EMAIL_SMTP_HOST`          | yes      | --      | SMTP relay hostname                                                                    |
| `KHIVE_EMAIL_SMTP_PORT`          | no       | 587     | SMTP submission port                                                                   |
| `KHIVE_EMAIL_IMAP_HOST`          | yes      | --      | IMAP server hostname                                                                   |
| `KHIVE_EMAIL_IMAP_PORT`          | no       | 993     | IMAP over TLS port                                                                     |
| `KHIVE_EMAIL_USERNAME`           | yes      | --      | IMAP/SMTP credential username                                                          |
| `KHIVE_EMAIL_PASSWORD`           | yes      | --      | IMAP/SMTP credential password (never logged)                                           |
| `KHIVE_EMAIL_MAINTAINER_ADDRESS` | yes      | --      | The sole authorized inbound sender (RFC 5322 addr-spec)                                |
| `KHIVE_EMAIL_INGEST_NAMESPACE`   | no       | `local` | Target namespace for ingested inbound messages; passed as `namespace` to `comm.ingest` |

When any required variable is absent, `EmailChannelConfig::from_env()` returns
`ChannelError::Config`. The MCP server logs a warning and skips the email adapter; it does not
crash.

#### Inbound authentication (OQ-2 resolved)

> **Superseded 2026-07-02.** The addr-spec allowlist described here is necessary but not
> sufficient: `From:`/`Sender:` are spoofable, so this check alone authenticates nothing. See
> [§Amendment 2026-07-02](#amendment-2026-07-02----inbound-authentication-hardening), which
> requires `Authentication-Results` domain authentication with alignment before attribution and
> quarantines everything else. The allowlist rules below remain in force as one of the two
> required conditions.

The adapter enforces a single-maintainer allowlist at the adapter boundary, before any note is
written. Authentication rules:

1. The `From:` header must contain exactly one address. Messages with zero or more than one
   From address are rejected with `ChannelError::UnauthorizedSender`. Multi-From is an
   unauthorized state regardless of address content.
2. That single From addr-spec (lowercased, display name stripped) must match
   `KHIVE_EMAIL_MAINTAINER_ADDRESS` exactly.
3. If a `Sender:` header is present, its addr-spec must also match the maintainer. A Sender
   that differs from the From address is rejected.
4. Error messages intentionally omit the actual address values to avoid leaking sender
   addresses to logs.

_(Superseded 2026-07-02)_ ~~Messages that fail any check are skipped with a warning that logs
only the IMAP UID. No note is written for unauthorized or malformed senders.~~ Failed messages
are now quarantined per the amendment (stored unattributed when `KHIVE_EMAIL_QUARANTINE_STORE`
is on; dropped with only the IMAP UID logged when it is off).

_(Superseded 2026-07-02)_ ~~This resolves OQ-2: env-var addr-spec comparison is the
authoritative check for v1.~~ The addr-spec comparison is retained only as the allowlist
condition; it is not authoritative on its own. The anonymous actor default for
`VerbRegistry::dispatch` remains acceptable for quarantined mail specifically -- quarantined
notes are exactly the ones that must not carry a trusted actor identity. Attributed mail
requires both amendment conditions before the maintainer identity is stamped.

#### Sender address format (OQ-1 resolved)

Inbound envelopes carry the sender's email address in channel-prefixed form:
`email:sender@example.com`. The `channel_kind` property on the written note is `"email"`.
`comm.inbox` displays the channel-prefixed address as the sender (Option A from §OQ-1), which
preserves the external address for agents that need to reply or correlate by sender.

Outbound `ChannelEnvelope.from` values in `email:addr` form have the `email:` prefix stripped
before the SMTP message is built.

This resolves OQ-1: Option A (channel-prefixed form) is the normative choice.

#### Thread correlation via `X-Khive-Thread-ID`

Outbound emails carry a custom `X-Khive-Thread-ID` header whose value is the internal UUID
`thread_id` of the outbound note. When a reply arrives, the adapter reads this header first; if
absent, it falls back to the `In-Reply-To` header. The resolved value is passed as
`correlation_external_id` in the `comm.ingest` dispatch, and the handler performs the two-step
DB lookup described in §5b.

The inbound `external_id` for dedup is derived from the IMAP UIDVALIDITY and UID values
obtained when selecting the INBOX: `imap:{host}:{uidvalidity}:{uid}`. This key is stable
across reconnects and does not depend on the presence of a `Message-ID` header (which is
optional and may be absent or forged). The `Message-ID` header is not used for dedup.

#### Dependencies

The email adapter uses `lettre 0.11` (SMTP, `tokio1-rustls-tls` transport), `async-imap 0.9`
(IMAP UID fetch), `async-native-tls 0.5` (TLS layer), and `mail-parser 0.9` (RFC 822 parsing).
These are workspace dependencies; the adapter crate declares them as non-optional.

The `channel-email` feature in `khive-mcp/Cargo.toml` gates the adapter and the polling loop.
When the feature is disabled (default), the binary compiles without any email dependency. When
the feature is enabled and the required env vars are present at runtime, the loop starts; when
they are absent, the loop is skipped with a log warning.

## Open Questions

The open questions from the original Proposed status are resolved by the decisions above and by
the email adapter implementation (#114).

**OQ-1 (from field format)** -- resolved: Option A (channel-prefixed form, e.g.,
`email:sender@example.com`) is the normative choice. See §14.

**OQ-2 (inbound auth identity model)** -- resolved 2026-06-14, **hardened 2026-07-02**: the
env-var addr-spec comparison is retained as the sender allowlist but is no longer sufficient on
its own. Attribution now also requires `Authentication-Results` domain authentication with
alignment from the trusted receiving boundary; unauthenticated mail is quarantined and never
attributed. See [§Amendment 2026-07-02](#amendment-2026-07-02----inbound-authentication-hardening).

## Consequences

- `comm.inbox`, `comm.read`, `comm.reply`, and `comm.thread` are unchanged for agents.
- A new `comm.ingest` `Visibility::Subhandler` verb is added to `khive-pack-comm` (`vocab.rs`
  - handler dispatch). It is not visible on the MCP wire.
- `COMM_SCHEMA_PLAN_STMTS` has three entries (inbox, thread, and to-actor indexes). The
  `idx_comm_message_external_id` PARTIAL UNIQUE index is NOT in the pack schema plan; it is
  created by schema migration V5 (`005-unique-comm-external-id.sql`), which is the sole durable
  authority. Dedup is atomic via `INSERT OR IGNORE` in `KhiveRuntime::try_create_note`.
- The polling loop runs in `kkernel` as a `tokio::task`, holding `Arc<VerbRegistry>` and
  `Arc<ChannelRegistry>` (no `NamespaceToken` -- `dispatch` mints its own per call). It is
  cancelled on shutdown.
- Every inbound write passes through `VerbRegistry::dispatch:678`, satisfying the ADR-018
  single-dispatch-site invariant. The registry mints the `NamespaceToken` internally from the
  `"namespace"` param on each call.
- No credentials appear in any note, entity, or KG store.
- `CommPack` construction and `PackFactory` wiring are unchanged.
- `khive-channel` and `khive-channel-telegram` are new crates at the platform layer. (OQ-1 and
  OQ-2 are resolved; OQ-2 as hardened by the 2026-07-02 amendment. The email adapter's
  attribution gate and quarantine disposition follow that amendment.)

## Related ADRs

- ADR-017: Pack Standard -- the Pack trait this ADR explicitly decides not to implement.
- ADR-018: Authorization Gate (original) -- defines the single-dispatch-site invariant and the
  `Gate` trait. Every ingest write passes through `VerbRegistry::dispatch:678`.
- ADR-040: Communication and Schedule Packs -- the `comm.*` verb surface and `message` note kind.
- ADR-053: Authorization Gate (ActorStore / SessionStore extension) -- extends ADR-018's actor
  model. `KhiveRuntime::authorize` (the public gate-checked door) and
  `NamespaceToken::mint_authorized` (`pub(crate)`, unreachable externally) are introduced here.
  The ruling on how the ingest loop's identity interacts with this model is delivered by the
  2026-07-02 amendment: attribution requires the two-part authentication gate, and quarantined
  mail is written without a trusted actor identity.
- ADR-028: Pack-Scoped Backends -- offset/cursor persistence pattern for channel adapters.
