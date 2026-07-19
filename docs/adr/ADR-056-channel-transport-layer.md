# ADR-056: Channel Transport Layer -- `khive-channel` and External Messaging Adapters

**Status**: Accepted (amended 2026-07-02 -- inbound authentication hardening; amended 2026-07-03
-- Exchange Online no-authserv-id boundary; amended 2026-07-05 -- Telegram adapter
implementation and two-way chat; amended 2026-07-09 -- durable IMAP UID cursor; amended
2026-07-17 -- iMessage channel over an SSH bridge; see
[§Amendment 2026-07-02](#amendment-2026-07-02----inbound-authentication-hardening),
[§Amendment 2026-07-03](#amendment-2026-07-03----exchange-online-no-authserv-id-boundary),
[§Amendment 2026-07-05](#amendment-2026-07-05----telegram-adapter-implementation-and-two-way-chat),
[§Amendment 2026-07-09](#amendment-2026-07-09----durable-imap-uid-cursor),
[§Amendment 2026-07-17](#amendment-2026-07-17----imessage-channel-over-an-ssh-bridge))\
**Date**: 2026-06-14 (amended 2026-07-02, 2026-07-03, 2026-07-05, 2026-07-09, 2026-07-17)\
**Authors**: khive maintainers
**Depends on**: ADR-017 (Pack Standard), ADR-018 (Authorization Gate), ADR-040 (Communication
and Schedule Packs), ADR-053 (ActorStore / SessionStore -- extends ADR-018's actor model),
ADR-108 (Git Write Surface -- hardened shell-out argv pattern reused by this amendment)\
**Related issues**: #112 (khive-channel umbrella), #113 (Telegram adapter), #114 (email adapter),
#448 (inbound header spoofing -- resolved by this amendment), #449 (IMAP UID progress -- resolved
by the 2026-07-09 amendment)

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
   `authserv-id` -- _amended 2026-07-03: the boundary is matched by a configured trust anchor
   that is either an `authserv-id` or, for a boundary that emits no `authserv-id`, the topmost
   no-`authserv-id` position; see [§Amendment 2026-07-03](#amendment-2026-07-03----exchange-online-no-authserv-id-boundary)_),
   that shows `dmarc=pass` _with `header.from` alignment_ (amended 2026-07-03), or equivalently at least one of SPF-pass with
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
  that delivers to the ingest mailbox); _(amended 2026-07-03: this equality clause applies to a
  boundary that emits an `authserv-id`. Exchange Online emits none on its plain
  `Authentication-Results`; for that class of boundary the configured trust anchor is the reserved
  sentinel `!topmost-no-authserv-id` and selection is positional -- see
  [§Amendment 2026-07-03](#amendment-2026-07-03----exchange-online-no-authserv-id-boundary).)_
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

| Variable                       | Required | Default | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| ------------------------------ | -------- | ------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KHIVE_EMAIL_AUTHSERV_ID`      | yes      | --      | The trusted receiving boundary's trust anchor. A literal `authserv-id`: only `Authentication-Results` headers bearing this id are trusted. The reserved sentinel `!topmost-no-authserv-id` (amended 2026-07-03): the boundary emits no `authserv-id` (e.g. Exchange Online) and the topmost no-`authserv-id` header is trusted positionally. Unset/empty fails construction (channel never starts); a typo of the sentinel is treated as a literal id, matches nothing, and quarantines all mail. No value silently degrades to trust-topmost. |
| `KHIVE_EMAIL_QUARANTINE_STORE` | no       | `on`    | When `on`, unauthenticated mail is stored as an unattributed quarantined note; when `off`, it is dropped.                                                                                                                                                                                                                                                                                                                                                                                                                                      |

`KHIVE_EMAIL_MAINTAINER_ADDRESS` remains the sender allowlist. It stays a single addr-spec for
v1; a multi-entry allowlist is a compatible later extension.

### Scope

This amendment governs the email adapter (#114, #448). The Telegram adapter's numeric `chat.id`
authentication (§8) is a stable transport-authenticated identifier and is unaffected. The
`comm.ingest` dispatch path, the single-dispatch-site gate invariant, and the dedup model are
unchanged; the amendment adds an attribution gate in front of them and a quarantine disposition
beside them. Implementation (#448) follows this accepted revision.

## Amendment 2026-07-03 -- Exchange Online no-authserv-id boundary

This amendment refines the [§Trusted-header selection](#trusted-header-selection-the-load-bearing-detail)
rule of the 2026-07-02 amendment. That rule made "select only `Authentication-Results` headers
whose `authserv-id` equals the configured `KHIVE_EMAIL_AUTHSERV_ID`" the normative selector. The
2026-07-02 clauses above are retained, marked amended in place; the rule below governs where they
are amended.

### Finding

The trusted receiving boundary for the production ingest mailbox (Microsoft
Exchange Online) emits **no `authserv-id`** on the plain `Authentication-Results` header it
stamps. This is a known Microsoft deviation from RFC 8601 §2.2, which requires the header to begin
with an `authserv-id` token. The observed header from a real delivered message begins directly
with the first method verdict:

```
Authentication-Results: spf=pass (sender IP is ...) smtp.mailfrom=gmail.com;
  dkim=pass (signature was verified) header.d=gmail.com;
  dmarc=pass action=none header.from=gmail.com; compauth=pass reason=100
```

The only `authserv-id` present anywhere in the message is on the `ARC-Authentication-Results`
variant (`i=2; mx.microsoft.com`), a distinct header carrying a distinct trust semantics.

Consequence: a strict RFC 8601 selector can never match a configured `authserv-id` against a
header that has none, so every Exchange Online delivery fails selection and is quarantined. This
is a defect in the trust-anchor model for no-`authserv-id` boundaries, not a misconfiguration; no
environment value can identify a token the boundary does not emit.

### Decision

The configured trust anchor `KHIVE_EMAIL_AUTHSERV_ID` selects one of two modes, chosen only from
configuration, never inferred from message content:

1. **`authserv-id` mode** (RFC-compliant boundary): unchanged. Select the topmost
   `Authentication-Results` whose `authserv-id` equals the configured value; ignore all others;
   absence quarantines.
2. **Topmost-no-`authserv-id` mode** (boundary emitting no `authserv-id`, e.g. Exchange Online):
   selected by the reserved sentinel value `!topmost-no-authserv-id`. Select the **topmost**
   `Authentication-Results` header and trust it **only if it carries no `authserv-id`**. If the
   topmost header unexpectedly carries any `authserv-id`, quarantine (fail closed): under this
   mode the trusted boundary is defined to emit none at the top, so an `authserv-id` at the top
   signals either a boundary behavior change or an injected header that has floated above the
   boundary's own stamp. Absence of any `Authentication-Results` quarantines, unchanged.

In topmost-no-`authserv-id` mode **position is the sole discriminator** between the boundary's own
verdict and a sender-forged one. This is sound only under the operational precondition below.

### Domain-authentication alignment hardening

The domain-authentication leg (Decision §1) requires `dmarc=pass` **with `header.from` alignment**
to the message `From:` domain that will be attributed, not a bare `dmarc=pass`. The prior
implementation trusted `dmarc=pass` without confirming its `header.from` equalled the attributed
`From:` domain; that gap was closed only by conjunction with the sender-allowlist leg. Because
positional trust makes the auth leg more load-bearing, the auth leg is made self-sufficient: a
`dmarc=pass` whose `header.from` domain does not match the message `From:` domain does not satisfy
the check (SPF-with-envelope-alignment and DKIM-with-`d=`-alignment already required alignment and
are unchanged).

### Operational precondition (sharpened)

The RFC 8601 §5 operational precondition of the 2026-07-02 amendment becomes strictly
load-bearing in topmost-no-`authserv-id` mode, where no `authserv-id` string participates in the
decision. Enabling this mode for a mailbox requires that the receiving boundary prepend its own
`Authentication-Results` above all sender-supplied content on **every** delivered message, and
that the ingest mailbox have **no path that bypasses that prepend** -- no internal
tenant-authenticated submitter, no distribution list, and no forwarding or journaling rule that
could deliver a message whose topmost `Authentication-Results` is not the boundary's own. This is
an operational fact about the live tenant that code cannot verify; it MUST be attested before
attribution is enabled, via the issue #448 live probe (send a message carrying a forged topmost
`Authentication-Results: ...; dmarc=pass` and confirm the boundary's genuine stamp lands above
it). Until attested, the mailbox is quarantined.

### Rejected alternatives (do not re-open without new evidence)

- **Match the `ARC-Authentication-Results` `authserv-id` (`mx.microsoft.com`).** Rejected.
  `mx.microsoft.com` is shared by every Microsoft 365 tenant and its ARC chain is signed by one
  shared `d=microsoft.com` key, so neither the string nor ARC-Seal verification identifies _this_
  boundary; only position (highest ARC instance) discriminates, which is what topmost-no-
  `authserv-id` mode already does without a connector change or a false anchor.
- **Verify ARC-Seal cryptography.** Rejected: out of the v1 re-verification scope, and a valid
  seal proves only that some Microsoft 365 tenant sealed the message, not that this boundary did.

### Configuration

No new variable. `KHIVE_EMAIL_AUTHSERV_ID` gains the reserved sentinel `!topmost-no-authserv-id`.
The sentinel is reserved by convention, not by grammar: RFC 8601's `authserv-id` is a value (RFC
2045 token or quoted-string) and `!` is not in `tspecials`, so a leading `!` is grammar-legal in a
token. It is safe here because no real-world domain-form receiving-boundary identifier begins with
`!` and this config contract reserves the exact string, so it does not collide with a real anchor
in practice. Every failure direction remains closed: unset or empty fails construction and the
channel never starts; any non-sentinel value is a literal `authserv-id` (a typo of the sentinel
matches no header and quarantines all mail); no value silently degrades to trust-topmost.

### Consequences

The production Exchange Online mailbox can be attributed once the operational precondition is
attested. The security posture is unchanged for RFC-compliant boundaries. The change is confined
to `parse_header` (recognize the no-`authserv-id` header form), `select_trusted` (mode dispatch),
the auth-leg alignment check, and the config trust-anchor parse; the connector, dedup, quarantine
disposition, and dispatch-gate invariants are untouched. Implementation follows this accepted
revision.

## Amendment 2026-07-05 -- Telegram adapter implementation and two-way chat

### Motivation

The maintainer wants a real-time **chat** channel (greetings, nudges, reminders)
that is separate from email. Email remains the decision and tracking thread;
chat is the lightweight side channel. Concretely, `comm.send(to="telegram:maintainer")`
must deliver a message to the maintainer's Telegram, and the maintainer's replies
must arrive as inbound `comm` messages that wake the inbox monitor -- the same
two-way shape email already has.

### The outbound path is live, not deferred (un-stales §5c/§5d)

ADR-056 §5c/§5d mark the outbound path and reply routing DEFERRED. That labeling
is stale relative to the shipped code: the email adapter made outbound delivery
live via a per-channel **outbox loop**. `spawn_email_channel_loops`
(`khive-mcp/src/serve.rs`) spawns two tasks under the daemon role -- an inbound
`channel_poll_loop` and a `channel_outbox_loop` that drains outbound `message`
notes destined for the channel and calls the channel's `Channel::send`. Outbound
therefore already works for a live adapter; §5c/§5d describe an earlier plan, not
the current mechanism.

This amendment records that current mechanism as the normative outbound design and
applies it to Telegram: a `channel-telegram`-feature-gated
`spawn_telegram_channel_loops`, mirroring `spawn_email_channel_loops`, spawns a
Telegram poll loop and a Telegram outbox loop holding an `Arc<TelegramChannel>`.
No change to the comm pack, the `comm.ingest` verb, the dispatch gate, the dedup
index, or the envelope: Telegram reuses all of them exactly as email does.

### The `khive-channel-telegram` crate

A new sibling crate `crates/khive-channel-telegram` (named throughout ADR-056 §1)
implements the `Channel` trait for the Telegram Bot API:

- `kind()` -> `"telegram"`.
- `send(envelope)` -> HTTP `POST https://api.telegram.org/bot<TOKEN>/sendMessage`
  with `{chat_id, text}`. Reuses the workspace `reqwest` client (v0.12,
  rustls-tls + json) -- the same client the email adapter's OAuth path uses. No
  Telegram SDK: the Bot API is plain HTTPS + JSON.
- `poll(since)` -> `getUpdates` long-poll with an in-memory `offset` watermark
  (ADR-056 §7: long-poll is the default; the embedded/mini deployment has no
  routable public URL, so no webhook). Each update becomes a `ChannelEnvelope`.
  Offset persistence is covered in its own section below.
- Inbound authentication by numeric `chat.id` (ADR-056 §8, unchanged; the
  2026-07-02 email hardening explicitly exempts Telegram's transport-authenticated
  `chat.id`). An update whose `chat.id` != the configured maintainer chat id
  returns `ChannelError::UnauthorizedSender` and is dropped with no note (ADR-056
  §8 Telegram drop behavior, unchanged -- no quarantine, that is email-only).
- `external_id` = `tg:{chat_id}:{update_id}` (ADR-056 §3), the primary dedup key
  through the existing `idx_comm_message_external_id` unique index (§10/§11).

### Poll offset and restart durability

The `offset` watermark is held **in memory** in the Telegram poll loop, mirroring
the email poll loop's in-memory `last_poll` timestamp (`channel_poll_loop` in
`khive-mcp/src/serve.rs`). It is NOT persisted -- not to a khive content store and
not to a new transport-state table. Restart durability does not depend on a stored
cursor: (1) Telegram's `getUpdates` confirms and drops updates below a
previously-acknowledged offset server-side, and (2) any update re-delivered in the
still-unconfirmed window is idempotently rejected by the existing `external_id`
unique index (`tg:{chat_id}:{update_id}`). This is the same durability model the
email adapter already relies on: its IMAP cursor is likewise not persisted; the
dedup index is the durable guard. FindExisting -- no new schema, no new table.

ADR-028's pack-scoped-backend cursor-persistence pattern is available but is
deliberately not used for the poll cursor here, exactly as the email adapter
does not use it -- the in-memory watermark plus the durable dedup index is
sufficient, and consistency with the shipped adapter wins.

### Outbound addressing (the one new decision)

Outbound recipients use the channel-prefixed form `telegram:<slug>` (ADR-056 OQ-1
Option A, matching `email:<addr>`). The Telegram outbox loop recognizes the
`telegram:` prefix, strips it, and resolves `<slug>` to a numeric `chat_id`:

- v1 is single-maintainer. The only routable slug is the maintainer slug
  (default `maintainer`, overridable by `KHIVE_TELEGRAM_MAINTAINER_SLUG`), which
  resolves to `KHIVE_TELEGRAM_MAINTAINER_CHAT_ID`. An outbound note addressed to
  any other `telegram:` slug is unroutable and is logged and dropped (never sent
  to the maintainer chat by default) -- no silent misdelivery.
- The `chat_id` is env configuration only. It is never written to the KG, a note
  property, or any content verb (ADR-056 §9).

Inbound envelopes carry `from = "telegram:<maintainer-slug>"` (channel-prefixed,
OQ-1 Option A) so an agent can reply with `comm.reply`, which the outbox loop
routes back out through `telegram:<slug>`.

Operational precondition (Telegram-specific): a bot cannot initiate a direct
message. The maintainer must send `/start` to the bot once; the first `getUpdates`
response then surfaces the `chat.id`, which the operator reads and sets as
`KHIVE_TELEGRAM_MAINTAINER_CHAT_ID`. Until the chat id is configured, the adapter
authenticates nothing and drops all inbound (fail-closed). The bot token itself is
created by the maintainer in BotFather and placed into the deploy node's
keychain/env by the maintainer's own hand -- it is never handled in plaintext by an
agent and never stored in any khive store.

### Configuration (env-only, mirroring §14)

| Variable                            | Required | Default      | Description                                                                                               |
| ----------------------------------- | -------- | ------------ | --------------------------------------------------------------------------------------------------------- |
| `KHIVE_TELEGRAM_BOT_TOKEN`          | yes      | --           | Bot API token (BotFather). Never logged (masked `{first6}...[N chars]`), never stored in any khive store. |
| `KHIVE_TELEGRAM_MAINTAINER_CHAT_ID` | yes      | --           | The single authorized inbound sender AND the outbound recipient for the maintainer slug. Numeric.         |
| `KHIVE_TELEGRAM_MAINTAINER_SLUG`    | no       | `maintainer` | The slug in `telegram:<slug>` that maps to the maintainer chat id.                                        |
| `KHIVE_TELEGRAM_INGEST_NAMESPACE`   | no       | `local`      | Target namespace for ingested inbound messages (passed as `namespace` to `comm.ingest`).                  |

When any required variable is absent, `TelegramChannelConfig::from_env()` returns
`ChannelError::Config`; the server logs a warning and skips the Telegram adapter
without crashing (mirrors the email adapter, §14).

**Inbound target namespace (deploy-config, not a code decision).** For the
two-way-chat deployment, `KHIVE_TELEGRAM_INGEST_NAMESPACE` is set to the target
agent's namespace so the maintainer's replies land in that agent's `comm.inbox`
and wake its monitor. This is the namespace-alignment requirement of ADR-056 §5a
applied to Telegram.

### Feature gating

`khive-mcp/Cargo.toml` gains `channel-telegram = ["khive-channel",
"khive-channel-telegram"]` with `khive-channel-telegram` an optional dependency,
mirroring `channel-email`. Default build excludes it (no Telegram dependency
compiled). `channel-email` and `channel-telegram` can be enabled together; the
`ChannelRegistry` already keys by `(kind, slug)` (#606), so both adapters coexist.

### Consequences

- `comm.inbox`/`read`/`reply`/`thread` unchanged for agents; two-way Telegram chat
  works through the existing verbs with no API change.
- The comm pack, `comm.ingest`, the dispatch gate, the dedup index, and the
  envelope are untouched -- Telegram reuses them exactly as email does.
- A new `khive-channel-telegram` crate at the platform layer; a
  `channel-telegram`-gated `spawn_telegram_channel_loops` in `khive-mcp`.
- No credentials in any note, entity, or KG store; bot token env/keychain only.
- The §5c/§5d "DEFERRED" labels are superseded by the live outbox-loop mechanism
  documented here (they described an earlier plan).

### Out of scope (v1)

- Multi-recipient / group chats, inline keyboards, media (text only for v1).
- Webhook inbound (long-poll only until a public-URL deployment exists -- ADR-056 §7).
- A general `telegram:<slug>` address book beyond the single maintainer slug.

## Amendment 2026-07-09 -- Durable IMAP UID cursor

### Motivation

Issue #449 (`khive-channel-email: IMAP polling can repeatedly fetch the same UID subset and
miss later messages`, P1/high): a mailbox with more messages in one `SINCE` day-window than the
50-item fetch limit could have its overflow permanently starved. `LiveImap::fetch_since` searched
`SINCE <date>`, collected the result into a `Vec` straight from `async-imap`'s `HashSet<Uid>`, and
truncated to the limit before any progress state existed. With 75 same-day UIDs and a 50-item
limit, the same arbitrary 50 could be returned forever while UIDs 51-75 were never fetched, and
once the date window advanced past that day, those UIDs would never match `SINCE` again.

This directly supersedes this ADR's earlier restart-durability claim (Amendment 2026-07-05,
"Poll offset and restart durability"): _"the email adapter's IMAP cursor is likewise not
persisted... no new schema, no new table."_ Issue #449 explicitly requires persisted per-mailbox
`UIDVALIDITY`/high-water progress, because -- unlike Telegram's `getUpdates`, which
server-side-drops already-acknowledged updates -- IMAP `SINCE` truncation could permanently skip
messages the dedup index never saw in the first place; the durable dedup index cannot recover
mail that pagination never fetched. `Last_in_time` governs: this amendment's persistence
requirement controls where it conflicts with the earlier claim.

### What changed

`crates/khive-channel-email/src/connector/imap.rs` and `channel.rs` replace unordered
day-level truncation with a `(UIDVALIDITY, last_seen_uid)` progress model (`ImapProgress`):
UID search results are rejected if they contain a protocol-invalid zero UID, sorted ascending,
deduplicated, filtered to strictly above the high-water when the epoch is unchanged, then
truncated to the page limit. A UIDVALIDITY change discards the old high-water. `khive-channel`
gains transport-neutral `ChannelCheckpoint`/`StoredChannelCheckpoint`/`ChannelPollPage` types and
a default `Channel::poll_page` method (existing `Channel::poll` and all other implementers are
unchanged and source-compatible). `khive-pack-comm` gains a pack-owned auxiliary table,
`comm_channel_cursor` (keyed by `(channel_kind, channel_slug)`, storing `source`, `generation`,
`high_water`, `updated_at`), plus internal (non-MCP-callable) `comm.cursor_get`/`comm.cursor_commit`
subhandlers. This table and its handlers are new, pack-owned operational bookkeeping -- introduced by the email
amendment as an idempotent pack `CREATE TABLE IF NOT EXISTS`, consistent with ADR-028's
pack-scoped-backend cursor pattern -- and they directly retract the "no new schema, no new table"
claim above for the email adapter only; the present amendment moves this table's schema to a core
`khive-db` versioned migration as it widens the key (§Migration: owner and sequence);
Telegram's offset remains in-memory as originally documented.

### The durable checkpoint path is wired into the daemon

1. **`ImapFetcher::fetch_since`** (unchanged public signature, still used by `EmailChannel::poll`,
   the non-checkpointed entrypoint) holds an in-memory `legacy_progress` mutex that applies the
   same sort/dedup/high-water/truncate discipline across repeated calls on one process. This
   closes the issue's literal reported failure -- the same 50-of-75 UIDs repeated forever -- for
   any caller that only uses `poll`. It is **not persisted**; on its own, a process restart resets
   it to an empty progress and the connector bootstraps by `SINCE` again.
2. **`EmailChannel::poll_page`** (a new override of the `Channel` trait's default) plus
   `comm.cursor_get`/`comm.cursor_commit`: a fully implemented and unit/integration-tested durable
   checkpoint path, keyed by a stable `imap+tls:{host}:{port}:{mailbox}:INBOX` source string so
   one account's high-water can never suppress another's. **`khive-mcp/src/serve.rs`'s
   `channel_poll_loop` now calls `cursor_get` -> `poll_page` -> every `comm.ingest` -> `cursor_commit`
   for each configured channel, committing the cursor only after every envelope in the page has
   durably ingested.** A partial-page `comm.ingest` failure leaves the checkpoint untouched, so the
   next poll re-selects the whole page; `comm.ingest`'s `INSERT OR IGNORE` dedup then skips
   re-storing the messages that already succeeded and only the failed one is effectively retried.
   A `cursor_get` failure skips that channel's poll for the cycle rather than risk polling from an
   empty checkpoint and silently discarding durable state.
3. **Poison-UID durability** (issue #449 High): a selected UID whose `UID FETCH` response carries
   no RFC822 body, or whose body fails to parse, no longer fails the whole page. It gets a durable
   terminal disposition -- a quarantine envelope carrying the stable `imap:{host}:{uidvalidity}:{uid}`
   dedup key and a `missing-body`/`parse-failure` reason, always stored regardless of the
   `quarantine_store` config flag -- so the cursor can advance past it instead of re-selecting the
   same poison UID on every subsequent poll.

### Consequences

- The issue's literal 75-message/50-limit backlog-drain scenario is fixed and covered by tests
  exercising the real `EmailChannel::poll` entrypoint, not only the pure helpers.
- Full restart durability (a daemon crash or redeploy resuming exactly above the last durably
  ingested UID) is delivered end-to-end: `channel_poll_loop` drives `cursor_get`/`poll_page`/
  `cursor_commit` on every cycle, and the commit-only-after-full-page-ingest ordering is covered by
  poll-loop regression tests for partial-ingest-failure non-advancement and cross-restart
  round-tripping.
- `comm_channel_cursor`'s subhandlers are pack-owned operational bookkeeping and not a new
  MCP-callable verb, exercised in production only via the daemon poll loop's internal dispatch
  calls. Its table schema was pack-declared through `CREATE TABLE IF NOT EXISTS` when this bullet
  was written; the 2026-07-17 amendment below moves that schema to core migration `V11`, so the
  table's schema is core-migrated while its subhandlers stay pack-internal.
- Telegram's in-memory offset watermark and its restart-durability rationale (Amendment
  2026-07-05) are unchanged by this amendment.

## Amendment 2026-07-17 -- iMessage channel over an SSH bridge

### Motivation

The maintainer wants a chat channel reachable through iMessage, alongside the existing email and
Telegram adapters. Unlike those two, no directly reachable protocol endpoint exists: sending and
reading iMessage requires the Messages application on a macOS host signed into a dedicated bridge
Apple ID that can exchange iMessages with the maintainer's own handle. The daemon host is not, in
general, that host. This amendment specifies a
`khive-channel-imessage` adapter that bridges to a remote macOS host over SSH and implements the
same `Channel` trait (§2) the email and Telegram adapters implement, so `comm.send`,
`comm.inbox`, `comm.reply`, and `comm.thread` are unchanged for agents.

### The `khive-channel-imessage` crate

A new sibling crate `crates/khive-channel-imessage`, at the same platform layer as
`khive-channel-telegram` and `khive-channel-email`:

- `kind()` -> `"imessage"`.
- Registered in the `ChannelRegistry` keyed by `(kind, slug)` (#606), coexisting with any other
  configured adapter, matching the Telegram and email precedent.
- Spawned only under the daemon role, via a `channel-imessage`-feature-gated
  `spawn_imessage_channel_loops` mirroring `spawn_email_channel_loops` and
  `spawn_telegram_channel_loops`: one inbound poll task and one outbound task per adapter
  instance, not the abstract `ChannelRegistry::poll_all` sweep §§4, 6, and 12 describe in the
  original decision text. The 2026-07-05 Telegram amendment established the per-adapter
  `spawn_*_channel_loops` task pair as the shipped mechanism, but its supersession claim was
  scoped narrowly to §5c/§5d (the outbound-path and reply-routing DEFERRED labels) -- it did not
  supersede §6's `poll_all` lifecycle description. This amendment corrects that gap directly: it
  supersedes the `poll_all` lifecycle prescriptions of §§4, 6, and 12. The authoritative
  lifecycle model, matching the shipped email and Telegram adapters, is per-adapter loop pairs
  spawned by the daemon role and registered through the `ChannelRegistry` keyed by `(kind,
  slug)`. Every future adapter ships its own daemon-spawned loop pair; `poll_all` is retired and
  MUST NOT be implemented.
- When configuration is absent, `ImessageChannelConfig::from_env()` returns `ChannelError::Config`
  and the server logs a warning and skips the adapter without crashing, matching Telegram and
  email `from_env()` behavior (§14, §Amendment 2026-07-05).

### Transport: SSH to a bridge host

The daemon reaches the bridge host by shelling `ssh` to a configured target
(`KHIVE_IMESSAGE_SSH_TARGET`, `user@host`) on the local network. The bridge host needs no
resident service beyond macOS's built-in `sshd`. The client keypair used to authenticate to the
bridge host is never stored in any khive store, consistent with §9 ("no secrets in the store").
An unreachable bridge host is a transport failure local to this one channel: it degrades to a
channel-down warning and does not affect any other configured channel or fault the daemon.

**Pinned bridge host identity.** Every `ssh` invocation this adapter makes uses a dedicated
known-hosts file, `/etc/khive/imessage/known_hosts`, provisioned during one-time setup with the
bridge host's genuine host key, and passes strict host-key checking and batch mode
(`-F none -o UserKnownHostsFile=/etc/khive/imessage/known_hosts -o GlobalKnownHostsFile=/dev/null
-o StrictHostKeyChecking=yes -o BatchMode=yes -i <pinned_bridge_key> -o IdentitiesOnly=yes
-o IdentityAgent=none -o PreferredAuthentications=publickey`) explicitly on the invocation. The adapter never inherits the operator's
own `~/.ssh/config`, never relies on SSH agent-based host trust, and never falls back to
trust-on-first-use for host identity: an unrecognized or changed host key is refused, not
recorded. Without this pin, a party on the local network positioned to intercept or substitute
the bridge connection could present its own host key, be silently trusted on first use, and then
serve fabricated `chat.db` rows that satisfy every sender-validation check below -- a trusted
instruction injection into the maintainer's inbox -- while also reading every outbound message
the adapter sends. A host-key mismatch stops the channel for that invocation, is counted, and the
resulting warning names the known-hosts file so the operator knows exactly what to inspect and,
once the change is verified legitimate, deliberately update. Acceptance property: a bridge host
presenting a host key that does not match the pinned entry refuses all transport until the pinned
entry is deliberately updated; the mismatch is never auto-accepted.

**Pinned known-hosts file integrity.** The host-key pin is only as trustworthy as the file that
holds it. `/etc/khive/imessage/known_hosts` is a fixed absolute path provisioned during one-time setup
and belongs to the same boundary as the client key file below: it must be root-owned and writable by
root alone -- mode carrying no group- or other-write bit (`mode & 022 == 0`) -- rather than merely
non-writable by the daemon account, because non-writability checked only against the daemon account
leaves the file open to any other non-root local principal on the host: a second local account, or a
group the file's mode happens to grant write to, could substitute a host-key entry just as effectively
as a compromised daemon could. The file must also resolve as a non-symlink so the path cannot be
redirected to an attacker-controlled known-hosts file after setup. Root ownership and a
no-group/other-write mode on the file alone are insufficient:
the containing directory chain up to the provisioning root -- the root-owned `/etc/khive/imessage/`
established by privileged setup, outside the daemon account's own home (§Provisioned files) -- must
carry the same root-owned, root-only-writable shape at every level, since replacing a name within a
directory writable by any non-root principal needs no write permission on the file it named. Were any
non-root local principal able to rewrite this file or redirect it through a writable ancestor
directory, that principal could record its own intercepting host key as
the pinned entry, and every subsequent `StrictHostKeyChecking=yes` invocation would silently accept
the substituted bridge host -- the exact instruction-injection path the host pin exists to close.
Acceptance property: the adapter refuses to start when `/etc/khive/imessage/known_hosts` is a symlink,
is not owned by root, carries a group- or other-write permission bit, or resolves through any
directory that is not similarly root-owned and root-only-writable at every level up to the
provisioning root.

**Pinned client identity.** Pinning the host key constrains which bridge host the client will
trust; it says nothing about which private key the client authenticates _with_. Without an
explicit identity restriction, an invocation can fall back to `ssh`'s default identity search
(`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, and so on) or to any key offered by a running `ssh-agent`
reachable through `SSH_AUTH_SOCK` in the daemon's own environment. Either path can authenticate
with a key that carries no `restrict`/forced-`command=` binding at all -- an operator's own
unrestricted interactive key, or an agent-held key authorized for some unrelated purpose -- which
defeats the server-side confinement below entirely: the forced-command boundary is a property of
one specific `authorized_keys` entry, and it holds only when the connection actually authenticates
with that entry's key. Every invocation this adapter makes therefore pins the client identity
explicitly: `-i <pinned_bridge_key>` names the dedicated bridge key's private-key file, `-o
IdentitiesOnly=yes` makes `ssh` offer only that named key and never probe a default identity file
or an agent-offered key, and `-o IdentityAgent=none` disables agent-based authentication outright;
the invocation additionally does not inherit `SSH_AUTH_SOCK` from the daemon's own environment, so
no agent is reachable even were `IdentityAgent` not set. `-o PreferredAuthentications=publickey`
completes the pin on the authentication-method axis: the client offers only public-key
authentication and never attempts keyboard-interactive, password, host-based, or GSSAPI, so neither
the daemon's own environment nor a bridge host impersonator can elicit a different credential, and
the connection authenticates by the pinned key or not at all. Without `IdentitiesOnly=yes` and agent
authentication disabled, a default or agent-offered key can authenticate ahead of the pinned key
and route the session to whatever `authorized_keys` entry that other key matches -- possibly one
with no `restrict`/forced-command binding at all -- defeating the restricted-key boundary
regardless of how carefully that boundary itself is configured. Acceptance property: an invocation
run with a default identity file present and an `ssh-agent` reachable via `SSH_AUTH_SOCK` in the
environment still authenticates only with the pinned bridge key; the session never falls back to
a default identity file or an agent-offered key.

The pinned key file is part of the same boundary, but its permission shape is constrained by what
OpenSSH will load. `<pinned_bridge_key>` is the fixed absolute path
`/etc/khive/imessage/bridge_key`, provisioned during one-time setup. OpenSSH refuses a private key
file that carries any group or other permission bit (`mode & 077 != 0`) or is owned by neither the
invoking user nor root, so the key cannot be root-owned and group-readable -- the layout that would
let a root-owned file stay unwritable by the daemon while the daemon still reads it is exactly the one
`ssh` rejects. The key is therefore owned by the daemon account with mode `0400`, the only shape that
both loads under `ssh` and grants the daemon the read it needs. Because the daemon owns the file it
can in principle rewrite its contents; the substitution resistance does not come from file
immutability but from two other properties. First, the key lives inside the root-owned
`/etc/khive/imessage/` provisioning root, and that directory and every directory up to it are
root-owned and writable by root alone -- not merely unwritable by the daemon account, which would
still leave the ancestor chain open to some other non-root local principal -- so neither the daemon
nor any other non-root account can unlink and recreate the file at the same
path or redirect the path through a symlink -- replacing a name within a directory is a directory
permission, and no non-root principal has any here. Second, and more fundamentally, substituting the client key
gains a compromised daemon nothing: any key it installs must still appear in the bridge account's
`authorized_keys` (which lives on the bridge host, unwritable from the daemon) to authenticate at all,
and even then it authenticates only as the forced-command, single-account-confined bridge account
(§Server-side key confinement). The confinement is a server-side property of the account, not a
client-side guarantee of key immutability. The key is also resolved as a non-symlink so the path
cannot be redirected to a substituted key after setup. This provisioned absolute path is the canonical
source for the `-i` invocation; the adapter does not discover the key location dynamically at runtime.
Acceptance property: the adapter refuses to start when the bridge key at its provisioned absolute path
is a symlink, is not owned by the daemon account, carries any group or other permission bit (a shape
`ssh` would itself refuse), or resolves through any directory that is not root-owned and root-only-writable.

**Client-side config isolation.** `-F none` is the literal argument OpenSSH's client documents
for suppressing configuration-file processing entirely -- both the system-wide
`/etc/ssh/ssh_config` and the per-user `~/.ssh/config` -- rather than a path substitution, so an
operator-controlled `~/.ssh/config` can never inject a `ProxyCommand`, a `Match exec` directive,
or any other override into a session opened with this key. `GlobalKnownHostsFile` is a
file-path-valued option (default `/etc/ssh/ssh_known_hosts` and `/etc/ssh/ssh_known_hosts2`), not
a toggle, so disabling it takes a path to an empty source rather than the word "none": pointing it
at `/dev/null` makes the dedicated `UserKnownHostsFile` above the sole host-trust source for this
adapter's connections, with no fallback to the system-wide known-hosts database. Together these
close the gap the pinned-host-identity pin above does not by itself: a correct
`UserKnownHostsFile` pin still leaves a hostile local `~/.ssh/config` or a stale
`/etc/ssh/ssh_known_hosts` entry able to redirect or pre-authorize a connection before the pin is
even consulted. Acceptance property: a hostile `~/.ssh/config` `ProxyCommand` or `Match exec`
directive placed on the invoking account never executes as part of this adapter's `ssh`
invocations, and an entry in the global known-hosts database that would otherwise satisfy host
verification does not substitute for a matching entry in the dedicated
`/etc/khive/imessage/known_hosts` file.

**Transport deadlines and recovery.** Each `ssh` invocation runs under a total wall-clock
deadline of 35 seconds enforced by the adapter itself, which kills and reaps the child process on
expiry rather than allowing a hung invocation to accumulate indefinitely. Connection
establishment is separately bounded by a 10-second connect timeout passed as an `ssh` option
(`-o ConnectTimeout=10`), so a bridge host that is up but not accepting connections fails fast
instead of consuming the whole 35-second deadline waiting on a connection that will never
establish.

A tick that drains multiple pages MAY pipeline those `poll` operations over a single SSH session
rather than opening one session per page -- the wire framing permits both (§Typed wire schema) -- so
a multi-page drain amortizes the connection handshake across its pages instead of paying one
handshake per page. Session reuse is a transport optimization only and changes no correctness
property: the 35-second invocation deadline still bounds each session and each `poll` still runs
under its own scan budget, so a session carries as many pages as fit its deadline and any page
beyond that resumes from the last committed checkpoint -- the same page-granular resumption the
10-page cap and `budget_truncated` paths already define (§Bounded drain per tick).

Consecutive transport failures back off exponentially between poll ticks, doubling from a
1-second floor and capped at 5 minutes; a transport failure increments the consecutive-failure
counter, and the counter resets to zero on the next success. Acceptance property: an invocation
that hangs past its deadline is killed and reaped at the deadline, and the adapter's next tick
proceeds only after the backoff interval for the current consecutive-failure count has elapsed.

**SSH target validation and the end-of-options delimiter.** `KHIVE_IMESSAGE_SSH_TARGET` is
validated fail-closed at config load, not at first use: it must match a restrictive shape, a
mandatory `user@` prefix naming the bridge account the one-time setup provisioned the restricted
`authorized_keys` entry and `Match User` lockdown against, followed by a hostname or address; a value
with no `user@` prefix, beginning with `-`, or containing whitespace or a control character, is
rejected -- `ImessageChannelConfig::from_env` returns `ChannelError::Config` and the channel does not
start. The username is mandatory because the entire server-side confinement below (§Server-side key
confinement, §Bridge-account authentication lockdown) is scoped to the bridge account: a target with
no `user@` lets `ssh` default the remote user to the daemon's own local login name, routing
authentication to whatever account that name maps to on the bridge host -- an account carrying
neither the forced-command `authorized_keys` entry nor the `Match User` lockdown -- so that entire
confinement would simply not apply to the session. This closes a specific attack: an
option-prefixed target such as `-oProxyCommand=...` would otherwise be parsed by the `ssh` client
as an option rather than a destination, and a `ProxyCommand` override executes an arbitrary local
command under the daemon account. Fixed argv construction alone does not close this, because argv
construction fixes how arguments are assembled, not what the target string itself is interpreted
as once `ssh` parses it. As defense in depth beyond the validation gate, every `ssh` invocation
this adapter makes additionally passes the `--` end-of-options delimiter immediately before the
target argument, so even a target value that somehow reached the invocation unvalidated can never
be parsed as an `ssh` option. Acceptance property: an option-prefixed `KHIVE_IMESSAGE_SSH_TARGET`
value refuses channel start.

Presence and shape of the `user@` prefix are necessary but not sufficient: the adapter also pins the
username to a specific value. One-time setup records the provisioned bridge-account name in a
dedicated provisioned file, `/etc/khive/imessage/bridge_account`, holding the bare account name as its
entire contents: a fixed absolute path under the same root-owned `/etc/khive/imessage/` provisioning
root as the pinned known-hosts and bridge-key files (§Provisioned files), carrying the same integrity
shape -- root-owned, writable by root alone, non-symlink, resolved through a root-owned and
root-only-writable directory chain (§Pinned known-hosts file integrity) -- so a compromised daemon
cannot rewrite its own comparator target. Config-load reads this file once and requires the username parsed from
`KHIVE_IMESSAGE_SSH_TARGET` to equal that pinned account exactly -- any other username, even a
syntactically valid one, returns `ChannelError::Config` and the channel does not start. This is the
client-side complement to the server-side single-account key confinement (§Server-side key
confinement): the server side guarantees a stolen key can authenticate only as the bridge account,
while this check refuses a target repointed at any other account at config load on the daemon host,
before an `ssh` invocation is ever made, rather than relying on the remote `sshd` to reject a
mispointed session after the fact. It also fails closed loudly in the provisioning-error case where
the dedicated key was mistakenly installed on a second account -- a case the server-side
single-account invariant assumes away but cannot itself detect. Acceptance property: a
`KHIVE_IMESSAGE_SSH_TARGET` whose username differs from the setup-pinned bridge account refuses
channel start, independent of whether the named account exists on the bridge host.

### Remote command boundary: a fixed helper, data-only stdin

Every `ssh` invocation this adapter makes runs exactly one fixed, pre-installed remote command on
the bridge host, never a command line assembled from configuration or runtime values. OpenSSH does
not preserve a local argv array across the wire: the client space-joins the remote command and its
arguments into a single string that the remote shell re-parses, so a dynamic value placed anywhere
in that remote command line is exposed to remote shell parsing regardless of how carefully the
local argv was constructed -- fixed local argv construction alone does not close this, because it
governs only how the local `ssh` process is invoked, not what the string it sends across the wire
is interpreted as once it reaches the remote shell. Every caller-supplied value this adapter sends the helper -- the poll floor for a `poll`, the
message text for a `send` -- other than the SSH target itself, which the local `ssh` client consumes
directly and never forwards into the remote command line, is therefore never interpolated into the
remote command line in any form, quoted or otherwise. The database path and the recipient are not
caller-supplied at all (§Helper-side authority pinning): they are fixed in the helper's root-owned
provisioning configuration, so the question of interpolating them does not arise.

Instead, each SSH invocation runs a single fixed remote helper: a pre-written script or small
binary installed on the bridge host during one-time setup and invoked by a literal,
unparameterized name. Every dynamic value the helper needs from the caller -- the poll floor for a `poll`, the message
text for a `send` -- is delivered to it over stdin as structured, data-only input that the helper
reads and interprets itself; none of it is ever concatenated into the command the remote shell
parses. The database path and the outbound recipient are not among these caller-supplied values:
they are helper-fixed at provisioning (§Helper-side authority pinning), so no wire field carries
either and the caller cannot name them. A fixed remote command with data-only stdin is structurally
injection-proof: there is no remote parsing step a crafted value could influence, because the
value never occupies a position the remote shell treats as syntax. This is preferred over
validating the caller-supplied message text against a metacharacter grammar and passing the
validated result as a remote argument, because an allowlist grammar is a rule that must anticipate
every dangerous shape and rots as shell and AppleScript escaping rules shift across macOS releases;
a fixed command with no argument position for the value at all has no such surface to rot. Regression
coverage names the adversarial case directly: a test asserting that an outbound message body
containing shell metacharacters (backticks, `$( )`, semicolons, quotes) cannot alter the remote
command the bridge host executes, distinct from and in
addition to the local-side `KHIVE_IMESSAGE_SSH_TARGET` validation above.

### Helper artifact and protocol versioning

The fixed remote helper is an owned, versioned artifact, not an ad hoc script assembled at setup
time. It ships as a small script (or binary, for environments where a shell dependency is
undesirable) checked into the `khive-channel-imessage` crate alongside the adapter code that
invokes it, so the helper and the adapter's expectations of it evolve together in the same
repository. One-time bridge-host setup copies this checked-in artifact to a fixed path on
the bridge host (the same path named in the forced `command=` binding below) and records its
embedded version alongside the other one-time provisioning steps (host-key pinning, permission
grants) described elsewhere in this document; upgrading the helper is a deliberate, operator-run
re-copy, never something the daemon does to the bridge host over the SSH channel it also uses for
polling and sending.

**Helper install integrity.** The forced `command=` binding (§Server-side key confinement) runs
whatever binary sits at the helper's installed absolute path, so that path is a trust anchor as
load-bearing as the pinned key: if the bridge account could replace the helper binary, it could
replace the program the forced command runs. One-time setup therefore installs the helper binary, its
containing directory chain up to root, its root-owned provisioning configuration (the database path,
maintainer handle, and recipient pins the helper reads, §Helper-side authority pinning), and the
`/etc/khive/imessage/scan_floors` ledger (§Floor storage) all root-owned, mode carrying no group- or
other-write bit -- writable by root alone, not merely unwritable by the bridge account, since a mode
that excludes only the bridge account can still admit some other non-root local principal on the
host -- the same integrity shape the pinned known-hosts file carries (§Pinned known-hosts file
integrity). A bridge account, or any other non-root local principal, that could write any of these
could redirect the forced
command, relax the helper's authority pins, or move a scan floor from the account the forced command
already runs as; rooting them outside every non-root account's reach is what keeps the helper's own authority
pins meaningful against a compromised bridge account, not only against a compromised daemon. Setup
verifies each and refuses to complete when any is a symlink, is not root-owned, carries a group- or
other-write permission bit, or
resolves through a directory that is not similarly root-owned and root-only-writable (acceptance property 35).

**Helper execution hygiene.** The server-side directive-family table (§Directive family) closes the
paths by which the SSH server itself could inject an environment or substitute a command into the
helper's process, so the forced command starts with no client- or attacker-supplied variable in its
environment to begin with. Sanitizing the loader- and interpreter-hijacking variables from _inside_
the helper, after its interpreter has already started, would be too late for exactly the variables
that matter: `DYLD_INSERT_LIBRARIES` and the other `DYLD_*` dynamic-loader controls, and `LD_PRELOAD`,
are consumed by the dynamic loader before the program's `main` runs, and `BASH_ENV`/`ENV` are read by
a shell before it reaches the script body. The forced command is therefore not the helper interpreter
directly but a fixed pre-interpreter launcher that establishes a clean environment _before_ the
interpreter or loader runs: its `command=` binding invokes `/usr/bin/env -i` with an explicit minimal
environment -- an absolute `PATH` and only the variables the helper genuinely needs -- and then execs
the recorded artifact, the interpreter at an absolute path for an interpreted helper or the compiled
binary directly, so the loader and any shell see only the launcher-set environment and never a leaked
`DYLD_*`, `LD_PRELOAD`, `BASH_ENV`, `ENV`, or `IFS`. The launcher binary itself (`/usr/bin/env`) is a
SIP-protected system binary whose dynamic loader ignores `DYLD_*` injection, and the SSH boundary
above has already kept any such variable out of the forced command's initial environment, so nothing
hijacks the launcher before it clears the environment. Every subprocess the helper then spawns --
`/usr/bin/osascript` for a `send` above all -- is invoked by absolute path, never by a bare name
resolved through an inherited `PATH`, so a binary planted earlier in a leaked `PATH` cannot shadow the
system one. The helper additionally re-clears these same variables at its own entry as defense-in-depth
beneath the launcher, not as the boundary. The helper's artifact type is pinned once at provisioning
rather than chosen per invocation: one-time setup records whether the installed artifact is the
interpreted script or the compiled binary, and the recorded type fixes both which artifact the launcher
execs and the hygiene contract -- an interpreted helper presents an interpreter-startup surface the
launcher's pre-cleared environment closes, while a compiled helper presents none. Leaving the type
ambiguous, as an earlier form of this section did, would leave the injection surface ambiguous with it.
Acceptance property: the forced command's `command=` binding is the pre-interpreter `env -i` launcher,
not a bare interpreter invocation, so the helper's interpreter and dynamic loader start under an
environment carrying only the launcher-set variables; setup verifies the recorded forced command has
this launcher shape.

The stdin/stdout contract between the adapter and the helper is itself versioned, not an
implicit, unversioned shape inferred from field names. Every request the adapter writes to the
helper's stdin carries an explicit protocol version field and an operation discriminator --
`activation`, `poll`, `identity`, `send`, or any future operation (§The concrete operation set) --
and the helper's structured stdout response carries the same
protocol version field back alongside either a result payload or a structured error. The adapter
refuses to proceed -- treating the invocation as a transport failure under §Transport deadlines
and recovery above -- when the version the helper reports falls outside the adapter's supported
window; a version outside that window is a hard refusal, never a silent best-effort attempt to
interpret an unrecognized shape. The window is defined narrowly and explicitly, because the helper
is upgraded independently of the daemon (a bridge-host-local, operator-run step, above): the adapter
accepts a helper reporting either its own preferred version or exactly one version behind it, and
refuses cleanly when the gap is larger. The tolerance is this negotiated one-behind window and
nothing wider -- the daemon's own release upgrades the adapter first, and the bridge-host helper is
expected to follow before the adapter raises its minimum-supported version (preferred minus one) past
what the deployed helper reports, so a correctly sequenced upgrade never leaves the helper outside the
window.

**Authority-floor invariant (the one-behind window must never span an authority change).** The
one-version-behind tolerance is a compatibility convenience, not a security relaxation, and it is
bounded by a hard rule: the compatibility window MUST NOT span a version in which the helper's
authority semantics changed. The helper enforces the trust boundary's substantive pins -- the
setup-fixed database identity, the maintainer conversation and sender-handle filter, the outbound
recipient, and the scan floor (§Helper-side authority pinning, §Helper-enforced scan floor). Any
protocol version that adds, removes, or alters one of those authority checks establishes a hard
floor: an adapter at the newer version refuses a helper reporting any version below that floor
outright, never accepting it under the one-behind tolerance, because a helper predating an
authority-semantics change may not enforce the pin the newer adapter relies on. Without this rule a
compromised daemon could deliberately pair a current adapter with a one-version-behind helper that
predates a database-identity or recipient pin and, through that older helper, reach an
FDA-accessible database or a recipient the newer authority check would have refused -- turning the
compatibility window itself into the bypass. The one-behind tolerance therefore applies only across
versions whose authority semantics are identical; an authority-affecting bump is a floor, and the
adapter's minimum-supported version is raised to it, not to preferred-minus-one. This floor is a
property of the version, recorded in the typed protocol schema (§The concrete operation set) so the
adapter distinguishes an authority-affecting version from a compatible one rather than making a
runtime judgment.

This versioned request/response/error contract is
the protocol half of the trust boundary the forced-command binding below establishes on the server
side: the boundary is not just "only this one binary can run" but "only this one binary, speaking
a version it explicitly negotiates, can run." Acceptance property: an adapter invocation against a
helper reporting a protocol version the adapter does not support fails closed as a transport
failure, without attempting to parse the mismatched response as if it were a supported shape.

**The concrete operation set.** The versioned request/response contract carries a fixed, closed set
of operation discriminators; an unrecognized discriminator is a structured error, never a
best-effort guess. The operations are:

- `activation` -- request: no row-selecting fields; this operation imports no message history.
  Response: the current maximum `ROWID` in the messages table (the whole-table high-water mark, not
  the maintainer-filtered subset, so no later message can fall below it), the `guid` of the row at
  that `ROWID` as the initial identity anchor, and the database-identity token the helper pins
  (§Helper-side authority pinning). When the messages table is empty at activation there is no row
  to anchor: the helper reports `max_rowid` as 0 -- a floor below every future `ROWID`, since SQLite
  assigns `ROWID`s from 1 -- and `anchor_guid` as null, and the identity anchor is established from
  the first row the adapter later observes. This empty-table branch is a defensive wire semantic that
  a provisioned deployment never exercises: one-time setup refuses to provision a database whose
  messages table is empty and records only a non-null anchor (§Floor storage), so activation against a
  provisioned database always returns a non-null `anchor_guid` and a `max_rowid` at or above 1. The
  adapter records the returned maximum `ROWID` as its starting
  checkpoint and scan floor: the first `poll` after activation requests rows strictly above it, so
  activation establishes a forward-only starting point without ingesting a single pre-existing row.
  This is the first operation a freshly provisioned adapter issues -- the only way to obtain a
  starting `ROWID` and its anchor `guid` without a history-bearing scan -- and it carries no message
  text, sender, service, or conversation membership for any row.
- `poll` -- request: the `ROWID` scan floor, and nothing else that selects rows; the
  maintainer-conversation and sender-handle filter and the page size are fixed at the helper
  (§Helper-side authority pinning, §Bounded drain per tick), not caller-supplied. Response: the
  maintainer-matching rows of the scanned window, each carrying the fields ingest needs (`ROWID`,
  `guid`, sender handle, service, text, timestamp, and `is_from_me`); the count of rows scanned; and
  the page's scan floor as a `ROWID` together with the `guid` of the row at that floor -- identity-only
  metadata for the checkpoint anchor, emitted even when that row was withheld or dropped, and never
  carrying a withheld row's text, sender, service, or conversation membership. `is_from_me` is a
  required response field because the adapter's `is_from_me = 0` sender-validation check (§Sender
  validation) can only run on a field the response actually carries -- without it the self-echo
  exclusion the adapter is required to enforce would have nothing to test. The helper MAY additionally
  drop `is_from_me = 1` rows on the bridge side as defense in depth, but the field is mandatory in the
  response either way so the adapter can enforce the check itself. The response also echoes the `guid`
  at the requested scan floor -- the checkpoint anchor the adapter sent -- so the per-tick identity
  guard (§Identity guard and forward-only reset) can confirm its stored high-water mark still names
  the same row without a separate `identity` round-trip; this echoed `guid` is absent only when no row
  remains at that `ROWID`.
- `identity` -- request: a single `ROWID` together with the `guid` the caller already holds for it
  (the stored anchor it is re-verifying). Response: `found` -- true when a row still
  exists at that `ROWID`, false when none does -- and `matches`, a boolean when `found` is true (true
  when the row's `guid` equals the supplied one, false when it differs) and `null` when `found` is
  false, so the nullable `matches` is always present and never omitted (§Typed wire schema). The helper never
  returns the row's actual `guid`, so this operation verifies an anchor the caller already holds
  rather than disclosing one it does not: a caller cannot use it to learn the `guid` of a `ROWID`
  whose `guid` it has not already observed, which closes the arbitrary-`ROWID`-to-`guid` enumeration a
  disclosure form would expose while still answering the only question the identity guard asks --
  whether the row at the caller's checkpoint still carries the caller's stored anchor, regardless of
  that row's conversation membership. A `ROWID` below the helper's recorded scan floor (§Helper-enforced
  scan floor) is refused with the `below_scan_floor` structured error rather than verified, exactly as
  `poll` refuses a sub-floor request (§Typed wire schema, error codes): a legitimate anchor
  re-verification only ever names a checkpoint row, which sits at or above the activation checkpoint
  and therefore at or above the floor, so this refuses nothing the identity guard legitimately asks
  for. The `found` false result is reserved for the distinct, legitimate case of an at-or-above-floor
  anchor row that has since been deleted, which the identity guard reads as a signal to reset
  forward-only. The per-tick guard normally needs no `identity` round-trip at all -- the `poll`
  response already echoes the `guid` at the requested scan floor (the sanctioned checkpoint-anchor
  disclosure, above, which a reset needs to seed the new anchor), and the guard compares that against
  its stored anchor -- so `identity` is the standalone re-verification path for a check made outside a
  poll, disclosing nothing where `poll` must disclose the new floor `guid` (§Identity guard and
  forward-only reset).
- `send` -- request: the message body as data plus the daemon-derived `send_id` idempotency key
  (§Outbound delivery idempotency); the recipient is fixed at the helper, not caller-selected.
  Response: a structured success or a structured error.

Every response carries the protocol version field (above) and is either a result payload for the
requested operation or a structured error of the form `{code, message}`. A helper that cannot
satisfy a request -- an unsupported version, an unknown operation, or a request that violates the
helper-enforced authority pins or scan floor -- returns the structured error and performs no side
effect. Acceptance property: a request bearing an operation discriminator outside this closed set is
refused with a structured error and executed as no operation, never interpreted as the nearest known
operation.

**Typed wire schema.** The prose above fixes the operations; this schema fixes their bytes, so an
implementation of either side has one normative reference rather than a description to reconstruct.

_Framing._ Each request and each response is a single self-delimiting frame: a four-byte big-endian
unsigned length prefix followed by exactly that many bytes of the serialized envelope. The helper
reads one request frame from stdin, writes one response frame to stdout, and processes further
request frames in order until stdin reaches EOF; framing never depends on stream position or on the
SSH session boundary, so a tick MAY pipeline several operations over one session or open one session
per operation without changing the wire contract. A frame whose declared length exceeds a fixed
maximum, or whose body fails to deserialize, is a `malformed_request` for a request, and a transport
failure the adapter fails closed on for a response; neither side parses a truncated or oversized
frame.

_Serialization._ Each envelope is serialized as canonical JSON encoded in UTF-8: the frame body is
the JSON text and the four-byte length prefix counts its bytes. Canonical form fixes the bytes both
sides produce -- object keys in the order the schema tables below list them, no insignificant
whitespace, no byte-order mark, and strings escaped by the shortest legal JSON escape. Every numeric
field (`protocol_version`, `max_rowid`, `scan_floor`, `rowid`, `next_floor`, `timestamp`, and the
counts) is a JSON integer within its stated `u16` or `i64` range; the protocol carries no
floating-point value, so `NaN`, `Infinity`, and any non-integer number are malformed. A nullable
field is encoded as JSON `null` when absent, never omitted, and a non-nullable field is never `null`;
a missing non-nullable field, or a present-but-`null` one, is a `malformed_request` for a request and
a fail-closed transport failure for a response. Enum fields (`op`, `page_status`, `error.code`) are
the exact lowercase discriminator strings named below. The preferred protocol version is **1**. The
authority-floor registry (§Helper artifact and protocol versioning) is a table mapping each protocol
version to its authority floor; at version 1 it holds a single row -- version 1, floor 1 -- and every
later version that changes an authority check adds a row recording the floor it establishes, so both
sides read the same version-to-floor map rather than hard-coding it.

_Request envelope._ Every request carries exactly these fields:

| Field              | Type               | Nullable | Meaning                                                                           |
| ------------------ | ------------------ | -------- | --------------------------------------------------------------------------------- |
| `protocol_version` | u16                | no       | The adapter's spoken protocol version (§Helper artifact and protocol versioning). |
| `op`               | enum discriminator | no       | Exactly one of `activation`, `poll`, `identity`, `send`.                          |
| `body`             | op-specific        | no       | The payload for `op`; shapes below. An empty object for `activation`.             |

_Response envelope._ Every response carries the protocol version and exactly one of a result or a
structured error:

| Field              | Type                | Nullable | Meaning                                                                                                                                       |
| ------------------ | ------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `protocol_version` | u16                 | no       | The version the helper processed the request at, or on an `unsupported_version` error the helper's preferred version (see negotiation below). |
| `result`           | op-specific payload | yes      | Present if and only if the operation succeeded; shapes below.                                                                                 |
| `error`            | `{code, message}`   | yes      | Present if and only if the operation failed. `code` is drawn from the closed set below; `message` is human-readable and non-authoritative.    |

Exactly one of `result` and `error` is present; a response carrying both or neither is a transport
failure the adapter fails closed on.

_Operation payloads._

| Op           | Request body fields                       | Success `result` fields                                                                                                                                                                                                                                                                                                                                    |
| ------------ | ----------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `activation` | (none)                                    | `max_rowid` i64 (no); `anchor_guid` string (yes, null when the messages table is empty at activation, in which case `max_rowid` is 0); `db_identity` string (no); `maintainer_handle` string (no) -- the setup-time-fixed provisioned handle the helper filters inbound on, carried back so the adapter validates against it rather than a mutable env var |
| `poll`       | `scan_floor` i64 (no)                     | `rows` array of MessageRow (no, possibly empty); `page_status` enum (no); `scanned_count` i64 (no); `next_floor` i64 (no); `next_floor_guid` string (yes); `floor_guid` string (yes)                                                                                                                                                                       |
| `identity`   | `rowid` i64 (no); `guid` string (no)      | `found` bool (no); `matches` bool (yes; `null` when `found` is false, a boolean when `found` is true) -- true when the row at `rowid` still carries the supplied `guid`, false when it carries a different one; the helper never returns the row's actual `guid`                                                                                           |
| `send`       | `body` string (no); `send_id` string (no) | empty object -- success is the absence of `error`                                                                                                                                                                                                                                                                                                          |

`MessageRow` fields: `rowid` i64 (no); `guid` string (no); `sender_handle` string (yes); `service`
string (yes); `text` string (yes, null for a textless row such as a bare attachment); `text_truncated`
bool (no); `timestamp` i64 (no); `is_from_me` bool (no). `text_truncated` is true exactly when the
helper capped an oversized body at the 1 MiB per-row text cap (§Oversized single rows) and false on
every row whose text was carried whole. `timestamp` is the raw `message.date` value from `chat.db` on
a modern macOS bridge: an integer count of **nanoseconds since the Apple/Core Data reference epoch
2001-01-01T00:00:00Z (UTC)**, not the Unix epoch. The adapter converts it to the RFC 3339 string
`comm.ingest` stores in `properties.sent_at` by `unix_seconds = timestamp / 1_000_000_000 +
978_307_200` (978,307,200 is the offset in seconds from 1970-01-01 to 2001-01-01), then formatting
that instant as RFC 3339 in UTC. The design assumes a modern macOS bridge, whose `chat.db` stores
`date` in nanoseconds; the adapter treats a `timestamp` whose magnitude is inconsistent with that unit
-- for example a legacy whole-seconds value from a pre-High-Sierra store -- as an environment
misconfiguration it surfaces loudly rather than silently converting to a `sent_at` decades off.
`next_floor_guid` is the `guid` of the row at `next_floor`, is
emitted even when that row was withheld from `rows`, and is null when `next_floor` names no row (an
empty table, or a `caught_up` page whose `next_floor` is 0); `floor_guid` is the `guid` at the requested
`scan_floor` (null when no row remains there). Like the `identity` response, both carry identity
metadata only, never a withheld row's `sender_handle`, `service`, `text`, or conversation membership.
`page_status` is one of `page_full` (a full page of candidates was returned and more may remain above
`next_floor`), `caught_up` (fewer than a full page of candidates remained before the scan reached the end of
the table, so the helper advanced `next_floor` to the greatest `ROWID` the scan itself examined --
not a separately read current maximum, so a row inserted after the scan's snapshot keeps a `ROWID`
above `next_floor` and is caught on a later poll rather than skipped), or `budget_truncated` (a mandatory scan budget
stopped the page before completion and candidate rows may remain above `next_floor`); it is the
explicit successor to inferring caught-up from a short row count (§Bounded drain per tick).

The `send` request carries, besides `body`, a required `send_id` string -- the daemon-derived
idempotency key the helper dedups deliveries on (§Outbound delivery idempotency). It is not
caller-selected: the daemon derives it from the outbound note's own stable identity, and a `send`
frame that omits it is a `malformed_request` like any other missing required field.

_Error codes (closed set)._ `code` is exactly one of:

| `code`                | Meaning                                                                                                                                                                 |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `unsupported_version` | The request's `protocol_version` is outside the helper's window or below an authority floor (§Helper artifact and protocol versioning).                                 |
| `unknown_operation`   | The `op` discriminator is outside the closed set.                                                                                                                       |
| `malformed_request`   | The request frame failed to deserialize, or a required field was absent or ill-typed.                                                                                   |
| `below_scan_floor`    | A `poll` or `identity` request named a `ROWID` below the recorded scan floor (§Helper-enforced scan floor).                                                             |
| `not_authorized`      | The request could not be authorized against a helper-fixed pin -- for example a database whose content identity does not match the provisioned record (§Floor storage). |
| `internal_error`      | The helper encountered a local failure (for example the database could not be opened) and performed no side effect.                                                     |

An `error` whose `code` is not one of these is itself a protocol violation the adapter treats as a
transport failure, exactly as it treats an unknown `op`. Every error path performs no side effect.

_Version negotiation._ Both envelopes carry `protocol_version`. The adapter sends its own preferred
version. The helper accepts a request whose `protocol_version` equals its preferred version or is
exactly one below it AND is not below any authority floor (§Helper artifact and protocol versioning),
processes the request at that version, and stamps its response with the same version it processed --
it speaks down to the adapter within the one-behind window rather than answering at a version the
adapter did not send. A `protocol_version` above the helper's preferred version, more than one below
it, or below an authority floor yields an `unsupported_version` error and no side effect; the
adapter, on receiving a response whose version it did not send or an `unsupported_version` error,
fails closed as a transport failure without parsing the payload as a supported shape -- save for the
single one-shot retry defined next, the more specific rule that governs the first `unsupported_version`
in an exchange and takes precedence over this general fail-closed statement. The negotiated
version is thus the lower of the two preferred versions when that lies within one of both and above
every authority floor, and no exchange ever proceeds at a version either side did not agree to. The
version every window and authority-floor check is evaluated against is the `protocol_version` the
helper stamps in its response -- the version it processed and whose authority semantics it enforced
on that request, which the negotiation rule requires to equal the version the adapter sent. Neither
side's separately-held preferred version, nor the request version considered apart from the response
that echoes it, is the authority for these checks; a floor test (§Helper artifact and protocol
versioning) is applied to the exact version the exchange proceeded at, so an authority-affecting
version can never be honored through a response the helper stamped at a lower one.

The one-behind window is only reachable if the side that is behind can be discovered. When the helper
is the one a version behind -- its preferred version is the adapter's preferred minus one -- the
adapter's first request, sent at the adapter's preferred version, is above the helper's preferred and
draws an `unsupported_version` error rather than a negotiated downgrade, because the helper never
answers at a version above its own. To make the window reachable rather than a dead end, an
`unsupported_version` response carries the helper's preferred version in its `protocol_version` field
-- the one version the helper is offering -- and the adapter retries exactly once at that version if
and only if it lies within the adapter's own supported window (at or above the adapter's minimum,
preferred minus one) and at or above every authority floor the adapter enforces (§Authority-floor
invariant). A helper-preferred version below the adapter's minimum, or below any authority floor, is
not retried: the adapter fails closed as before, because retrying there would either exceed the
one-behind tolerance or cross an authority-semantics change. The retry is one-shot -- a second
`unsupported_version` is a hard transport failure -- so negotiation converges in at most two
exchanges and never loops.

Acceptance property: a first-activation adapter obtains its starting `ROWID` and anchor `guid` from
an `activation` response alone, ingesting no pre-existing message, and a subsequent `poll` at that
floor returns only rows strictly above it. Acceptance property: for each operation, a response
missing a required field of the shape above, or carrying both a `result` and an `error`, is refused
as a transport failure and applied as no operation.

### Server-side key confinement: forced command and `restrict`

Pinning the client to a fixed remote helper and data-only stdin (above) constrains what the
daemon's own `ssh` invocations can do, but by itself says nothing about what the bridge host will
accept from that key if the key is ever exfiltrated. The bridge account carries Full Disk Access
and Automation permission for the Messages application (§Host requirements below), so a stolen key
usable for an arbitrary interactive session is arbitrary remote code execution against those
grants. This adapter therefore requires a matching server-side constraint on the same key, not
merely a client-side convention: the daemon's SSH public key, as provisioned in the bridge
account's `authorized_keys` during one-time setup, carries the `restrict` option together with a
forced command binding it to the fixed remote helper's absolute installed path --
`restrict,command="/absolute/path/to/imessage-bridge-helper" ssh-ed25519 AAAA...` in
`authorized_keys` shape. `restrict` disables PTY allocation, port forwarding, agent forwarding, and
X11 forwarding for sessions opened with that key. One-time setup provisions the bridge account's
`authorized_keys` with exactly this single entry and no other: the restricted-key boundary holds
only if no second, less-restricted entry exists in the same file for a fallback or mismatched
client identity to match, so setup installs this entry as the file's sole content and verifies no
other key is present.

**The dedicated key is authorized on exactly one account.** The mandatory `user@` in
`KHIVE_IMESSAGE_SSH_TARGET` (§SSH target validation) is validated on the daemon side, which is the
side an attacker who has compromised the daemon controls: a compromised daemon can point the target
at any account it likes, so daemon-side validation cannot on its own guarantee the session lands on
the confined bridge account. The binding guarantee is a provisioning-side invariant on the bridge
host: the dedicated bridge public key is installed in the bridge account's `authorized_keys` and in
**no other account's** `authorized_keys` anywhere on the bridge host -- not root's, not the
operator's own, not a second service account's. SSH public-key authentication succeeds only for an
account whose `authorized_keys` lists the presented key, so a key authorized on exactly one account
can only ever open a session as that account, whatever remote username the client names. A
compromised daemon that rewrites the target's `user@` to some other account then simply fails to
authenticate there rather than reaching an account without the forced-command and `Match`-block
confinement. One-time setup installs the key only in the bridge account and verifies it is absent
from every other account's `authorized_keys`; this is the provisioning-side complement to the
daemon-side `user@` validation, and it is the half that holds when the daemon is the adversary.

The absent-from-every-other-account check above covers only static `authorized_keys` files, and a
static-file check alone is not sufficient. A dynamic key source -- an `AuthorizedKeysCommand` or an
`AuthorizedPrincipalsCommand` configured for another account, whether globally or in a `Match` block
-- can authorize a presented key without that key appearing in any `authorized_keys` file at all. If
any account's effective configuration ran such a command and it returned the pinned key, or a
certificate principal that authorized it, the key could open a session as that account, outside the
single restricted entry, and the exactly-one-account guarantee would not hold. One-time setup
therefore also verifies host-wide that no dynamic key source is configured anywhere, and it does so by
a textual scan of the effective `sshd` configuration files -- the main `sshd_config` and every file it
pulls in through an `Include` directive -- refusing to complete if `AuthorizedKeysCommand` or
`AuthorizedPrincipalsCommand` is set to any value other than `none` in any context: the global scope
or any `Match` block, whatever that block's match criteria. The value `none` explicitly disables the
directive; it is the value the bridge account's own `Match` block sets for both (§Bridge-account
authentication lockdown) to clear any inherited global setting, so it is the one value the scan
accepts rather than rejects. Any other value names a program or file that could return the pinned key,
or a certificate principal authorizing it, which is precisely the surface this scan closes. A textual scan rather than an `sshd -T -C user=<account>` enumeration is
deliberate and load-bearing: `sshd -T -C` reports the effective configuration for one connection tuple
at a time, so a directive scoped by a `Match` block keyed on address, host, or another non-user
predicate surfaces only for the specific tuple that matches it, and no finite enumeration of accounts
can guarantee it was reached -- whereas a non-`none` setting of the directive anywhere in the config
text is decidable outright. A bridge host that configures either directive with a non-`none` value for any
account, in any `Match` context, is out of contract for this adapter: the exactly-one-account
guarantee is stated against a host whose key authorization is entirely static, and setup refuses to
complete -- provisioning no key whose confinement it cannot vouch for -- when it finds a dynamic key
source enabled anywhere in the host's effective configuration. Acceptance property: one-time setup
fails, and no key is provisioned, on a bridge host whose effective `sshd` configuration files set
`AuthorizedKeysCommand` or `AuthorizedPrincipalsCommand` to any value other than `none` in any
context, global or `Match`-scoped, regardless of the `Match` block's criteria; a host on which both
directives are absent or set to `none` everywhere passes this check.

TUN-device forwarding is governed by a separate
directive, not by `restrict`: OpenSSH gates it with the host-wide `PermitTunnel` setting in the
bridge host's own `sshd_config` (default `no`), and `authorized_keys` has no per-key option that
disables it outright -- only `tunnel="n"`, which pins the allowed tun device number for a key that
is otherwise permitted to request one. One-time bridge-host setup therefore also confirms
`sshd_config` sets `PermitTunnel no` explicitly, rather than treating `restrict` as already
covering this case. The forced `command=` binding below does not mitigate tunnel forwarding: a
tun device forwards IP packets below the command layer and needs no shell on the other end, so
`PermitTunnel no` is the load-bearing control here and is pinned explicitly, matching this
document's fail-closed posture for every other bridge-host permission. Acceptance property: the
bridge host's `sshd_config` sets `PermitTunnel no` explicitly, and a session opened with this key
that requests tunnel-device forwarding is refused. The forced `command=` binding
means the bridge host's `sshd` runs the named helper for every session opened with that key
regardless of what command the client requests, so the client-side fixed-invocation convention
above is backed by a server-side guarantee rather than trusted as client behavior alone. Forced
command (server side) and data-only structured stdin (client and protocol side, above) are the two
halves of one trust boundary: even a fully compromised daemon holding this key, free to send
`ssh` whatever remote command string it chooses, cannot cause the bridge host to run anything but
the fixed helper, and the helper's own versioned protocol (above) is the only channel through
which that compromised daemon could attempt to make the helper misbehave. Acceptance property:
using this key to request an arbitrary remote command, a PTY, or any form of forwarding or
tunneling fails at the bridge host's `sshd`; only the fixed helper ever executes, regardless of
what remote command the client requests.

**Bridge-account authentication lockdown.** Provisioning the bridge account's `authorized_keys` with
a single restricted entry (above) constrains what a session opened through that entry may do, but it
is not the only way a session could be opened: a password, a keyboard-interactive prompt, a
CA-signed certificate, or a key injected by an `AuthorizedKeysCommand` could each authorize the
bridge account without ever matching the one restricted `authorized_keys` line, bypassing the
forced-command boundary entirely. One-time setup therefore also pins the bridge host's `sshd_config`,
scoped to the bridge account with a `Match User <bridge-account>` block, to accept only the
provisioned public key and nothing else: `AuthenticationMethods publickey`, `PasswordAuthentication
no`, `KbdInteractiveAuthentication no`, `HostbasedAuthentication no`, `GSSAPIAuthentication no`, and
`KerberosAuthentication no`, so no credential path other than the single provisioned public key
exists -- a non-public-key method that authenticated the bridge account would open a session
carrying no `authorized_keys` `command=` binding and so escape the forced command entirely, which is
why each such method is pinned off rather than left to the global default;
`AuthorizedKeysFile` set to a single root-owned file outside the bridge account's home -- under
`/etc/khive/imessage/`, non-writable by the bridge account and with a root-owned parent chain -- and
`AuthorizedKeysCommand none`, so neither a dynamically generated key list nor a key the bridge account
appends to a home-directory `authorized_keys` can authorize the account; and no certificate-authority
trust for this account --
`TrustedUserCAKeys none`, `AuthorizedPrincipalsFile none`, and `AuthorizedPrincipalsCommand none`
set explicitly within the `Match` block. A keyword merely omitted from a `Match` block inherits the
global `sshd_config` value rather than clearing it, so a globally configured `TrustedUserCAKeys`
would otherwise still authenticate a CA-signed certificate for this account even with the `Match`
block present; each of the three is therefore pinned to `none` rather than left unset -- so a
certificate signed by an otherwise trusted CA, or an authorized-principals file or command, cannot
authorize a session the single restricted entry does not list. The out-of-home `AuthorizedKeysFile`
location is load-bearing, not cosmetic: the default `~/.ssh/authorized_keys` sits in the bridge
account's own writable tree, so anything able to write as that account could append a second,
unrestricted key and open a session that never matches the forced-command entry -- defeating the
boundary from inside, without the daemon's pinned key. Rooting the file where the bridge account
cannot write it, and holding its integrity to the same symlink-and-writability audit as the helper
binary and provisioning configuration (property 35), keeps the account's authorization list editable
by root alone.

The same block, together with the bridge host's global configuration, also closes the
environment-injection path into the forced command. A session that authenticates but arrives
carrying attacker-chosen environment variables can influence the helper's execution before the
helper's own logic runs -- `LD_PRELOAD` against a dynamically linked helper, `BASH_ENV` or `ENV`
against a shell helper, a rewritten `PATH` -- so environment acceptance is a pre-helper
code-execution surface even though the forced command itself is fixed. Two directives govern it, and
their scope differs. `PermitUserEnvironment` (which processes `~/.ssh/environment` and
`environment=` options in `authorized_keys`) is **not** among the keywords a `Match` block may set in
OpenSSH's grammar, so it cannot be scoped to the bridge account; one-time setup pins it to `no` in
the bridge host's global `sshd_config`. It defaults to `no`, but is pinned explicitly to hold the
fail-closed posture and because enabling it globally would let an `authorized_keys` `environment=`
option or a `~/.ssh/environment` file inject variables into the forced command's environment.
`AcceptEnv` (which copies client-sent variables into the session environment) is `Match`-settable
but purely additive: no directive subtracts an inherited global `AcceptEnv` inside a `Match` block,
so a `Match`-block `AcceptEnv` can only widen, never clear, what a global `AcceptEnv` already
granted. One-time setup closes it from both directions. For the global-inherited case, it verifies the
bridge account's effective `AcceptEnv` is empty against the configuration `sshd -T -C
user=<bridge-account>` reports, not against the config text, which cannot show an empty
inherited-additive result the way an explicit `none` would. For the `Match`-scoped case, the same
host-wide textual scan that rejects a dynamic key source (above) also rejects any `AcceptEnv`
appearing in a `Match` block whatever its criteria, and any `PermitUserEnvironment` set to a value
other than `no`: a `Match` block keyed on the daemon host's address or hostname could otherwise grant
the bridge connection an environment variable that a user-only `-T -C` enumeration never surfaces, and
`PermitUserEnvironment` -- not `Match`-settable, so global only -- would let an `authorized_keys`
`environment=` option or a `~/.ssh/environment` file inject one. Together these keep the forced
command's environment free of any client-injected variable, `LD_PRELOAD`, `BASH_ENV`, or a rewritten
`PATH` among them.

**PAM session and authentication modules.** A third pre-helper code-execution and environment
surface is PAM. When `UsePAM yes` -- the macOS default -- the account, authentication, and
**session** modules of the configured PAM service run for every session regardless of
authentication method, and a session module executes code and sets environment in the session
before the forced command does, so a PAM stack configured on the bridge host reaches the forced
command's process from below the command layer exactly as an accepted `AcceptEnv` variable would.
`UsePAM` is not among the keywords a `Match` block may set, so it cannot be scoped to the bridge
account; setup pins it in the bridge host's global `sshd_config`. The posture is to refuse PAM as
the boundary: on a bridge host dedicated to this adapter (§Host requirements), one-time setup pins
global `UsePAM no`, so no PAM module runs for any session and the forced command is the first code
the connection executes. Where an operator cannot set global `UsePAM no` because the host is not
dedicated to the bridge, the fallback keeps PAM globally enabled but neutralises it for the bridge
account through the one PAM keyword that is `Match`-settable, `PAMServiceName`: the bridge account's
`Match` block names an inert PAM policy whose account, auth, and session stacks contain no module
that executes code or mutates the environment. Helper-entry environment sanitisation and the
pre-interpreter launcher (§Helper execution hygiene) are defence-in-depth beneath this boundary, not a
substitute for it; because PAM is refused or neutralised at the sshd level rather than accepted, that
launcher's role is confined to loader- and shell-startup variables and it neither does nor need
interpose against PAM, which never runs to be interposed against. Acceptance property: on the bridge host either global `UsePAM no` holds,
or the bridge account's effective PAM service names a policy with no code-executing or
environment-setting module -- verified for the connection tuple by `sshd -T -C` and for a
`Match`-scoped `PAMServiceName` by the same host-wide textual scan that rejects a dynamic key source
-- and the forced command's environment carries no variable a PAM session module placed.

The same block disables session forwarding at the server-configuration layer rather than
leaving it to the `restrict` option alone: `AllowTcpForwarding no`, `AllowStreamLocalForwarding no`, and
`PermitTunnel no` close TCP, Unix-domain-socket (stream-local), and tunnel-device forwarding for the
account explicitly, so no forwarding class opened as the bridge account depends on how a given OpenSSH
build interprets `restrict`. This makes the one restricted `authorized_keys` entry the sole credential that can
open a session as the bridge account, which is the precondition the forced-command and `restrict`
boundary above assumes rather than establishes. Acceptance property: an attempt to authenticate to
the bridge account by any method other than the provisioned restricted public key -- a password, a
keyboard-interactive prompt, a CA-signed certificate, or a key returned by an `AuthorizedKeysCommand`
-- is refused by the bridge host's `sshd`; only the single provisioned restricted key authenticates.
Because these authentication-method directives are `Match`-scopable and a block keyed on the daemon's
source address or host -- not on the bridge account -- could set a bypass-permitting
`AuthenticationMethods` for the exact tuple the daemon connects from that a user-only
`sshd -T -C user=<bridge-account>` enumeration would never surface, setup resolves the effective value
for the full connection tuple (`sshd -T -C user=<bridge-account> addr=<daemon-source-address>
host=<daemon-source-host>`) and, by the same host-wide textual scan that rejects a dynamic key source
and a `Match`-scoped `AcceptEnv`, refuses any of `AuthenticationMethods`, `HostbasedAuthentication`,
`GSSAPIAuthentication`, or `KerberosAuthentication` set to a value admitting a non-public-key method
in any `Match` block whatever its criteria.

**Directive family: the complete execution-and-environment surface.** The named checks above --
dynamic key source, environment injection, forwarding -- are instances of one contract, and
enumerating them a directive at a time leaves the next un-named directive open. One-time setup instead
audits the bridge host against the complete set of `sshd_config(5)` directives that can either change
which program runs for an authenticated connection or place environment into its session, classified
in the table below. The host-wide textual scan already described (the effective `sshd_config` plus
every `Include`d file, with `Match` blocks in scope whatever their criteria) enforces every row: a
`refused` directive fails setup on textual presence anywhere, a `value-gated` directive is accepted
only at the listed value, a `neutralized` directive is closed by a per-key option with the host value
not relied upon, and an `irrelevant` directive cannot reach the bridge account's forced-command session
at all.

| `sshd_config` directive           | What it can do                                                                  | Disposition in setup's audit                                                                                                                                                                              |
| --------------------------------- | ------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `ForceCommand`                    | Runs a command for every session, ignoring the key's `command=` and `~/.ssh/rc` | Refused on textual presence -- the forced command is bound through `authorized_keys`, and no in-contract host sets a server-side `ForceCommand`                                                           |
| `AuthorizedKeysCommand`           | Runs a program to supply authorized keys                                        | Value-gated: `none` for every account host-wide -- a non-`none` command on any account could return the pinned key unconfined (audit scope below)                                                         |
| `AuthorizedKeysCommandUser`       | Account the key-supplying program runs as                                       | Irrelevant once `AuthorizedKeysCommand` is `none`                                                                                                                                                         |
| `AuthorizedPrincipalsCommand`     | Runs a program to supply certificate principals                                 | Value-gated: only `none`                                                                                                                                                                                  |
| `AuthorizedPrincipalsCommandUser` | Account the principal-supplying program runs as                                 | Irrelevant once `AuthorizedPrincipalsCommand` is `none`                                                                                                                                                   |
| `AuthorizedKeysFile`              | Names the static key file(s)                                                    | Value-gated: the bridge account resolves to the single root-owned file outside its home (property 35); every other account's resolved files are audited to not contain the pinned key (audit scope below) |
| `AuthorizedPrincipalsFile`        | Names a certificate-principal file                                              | Value-gated: only `none`, with `TrustedUserCAKeys none`                                                                                                                                                   |
| `PermitUserRC`                    | Executes `~/.ssh/rc` on login                                                   | Neutralized by the key's `restrict` (`no-user-rc`); the `Match` block additionally sets `no`                                                                                                              |
| `ChrootDirectory`                 | Changes the session's filesystem root                                           | Value-gated: only `none` (the default)                                                                                                                                                                    |
| `Subsystem`                       | Defines a named subsystem command                                               | Irrelevant -- reached only by a client subsystem request the daemon never issues, and `command=` governs subsystem execution too                                                                          |
| `SshdSessionPath`, `SshdAuthPath` | Override the `sshd-session` / `sshd-auth` binaries invoked per connection       | Refused on textual presence -- test-only overrides of the per-connection SSH binaries                                                                                                                     |
| `SecurityKeyProvider`             | Loads a FIDO middleware library during authentication                           | Refused on textual presence -- the bridge key needs no authenticator library                                                                                                                              |
| `X11Forwarding`                   | Enables `xauth` execution (`XAuthLocation`) and a `DISPLAY` variable            | Value-gated: only `no` (its default), which leaves `XAuthLocation` and `X11UseLocalhost` unreached                                                                                                        |
| `AcceptEnv`                       | Copies client-sent variables into the session                                   | Refused on textual presence in any `Match` block; the global-inherited value is verified empty via `sshd -T -C user=<bridge-account>`                                                                     |
| `PermitUserEnvironment`           | Processes `~/.ssh/environment` and `environment=`                               | Value-gated: only `no` (global only -- not `Match`-settable)                                                                                                                                              |
| `SetEnv`                          | Sets environment for child sessions server-side                                 | Refused on textual presence -- it overrides even `AcceptEnv` and `PermitUserEnvironment`                                                                                                                  |
| `ExposeAuthInfo`                  | Exposes an auth-info file path via `SSH_USER_AUTH`                              | Value-gated: only `no` (its default)                                                                                                                                                                      |

The table is complete against the execution-and-environment surface of `sshd_config(5)` once the PAM
posture above is taken with it -- PAM is the one directive family in that surface whose disposition
needs the dedicated treatment given, so the table does not restate it. Every other directive the table
does not list governs authentication method, key exchange and cipher choice, networking, port and
socket forwarding, connection rate-limiting, or logging, none of which changes which program the
bridge host runs for this account's connection or what environment that program inherits. `command=`
and `restrict` on the provisioned key, `AuthenticationMethods publickey` with password,
keyboard-interactive, host-based, GSSAPI, and Kerberos authentication disabled, and the forwarding
directives pinned `no` (all above) are the per-session half of the same surface; this table is the
host-configuration half. The complete keyword-by-keyword disposition, so that no unlisted directive is
left to a later reading, is the appendix below. Enumerating the family as a closed set, rather than the specific
directives an audit has so far named, is what makes setup's refusal decidable against the whole surface
rather than a growing list of instances.

**Audit scope: every account, not only the bridge account.** The directives that decide _which key
authorizes which account_ -- `AuthorizedKeysFile`, `AuthorizedKeysCommand` and its `...User`, and the
certificate-principal sources `AuthorizedPrincipalsFile` / `AuthorizedPrincipalsCommand` /
`TrustedUserCAKeys` -- are all `Match User`-scopable, so their effective value differs per account.
Auditing only the bridge account's effective configuration would therefore miss a decisive bypass: a
_different_ account whose own `AuthorizedKeysFile` names a file containing the pinned public key, or
whose `AuthorizedKeysCommand` returns it, would authenticate that key into an ordinary interactive
session with no `command=` and no `restrict`, defeating the forced-command confinement entirely -- the
holder of the key simply connects as that account instead of the bridge account. The confinement the
table above establishes is a property of one `authorized_keys` entry; it is worth nothing if a second
entry, under any account, admits the same key unconfined. One-time setup therefore audits the effective
configuration of _every_ account that can be an authentication principal on the bridge host,
enumerated from the host's own account directory rather than a curated list -- on macOS every user
`dscl . -list /Users` returns, system and role accounts included, since an audit that skipped the
unattended service accounts would leave exactly the account an attacker would target -- each resolved
through `sshd -T -C user=<account>,addr=<daemon-source-address>,host=<daemon-source-host>` -- the
daemon's real connection tuple, not a user-only enumeration -- which expands `%u`/`%h` tokens and
applies every `Match` block reachable under that tuple, including a block scoped on `Address` or
`Host` rather than `User`. A `Match` block keyed on the daemon's source address or host, rather than
on any account name, can set its own `AuthorizedKeysFile` for the connection regardless of which
account it authenticates: resolving each account with a bare `user=<account>` query evaluates the
config as though the connection originated from an unspecified peer, and so can miss exactly the
`Match Address`/`Match Host` block that applies when the daemon actually connects, letting it
authorize the pinned key into a different, unconstrained account under the daemon's real connection
tuple without the audit ever seeing it. Resolving under the full tuple for every account closes this:
the audit enforces one host-wide invariant, evaluated fail-closed across every reachable `Match`
context rather than only the ones a per-account, tuple-less query would surface: the pinned key
authenticates as exactly
one principal -- the bridge account -- through exactly one entry -- the single root-owned, `command=`-
and `restrict`-bearing `authorized_keys` line (property 35). Concretely, for every non-bridge account
the audit reads, under the daemon's real connection tuple, each file its effective `AuthorizedKeysFile`
resolves to and refuses setup if the
pinned public key appears in any of them, requires its `AuthorizedKeysCommand` and
`AuthorizedPrincipalsCommand` to be `none` (a non-`none` command could return the pinned key
dynamically and cannot be statically cleared, so any non-`none` value on any account fails the audit),
and requires its certificate-principal and CA sources to be `none` so no CA-signed path can vouch the
key either; for the bridge account, resolved under the same tuple, it confirms the pinned key appears
only in the one root-owned
forced-command entry and in no bare entry, and that no `Address`- or `Host`-scoped `Match` block
reachable under that tuple names an alternate `AuthorizedKeysFile` path or admits the key without the
forced-command binding. Acceptance property: presenting the provisioned private key
to the bridge host, from the daemon's actual source address and host, authenticates only as the
bridge account and only into the forced command -- an
attempt to authenticate as any other account with that key, or as the bridge account through any entry
lacking the forced command, is refused, and setup fails closed if any account's effective key or
principal sources, resolved under the daemon's connection tuple and across every reachable `Match`
context, could admit the pinned key outside that single confined entry.

**Appendix -- complete `sshd_config(5)` directive disposition.** The enumeration is every directive
the `sshd_config(5)` OPTIONS section documents -- the keyword heading each option entry -- which on the
OpenSSH release used here is 106 directives; the count is release-specific, and setup re-derives it
against the bridge host's own `sshd` version by the same enumeration, so a directive a later release
adds is classified rather than missed. Each directive resolves to exactly one disposition. The
execution-and-environment and authentication directives are dispositioned in the sections above; every
other directive cannot reach the bridge account's forced-command session, its environment, or the
credential that authorises it, for the reason its group gives.

Dispositioned above:

- _Execution and environment_ (directive-family table): `ForceCommand`, `SetEnv`, `AcceptEnv`,
  `SshdSessionPath`, `SshdAuthPath`, `SecurityKeyProvider`, `AuthorizedKeysCommand`,
  `AuthorizedKeysCommandUser`, `AuthorizedKeysFile`, `AuthorizedPrincipalsCommand`,
  `AuthorizedPrincipalsCommandUser`, `AuthorizedPrincipalsFile`, `PermitUserRC`, `ChrootDirectory`,
  `Subsystem`, `PermitUserEnvironment`, `ExposeAuthInfo`, `X11Forwarding`, `X11UseLocalhost`,
  `X11DisplayOffset`, `XAuthLocation`.
- _PAM_ (PAM posture): `UsePAM`, `PAMServiceName`.
- _Authentication method_ (bridge-account `Match` pin and tuple audit): `AuthenticationMethods`,
  `PubkeyAuthentication`, `PasswordAuthentication`, `KbdInteractiveAuthentication`,
  `HostbasedAuthentication`, `GSSAPIAuthentication`, `KerberosAuthentication`, `PermitEmptyPasswords`.
- _Session forwarding_ (forwarding pins): `AllowTcpForwarding`, `AllowStreamLocalForwarding`,
  `PermitTunnel`, `AllowAgentForwarding`, `DisableForwarding`, `GatewayPorts`, `PermitOpen`,
  `PermitListen`, `StreamLocalBindMask`, `StreamLocalBindUnlink`.
- _Key and certificate authorisation_ (per-account audit scope): `TrustedUserCAKeys`, `RevokedKeys`.

Inert remainder, by reason:

- _Transport cryptographic-primitive negotiation_ -- selects ciphers, MACs, key-exchange, host-key,
  and signature algorithms for an already-pinned public-key method, changing neither the program run,
  the environment, nor which credential authorises: `Ciphers`, `MACs`, `KexAlgorithms`,
  `HostKeyAlgorithms`, `CASignatureAlgorithms`, `HostbasedAcceptedAlgorithms`,
  `PubkeyAcceptedAlgorithms`, `PubkeyAuthOptions`, `RequiredRSASize`, `RekeyLimit`, `ModuliFile`,
  `Compression`, `FingerprintHash`.
- _Server host-key material_ -- the daemon's own identity presented to clients, with no bearing on the
  bridge account's session: `HostKey`, `HostKeyAgent`, `HostCertificate`.
- _Parent method refused_ -- reachable only if host-based, GSSAPI, or Kerberos authentication were
  enabled, each pinned off above: `HostbasedUsesNameFromPacketOnly`, `IgnoreRhosts`,
  `IgnoreUserKnownHosts`, `GSSAPICleanupCredentials`, `GSSAPIDelegateCredentials`,
  `GSSAPIStrictAcceptorCheck`, `KerberosGetAFSToken`, `KerberosOrLocalPasswd`, `KerberosTicketCleanup`.
- _Connection admission control_ -- narrows which accounts may authenticate and can only further
  restrict, never grant the bridge key a second unconfined path (the per-account audit already binds
  the key to one confined entry): `AllowUsers`, `DenyUsers`, `AllowGroups`, `DenyGroups`,
  `PermitRootLogin`, `RefuseConnection`, `StrictModes`.
- _Networking, listener, and routing_ -- where and how the daemon accepts connections, the forced
  command governing the session that results: `ListenAddress`, `Port`, `AddressFamily`, `IPQoS`,
  `RDomain`, `RoutingDomain`, `TCPKeepAlive`, `UseDNS`.
- _Connection rate-limiting and timeouts_ -- throttle or time out connections with no program,
  environment, or authentication effect: `MaxStartups`, `MaxAuthTries`, `MaxSessions`,
  `LoginGraceTime`, `PerSourceMaxStartups`, `PerSourceNetBlockSize`, `PerSourcePenalties`,
  `PerSourcePenaltyExemptList`, `UnusedConnectionTimeout`, `ChannelTimeout`, `ClientAliveInterval`,
  `ClientAliveCountMax`.
- _Session presentation and TTY_ -- allocate a pty for the forced command's own stdio or emit static
  pre- and post-authentication text, running no additional program and setting no environment:
  `PermitTTY`, `Banner`, `PrintMotd`, `PrintLastLog`, `VersionAddendum`.
- _Logging_ -- observability only: `LogLevel`, `LogVerbose`, `SyslogFacility`.
- _Daemon-structural_ -- `Include` pulls in further configuration the host-wide textual scan above
  already follows, `Match` is the scoping mechanism the tuple audit and textual scan evaluate, and
  `PidFile` names the daemon's pid file: `Include`, `Match`, `PidFile`.

The groups partition the release's keyword set (here 21 + 2 + 8 + 10 + 2 dispositioned above, and 13 +
3 + 9 + 7 + 8 + 12 + 5 + 3 + 3 in the remainder, totalling 106): every documented directive appears in
exactly one, which is what makes setup's refusal decidable against the entire surface rather than the
specific directives an audit has so far named.

**Helper-side authority pinning.** `restrict` and the forced `command=` binding constrain _which
program_ runs for this key; they say nothing about what that program is permitted to act on once
it runs. The database path and the recipient are not caller-supplied. They are fixed in the helper's own
root-owned provisioning configuration at one-time setup, and no wire field carries either: a `poll`
body is a scan floor and nothing else, and a `send` body carries the message text and the
daemon-derived `send_id` dedup key -- neither of which names a database or a recipient (§Typed wire
schema). A holder of this key -- forced-command-constrained though they are -- therefore cannot name
a database to open or a recipient to message by varying the stdin payload, because no payload field
selects either; the helper reads both from its provisioning configuration, not from the
request. This is the structural form of the principle the forced command applies to the program:
just as the client cannot choose which program runs, it cannot choose which database that program
opens or which recipient it messages. The helper MUST therefore, on the bridge host and independent
of the adapter's own configuration: open only the database its provisioning configuration pins,
verifying that database's content identity (§Floor storage -- a database whose recomputed anchor GUID
does not match the provisioned record is refused as unprovisioned) rather than
trusting a path a caller named; return only rows belonging to the maintainer conversation and bearing
the sender handle fixed during that same setup, filtered on the bridge host before any row leaves the
helper; and address every `send` to the maintainer recipient fixed during that same setup. Because
none of these three targets is a request field, the compromised-daemon reach an earlier
caller-supplied design would have had to refuse by validation does not exist here: the adapter's
`KHIVE_IMESSAGE_MAINTAINER_HANDLE` configuration is the surface a
compromised daemon controls, and it never reaches the helper as an authority input. Acceptance
property: no request field names a database path or a recipient; the helper opens only its
provisioning-pinned database -- refusing a replacement whose content identity does not match -- and
addresses every `send` only to its provisioning-pinned recipient, so a compromised daemon cannot
redirect either through any protocol-valid request.

**Helper-enforced scan floor.** The bounded drain (§Bounded drain per tick) sends the helper the
`ROWID` floor to scan above as caller-supplied stdin data -- the one authority input a caller does
supply, unlike the helper-fixed database and recipient above. A compromised daemon could submit a floor of zero, or any value below where the
channel began, to make the helper scan and return message history the forward-only activation
semantic (§Activation semantic) promises never to import. The adapter-side initialization of the
checkpoint to the current maximum `ROWID` on first activation is the cooperative path, not a
boundary, because the surface that computes it is the surface a compromised daemon controls. The
helper MUST therefore record, at one-time bridge-host setup, the maximum `ROWID` then present in the
pinned database, and refuse any `poll` whose requested scan floor is below that recorded value --
returning a structured protocol error rather than scanning older rows -- regardless of the floor the
stdin payload names. This bounds what a compromised daemon can reach to rows that arrived after
provisioning, the tightest floor a setup-time value can enforce on the bridge; the adapter's own
activation checkpoint, always at or above this floor, supplies the tighter post-activation bound on
the cooperative path. These two floors serve different ends and must not be read as a single
guarantee: the provisioning floor is a security minimum -- the lowest `ROWID` a compromised daemon
can ever drive the helper to return -- not a promise that every message after provisioning is
captured. Capture begins instead at the adapter's activation checkpoint, which the forward-only
activation semantic (§The concrete operation set) fixes at the maximum `ROWID` present when the
adapter first activates, strictly at or above the provisioning floor. A message arriving in the
window between provisioning and that first activation therefore sits above the security floor yet
below the activation checkpoint, and is intentionally not imported -- it is exactly the
pre-activation history the forward-only start declines to ingest. What this floor underwrites is "no
history before activation, and nothing below the provisioning maximum for a compromised daemon to
reach", never "every message after provisioning is delivered". The provisioning floor is scoped as an
adapter import boundary -- which rows can ever be ingested -- not a confidentiality boundary against a
compromised daemon; a compromised daemon that can invoke the helper at all can already request any row
at or above the floor, so the floor's guarantee is about what history is excluded, not about who may
read what is included. The confidentiality this provides is
bounded precisely at the provisioning floor: rows below it are unreachable by any daemon-supplied
poll, while rows above it are readable by a compromised daemon exactly as they are by the cooperative
adapter. The floor is a reachability boundary on old history, not a general secrecy guarantee over the
maintainer's conversation. Because the recorded floor is fixed
against the setup-pinned database identity
(above), it is only ever applied to the database it was provisioned against: a replaced database is a
new identity the database-identity check refuses until one-time setup is re-run (§Provisioning versus
configuration below), so the helper never applies a stale floor to a database it was not provisioned
against. The helper keys each recorded floor by database identity and records a floor exactly once, at
that identity's first provisioning: re-running setup against an identity the helper has already recorded
re-pins its keys and handles but does not advance its floor, and the original first-provisioning floor
is retained. This is load-bearing for acceptance property 12 across a return to a previously provisioned
database -- were re-provisioning to re-record the floor at the current maximum `ROWID`, rows that
arrived while that identity was inactive would fall below the new floor and the helper would refuse
exactly the backlog that source's retained high-water mark is entitled to resume. Acceptance property: a
`poll` request naming a scan floor
below the helper's setup-recorded floor is refused by the helper with a structured error and returns
no rows, even when correctly versioned, protocol-valid, and authenticated by the pinned key.

**Floor storage, and recovery from a database that legitimately shrinks.** The recorded floor
outlives both the helper process and the helper binary because it is not the helper's own state:
one-time setup, which runs as root, writes it to a root-owned ledger on the bridge host --
`/etc/khive/imessage/scan_floors`, owned `root:wheel` and mode `0644`, one record per database
identity of the form `{db_identity, anchor_guid, floor_rowid, provisioned_at}`,
where `db_identity` is the opaque token bound to that database's `anchor_guid`
(below). The ledger is root-owned and, like the pinned key and known-hosts files, sits inside the
root-owned `/etc/khive/imessage/` provisioning root that the bridge account cannot write, so neither
the helper nor a compromised daemon can move a floor by editing it. The helper runs as the
unprivileged bridge account and opens this ledger read-only; it has no write path to it, so nothing
the helper does on a poll -- and nothing a compromised daemon can drive the helper to do -- can move
a floor. `db_identity` is an opaque token bound to the pinned database's content lineage rather than to its
path, so that replacing the file at the same path is recognized as a different database instead of
silently inheriting the old floor. One-time setup derives it from the database's
**anchor GUID** -- the immutable anchor message GUID the cursor's `anchor` column pins at
provisioning, unchanged by deletion of other rows, though deleting the anchor row itself invalidates
the identity (below). `db_identity` carries no per-provisioning salt: it is a function of the anchor
GUID and content lineage alone, so it is stable across every re-provisioning of the same database.
That stability is load-bearing -- it is exactly what lets a re-provisioning find and retain that
database's first-provisioning floor (acceptance property 27) and lets a returning source resume its
own cursor row (§Source-keyed cursor history, acceptance property 12); a per-provisioning salt would
make each provisioning a distinct identity the helper had never recorded, so neither retention nor
resume could ever match. Setup requires that anchor to be non-null: it refuses to provision
a database whose messages table is empty, because an empty table offers no anchor message to pin, and
a null anchor would both deadlock the channel the moment the first real row landed -- the recomputed
non-null anchor could never match a null record -- and match any empty replacement database
indiscriminately. The operator seeds the first maintainer message (a provisioning handshake sent to
the bridge account) and re-runs setup, which then records a non-null anchor; a provisioned record
therefore always carries a non-null anchor GUID. On each poll the helper recomputes
the anchor GUID from the provisioned database its configuration pins and requires it to match the
anchor recorded at provisioning: the token is stable across invocations, across a helper reinstall,
and across deletion of non-anchor rows (their removal leaves the anchor row's GUID unchanged), and it
differs whenever the database's content lineage differs -- which a same-path replacement, carrying its
own distinct anchor GUID, necessarily does. Deleting the anchor row itself is the one deletion that
changes the outcome: the recomputed anchor GUID no longer matches -- or, if that `ROWID` now holds no
row or a reused one, is absent or different -- so the helper treats the database as unprovisioned and
refuses polls until setup re-runs (below), rather than continuing against a database whose pinned
identity is gone. Deriving identity from the resolved path alone would not distinguish a
replacement database from the original, letting the helper apply the provisioned floor to a database
it never provisioned; binding identity to the anchor GUID is what makes the helper refuse an
inadvertent same-path replacement -- a different database swapped in at the pinned path, carrying its
own distinct anchor GUID. This is a content-lineage check, not a cryptographic provenance guarantee:
a bridge-host-local attacker able to write `chat.db` could copy the pinned anchor message into a
substituted database to reproduce the lineage, but such an attacker already controls the bridge host
the helper runs on and is outside this boundary's threat model, which frames the in-scope adversary as
a compromised daemon rather than a party with write access to the bridge's own Messages store. A database whose recomputed anchor GUID does not match a provisioned record is treated
as unprovisioned: the helper refuses every `poll` against it with a structured error until one-time
setup is re-run for the new content identity (§Provisioning versus configuration below), rather than
applying any prior identity's floor. A helper reinstall or upgrade re-reads this ledger unchanged and
leaves every recorded floor in place; a fresh install against an empty ledger records each identity's
floor once, at that identity's first provisioning (above).

`ROWID` reuse is a hazard this design must not trust a bare `ROWID` against: SQLite reuses the
largest previously-used `ROWID` when the row holding it is deleted and a new row is inserted, because
an implicit `ROWID` (an `INTEGER PRIMARY KEY` not declared `AUTOINCREMENT`) is not monotonic across
deletions -- a property khive's own schema documents, `007-notes-seq.sql` adding an explicit sequence
precisely because an implicit `ROWID` can be reused. Two defenses already stated cover it, and neither
rests on a bare `ROWID`. First, the identity of a checkpoint row is its `guid`, not its `ROWID`: the
poll response pairs the floor `ROWID` with the `guid` of the row at it (`floor_guid` and
`next_floor_guid`, §Typed wire schema), and the adapter's per-tick identity guard (§Identity guard and
forward-only reset) re-reads the `guid` at its stored high-water `ROWID` and forces a forward-only
reset the moment it fails to match, so a reused `ROWID` presenting a different message is caught, never
accepted as the same logical row. Second, the helper's security floor is a lower bound: a message that
reuses a `ROWID` at or below the recorded floor sits at or below the floor and is declined by the
forward-only semantic, never rescanned, so reuse below the floor can lose a row but can never lower
the floor or replay history. The conservative disposition in both cases is refusal or forward-only
reset, never treating a reused `ROWID` as continuity.

Retaining the floor across re-provisionings (above) is what protects a returning source's backlog,
but it leaves one failure the poll path cannot resolve on its own: a legitimate cleanup of the bridge
host's Messages database -- the operator pruning old conversations, or Messages compacting its own
store -- can lower the database's current maximum `ROWID` below the retained floor. Every subsequent
`poll` then names a floor at or below the recorded floor and is refused with `below_scan_floor`, and
the channel is stuck until the `ROWID` sequence climbs back above the recorded floor, which after a
large cleanup may be never. The helper MUST NOT resolve this by lowering the floor automatically when
it observes a shrunken database: an automatic lower-on-shrink is exactly the history-scan primitive
the floor exists to deny, since a compromised daemon able to shrink the database, or to make it
appear shrunken, could then drive the floor down and rescan the history the forward-only semantic
promises never to import. Lowering a floor is therefore reserved to an explicit, root-run
re-provisioning ceremony -- a distinct setup mode from the ordinary identity re-pin, which retains
the floor -- that deliberately re-records the named identity's floor at the database's current maximum
`ROWID`. It runs under the same root trust anchor that created the floor, is never reachable from the
poll path or from any daemon-supplied value, and forfeits the no-skip guarantee for any row between
the old and new floor as a consequence the operator running it accepts. This keeps the one legitimate
reason to move a floor backward on the same privileged, deliberate footing as first provisioning, and
off the daemon-facing surface entirely. Acceptance property: no `poll`, and no daemon-supplied floor
value, ever lowers a recorded floor; the recorded floor decreases only after a root-run floor-reset
re-provisioning, and absent that ceremony a database whose maximum `ROWID` has fallen below the
recorded floor yields `below_scan_floor` on every poll rather than silently rescanning history.

**Provisioning versus configuration: changing the pinned identity.** The database path, sender
handle, recipient, and scan floor the helper enforces are fixed at one-time bridge-host setup, not
read from the adapter's configuration on each invocation (above). The database path in particular is
a setup-time provisioning parameter baked into the helper's root-owned configuration, not a daemon
runtime variable (§Configuration): the daemon never names a database at runtime, and the adapter
learns the provisioned database's `db_identity` from the helper's `activation` response rather than
choosing it. The one bridge-facing value the daemon does control at runtime is
`KHIVE_IMESSAGE_SSH_TARGET`, and switching it to a value that names a different bridge host is not a
configuration change the running channel can absorb on its own. The cursor layer keys a separate row
per `source` and resumes each source's own high-water mark
on return (§acceptance property 12). A re-provisioning to a genuinely different signed-in database
presents a new `db_identity` in its `activation` response, hence a new `source`, under which the
cursor layer opens a fresh forward-only row (§Source-keyed cursor history) rather than resuming a
stale mark. The identity guard's own forward-only reset (§Identity guard and forward-only reset
above) is the row-level path -- an anchor row deleted, or a `ROWID` reused under a different `guid`
-- each surfaced by `poll`/`identity` as data (`found`/`matches`), never as a refusal, and it fires
only against a database the helper still serves; a database swapped in without re-provisioning
matches no provisioned record, so the helper refuses every poll as unprovisioned (§Floor storage)
and the adapter drives no reset onto it. Neither the source switch nor the row-level reset
re-establishes trust in a bridge host or database that one-time setup never pinned. Pointing
at a host that was never provisioned fails closed at each layer setup would have configured:
`StrictHostKeyChecking=yes` refuses a host with no entry in `/etc/khive/imessage/known_hosts`, the
bridge account refuses a key with no matching restricted `authorized_keys` entry, and no fixed helper
is installed to answer. Adopting a genuinely new bridge host or a new signed-in database
therefore requires re-running one-time bridge-host setup -- re-pinning the host key, provisioning the
restricted key and `sshd` lockdown, installing the helper, and re-fixing the helper's database
path, handle, and scan floor against the new target -- and is deliberately not something a
running daemon can accomplish by rewriting an environment variable. The scan floor is the one
provisioned value not re-fixed when the target is a database identity the helper has recorded before:
floors are recorded once per identity at first provisioning and retained across later re-provisionings
of that identity (§Helper-enforced scan floor), so returning to a previously provisioned database
resumes above its original floor rather than above wherever its `ROWID` sequence has since advanced.
Runtime switching of `KHIVE_IMESSAGE_SSH_TARGET` is thus limited to the
bridge hosts one-time setup has already provisioned, each serving the database its own setup pinned:
returning to a previously provisioned host resumes its database above that database's retained floor,
and any other value is not a target the running channel can adopt without re-running setup.
Acceptance property: a change to
`KHIVE_IMESSAGE_SSH_TARGET` that names a bridge host not
covered by one-time setup fails closed at the host-key pin, the restricted-key match, or helper
presence, rather than operating against an unpinned target.

### Outbound: the fixed remote helper, no text interpolation

On the bridge host, delivery drives the Messages application through the fixed remote helper
described above. The message body and recipient are never interpolated into a shell string or an
AppleScript source string: they travel to the helper over stdin as structured, data-only input,
and the SSH invocation's remote command is always the same literal helper invocation -- the daemon
never assembles a remote or local shell command by string concatenation with caller-supplied
content. On receiving that structured input, the helper drives `osascript` locally on the bridge
host using a fixed, pre-written AppleScript that itself reads the message text from a temp file or
stdin rather than from an interpolated source string. This extends the same class of protection
ADR-108 Fork (b) B2 establishes for `git.commit`/`git.branch`/`git.push` -- no caller-supplied
value reaching a command-parsing boundary unvalidated -- across both the SSH hop and the local
`osascript` invocation. ADR-108's Amendment 1 requirement for a dedicated adversarial security
review at implementation time applies equally to this shell-out surface.

**Outbound delivery idempotency.** Delivery over SSH is at-least-once: the helper can drive
`osascript` to deliver a message and then lose the response on the way back to the daemon -- an SSH
drop, or an invocation-deadline kill after the send but before the acknowledgement -- and a naive
retry would deliver the same message to the maintainer twice. The outbound path therefore carries a
`send_id` idempotency key and dedups on the helper side. The daemon derives `send_id` from the
outbound `message` note's own stable identity, so every retry of the same logical send carries the
same key with no new daemon-side state, and passes it to the helper as structured, data-only input
alongside the body. Because the outbound note is marked delivered only after `Channel::send` returns
success, a lost response leaves the note undelivered and a daemon restart re-drains the same note
under the same `send_id` -- the durable note identity, not an in-memory send cursor, is the guard,
mirroring the inbound path's reliance on its durable dedup index over its in-memory poll cursor
(§Poll offset and restart durability). The helper keeps a small bounded ledger of recently-delivered
`send_id`s -- bridge-account-owned and writable by the helper, distinct from the root-owned floor
ledger it can only read, since idempotency is a correctness aid and not a trust boundary (the send
recipient is fixed at the helper regardless, above) -- with a retention window comfortably exceeding
the daemon's outbound retry horizon. On a send, the helper drives `osascript` only when the
`send_id` is absent from the ledger, records it on a successful delivery, and returns success; a
repeat `send_id` is acknowledged without re-driving `osascript`. This narrows the duplicate window
from every lost response to only a helper crash in the sub-second interval between a successful
`osascript` delivery and the ledger write -- the irreducible residual of any deliver-then-record
scheme, since recording before delivering would instead risk suppressing a send that never left the
host. The maintainer-chat context tolerates that residual; the ADR states it rather than claiming an
unqualified exactly-once.

**Outbound drain bounds and failure backoff.** The outbound task drains undelivered `message` notes
destined for the channel under bounds that keep one slow or failing send from monopolizing it,
mirroring the inbound drain (§Bounded drain per tick) and the poll transport recovery (§Transport
deadlines and recovery). Each outbound tick carries a per-tick wall-clock budget (default one
`KHIVE_IMESSAGE_POLL_SECS` interval) checked after each send, plus a secondary per-tick note-count
cap. Because every `Channel::send` runs under the same 35-second SSH invocation deadline as a poll, a
single slow or timed-out send already exceeds the per-tick budget, so the task yields after that one
send and re-drains on the next tick: an unbounded serial drain of a full 200-note backlog whose every
send hit the deadline would otherwise occupy the task for roughly 100 minutes, and the per-tick budget
caps that at one deadline-length attempt per tick. Consecutive send failures back off exactly as poll
failures do -- exponentially from a 1-second floor, capped at 5 minutes, incremented on a failed send
(including an invocation-deadline kill) and reset to zero on the next success -- so a persistently
unreachable bridge does not spin the outbound task at full rate. A failed send leaves its note
undelivered; the durable note identity re-drains it later under the same `send_id` (§Outbound delivery
idempotency), and a note that fails on a bounded number of consecutive ticks is deferred behind newer
undelivered notes, so a single note the helper cannot deliver never holds the head of the queue and
starves every message behind it. Because the inbound poll loop and the outbound send loop run as
separate daemon tasks, these bounds are independent of the inbound drain bounds and neither task's
backlog delays the other. Acceptance property: an outbound backlog of 200 notes whose sends all hit
the invocation deadline occupies the outbound task for at most one send past each backoff interval
rather than one continuous 100-minute drain, a note enqueued during that backlog is attempted within a
bounded number of ticks rather than waiting behind the entire backlog, and a single repeatedly failing
note does not indefinitely block delivery of newer notes.

### Inbound: read-only polling of the bridge host's Messages database

Inbound messages are read by polling the bridge host's Messages database (Apple's own SQLite
store for the Messages application, at the path fixed in the helper's root-owned provisioning
configuration during one-time setup, conventionally `~/Library/Messages/chat.db`) through the same
fixed remote helper the outbound path uses
(§Remote command boundary above): no database path crosses the wire -- the helper opens only the
database its provisioning configuration names, with read-only semantics on the bridge host, and
verifies that database's content identity before use (§Helper-side authority pinning, §Floor
storage). Polling runs at a configurable interval
(`KHIVE_IMESSAGE_POLL_SECS`, default 5 seconds, mirroring §12's default inter-poll interval;
validation rule below). This interval is the target gap between the end of one tick's work and the
start of the next, not a bound on how long a single tick may run: the inbound task is a single
sequential loop, so ticks never overlap, and a tick whose own SSH invocation and drain take longer
than the interval is followed immediately by the next tick with no additional sleep, rather than the
two running concurrently. The interval therefore does not, and is not intended to, bound end-to-end
poll latency or how long the poll task is occupied on a given tick -- that occupancy is bounded
separately and explicitly by the 35-second SSH invocation deadline (§Transport deadlines and
recovery) plus the per-tick drain wall-clock cap (§Bounded drain per tick): a sparse or poorly
indexed `chat.db` can legitimately hold a tick for close to the helper's 20-second scan budget before
returning any candidate, and that is expected, bounded behavior, not a violation of the configured
5-second interval. The adapter never writes to `chat.db`. Delivery into `comm.ingest` is
attributed as `imessage:<slug>` inbound, following the channel-prefixed `from` form OQ-1
established (§14; also used by the Telegram amendment's `from = "telegram:<maintainer-slug>"`).

**Sender validation (ADR-056 §8 applied to this adapter).** §8 requires every adapter to
validate the external sender identity on every inbound item before it becomes an envelope. For
iMessage, that requirement is met by the helper's maintainer-conversation filter (below) together
with three adapter checks, all mandatory: `message.is_from_me` must be `0`, the row's sender handle
must equal the maintainer handle the helper returns in its `activation` response -- the
setup-time-fixed provisioned handle the helper itself filters on, not the adapter's mutable
`KHIVE_IMESSAGE_MAINTAINER_HANDLE` env var -- and the row's service must be iMessage. The maintainer-conversation boundary is enforced helper-side, not by the adapter: the
helper withholds every row outside the setup-time-fixed maintainer conversation, and the wire
`MessageRow` carries no conversation identifier, so the adapter neither can nor needs to re-check
conversation membership.
A row failing any check is dropped -- it is never ingested and never attributed -- and the drop
is counted, mirroring the Telegram adapter's `UnauthorizedSender` drop-and-count behavior (§8)
rather than the email adapter's quarantine disposition, since (unlike inbound email) the bridge
host's Messages database has no open-relay exposure: only the maintainer's own conversation is
polled.

The maintainer-conversation and sender-handle boundaries are not the adapter's to enforce alone.
The helper on the bridge host already filters returned poll rows to the setup-time-fixed maintainer
conversation and sender handle (§Remote command boundary above), the same setup-time identity that
pins the `send` recipient, so a row that reaches the adapter has already passed the authoritative
identity filter on the bridge host. The three adapter checks then run as defense-in-depth over the
fields the wire `MessageRow` actually carries -- `is_from_me`, `sender_handle`, and `service` --
validating the sender handle against the helper's `activation`-reported provisioned handle rather
than any adapter-local value: a compromised daemon cannot widen the boundary by rewriting
`KHIVE_IMESSAGE_MAINTAINER_HANDLE`, because the rows it receives were already constrained at the
helper against an identity the daemon does not control, and the defense-in-depth check compares
against that same helper-reported identity rather than the env var. Binding the check to the
helper-reported handle also closes an availability trap the env-var form carries. Were the check to
read `KHIVE_IMESSAGE_MAINTAINER_HANDLE` directly, an operator editing that env var to any value other
than the setup-fixed handle would leave the helper still returning the maintainer's rows while every
adapter check rejected them -- silently dropping all inbound as an unauthorized sender while outbound
continued reaching the provisioned recipient, a one-way break with no error surfaced. The env var is
therefore a bootstrap and display value only; activation performs an equality check between it and the
helper-reported provisioned handle and fails loud on a mismatch, rather than degrading to silent
inbound loss. This equality check normalizes both sides with the same `normalized_handle` reduction
used for `handle_token` derivation (§Canonical encoding of `db_identity`, `handle_token`, and the
`source` string below) before comparing them, rather than comparing the raw strings: without that
shared normalization, two textually different but equivalent handles -- `+1 (555) 123-4567` against
`+15551234567`, or an Apple ID email that differs only in case or surrounding whitespace -- would
derive the identical `handle_token` and so the identical cursor `source`, yet fail this raw-string
equality check and refuse to start, even though the handle they name is the one the helper actually
filters on. Normalizing both sides of the check the same way `handle_token` normalizes its input
closes that gap: equivalent handles validate and start correctly, and only a handle that is genuinely
different after normalization fails loud. Conversation membership is not among the defense-in-depth checks: the helper
withholds non-member rows and `MessageRow` exposes no conversation field, so it is a helper-side
boundary only. Acceptance property: a `chat.db` row
outside the setup-time-fixed maintainer conversation, or bearing a sender handle other than the
setup-time-fixed one, is withheld by the helper and its content never returned to the adapter,
independent of the adapter's `KHIVE_IMESSAGE_MAINTAINER_HANDLE` value; the row's identity-only
`ROWID` and GUID may still appear as scan-floor checkpoint metadata (§Terminal disposition,
§Restart durability), but its text, sender, service, and conversation membership never leave the
helper.

**Terminal disposition.** A scanned row is HANDLED when it reaches one of four terminal
outcomes: it is ingested; it is dropped by one of the three sender-validation checks above; it is
withheld by the helper's bridge-side maintainer-conversation and sender-handle filter (§Helper-side
authority pinning) and never returned to the adapter at all; or it carries no ingestible text -- a
row whose `text` is null, such as a bare attachment -- which cannot become a `ChannelEnvelope`
(whose `body` is required text) and is skipped, counted toward `scanned_count` and the floor advance
exactly as a dropped row is, never silently discarded and never left pending. All four outcomes are
terminal and all four count toward page completion; none is left pending or retried on the next
poll. The page
checkpoint (§Restart durability below) commits once every scanned row in the page has been
handled, and the `ROWID` floor advances to the page's scan floor -- the maximum `ROWID` the helper
scanned (§Bounded drain per tick) -- so it moves past withheld and dropped rows exactly as it does
past ingested ones. Acceptance property: a page whose rows are all withheld by the helper still
advances the checkpoint past them, and no scanned row below the checkpoint floor -- withheld,
dropped, skipped for lack of ingestible text, or ingested -- ever blocks or re-selects on a later
poll or stalls the rows that follow it in `ROWID` order.

The `is_from_me` check exists because the adapter's own outbound sends (via `osascript`, above)
land in the same one-to-one conversation the inbound poll reads, and, being part of that
conversation, can carry the maintainer's own handle on the row. Without excluding
`is_from_me = 1` rows, the adapter would read back its own outbound sends as trusted inbound
maintainer input: for a chat channel feeding an autonomous loop, that is a self-echo path -- the
daemon replying to its own prior message and re-ingesting that reply in turn. The other adapter
checks -- handle match and service -- do not catch this case, because an adapter-sent row satisfies
both; nor does the helper's maintainer-conversation filter, since an adapter-sent row is part of
that very conversation. Only the `is_from_me` exclusion distinguishes it. Acceptance property: an adapter-sent row in the maintainer conversation
is never ingested as inbound.

The service check exists because `chat.db` records a service discriminator per message, and a
row carried over SMS, MMS, or RCS is not an iMessage-authenticated delivery: a forwarded SMS
message's sender number is spoofable in a way a delivery tied to a signed-in Apple ID is not.
Without this check, a spoofed SMS sender number matching the configured handle could be
attributed as trusted maintainer input. Rows carried over SMS, MMS, or RCS are dropped and
counted, never ingested -- the same disposition as the other adapter checks. This check also makes
structural a property this deployment model otherwise leaves incidental: the bridge Mac signs
into a dedicated bridge Apple ID that is email-only, with no phone number and no paired SMS
forwarding path (§Two-identity model below), so every row in the bridge's Messages database is
iMessage-service by construction -- there is no SMS-forwarding path through which an SMS message
could reach that database at all. The maintainer's own handle, by contrast, may be a phone number
or an email address; only the provisioned form -- the handle the helper filters on and returns at
activation, which `KHIVE_IMESSAGE_MAINTAINER_HANDLE` must match -- is trusted. The service check therefore enforces a structural property of the bridge account, not an
incidental side effect of the maintainer's handle choice. Acceptance property: a `chat.db` row
carried over SMS, MMS, or RCS that otherwise matches the maintainer handle and conversation is
never ingested.

### Restart durability: a persisted ROWID checkpoint

The iMessage adapter does not use an in-memory poll offset. It uses the same durability
mechanism class Amendment 2026-07-09 established for email: a durable per-channel checkpoint,
committed to the daemon's store only after a batch has fully and successfully ingested. That
amendment introduced this pattern for exactly this reason -- issue #449 showed that an
in-memory or window-bounded cursor can permanently skip rows once a page limit or time window
moves past them, and that a dedup index alone cannot recover a row that polling never fetched in
the first place. The iMessage adapter adopts the pattern rather than repeating the email
adapter's earlier mistake.

Concretely:

- The checkpoint reuses the pack-owned `comm_channel_cursor` table and the `comm.cursor_get` /
  `comm.cursor_commit` subhandlers Amendment 2026-07-09 added to `khive-pack-comm`: the iMessage
  adapter is a second caller of the same checkpoint mechanism the email adapter's
  `channel_poll_loop` already drives, not a parallel implementation of it. Reuse here is not
  unchanged reuse, though: Amendment 2026-07-09's `comm_channel_cursor` table persists only
  `source`, `generation`, `high_water`, and `updated_at`, keyed on `(channel_kind,
  channel_slug)`, and has neither a field for the anchor-row identity guard this adapter's
  checkpoint needs nor a key that supports per-source cursor history (both below). This amendment
  therefore specifies an explicit, additive extension of the shared checkpoint contract -- a
  schema migration plus the corresponding `comm.cursor_get`/`comm.cursor_commit` API surface
  change -- not a claim that the existing mechanism already covers this case unmodified.
- The checkpoint commits only after every scanned row in a drained page has been handled --
  ingested, terminally dropped by one of the three sender-validation checks above, withheld by
  the helper's bridge-side filter, or skipped for carrying no ingestible text (§Terminal disposition) -- mirroring `channel_poll_loop`'s
  `cursor_get -> poll_page -> handle each -> cursor_commit` sequencing for email (§Amendment
  2026-07-09), with dropped and helper-withheld rows added as terminal outcomes alongside
  ingestion, and `high_water` advancing to the page's scan floor rather than to the last returned
  row. A row that fails to reach `comm.ingest` for a transport or storage
  reason (as opposed to a terminal sender-validation drop) leaves the checkpoint at its prior
  value, so the next poll re-selects the whole page; GUID dedup through
  `idx_comm_message_external_id` (§11) then skips re-storing the rows that already succeeded, and
  only the unhandled row is effectively retried.
- GUID dedup is retained, but its role changes: it is overlap protection for the re-selected
  page on a partial-failure retry, not the primary durability mechanism. The `ROWID` checkpoint
  is what prevents rows from being skipped; GUID dedup is what prevents an already-ingested row
  in a re-fetched page from being written twice.

**Cursor identity: source and generation.** The `comm_channel_cursor` table (Amendment
2026-07-09) stores `source` and `generation` alongside the `high_water` mark, and this amendment
defines both for iMessage. `source` is a non-secret stable identity string composed of the
configured `KHIVE_IMESSAGE_SSH_TARGET`, the provisioned database's `db_identity` -- the
content-lineage token the helper returns in its `activation` response (§Floor storage) -- and a
`handle_token` derived from the setup-fixed provisioned maintainer handle the same `activation`
response returns (§Sender validation), in the
form `imessage-ssh-v1:{pct(ssh_target)}:{db_identity}:{handle_token}` (§Canonical encoding below
defines `pct()`), mirroring the shape of the email adapter's
`imap+tls:{host}:{port}:{mailbox}:INBOX` source string (§Amendment 2026-07-09). `imessage-ssh-v1:` is
the sole normative encoding of this source key; no unversioned `imessage-ssh:` form exists or is ever
written or accepted. The `handle_token`
is a stable non-reversible digest of the provisioned handle, never the raw handle: it keeps the raw
phone number or Apple ID out of the cursor identity string, and -- being a function of the handle
alone, not of provisioning time -- it is identical across every re-provisioning that keeps the same
maintainer handle, so a benign re-provision such as an SSH-key rotation never relabels the source,
while a re-provision that changes the maintainer handle does. Because the database
half is the provisioned `db_identity` rather than a local path, the adapter learns it from a startup
`activation` handshake before its `source` is resolvable: the adapter issues `activation` once when
it starts, caches the returned `db_identity`, and its source accessor (§The widened cursor contract)
returns the composed `source` from that cached value, so the daemon loop resolves the cursor row
against a `db_identity` the helper has vouched for rather than against a path the daemon named. That
same `activation` response's `max_rowid` and `anchor_guid` seed the checkpoint on a first activation
(no cursor row yet exists for the resulting `source`, §Activation semantic). Neither component
is a credential, and the cursor row itself lives only in the daemon's
own local store, never on the bridge host. Because the `db_identity` token is bound to the database's
content lineage and carries no per-provisioning salt (§Floor storage), it is stable across
re-provisionings of the same database, so a local configuration change can never relabel the rows of
a database the daemon is still polling. A genuinely different provisioned database presents a
different `db_identity` and therefore a different `source` (§Source-keyed cursor history below),
exactly as pointing `KHIVE_IMESSAGE_SSH_TARGET` at a different bridge host does. `generation` starts
at `1` when the cursor row is
first created for a given `(channel_kind, channel_slug, source)` and increments by one on every
checkpoint reset (below).

**Canonical encoding of `db_identity`, `handle_token`, and the `source` string.** Because these
strings are the persistent cursor key, an independent change to the helper or the adapter must
produce byte-identical values for the same database and handle or a returning source would fail to
match its own row. The encodings are therefore fixed, not opaque:

- **`db_identity`** = the lowercase hex encoding of `BLAKE3("imessage-db-v1\0" || anchor_guid)`, where
  `anchor_guid` is the exact UTF-8 GUID string `chat.db` stores for the anchor message (Apple emits
  GUIDs in a fixed canonical form; the helper hashes that byte string verbatim, applying no
  case-folding or reformatting of its own). The `imessage-db-v1\0` domain-separation prefix binds the
  digest to this scheme and version. The result is 64 lowercase hex characters and contains no `:`.
  This realizes the "function of the anchor GUID alone, no per-provisioning salt" property (§Floor
  storage) as a concrete, reproducible computation.
- **`handle_token`** = the lowercase hex encoding of `BLAKE3("imessage-handle-v1\0" ||
  normalized_handle)`. `normalized_handle` is the provisioned maintainer handle reduced to `chat.db`'s
  own canonical handle form before hashing: a phone number to E.164 (leading `+`, digits only, no
  spaces or punctuation), an Apple ID to its address lowercased with surrounding whitespace stripped.
  Normalizing before the digest is what makes `+1 (555) 123-4567` and `+15551234567` yield one token;
  the digest is non-reversible, keeping the raw number or address out of the key. This is a
  preimage-resistance guarantee, not a secrecy one: the input space of phone numbers and email
  addresses is small enough for offline dictionary search to recover a specific handle from its
  token without a salt, so `handle_token` guarantees the raw handle is omitted from the cursor
  store, not that the handle is unrecoverable to an attacker willing to search that space. The
  result is 64
  lowercase hex characters and contains no `:`.
- **The `source` string** is `imessage-ssh-v1:{pct(ssh_target)}:{db_identity}:{handle_token}`, four
  `:`-delimited fields. Only `ssh_target` can contain a `:` (an IPv6 literal such as `user@[::1]`, or
  a `host:port`), which would make a naive split ambiguous, so `ssh_target` is percent-encoded:
  `pct()` encodes every byte outside the RFC 3986 unreserved set `[A-Za-z0-9._~-]` as `%HH` (uppercase
  hex), so no literal `:`, `@`, `[`, `]`, `/`, or `%` survives inside the field. `db_identity` and
  `handle_token` are fixed-width hex and need no encoding. A parser therefore splits on `:` into
  exactly four fields unambiguously, and percent-decodes only the second. The `imessage-ssh-v1` scheme
  tag carries the version: any future change to a normalization rule or the digest domain bumps the
  scheme (and the token prefixes) to `-v2`, producing distinct `source` values that the source-keyed
  cursor (below) isolates from `-v1` rows rather than silently colliding with them.

**Source-keyed cursor history.** Amendment 2026-07-09's `comm_channel_cursor` table carries a
`source` column, but its uniqueness key is `(channel_kind, channel_slug)` alone, so a commit
under a new `source` value would overwrite the existing row rather than create a new one --
incompatible with this adapter's retention need that a changed source produce a new row while
the old row is left intact. The same migration that adds the `anchor` column (below) extends the
uniqueness key to `(channel_kind, channel_slug, source)`: `comm.cursor_get` and
`comm.cursor_commit` look up and commit on all three fields, not two, and existing rows migrate
forward preserving their current `source` value, so the email adapter -- whose `source` is
constant per slug -- is semantically unchanged. Telegram keeps its own in-memory `offset`
watermark (§Amendment 2026-07-05, "Poll offset and restart durability") and has never used
`comm_channel_cursor`; this migration touches the email adapter's existing row and establishes
the shape future adapters, including iMessage, use going forward. It does not add, change, or
remove any Telegram row, because none exists. Under this key, a configuration
change that alters the source identity -- a different `KHIVE_IMESSAGE_SSH_TARGET`, a return to a
different provisioned database whose `db_identity` differs, or a re-provisioning that changes the
maintainer handle and so changes the `handle_token` -- produces a different `source` string
and therefore a new cursor row
keyed by that new source; the prior row is left intact, untouched and unreferenced, never
overwritten or migrated. Reverting to a previously used source therefore resumes that source's
own row at its own high-water mark, and the poll drains whatever backlog accumulated in that
source's database while a different source was active: no message received under a configured
source is ever skipped by switching away from it and back. Folding the handle into the source is
load-bearing for a specific hazard: were the source keyed on `{ssh_target, db_identity}` alone,
re-provisioning the same database against a different maintainer handle would reuse the prior
conversation's high-water mark against a new conversation whose rows interleave below it in the same
`chat.db`, silently skipping every new-conversation row beneath that stale mark. Because the handle
is part of the source, that re-provisioning instead starts a fresh forward-only cursor for the new
conversation (§Activation semantic) and leaves the old conversation's cursor intact. Acceptance
property: switching from source A to source B and back to source A ingests the rows that arrived in
source A's database during the period source B was active; and re-provisioning a database against a
changed maintainer handle drains the new maintainer conversation forward-only from activation rather
than resuming a prior handle's high-water mark.

**Identity guard and forward-only reset.** A `ROWID` alone is not a stable identity across a
replaced or migrated `chat.db`: a new database can reuse the same `ROWID` values for entirely
different messages. Guarding against this requires the explicit, additive schema extension named
above: a new optional opaque `anchor` text column on `comm_channel_cursor`, nullable, delivered
as a schema migration alongside the corresponding `comm.cursor_get`/`comm.cursor_commit` API
surface change to read and write it. The email and Telegram adapters leave this column null and
are otherwise unaffected; the iMessage adapter is the first caller to populate it. The anchor is
always the GUID of the row the high-water mark points at -- the row at the page's scan floor
(§Bounded drain per tick), whether that row was ingested, terminally dropped, withheld by the
helper's bridge-side filter, or skipped for lack of ingestible text. Because `high_water` advances to the scan floor and the scan-floor row
may be one the helper withheld, the helper returns that row's `ROWID` and GUID as identity-only
checkpoint metadata -- never its text, sender, service, or conversation membership -- so the adapter
learns nothing about a withheld row beyond the boundary identity the checkpoint already advances
past. This is deliberate, not an approximation: §Terminal disposition above establishes that the
checkpoint's `ROWID` floor advances past a withheld or terminally dropped row exactly as it does
past an ingested one, so a page whose scan floor is a withheld or dropped row still leaves
`high_water` pointing at that row, and the stored `anchor` must name that same row's GUID for the
identity guard below to compare like against like. Storing the last-_ingested_ row's GUID instead
would be wrong precisely in this case: when a page ends on a dropped row, `high_water` points past
it while a last-ingested anchor would name an earlier row, so the re-read below would deterministically
compare the wrong row's GUID against the wrong `ROWID` and misreport a mismatch on every such page
-- forcing a spurious forward-only reset that permanently skips the valid rows between the two
(the same `chat.db` message GUID used for dedup below is reused here for a different purpose:
identity of the anchor row, not overlap protection for retries). Before applying the stored
high-water mark, each poll verifies the row at that same `ROWID` still carries the stored anchor,
without a guid-disclosing round-trip: the `poll` response already carries `floor_guid`, the `guid` at
the requested scan floor (the sanctioned floor-anchor disclosure, emitted regardless of the row's
conversation membership since the scan-floor anchor may name a withheld row), and the guard compares
that against the stored `anchor`. Where the same check runs outside a poll, the standalone `identity`
operation answers it with its boolean `matches` alone and never returns the row's actual `guid`
(§Typed wire schema). The stored high-water mark is applied only when the anchor still matches:

- **Match**: the stored high-water mark is trusted; the poll proceeds as normal.
- **Mismatch, or the `ROWID` no longer exists** (the database was replaced, reset, migrated, or
  the anchor row was deleted): the adapter does not trust the stale mark. It resets
  forward-only -- the checkpoint becomes the current maximum `ROWID` in the database, `anchor`
  updates to that row's GUID, `generation` increments, and the reset is logged and counted as a
  distinct, observable event.

This is a deliberate trade-off, stated explicitly: a deleted anchor row forces a forward-only
reset that can skip rows that arrived between the old checkpoint and the new one. That is
accepted because it matches this adapter's own activation semantic (history is not imported on
first activation, below), it is always loudly counted rather than silent, and the alternative --
applying an old high-water `ROWID` to a database that may no longer be the one it was recorded
against -- risks silently skipping unrelated rows or ingesting the wrong conversation's history
under a stale mark. The guard's job is narrower than perfect continuity: a stale mark is never
applied once the GUID check fails. Acceptance property: a page whose scan floor is a terminally
dropped or helper-withheld row does not trigger a forward-only reset on the next poll; the stored
anchor matches that scan-floor row's GUID and the high-water mark is trusted.

**The widened cursor contract, precisely.** The field belongs on the type the poller actually
commits, not only on the type storage returns. `ChannelPollPage` (§Amendment 2026-07-09) carries a
`next_checkpoint: ChannelCheckpoint` field -- the value `EmailChannel::poll_page` (and this
adapter's `poll_page`) hands back to the daemon loop after each page, and the value the loop passes
to `comm.cursor_commit`. `ChannelCheckpoint` gains one new field: `anchor: Option<String>`,
nullable for every existing adapter. `StoredChannelCheckpoint` -- the type `comm.cursor_get`
returns, which layers `source`, `generation`, and `updated_at` storage bookkeeping around the same
checkpoint fields -- carries `anchor` because it is built on `ChannelCheckpoint`, not as a second,
independent field; a `ChannelCheckpoint` read from storage and a `ChannelCheckpoint` produced by a
poll page are the same shape, so the value the loop commits is exactly the value that round-trips
through storage. The cursor read and commit operations (`comm.cursor_get` / `comm.cursor_commit`)
are widened to take `(channel_kind, channel_slug, source)` rather than `(channel_kind,
channel_slug)` alone, and both carry `anchor` on read and on write. The `Channel` trait (§2) gains
a source accessor -- a method returning the adapter instance's own `source` string -- so the daemon
loop can resolve which cursor row an adapter reads and commits against before the first poll of a
tick, not after. Stepwise, the daemon loop's per-tick sequence is:

1. Resolve the adapter's source via the trait's source accessor.
2. Read the cursor by `(channel_kind, channel_slug, source)`, yielding a `StoredChannelCheckpoint`.
3. Poll pages starting from the checkpoint the read returned.
4. For each page: handle every scanned row -- ingest, terminal drop, helper-withheld, or
   textless-skip (§Terminal disposition above) -- then, once the whole page is handled, commit the
   page's `next_checkpoint` (a `ChannelCheckpoint`, `anchor` included, `high_water` at the page's
   scan floor) via `comm.cursor_commit` when the page advanced the floor or reset the anchor. An
   empty page that changed neither is the one page the loop does not commit (below).

**Startup ordering: `source()` resolvable before the first cursor read.** Step 1 requires the source
accessor to return a resolved `source` on the loop's first tick. For an adapter whose `source` is
constant -- the email adapter, whose `source` is fixed per slug from configuration -- the accessor
resolves synchronously from construction and no ordering constraint arises. The iMessage adapter's
`source`, by contrast, embeds the provisioned `db_identity` the helper returns only in its
`activation` response (§Cursor identity: source and generation), so the accessor cannot resolve until
that handshake has completed. `spawn_imessage_channel_loops` therefore establishes the ordering as a
property of the spawn hook rather than an assumption of the per-tick loop: it issues the one-time
`activation` handshake and caches the returned `db_identity` -- together with the `max_rowid` and
`anchor_guid` that seed a first checkpoint (§Activation semantic) -- **before** it spawns the inbound
poll task. By the time that task runs step 1, the accessor resolves the composed `source` from the
cached value, so there is no window in which the loop reaches step 2's `comm.cursor_get` with an
unresolved source. If the startup `activation` fails -- the bridge host is unreachable, or the helper
refuses because the pinned database is unprovisioned (§Floor storage) -- the spawn hook logs the
failure and does not start the adapter's loop pair, fail-closed exactly as the §6 startup pre-flight
does for a denied ingest namespace; the next daemon start, or a bounded activation retry, re-attempts.
A started iMessage loop pair therefore always has a resolved `source`, making the
`activation` -> `source()` -> `cursor_get` ordering guaranteed by construction, not by tick timing.

On an empty page (no rows above the prior high-water), `poll_page` returns a `next_checkpoint`
equal to the checkpoint it was given -- `high_water` and `anchor` unchanged -- and the loop skips the
durable `comm.cursor_commit` rather than rewriting an unchanged row: a commit is issued only when a
page advances the floor or resets the anchor. An idle channel polling on its interval would
otherwise write one no-op cursor row per empty tick purely to bump `updated_at`, and at a
multi-second poll interval across every configured channel that is a steady stream of WAL-logged
transactions carrying no progress. `updated_at` is therefore a last-progress timestamp, not a
last-poll heartbeat, and channel liveness is read from the daemon's own tick accounting rather than
from a cursor row rewritten on every empty poll. On the forward-only reset
described above (identity guard mismatch), the adapter's `poll_page` sets `next_checkpoint.anchor`
to the new anchor row's GUID and `next_checkpoint.high_water` to the new maximum `ROWID` in the
same `ChannelCheckpoint` value, so the reset commits atomically with the rest of the page rather
than as a separate write. This is the precise, API-level statement of the `cursor_get -> poll_page
-> handle each -> cursor_commit` sequencing described in §Restart durability above.

**Migration: owner and sequence.** The comm pack owns `comm_channel_cursor` and today ships it
via a constant `CREATE TABLE IF NOT EXISTS` declaration (`COMM_CHANNEL_CURSOR_SCHEMA_STMT`) with a
two-column primary key `(channel_kind, channel_slug)` -- idempotent on a fresh database, but a
no-op against an existing table, so it cannot by itself add the `anchor` column or widen the
primary key on a database that already has the table. Adding both requires an actual schema
migration. Pack-auxiliary tables are non-versioned in v1 (ADR-017): a pack declares its tables
through idempotent `CREATE TABLE IF NOT EXISTS` statements and has no per-pack versioned-migration
lane for evolving a table's shape after production use. The accepted path for that evolution is a
core `khive-db` versioned migration recorded in the ADR-015 ledger, exactly as
`005-unique-comm-external-id.sql` (V5) promoted the comm pack's own `idx_comm_message_external_id`
to a durable unique index over an existing table, and `006-brain-retune-driver.sql` (V6) shipped the
brain pack's `brain_implicit_mass` accounting table: pack-owned logical state whose evolving schema
lands as a core `khive-db` versioned migration, so the production ledger records it. This amendment
therefore ships the cursor change as the next core migration, `V11`
(`crates/khive-db/sql/011-comm-channel-cursor-source-key.sql`, the slot immediately after the current
latest migration `V10`/`010-entities-content-ref.sql`), registered in the `MIGRATIONS` array and
recorded in `_schema_migrations` -- not as a further extension of the constant
`CREATE TABLE IF NOT EXISTS` statement and not a pack-owned versioned migration. It introduces no new
migration mechanism: it uses the core versioned-migration lane V5 and V6 already exercise for
pack-owned state, and records `V11` as a new row in ADR-015's canonical migration ledger. Appending
that ledger row is the ledger's defined bookkeeping for every core migration, not a normative change
to ADR-015's design.

**Current tree state versus this amendment's target.** This section is a normative specification, not
a description of the checked-out tree. As of this amendment the tree is at core migration `V10`;
`comm_channel_cursor` exists only in its two-column-primary-key form, minted by the pack constant
`COMM_CHANNEL_CURSOR_SCHEMA_STMT` (`crates/khive-pack-comm/src/vocab.rs`) and lazily re-declared by
the two cursor handlers before they query it (`crates/khive-pack-comm/src/handlers.rs`: the
`cursor_get` read path and the cursor upsert path each `execute_script` that same constant). No
`011-*.sql` file, no `MIGRATIONS`-array `V11` entry, and no three-column key are present in the tree
yet. The migration file, its ledger registration, and the constant/bootstrap retirements described
below are authored in the implementation PR that follows this design amendment, not in this docs
change. The reconciliation is therefore explicit and single-sourced: **current** = `V10`, the
two-column constant, and the two lazy handler bootstraps; **target** = `V11` rebuilding
`comm_channel_cursor` to the three-column `(channel_kind, channel_slug, source)` key with the
constant and both bootstraps retired so `V11` is the sole creator. Every sentence below that reads in
the present tense ("the constant is retired", "`V11` is applied by `kkernel db migrate`") states the
target this amendment mandates, which the implementation PR realizes.

Because `CREATE TABLE IF NOT EXISTS` cannot alter an existing table's primary key, widening the
uniqueness key to three columns requires a full table rebuild rather than an in-place `ALTER TABLE`.
The migration guarantees a source table to rebuild from and then rebuilds it unconditionally, so it
converges whether or not `comm_channel_cursor` already exists: a leading
`CREATE TABLE IF NOT EXISTS comm_channel_cursor` in the current two-column shape is a no-op on a
database that already has the table and an empty scaffold on one that does not, after which the
migration creates a new table carrying the three-column primary key
`(channel_kind, channel_slug, source)` and the `anchor` column, copies every row across carrying
each row's existing `source` value forward, drops the old table, and renames the new one into place.
The 2026-07-09 amendment introduced this table already carrying the `source` column, so no schema
state exists in which the column is genuinely absent from an existing row -- the only pre-population
case is a row whose `source` is `NULL`, coalesced to the email adapter's constant source (the
`imap+tls:{host}:{port}:{mailbox}:INBOX` shape), the only adapter whose rows can predate a populated
`source`. A never-had-the-table database and a two-column-table database therefore converge on the
identical three-column-primary-key, `anchor`-bearing shape.

**Atomicity and crash-idempotency.** The core migration runner (`run_migrations` in
`khive-db::migrations`) executes each migration's DDL and the matching `_schema_migrations` ledger
insert inside one SQLite transaction and commits them together. A crash at any point rolls the whole
transaction back: the database is left either entirely pre-migration (the original table intact) or
entirely post-migration with the ledger row recorded, and the runner's version guard skips the
migration on every later run. There is no half-rebuilt table and no applied-but-unrecorded state, so
the create-copy-drop-rename sequence above needs no crash-repair mechanism of its own -- the
migration's `.sql` carries no `BEGIN`/`COMMIT` of its own, because the runner owns the transaction
that makes the rebuild all-or-nothing. This is the reason the change belongs in the core lane rather
than a pack boot migration: the core runner already provides apply-and-record in one transaction,
whereas the pack's idempotent `CREATE TABLE IF NOT EXISTS` path has no ledger and no such guarantee.

Acceptance property: interrupting `kkernel db migrate` at any point during the V11 migration and
re-running it leaves the database either entirely pre-migration or entirely post-migration -- never a
half-rebuilt, data-less, or applied-but-unrecorded `comm_channel_cursor` -- because the runner
commits the rebuild DDL and the `_schema_migrations` record in one transaction, and the version guard
skips an already-recorded migration on re-run.

The pack's `COMM_CHANNEL_CURSOR_SCHEMA_STMT` constant is retired in the same change -- from
`COMM_SCHEMA_PLAN_STMTS` (the plain-DDL boot path) and from the two inline lazy bootstraps the cursor
handlers run, where the `cursor_get` read path and the cursor upsert path each `execute_script` the
same `CREATE TABLE IF NOT EXISTS` before their own query today -- so the V11 migration is the sole
creator of `comm_channel_cursor` and no boot path or verb handler mints the stale two-column-PK table
ahead of it. As a core migration, V11 is applied by `kkernel db migrate` (and auto-applied on
in-memory and ephemeral backends at creation, per ADR-015's exception for stores with no operator to
invoke it), and the MCP binary's existing fail-fast-on-stale-schema guard refuses to serve until the
operator has migrated -- so a production database reaches the three-column shape before the daemon
serves any cursor verb, and a database created through the migration path never lands on the retired
two-column shape. No new boot-wiring, pack-ordering, or fail-closed-boot semantics are introduced:
V11 reuses the operator-run migration contract of ADR-015 and ADR-071 unchanged.

Acceptance property: an upgraded database preserves every pre-existing cursor row's progress -- no
adapter's high-water mark regresses or is lost across the migration -- and a fresh database created
after this change has the three-column primary key and the `anchor` column from its first migration,
never the retired two-column shape.

**Bounded drain per tick.** The poll query is anchored on `chat.db`'s own monotone insertion
key, the message table's `ROWID`: each poll fetches rows with `ROWID` strictly greater than the
committed checkpoint, ordered ascending. The maintainer-conversation and sender-handle filter is pushed into the poll
query itself, evaluated on the bridge host inside the helper (§Sender validation, §Helper-side
authority pinning), so a page is a page of **candidate rows** -- up to **200 maintainer-matching
rows** above the floor -- rather than a fixed window of scanned rows of which only some match.
Counting the cap on candidates, not on rows scanned, is what keeps a maintainer message's drain
latency tied to maintainer traffic rather than to the bridge database's total cross-conversation
volume. Alongside the returned candidates the helper reports the greatest `ROWID` it has accounted
for -- the page's **scan floor** -- and the page's status, because the floor must advance past
non-candidate rows too or they would be rescanned every tick: on a **full page** of 200 candidates
the scan floor is the 200th candidate's `ROWID` and more candidates may remain above it (status
`page_full`); on a **short page** of fewer than 200 candidates the helper has exhausted every
candidate above the floor, so it advances the scan floor to the greatest `ROWID` its own scan
examined -- the upper bound of the window it just read under one consistent snapshot, jumping the
whole non-candidate tail in one step -- and marks the page `caught_up`. The advance uses the scan's
own examined maximum, never a separately issued `SELECT MAX(ROWID)`: a candidate inserted after the
scan's snapshot receives a `ROWID` above that examined maximum, so it stays above the committed
floor and is delivered on a later poll instead of being skipped. A tick drains
pages while each comes back `page_full`, and stops when a page returns `caught_up` (sleep until the
next interval), when a page returns `budget_truncated` (a mandatory scan budget fired mid-page; commit
and resume next tick, per §Bounded scan budgets below), or when a fixed cap of **10 pages** is
reached -- whichever comes first. Both the 200-candidate page size and the 10-page cap are normative
amendment text, not configuration or a new environment variable. Because only candidate rows are
returned and only candidate rows are ingested, at most **2000 candidate rows are ingested per tick**,
which bounds the local ingest phase in rows as well as bounding the remote scan (below): the tick's
work is bounded end to end, not only on the bridge side. The no-skip invariant survives the candidate
filter -- a non-candidate row below the advanced floor is one the maintainer filter would never have
returned, so skipping it permanently is correct, while every candidate row above the floor stays
selectable on a later poll -- and the checkpoint's `ROWID` floor advances to the page's scan floor
exactly as before, moving past withheld and non-candidate rows just as it moves past ingested ones. The checkpoint commits after every page that advances the floor, not only at the
end of the tick -- and, when the per-tick wall-clock cap fires mid-page (§Bounded drain per tick), at
the last candidate row ingested this tick; a page that finds no row above the floor advances nothing
and, having no progress to persist, is the one page not separately committed. When the 10-page cap is reached before catching
up, the task does not keep draining: it sleeps its normal `KHIVE_IMESSAGE_POLL_SECS` interval and
resumes on the next tick from the checkpoint the last page committed. This bounds each tick's
SSH/query/ingest work under sustained arrivals -- a conversation receiving messages faster than a
200-candidate page drains cannot pin the task in a continuous poll loop -- while the per-page commit
means no row is lost or re-skipped across the sleep boundary: the next tick continues from
exactly where the last page left off. No row between polls can fall outside a bounded window and
be skipped, because the query has no window -- only a floor -- and the 200-candidate-page/10-page-cap
bound (2000 candidate rows per tick) bounds duty cycle, not correctness. This bound also gives a
latency bound in ticks rather than wall-clock time, conditional on ticks that successfully drain
and commit: a fresh arrival is first observed by the next poll, a backlog at or below 2000 candidate
rows then drains within that tick, and a deeper backlog drains at 2000 candidate rows per tick
thereafter, so a row's wait from arrival to ingestion is bounded by
`1 + ceil(candidate_rows_ahead_of_it / 2000)` successful ticks -- the leading term is the poll that
first observes the row, so even a row with none ahead of it waits one tick rather than zero. Each
tick is one drain phase plus one `KHIVE_IMESSAGE_POLL_SECS` interval. The drain phase's per-row
ingest cost (embedding and FTS indexing) is bounded in count -- at most 2000 candidate rows per tick
(§Bounded drain per tick) -- but not in wall-clock seconds per row, so on a slow ingest backend a
single tick could otherwise run for an unbounded elapsed time and the tick-count bound above would
name no bounded latency at all. The drain phase therefore also carries a per-tick wall-clock
cap (default one `KHIVE_IMESSAGE_POLL_SECS` interval), checked after each candidate row's ingest rather
than only at page boundaries. When a tick has spent the cap, the adapter stops after the row it just
ingested and commits the checkpoint at that row's `ROWID` and `guid` -- a mid-page floor, valid because
the checkpoint floor is a `ROWID` and that row was fully handled -- then resumes the drain on the next
tick from just above it. The unhandled remainder of the in-flight page is re-fetched and re-ingested
next tick, which `guid` dedup makes idempotent (§Restart durability), so no row between the stopping
point and the page's scan floor is skipped. Checking the cap per row rather than per page bounds a
tick's ingest occupancy to the cap plus at most one row's embedding and FTS cost, not a whole page's:
a slow ingest backend can no longer hold the inbound worker for an unbounded 200-row page beyond the
interval. The helper's larger per-invocation scan budget (default 20 seconds, §Bounded scan budgets and
partial pages) is bounded independently, so it cannot livelock the smaller per-tick drain cap. Each
tick is thus bounded in candidate-row count (at most 2000 ingested), in wall-clock (the cap plus one
row's ingest), and in committed progress (the checkpoint advances past every row ingested this tick).
The tick-count latency figure is accordingly an **amortized** bound in successful ticks, each draining
a bounded candidate-row count under the per-row cap. The ADR therefore claims no hard
wall-clock bound per tick; the wall-clock budget is a best-effort cap, and what holds unconditionally is
the row-count bound above and the committed progress: a slow embedding or FTS backend behind a deep
backlog cannot occupy the inbound loop in one open-ended, uncommitted tick -- the backlog drains at the
backend's own throughput in bounded, individually-committed steps. Because the inbound poll loop and the
outbound send loop run as separate daemon tasks, a slow inbound drain never delays outbound sends.
The bound holds only across ticks that make progress: a tick whose poll, ingest, or checkpoint commit
persistently fails parks the checkpoint and is a liveness failure outside this latency bound, not a
longer value of it. A row queued behind a 4000-candidate backlog waits on the order of three
successful ticks (the observing poll plus two drain ticks), not an unbounded number.

**Bounded scan budgets and partial pages.** Pushing the maintainer predicate into the poll query
bounds the returned and ingested work by candidate count (above), but the bridge-side cost of
_finding_ those candidates still depends on how well `chat.db`'s own association indexes support the
maintainer-conversation and handle predicate. Where those indexes serve it, the scan cost also scales
with maintainer traffic; where they do not, the storage engine may examine unrelated rows to locate
each candidate, and that examination is not bounded by the candidate page cap. Because `chat.db` is
Apple's own read-only schema on the bridge host (§Inbound), the adapter cannot add an index to serve
this predicate: the poll has no usable-index guarantee, and this amendment does not assume one. Its
worst case is therefore explicit and accepted -- where no existing `chat.db` index serves the
maintainer predicate, a scan's discovery cost is proportional to the bridge database's total
cross-conversation insertion traffic above the floor, not to maintainer traffic alone. The sole
control on that cost is the per-invocation wall-clock scan budget below, which bounds scan time
regardless of the query's selectivity; the candidate page cap bounds only what is returned and
ingested, never the rows the engine examines to find them. The helper therefore
MUST impose a per-invocation wall-clock budget on the scan -- a mandatory bound, not the optional one
an earlier form of this section allowed -- together with a maximum serialized response size in bytes.
Both carry normative defaults tied to the transport envelope rather than left to the deployment: the
scan wall-clock budget defaults to 20 seconds and MUST in all cases stay strictly below the 35-second
SSH invocation deadline (§Transport deadlines and recovery), so the helper returns a `budget_truncated`
partial page -- preserving forward progress -- before the adapter's own deadline kills the child and
loses the page to a transport failure; the response-byte budget defaults to 8 MiB -- a size a single
`poll` response and the transport move comfortably inside the deadline, large enough that a page of
ordinary maintainer text messages reaches the 200-candidate cap long before the byte budget (only text
is ingested, attachment blobs are never carried). The byte budget is not claimed to exceed a worst-case
200-row page at the 1 MiB per-row cap -- 200 such rows would be far larger; the two caps are
independent and whichever binds first truncates the page, so a page whose accumulated serialized bytes
reach 8 MiB before 200 candidates are gathered is returned as a byte-`budget_truncated` partial page
carrying fewer rows, exactly as the scan budget truncates on time, and the response a caller must
receive within the deadline is bounded to 8 MiB regardless of row count. The budget arithmetic is
explicit and additive, not merely each term staying individually under the deadline: the 10-second
connect timeout and the 20-second scan default are both worst-case phases of the same invocation, so
together they can consume 30 of the 35-second deadline before any response leaves the bridge host,
leaving a 5-second reserve for the bounded post-scan stages the deadline must also cover --
serializing the byte-capped response, framing it, moving it over the SSH session, and the adapter
reading and deserializing it. Because the response is byte-bounded and the scan time-bounded, that
5-second reserve covers moving at most 8 MiB plus framing, so connect, scan, transport, and response
handling together complete within the invocation deadline rather than the scan alone consuming what
the connect phase leaves. Setup verifies `connect_timeout + scan_budget + transport_margin <=
invocation_deadline` with the margin sized for the 8 MiB response-byte budget, refusing a connect
timeout or scan budget raised so high that no reserve remains to return the page.
When either budget is reached before a page completes, the helper returns the candidates gathered so
far as a **partial page** with status `budget_truncated`: it sets the scan floor to the greatest
`ROWID` below which it has examined every row this page, so no candidate at or below that floor is
left unaccounted for, and returns rather than running to the page limit. The adapter commits that
partial floor exactly as it commits a full page and resumes the next tick from it. A
`budget_truncated` page MUST make forward progress: its `next_floor` is strictly greater than the
requested `scan_floor`, naming at least one row the helper examined this page, so the next tick
resumes above where this one stopped. A budget that fired before the helper could examine even the
first row above the floor -- which would leave `next_floor` equal to `scan_floor` and loop the tick
forever on the same page -- is a helper-side fault returned as `internal_error` rather than a
zero-progress `budget_truncated`, so a page that cannot advance at all fails loudly instead of
silently stalling. This is what makes
progress durable at sub-page granularity: a single fat page -- a burst of large-body or
attachment-bearing rows whose scan or serialization would otherwise overrun the SSH invocation's own
deadline before any checkpoint commits -- can no longer strand a tick that redoes the same work and
commits nothing on every retry. The byte budget bounds response size independently of row count, so a
page of unusually large rows is truncated by bytes before it can overrun the transport, and the
wall-clock budget bounds scan time independently of both, so a predicate the database indexes serve
poorly cannot hang the tick. Both budgets are the helper's own, fixed at build or setup and never
caller-supplied, so a compromised daemon can neither raise them to force an overrun nor lower them to
starve the drain; the `budget_truncated` status tells the adapter to resume promptly rather than
sleep as if caught up.

**Oversized single rows.** A page-level byte budget bounds a page that _accumulates_ past the limit,
but not a single message whose serialized text alone approaches or exceeds it. That row is a distinct
pathology: it can neither be returned within the page budget nor let the floor advance past itself,
so left unhandled it wedges the drain forever on that one `ROWID` -- the same zero-progress stall the
`internal_error` guard catches -- or forces the helper to silently drop it. The helper therefore caps
each row's serialized text at a normative **1 MiB** -- far below the 8 MiB page budget, so a single
capped row always fits a page, and far above any real maintainer text message (attachment blobs are
never carried, so only a pathologically long text body can reach the cap) -- and sets a
`text_truncated` boolean on the `MessageRow` whenever it truncates. The oversized row is ingested with
its text cut to the cap and the marker set, so the scan floor advances past it exactly as for any other
row. Truncation is a terminal disposition, not a retry: the truncated row is checkpointed like any
other, so no message can both fail to ingest and block the ones queued behind it.

Truncation is content-lossy in the ingested note but not at the system level, and the ADR takes that
position deliberately rather than adding a spill store. `chat.db` on the bridge host remains the system
of record and retains the full message body untouched -- the adapter only ever reads it, never mutates
or deletes a row (§Inbound) -- and the truncated note already carries the row's `guid`, the same
`chat.db` message GUID that keys inbound dedup (§Identity guard). That GUID is the overflow reference:
it locates the full original in the system of record. Recovering that full body is an out-of-band
operator action -- direct read access to the bridge host's `chat.db`, the same local access one-time
setup already requires -- not a helper-protocol operation: the wire surface deliberately exposes no
full-text-by-GUID retrieval, since a `poll` returns only maintainer-filtered, text-capped rows and the
`identity` op discloses nothing (§The concrete operation set). So `text_truncated` on a note means precisely "the
ingested text is a bounded projection whose full body remains in `chat.db`, recoverable only by local
access to it", never "the full body is gone" and never "ask the helper for it". Retention of that full body is exactly as durable as any ingested note's source
row -- a snapshot the source can later change or delete, the standard projection semantic every adapter
already has, not a new guarantee this case weakens. A dedicated spill mechanism (writing the overflow
to a blob store, an envelope carrying a separate storage handle) was considered and rejected: it would
introduce a second write-bearing surface with its own retention, addressing, and checkpoint-frontier
questions to serve a cap that only a pathologically long single message ever reaches, when the system
of record already holds the full body under a reference the note already carries. The cap plus the
`text_truncated` flag plus the carried GUID therefore satisfy forward progress, no-skip, and
recoverability of the full content together, without a spill subsystem.

**Behavior under sustained backlog.** The bound above is a duty-cycle bound, not a throughput
guarantee, and this section states plainly what happens when arrivals outrun it rather than leaving
it implicit. When the maintainer's `chat.db` accumulates more unhandled candidate rows than one tick
drains -- 2000 candidate rows per tick, all of which are maintainer-matching and therefore ingested
-- the excess is neither lost nor force-drained: it stays above the committed `ROWID` checkpoint and
is picked up on subsequent ticks, one bounded page at a time, until a page comes back `caught_up`.
Each tick's ingest phase is bounded in row count -- at most 2000 candidate rows -- but not in
wall-clock seconds: `comm.ingest` is serial and dominated by per-row embedding and FTS indexing,
whose per-row cost this amendment does not cap, so a deep backlog manifests as observable ingest lag
-- the maintainer's newest message waits behind the backlog ahead of it, bounded in successful ticks
(`1 + ceil(candidate_rows_ahead / 2000)`) but not in seconds. This is a deliberate
liveness-over-latency choice: correctness (no row skipped, no row double-ingested) holds regardless of
depth, and the bounded page/tick structure keeps a flooded conversation from pinning the daemon in a
continuous poll loop or starving the other channels' loops. Operator guidance: sustained lag shows as
a checkpoint whose `high_water` advances every tick yet never catches up while fresh rows keep
arriving above it. The helper's mandatory per-invocation wall-clock scan budget (§Bounded scan
budgets and partial pages above) already caps bridge-side scan time on every tick, so the remaining
lever for ingest lag is daemon-side embedding and FTS throughput, outside this amendment's scope.

**Activation semantic.** On first activation for a given `(kind, slug, source)` -- no prior
checkpoint row exists for that source triple (§Source-keyed cursor history) -- the checkpoint
initializes to the current maximum `ROWID` in `chat.db`, not to zero, and `generation` initializes to
`1` (§Cursor identity above). Because the cursor is keyed on `source` -- the provisioned database's
`db_identity` and the maintainer `handle_token` -- a source switch presents no prior row and is itself
a first activation for the new source, initialized forward-only exactly as a fresh channel is. The
channel is forward-only
from the moment it is activated: message history that predates activation is never imported,
matching the Telegram and email adapters' first-poll behavior. This also bounds the very first
drain to new messages only, so activation on an established `chat.db` does not attempt to ingest
years of prior conversation history.

This resolves the query-shape question the original decision text of this amendment left open:
the windowed `SELECT ... WHERE date > T LIMIT N` form is prohibited -- it is exactly the shape
issue #449 fixed for email -- and the `ROWID`-floor, bounded-drain form above (§Bounded drain
per tick) is the normative query.

### Outbound addressing

The routable address is `imessage:<slug>` (default slug `maintainer`, overridable by
`KHIVE_IMESSAGE_MAINTAINER_SLUG`), following the Telegram amendment's precedent exactly:
`<slug>` is a stable local name that resolves via config to the transport identifier, here
`KHIVE_IMESSAGE_MAINTAINER_HANDLE`. The raw handle -- a phone number or Apple ID email, and
comparably sensitive transport-identifying material to Telegram's numeric `chat_id` -- is
config-only and never appears in a `ChannelEnvelope`, a note property, or any content verb.
`imessage:<handle>` is not a supported address form in v1; `imessage:<slug>` is the only
normative form.

v1 is single-maintainer, matching Telegram and email: an outbound note addressed to any
`imessage:` slug other than the configured maintainer slug is unroutable and is logged and
dropped, never sent to the maintainer chat by default.

### Configuration (env-only, canonical config home `~/.khive/.env`)

| Variable                           | Required | Default      | Description                                                                                                                                                                                                                                                                                                                                                                                                                           |
| ---------------------------------- | -------- | ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KHIVE_IMESSAGE_SSH_TARGET`        | yes      | --           | SSH target for the bridge host (`user@host`). Credentials resolve via the SSH client, not this config. Validated fail-closed at config load (SSH target validation above).                                                                                                                                                                                                                                                            |
| `KHIVE_IMESSAGE_MAINTAINER_HANDLE` | yes      | --           | The maintainer's own iMessage handle (phone number or email) -- the remote counterparty, never the bridge account's own handle (§Two-identity model above). A bootstrap and display value: the helper filters inbound on its own setup-fixed provisioned handle and returns it at activation, which the adapter validates against, failing loud if this env var disagrees (§Sender validation). Never appears in an envelope or note. |
| `KHIVE_IMESSAGE_MAINTAINER_SLUG`   | no       | `maintainer` | The slug in `imessage:<slug>` that maps to the maintainer handle.                                                                                                                                                                                                                                                                                                                                                                     |
| `KHIVE_IMESSAGE_POLL_SECS`         | no       | `5`          | Inbound poll interval against the bridge host's Messages database. Must parse as an integer >= 1 (validation rule below).                                                                                                                                                                                                                                                                                                             |
| `KHIVE_IMESSAGE_INGEST_NAMESPACE`  | no       | `local`      | Target namespace for ingested inbound messages (passed as `namespace` to `comm.ingest`).                                                                                                                                                                                                                                                                                                                                              |

No secret material beyond what the SSH client already manages is configured here, and nothing
above is written to any khive store, matching §9. The Messages database path is deliberately not in
this table: it is a setup-time provisioning parameter fixed in the helper's root-owned configuration
on the bridge host (§Remote command boundary, §Helper-side authority pinning), not a daemon runtime
variable -- the daemon never names a database at runtime, so there is no `KHIVE_IMESSAGE_DB_PATH` for
a compromised daemon to repoint. The helper opens only the database its provisioning configuration
names, conventionally `~/Library/Messages/chat.db`.

**Provisioned files (fixed paths, not environment variables).** Three files the transport depends on
are deliberately not configurable through the environment, because the adapter must not discover
their location dynamically at runtime (§Transport: the pinned key file above): the pinned known-hosts
file `/etc/khive/imessage/known_hosts`, the pinned bridge key file `/etc/khive/imessage/bridge_key`
(the `<pinned_bridge_key>` named in every `ssh` invocation above), and the pinned bridge-account name
file `/etc/khive/imessage/bridge_account` (§SSH target validation and the end-of-options delimiter)
that the mandatory bridge-account username comparison reads. All three are fixed absolute paths
under the `/etc/khive/imessage/` provisioning root -- a root-owned system directory outside the daemon
account's home, deliberately not under the daemon's own state root. The non-writability the transport
section requires of the pin files and every directory up to the provisioning root could never hold
under that state root, which is the daemon's per-user config and data home (it holds `khive.db` and the
other live databases the daemon writes) beneath the daemon account's own home -- both writable by the
daemon by definition. Rooting the
pin files at a root-owned `/etc/khive/imessage/`, established by the privileged one-time setup step, is
what makes the non-writability requirement satisfiable: the daemon account reads the pinned host key
and presents the pinned client key -- the private key owned by the daemon account with mode `0400`,
the shape OpenSSH requires (it refuses a key bearing any group or other permission bit, so the key
cannot instead be root-owned and group-readable), inside the root-owned provisioning directory the
daemon cannot write -- so the daemon can read the key but can neither redirect its resolved path nor
replace the file at that path, and rewriting the contents in place gains nothing against the
server-side confinement (§Transport: the pinned key file above).
All three are subject to the integrity checks the transport section states -- non-symlink, root-owned
and root-only-writable (the bridge key file itself excepted, per its OpenSSH-constrained shape
above), reached only through root-owned, root-only-writable directories -- with the adapter refusing
to start when any check fails. Fixing these paths rather than deriving them from the environment removes the
substitution surface an env-configured path would open: a compromised daemon cannot point the
transport at a different key, a rewritten known-hosts file, or a different pinned bridge-account name
by altering its own environment.

**Poll-interval validation.** `KHIVE_IMESSAGE_POLL_SECS` is validated at config load, not at
first use: it must parse as an integer greater than or equal to 1. A zero value, a negative
value, or a value that fails to parse as an integer is rejected: `ImessageChannelConfig::from_env`
returns `ChannelError::Config`, the channel does not start, and the logged warning names the
variable. This matches the fail-loud posture the rest of this adapter's configuration already
has (`KHIVE_IMESSAGE_SSH_TARGET` and `KHIVE_IMESSAGE_MAINTAINER_HANDLE` fail construction the
same way when absent) rather than silently clamping to the default or to 1. Regression coverage
is named in the acceptance properties below.

### Two-identity model

The bridge Mac signs into the Messages application under a dedicated bridge Apple ID: a new
account, created solely for this purpose, that carries only an email address and no phone
number. `KHIVE_IMESSAGE_MAINTAINER_HANDLE` is the maintainer's own handle -- the remote
counterparty the bridge account exchanges messages with -- and is never the bridge account's own
handle.

Distinct identities are a functional requirement, not a preference. The bridge's read side
(§Sender validation below) enforces `message.is_from_me = 0` on every row it ingests, precisely
to exclude the adapter's own outbound sends from being re-ingested as inbound. If the bridge Mac
signed into the same account as the maintainer's own handle, every message the maintainer sent
would sync into that shared account's conversation as a row carrying `is_from_me = 1` on the
bridge side (Messages syncs sent-from-any-device state across all devices signed into one Apple
ID) and would be rejected by that same mandatory filter -- the maintainer's real replies would be
silently dropped, not the adapter's own echoes. Two distinct Apple IDs are what makes
`is_from_me` a correct discriminator between "the bridge sent this" and "the maintainer sent
this" in the first place.

The bridge account's email-only, no-phone-number shape is also what the SMS/MMS/RCS service check
(§Sender validation below) depends on structurally: an Apple ID with no paired phone number has
no SMS forwarding path, so every row that lands in the bridge's own `chat.db` is iMessage-service
by construction, not merely by the configured handle's shape.

### Host requirements (informative)

The bridge host must be signed into the Messages application under the dedicated bridge Apple ID
described above. The SSH-invoked processes need two one-time macOS permission grants on the
bridge host: Full Disk Access (for the `chat.db` reads) and Automation permission for the
Messages application (for the `osascript` sends). Both are host-local, one-time grants made by
whoever administers that Mac; this amendment names the requirement without prescribing the exact
System Settings click path, which drifts across macOS releases.

### Feature gating

`khive-mcp/Cargo.toml` gains `channel-imessage = ["khive-channel", "khive-channel-imessage"]`
with `khive-channel-imessage` an optional dependency, mirroring `channel-email` and
`channel-telegram`. Default build excludes it. `channel-email`, `channel-telegram`, and
`channel-imessage` can all be enabled together; the `ChannelRegistry` keys by `(kind, slug)`, so
all three adapters coexist. Shipping the adapter through the product executable additionally
requires a kkernel `channel-imessage` pass-through feature. The kkernel `channel-email`
pass-through already exists; the `channel-telegram` pass-through is pending in a separate build
change not yet merged. This amendment's `channel-imessage` pass-through is its own addition and
does not depend on the Telegram pass-through landing first.

### Out of scope (v1)

- Group chats.
- Attachments and rich content (text only).
- Multiple maintainer handles / a general `imessage:<slug>` address book beyond the single
  maintainer slug.
- Writing to `chat.db` in any form. This is a permanent prohibition, not a v1 deferral: the
  adapter never writes to Apple's own database, in any future version.

### Acceptance properties

These properties fall into two tiers, and the tier determines where each is provable. **Tier A
(unit)** properties hold against a mocked SSH transport and a `chat.db` test double in ordinary CI,
with no bridge host: they constrain the adapter's own logic -- input validation, dedup, checkpoint
and resume, forward-only reset, duty cycle, sender-validation defense in depth, and fail-closed
parsing of malformed or version-mismatched responses. **Tier B (integration harness)** properties
require a real bridge host -- a live `sshd`, the installed helper binary, real filesystem checks,
migration crash-injection, or `sshd -T -C` -- and cannot be proven by a mock, because a mock would
merely assert the behavior it is standing in for; they constrain the bridge-side and server-side
controls the adapter depends on but does not itself implement. Properties 1-13, 16, 23, and 28-30 are
Tier A; properties 14, 15, 17-22, 24-27, and 31-39 are Tier B. The header on an earlier revision of
this section, which labeled the whole set "transport mocked," was inaccurate for the Tier B
properties and is corrected here.

1. Every SSH invocation's remote command line is the fixed remote helper's literal, unparameterized
   name; the outbound message text and the poll floor reach the bridge
   host only over stdin as structured data, never as a remote-argv element or an interpolated
   shell/AppleScript string (the injection fence). The database path and the recipient are not wire
   inputs at all -- the helper resolves both from its root-owned provisioning configuration (§Remote
   command boundary, property 17) -- so no daemon-supplied value can name them. A dedicated adversarial
   regression test asserts that an outbound message body containing shell metacharacters (backticks,
   `$( )`, semicolons, quotes) cannot alter the remote command the bridge host executes.
2. Simulating a restart mid-poll and re-delivering an already-seen `chat.db` row does not produce
   a duplicate inbound note; dedup is by GUID through `idx_comm_message_external_id`.
3. An unreachable bridge host degrades that one channel to a channel-down warning; other
   configured channels are unaffected and the daemon does not fault.
4. Every `chat.db` open the adapter performs is asserted read-only by the test double (no write
   flags requested).
5. A poll after a simulated restart resumes strictly above the committed `ROWID` checkpoint, not
   from an in-memory default; a `chat.db` row with `ROWID` at or below the checkpoint is never
   re-ingested and a row above it is never skipped, across a page-limit boundary.
6. On first activation for a `(kind, slug, source)` with no prior checkpoint row for that source, the
   checkpoint initializes to the maximum `ROWID` the helper's `activation` response reports -- at or above 1
   for a provisioned database, since one-time setup refuses to provision a database whose messages
   table is empty (§Floor storage); a `chat.db` row inserted before activation is never ingested.
7. `KHIVE_IMESSAGE_POLL_SECS` set to `0`, a negative integer, or a non-integer string fails
   channel construction with a warning naming the variable; the channel does not start.
8. A `chat.db` row with `is_from_me = 1`, a sender handle that does not equal the helper-reported
   provisioned maintainer handle carried at activation (§Sender validation), a conversation outside
   the maintainer conversation, or a service other than iMessage (SMS, MMS, or RCS), is not ingested;
   the drop is counted. An adapter-sent row in the maintainer conversation is never ingested as
   inbound.
9. Under sustained arrivals exceeding the bound of 10 pages of 200 rows each (2000 rows per
   tick), the inbound task sleeps its normal poll interval between ticks rather than draining
   continuously, and no row is skipped across that sleep boundary -- the next tick resumes from
   the checkpoint the last page committed.
10. When the GUID at the stored checkpoint `ROWID` no longer matches the remote database (row
    deleted, database replaced), the next poll resets forward-only to the current maximum
    `ROWID`, increments `generation`, and logs and counts the reset; a stale high-water mark is
    never applied once the GUID check fails.
11. `KHIVE_IMESSAGE_SSH_TARGET` values with no `user@` prefix, beginning with `-`, or containing
    whitespace or a control character, fail channel construction with a warning naming the variable
    and the channel does not start; the mandatory `user@` names the bridge account, so a bare-host
    target that would let `ssh` default the remote user to the daemon's own login name -- reaching an
    account with none of the server-side confinement -- is refused. Every outbound `ssh` invocation
    additionally passes `--` immediately before the target so a value that reached the invocation
    unvalidated can still never be parsed as an `ssh` option.
12. Switching `KHIVE_IMESSAGE_SSH_TARGET` from a source A to a
    source B and back to A resumes source A's own checkpoint row at its own high-water mark; rows
    that arrived in source A's database while source B was active are ingested on return, never
    skipped.
13. A page whose final row is terminally dropped (§Terminal disposition) does not trigger a
    forward-only reset on the next poll: the stored `anchor` names that dropped row's GUID, the
    re-read at the checkpoint `ROWID` matches it, and the stored high-water mark is trusted.
14. Using the daemon's bridge-account SSH key to request an arbitrary remote command, a PTY, or
    any form of TCP, Unix-domain-socket (stream-local), agent, or X11 forwarding fails at the bridge
    host's `sshd` -- by `restrict` and, at the server-configuration layer, by the bridge account's
    `Match` block setting `AllowTcpForwarding no` and `AllowStreamLocalForwarding no` -- and a
    tunnel-device forwarding request fails because the `sshd_config` sets `PermitTunnel no` explicitly;
    only the fixed remote helper, at its forced-command path, ever executes for that key.
15. A hostile `~/.ssh/config` (or `/etc/ssh/ssh_config`) `ProxyCommand` or `Match exec` directive
    on the daemon's own account never executes as part of this adapter's `ssh` invocations, and an
    entry in the global known-hosts database that would otherwise satisfy host verification does
    not substitute for a matching entry in the dedicated `/etc/khive/imessage/known_hosts` file.
16. An adapter invocation against a helper reporting a protocol version the adapter does not
    support fails closed as a transport failure; the mismatched response is never parsed as if it
    were a supported shape.
17. The `poll` and `send` request bodies carry no database-path or recipient field (§Remote
    command boundary, §Helper-side authority pinning): the helper opens only the database its
    root-owned provisioning config names -- verifying that database's content identity (§Floor
    storage) before use -- and addresses every `send` to the provisioning-pinned recipient,
    regardless of request content. No wire input can redirect the helper to a different database
    or recipient; a compromised daemon can vary only the poll floor and the outbound message text.
18. An invocation run with a default SSH identity file present and an `ssh-agent` reachable via
    `SSH_AUTH_SOCK` in the environment still authenticates only with the pinned bridge key; the
    session never falls back to a default identity file or an agent-offered key.
19. The bridge host's `sshd_config` sets `PermitTunnel no` explicitly, and a session opened with
    the daemon's key that requests tunnel-device forwarding is refused.
20. Interrupting `kkernel db migrate` at any point during the cursor-widening migration (V11) and
    re-running it leaves the database either entirely pre-migration or entirely post-migration, never
    with a half-rebuilt, data-less, or applied-but-unrecorded `comm_channel_cursor`: the core runner
    commits the rebuild DDL and the `_schema_migrations` record in one transaction, and the version
    guard skips the migration once it is recorded (§Atomicity and crash-idempotency).
21. The adapter refuses to start when `/etc/khive/imessage/known_hosts` or the pinned bridge key at
    `/etc/khive/imessage/bridge_key` is a symlink or resolves through a directory that is not
    root-owned and root-only-writable, when the known-hosts file is not root-owned or carries a
    group- or other-write permission bit, or when the pinned
    bridge key is not owned by the daemon account or carries any group or other permission bit -- a
    key shape `ssh` would itself refuse to load.
22. Authentication to the bridge account by any method other than the single provisioned restricted
    public key -- a password, a keyboard-interactive prompt, a CA-signed certificate, or a key
    returned by an `AuthorizedKeysCommand` -- is refused by the bridge host's `sshd`, and the
    account's effective configuration read back with `sshd -T -C user=<bridge-account>` shows
    `trustedusercakeys none`, `authorizedprincipalsfile none`, `authorizedprincipalscommand none`,
    `passwordauthentication no`, `kbdinteractiveauthentication no`, and `permituserenvironment no`,
    with an empty effective `acceptenv` list -- verifying the CA, principals, and
    environment-injection controls are cleared for this account in the effective config, not merely
    omitted from the `Match` block and inherited from a global setting, which config-text inspection
    alone would miss.
23. A page whose scan floor is a row the helper withheld (not returned by its bridge-side maintainer
    filter) does not trigger a forward-only reset on the next poll: the stored `anchor` names that
    withheld row's GUID, returned by the helper as identity-only metadata without the row's text or
    sender, the re-read at the checkpoint `ROWID` matches it, and the stored high-water mark is
    trusted.
24. A `poll` request naming a scan floor below the helper's setup-recorded activation floor is
    refused by the helper with a structured error and returns no rows, even when correctly versioned,
    protocol-valid, and authenticated by the pinned key.
25. A helper request bearing an operation discriminator outside the closed
    `{activation, poll, identity, send}` set is refused with a structured error and executed as no
    operation, never interpreted as the nearest known operation.
26. An `identity` request naming a `ROWID` below the helper's recorded scan floor is refused with a
    `below_scan_floor` structured error and discloses nothing, while an `identity` request for a
    checkpoint row at or above the floor -- carrying the caller's stored anchor `guid` -- returns a
    `matches` boolean and never the row's actual `guid`, so the operation verifies an anchor the
    caller already holds and cannot enumerate the `guid` of a `ROWID` it has not already observed;
    a `found` false indication is returned when that row has since been deleted.
27. Re-running one-time setup against a database identity the helper has already recorded retains that
    identity's first-provisioning scan floor rather than advancing it; a return to a previously
    provisioned database resumes above its original floor, so backlog rows that arrived while it was
    inactive are accepted, not refused.
28. **[unit]** A poll response missing a required field of the typed wire schema (§Typed wire
    schema), carrying both a `result` and an `error`, or bearing a `page_status` outside
    `{page_full, caught_up, budget_truncated}`, is refused as a transport failure and applied as no
    operation; a well-formed `activation` response yields a starting `ROWID`, an anchor `guid`, and a
    database-identity token.
29. **[unit]** A freshly provisioned adapter obtains its starting `ROWID` and anchor `guid` from a
    single `activation` response and ingests no pre-existing row; the first `poll` it issues requests
    rows strictly above that `ROWID`.
30. **[unit]** Given a `budget_truncated` poll response whose `next_floor` is above the requested
    `scan_floor`, the adapter commits that floor and resumes the next tick strictly above it,
    ingesting the returned candidates exactly once, neither re-requesting them nor treating the short
    page as caught-up.
31. **[integration]** An adapter at a protocol version whose authority semantics differ from the
    version a helper reports refuses that helper outright rather than accepting it under the
    one-behind tolerance; a helper predating an authority-affecting version bump cannot serve an
    adapter past that bump's floor (§Helper artifact and protocol versioning).
32. **[integration]** The dedicated bridge key authenticates as exactly one principal -- the bridge
    account -- through exactly one entry, the single root-owned forced-command `authorized_keys` line:
    presented against any other account on the bridge host it is refused. One-time setup establishes
    this by auditing every account the host's own directory enumerates (`dscl . -list /Users` on
    macOS, system and role accounts included, never a curated list), resolving each account's
    effective configuration with `sshd -T -C user=<account>,addr=<daemon-source-address>,host=<daemon-source-host>`
    -- the daemon's real connection tuple, so an `Address`- or `Host`-scoped `Match` block a bare
    `user=<account>` query would miss is still evaluated -- and failing closed if any non-bridge
    account's resolved `AuthorizedKeysFile` contains the pinned key, if any account's
    `AuthorizedKeysCommand` or `AuthorizedPrincipalsCommand` is other than `none`, or if the bridge
    account admits the key through any entry lacking the forced command -- so a daemon-chosen `user@`
    naming a different account, or a `Match` block scoped to the daemon's address or host rather than
    to a user, cannot escape the forced-command and `Match`-block confinement
    (§Audit scope: every account, not only the bridge account).
33. **[integration]** No `poll`, and no daemon-supplied floor value, ever lowers a recorded scan
    floor; the recorded floor decreases only after an explicit root-run floor-reset re-provisioning,
    and a database whose maximum `ROWID` has fallen below the recorded floor after a legitimate
    cleanup yields `below_scan_floor` on every poll until that ceremony runs, never a silent history
    rescan (§Floor storage, and recovery from a database that legitimately shrinks).
34. **[integration]** The helper enforces its own per-invocation wall-clock and response-byte budgets,
    each with a normative default (scan budget 20 seconds, strictly below the 35-second SSH invocation
    deadline; response-byte budget 8 MiB) and neither raisable nor lowerable by any caller-supplied
    value: a page whose scan or serialization would exceed either budget returns `budget_truncated`
    with `next_floor` set to the greatest fully-examined `ROWID`, before the adapter's transport
    deadline can kill the invocation. Separately, the adapter bounds each poll tick's drain phase by a
    wall-clock budget (default one `KHIVE_IMESSAGE_POLL_SECS` interval), checked after each candidate
    row's ingest rather than only at a page boundary: when the cap fires mid-page, the adapter stops
    after the row it just ingested and commits the checkpoint at that row's `ROWID`, a mid-page floor,
    rather than waiting for the 200-row page to complete, so a slow ingest backend cannot make a
    single tick run for an unbounded duration (§Bounded drain per tick).
35. **[integration]** One-time setup fails, and no key is provisioned, when the helper binary at its
    forced-command path, any directory up to root on that path, the helper's root-owned provisioning
    configuration, the bridge account's `AuthorizedKeysFile` or any directory up to root on its path,
    or the `/etc/khive/imessage/scan_floors` ledger is a symlink, is not root-owned, carries a group- or
    other-write permission bit,
    or resolves through a directory that is not similarly root-owned and root-only-writable.
36. **[integration]** One-time setup fails, and no key is provisioned, on a bridge host whose
    effective `sshd` configuration files -- the main `sshd_config` and every file reached through an
    `Include` -- set `AuthorizedKeysCommand` or `AuthorizedPrincipalsCommand` to any value other than
    `none`, in any context, global or `Match`-scoped whatever the block's criteria; carry an
    `AcceptEnv` inside any `Match` block; or set `PermitUserEnvironment` to any value other than `no`.
    The bridge account's own `Match` block sets both key-source directives to `none` (§Bridge-account
    authentication lockdown), so the scan accepts `none` and rejects only an enabling value. These are
    the directives a per-account `sshd -T -C` read (property 22) cannot rule out for a `Match` block
    keyed on address, host, or an `exec` predicate: property 22 verifies the bridge account's effective
    values are cleared, and this scan refuses an enabling setting of any of them by textual inspection
    of the config text, the two halves closing the dynamic-key and environment-injection surfaces from
    both directions (§Server-side key confinement).
37. **[integration]** A maintainer message whose text alone exceeds the 1 MiB per-row cap is ingested
    with its text truncated at the cap and `text_truncated` set on the row, and the scan floor
    advances past it: the row neither wedges the drain with zero progress on its `ROWID` nor is
    silently dropped, and its `guid` -- carried on the note -- locates the untruncated body in the
    bridge's `chat.db`, recoverable only by local operator access and never through a helper-protocol
    read (§Oversized single rows).
38. **[integration]** An outbound `send` whose response is lost after the helper delivered it is not
    delivered a second time on retry: the retry carries the same `send_id` derived from the outbound
    note's identity, the helper finds it in its bounded delivered-`send_id` ledger and acknowledges
    without re-driving `osascript`, so the maintainer receives one message; the sole residual
    duplicate window is a helper crash between a successful delivery and the ledger write
    (§Outbound delivery idempotency).
39. **[integration]** Re-provisioning the same `chat.db` against a changed maintainer handle drains
    the new maintainer conversation forward-only from activation rather than resuming the prior
    handle's high-water mark: the changed handle changes the `handle_token` and therefore the cursor
    `source`, so a fresh cursor row is keyed while the prior handle's row is retained intact, and no
    new-conversation row below the stale mark is skipped (§Source-keyed cursor history).

### Consequences

- `comm.inbox`/`read`/`reply`/`thread` unchanged for agents; a new channel-prefixed address
  space (`imessage:<slug>`) reuses `comm.ingest`, the dispatch gate, the dedup index, and the
  envelope exactly as email and Telegram do.
- A new `khive-channel-imessage` crate at the platform layer; a `channel-imessage`-gated
  `spawn_imessage_channel_loops` in `khive-mcp`. This amendment corrects and extends the
  per-adapter loop pair pattern to a fleet-wide rule: it supersedes the `poll_all` lifecycle
  prescriptions of §§4, 6, and 12, and every future adapter is required to ship its own
  daemon-spawned loop pair.
- No credentials in any note, entity, or KG store; the SSH keypair and the maintainer handle are
  env/SSH-client configuration only.
- The inbound poll cursor is durable, not in-memory: it reuses the `comm_channel_cursor`
  checkpoint mechanism Amendment 2026-07-09 introduced for email, anchored on `chat.db`'s `ROWID`
  rather than a time window, so the missed-ingest failure class issue #449 fixed for email cannot
  recur here. Reuse required an explicit, additive schema migration: a new optional `anchor`
  column (null for existing adapters) and a uniqueness key widened from `(channel_kind,
  channel_slug)` to `(channel_kind, channel_slug, source)`, plus the matching
  `comm.cursor_get`/`comm.cursor_commit` API surface change. The cursor's `source` identifies the
  configured SSH target, the provisioned database's `db_identity` content-lineage token, and a token
  derived from the provisioned maintainer handle -- never a database path or a raw handle, neither of
  which crosses the wire (§The widened cursor contract, §Source-keyed cursor history) -- and, under the
  widened key, gives each source its own persistent row, so switching sources and back, or
  re-provisioning against a changed handle, resumes or newly keys the correct row rather than reusing
  another conversation's mark. Its `generation` tracks resets, and a GUID identity guard on the `anchor` row
  forces a loudly counted, forward-only reset rather than silently applying a stale mark to a
  replaced database. Each poll tick is bounded to 10 pages of 200 rows each (2000 rows), sleeping
  between ticks under sustained arrivals rather than draining continuously.
- Inbound rows sent by the adapter itself (`is_from_me = 1`) are excluded at ingest, preventing
  the adapter from re-ingesting its own outbound sends as trusted maintainer input, and rows
  carried over SMS, MMS, or RCS are excluded regardless of sender handle, since only an
  iMessage-service row is treated as an authenticated delivery from the signed-in Apple ID.
- `KHIVE_IMESSAGE_SSH_TARGET` is validated fail-closed at config load against a restrictive
  shape, and every `ssh` invocation passes `--` before the target, closing the option-injection
  path an unvalidated or malformed target value would otherwise open.
- The address form (`imessage:<slug>`, normative) and the poll-interval and sender-validation
  rules are settled by this amendment text; no open items remain for design sign-off.

## Context

The autonomous build loop blocks regularly on the maintainer. Merge approvals and ADR
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
    /// The per-channel outbox loop calls this for live outbound delivery
    /// (§"The outbound path is live, not deferred"); `external_id`
    /// write-back and reply routing are described in §5c/§5d.
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError>;

    /// Poll for new inbound messages received since `since`.
    ///
    /// Returns envelopes ready to be ingested. Per-message validation errors
    /// must be logged and skipped; one bad message must not abort the batch.
    ///
    /// The original time-window poll; superseded for cursor-durable adapters by
    /// `poll_page` below (see the note after this block).
    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError>;

    /// Stable `source` identity string for a cursor-durable adapter, composed
    /// from its transport-fixed identity (email: host/port/mailbox; iMessage:
    /// SSH target, `db_identity`, and provisioned-handle token). The daemon loop
    /// resolves which `comm_channel_cursor` row to read and commit against,
    /// before the first poll of a tick, by calling this. Time-window adapters
    /// (Telegram) keep their own watermark and return `None`.
    fn source(&self) -> Option<String> {
        None
    }

    /// Checkpoint-aware poll for cursor-durable adapters. Given the stored
    /// checkpoint (`source`, `generation`, high-water `ROWID`/UID, and the
    /// `anchor` GUID), returns one bounded `ChannelPollPage` whose
    /// `next_checkpoint` the daemon loop commits after the whole page is handled
    /// (§"The widened cursor contract, precisely"). This is the normative poll
    /// for any adapter that persists a durable checkpoint; the default reports
    /// that a time-window adapter does not implement it.
    async fn poll_page(
        &self,
        checkpoint: StoredChannelCheckpoint,
    ) -> Result<ChannelPollPage, ChannelError> {
        let _ = checkpoint;
        Err(ChannelError::Config(
            "poll_page is not implemented by this time-window adapter; use poll".into(),
        ))
    }
}

pub enum ChannelError {
    Config(String),
    Transport(String),
    Auth(String),
    UnauthorizedSender(String),
    InvalidEnvelope(String),
}
```

> **Cursor-durable poll superseded 2026-07-17.** The `poll(&self, since: DateTime<Utc>)` signature
> above is the original time-window poll the Telegram-era adapters used. It is superseded for
> cursor-durable adapters -- the email adapter (§Amendment 2026-07-09) and the iMessage adapter
> (§Amendment 2026-07-17) -- by `poll_page`, which returns a `ChannelPollPage` carrying a
> `next_checkpoint` and is driven by a `ROWID`/UID cursor rather than a wall-clock `since`, together
> with the `Channel::source` accessor and `Channel::poll_page` method the trait above now declares
> (§"The widened cursor contract, precisely" in the 2026-07-17 amendment). A `since`-based time-window poll is the shape issue #449
> showed can permanently skip rows once a page limit moves past them; `poll_page` is the normative
> poll for any adapter that persists a durable checkpoint. Telegram retains its own in-memory offset
> watermark and is unaffected.

### 3. The normalized envelope

```rust
pub struct ChannelEnvelope {
    /// Logical sender address. For inbound, the maintainer's logical address (see §8).
    pub from: String,
    /// Logical recipient namespace.
    pub to: String,
    /// Message body, plain text.
    pub body: String,
    /// "telegram", "email", "imessage". Stored in properties.channel_kind on the note.
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
    /// Outbound idempotency key, daemon-derived from the outbound note's stable identity and carried
    /// on the send path so a transport that can redeliver (the iMessage SSH bridge) dedups duplicate
    /// sends (§Outbound delivery idempotency). `None` on an inbound envelope.
    pub send_id: Option<String>,
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
is untyped and existing notes without these fields are unaffected. `send_id` maps to no inbound note
property: it is outbound-only, populated by the outbox loop from the outbound note's stable identity
and forwarded by the adapter as the `send` operation's idempotency key (§Outbound delivery
idempotency).

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

    // Inbound is not a registry-level sweep: each adapter spawns its own inbound and outbound
    // loop pair (`spawn_*_channel_loops`). There is no `poll_all` method on this interface.
}
```

> **Lifecycle retired 2026-07-17.** Earlier revisions of this section declared a registry-level
> `pub async fn poll_all(&self) -> Vec<ChannelEnvelope>` sweep. That method is retired: it is not
> part of the `ChannelRegistry` interface and nothing implements it. The normative inbound lifecycle
> is per-adapter loop pairs registered through `ChannelRegistry` and spawned individually by the
> daemon role -- one inbound task and one outbound task per adapter. See
> [§Amendment 2026-07-17](#amendment-2026-07-17----imessage-channel-over-an-ssh-bridge).

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
2. The ingest loop MUST always pass `"namespace": "<target_agent_namespace>"` explicitly
   in every `comm.ingest` dispatch call.

Params:

| Param          | Type   | Required | Description                                                                                                                                                                     |
| -------------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `namespace`    | string | yes      | Target agent namespace (the namespace whose `comm.inbox` the message must land in). Declared as `ParamDef` so dispatch forwards rather than strips it. Must match `comm.inbox`. |
| `from`         | string | yes      | Sender address. Preserved as channel-prefixed form (see open question §OQ-1).                                                                                                   |
| `to`           | string | yes      | Recipient logical address.                                                                                                                                                      |
| `content`      | string | yes      | Message body.                                                                                                                                                                   |
| `subject`      | string | no       | Optional subject line.                                                                                                                                                          |
| `thread_id`    | string | no       | 36-char UUID; supplied after thread resolution (§5b).                                                                                                                           |
| `channel_kind` | string | no       | `"telegram"`, `"email"`, `"imessage"`. Stored in `properties.channel_kind`.                                                                                                     |
| `external_id`  | string | no       | Transport id. Stored in `properties.external_id`; primary dedup key.                                                                                                            |
| `sent_at`      | string | no       | RFC 3339; defaults to now.                                                                                                                                                      |

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

#### 5c. Outbound path and `external_id` write-back (superseded -- see §"The outbound path is live, not deferred")

**Superseded.** The authoritative outbound-lifecycle contract is the amendment §"The outbound
path is live, not deferred" above: a per-channel outbox loop drains outbound `message` notes and
calls the channel's `send`, so outbound delivery is live for a shipped adapter, not deferred. The
design below describes an earlier, different write-back mechanism (`channel_registry.send_outbound`)
that the shipped outbox loop did not adopt; it is retained for historical context and is not the
current contract.

The intended design: the binary calls `channel_registry.send_outbound` after a successful
`VerbRegistry::dispatch("comm.send", ...)` for messages directed to a configured external
target. The external id returned by `send_outbound` is written back to the outbound note's
`properties.external_id` via `VerbRegistry::dispatch("update", ...)` with a properties patch.
`update_note` uses `merge_properties` with `PreferFrom` policy, preserving existing properties.

#### 5d. Reply routing (superseded -- see §"The outbound path is live, not deferred")

**Superseded.** Reply routing rides the live outbound path (§"The outbound path is live, not
deferred"); the authoritative outbound-lifecycle contract is that amendment. The design below is
retained for historical context. Along these lines the binary observes a `comm.reply` dispatch
result, reads
`properties.channel_kind` from the original inbound note, and calls
`channel_registry.send_outbound` for the reply. The comm-pack handler is unchanged.

### 6. The polling loop lives in the binary, not in a pack

**Lifecycle model superseded.** This section describes the original single shared `poll_all`-driven
task; that model is retired. The 2026-07-17 amendment's per-adapter loop pairs
(`spawn_*_channel_loops`, §The `khive-channel-imessage` crate) are the sole authoritative lifecycle
model, correcting the 2026-07-05 amendment's narrower supersession claim that scoped itself only to
the outbound path and reply routing (§5c, §5d) without retiring this section's single-task
description. Every subsequent reference below to "the polling loop" as a single task -- what it
holds, its startup pre-flight, its per-dispatch namespace handling -- describes mechanics that still
apply, individually, to each adapter's own inbound loop; it is retained for that mechanical detail,
not as a description of a currently-live shared task. Where this section and the per-adapter model
disagree on how many tasks run, the per-adapter model governs.

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

> **Retired 2026-07-17.** There is no single `poll_all`-driven loop. The model the sentence above
> describes is retired and replaced by per-adapter loop pairs (`spawn_*_channel_loops`), one inbound
> task and one outbound task per configured channel, each sleeping its own inter-poll interval,
> matching the shipped email and Telegram adapters. See
> [§Amendment 2026-07-17](#amendment-2026-07-17----imessage-channel-over-an-ssh-bridge).

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

> **Retired 2026-07-17.** The interval is unchanged in shape, but there is no shared `poll_all`
> sweep for it to govern: it now governs the sleep inside each adapter's own inbound loop. See
> [§Amendment 2026-07-17](#amendment-2026-07-17----imessage-channel-over-an-ssh-bridge).

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
- ADR-108: Git Write Surface -- the hardened, allowlisted argv-construction pattern for shelling
  out to an external process, reused by the 2026-07-17 amendment's `ssh`/`osascript` outbound
  path.
