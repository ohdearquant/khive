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
subhandlers. This table and its handlers are new, pack-owned operational bookkeeping -- not a
core `khive-db` migration -- consistent with ADR-028's pack-scoped-backend cursor pattern, and
they directly retract the "no new schema, no new table" claim above for the email adapter only;
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
- `comm_channel_cursor` and its subhandlers are pack-owned operational bookkeeping, not a new
  MCP-callable verb and not a core schema migration; they are exercised in production only via the
  daemon poll loop's internal dispatch calls.
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
known-hosts file, `~/.khive/imessage_known_hosts`, provisioned during one-time setup with the
bridge host's genuine host key, and passes strict host-key checking and batch mode
(`-o UserKnownHostsFile=~/.khive/imessage_known_hosts -o StrictHostKeyChecking=yes
-o BatchMode=yes`) explicitly on the invocation. The adapter never inherits the operator's own
`~/.ssh/config`, never relies on SSH agent-based host trust, and never falls back to
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

**Transport deadlines and recovery.** Each `ssh` invocation runs under a total wall-clock
deadline of 30 seconds enforced by the adapter itself, which kills and reaps the child process on
expiry rather than allowing a hung invocation to accumulate indefinitely. Connection
establishment is separately bounded by a 10-second connect timeout passed as an `ssh` option
(`-o ConnectTimeout=10`), so a bridge host that is up but not accepting connections fails fast
instead of consuming the whole 30-second deadline waiting on a connection that will never
establish.

Consecutive transport failures back off exponentially between poll ticks, doubling from a
1-second floor and capped at 5 minutes; a transport failure increments the consecutive-failure
counter, and the counter resets to zero on the next success. Acceptance property: an invocation
that hangs past its deadline is killed and reaped at the deadline, and the adapter's next tick
proceeds only after the backoff interval for the current consecutive-failure count has elapsed.

**SSH target validation and the end-of-options delimiter.** `KHIVE_IMESSAGE_SSH_TARGET` is
validated fail-closed at config load, not at first use: it must match a restrictive shape, an
optional `user@` prefix followed by a hostname or address, and a value beginning with `-`, or
containing whitespace or a control character, is rejected -- `ImessageChannelConfig::from_env`
returns `ChannelError::Config` and the channel does not start. This closes a specific attack: an
option-prefixed target such as `-oProxyCommand=...` would otherwise be parsed by the `ssh` client
as an option rather than a destination, and a `ProxyCommand` override executes an arbitrary local
command under the daemon account. Fixed argv construction alone does not close this, because argv
construction fixes how arguments are assembled, not what the target string itself is interpreted
as once `ssh` parses it. As defense in depth beyond the validation gate, every `ssh` invocation
this adapter makes additionally passes the `--` end-of-options delimiter immediately before the
target argument, so even a target value that somehow reached the invocation unvalidated can never
be parsed as an `ssh` option. Acceptance property: an option-prefixed `KHIVE_IMESSAGE_SSH_TARGET`
value refuses channel start.

### Outbound: `osascript` via hardened argv, no text interpolation

On the bridge host, delivery drives the Messages application via `osascript`. Message text is
never interpolated into a shell string or an AppleScript source string: the text travels over
stdin (or a temp file consumed by a fixed, pre-written script), and the `ssh`/`osascript`
invocation argv is constructed from an allowlisted fixed form -- the daemon never assembles a
shell command by string concatenation with caller-supplied content. This is the same
argv-only-construction pattern ADR-108 Fork (b) B2 establishes for `git.commit`/`git.branch`/
`git.push`: a fixed argument vector via `std::process::Command::new(...).args([...])`, no shell
interpolation, and no caller-supplied value reaching the process boundary unvalidated. ADR-108's
Amendment 1 requirement for a dedicated adversarial security review at implementation time
applies equally to this shell-out surface.

### Inbound: read-only polling of the bridge host's Messages database

Inbound messages are read by polling the bridge host's Messages database (Apple's own SQLite
store for the Messages application, path configured by `KHIVE_IMESSAGE_DB_PATH`, default
`~/Library/Messages/chat.db`) over the same SSH transport, at a configurable interval
(`KHIVE_IMESSAGE_POLL_SECS`, default 5 seconds, mirroring §12's default inter-poll interval;
validation rule below). The database is always opened with read-only semantics; the adapter never
writes to `chat.db`. Delivery into `comm.ingest` is attributed as `imessage:<slug>` inbound,
following the channel-prefixed `from` form OQ-1 established (§14; also used by the Telegram
amendment's `from = "telegram:<maintainer-slug>"`).

**Sender validation (ADR-056 §8 applied to this adapter).** §8 requires every adapter to
validate the external sender identity on every inbound item before it becomes an envelope. For
iMessage, that requirement is met by four checks, all mandatory: `message.is_from_me` must be
`0`, the row must belong to the configured maintainer conversation, the row's sender handle must
equal the configured `KHIVE_IMESSAGE_MAINTAINER_HANDLE`, and the row's service must be iMessage.
A row failing any check is dropped -- it is never ingested and never attributed -- and the drop
is counted, mirroring the Telegram adapter's `UnauthorizedSender` drop-and-count behavior (§8)
rather than the email adapter's quarantine disposition, since (unlike inbound email) the bridge
host's Messages database has no open-relay exposure: only the maintainer's own conversation is
polled.

**Terminal disposition.** A drained row is HANDLED when it reaches either of two terminal
outcomes: it is ingested, or it is dropped by one of the four checks above. Both outcomes are
terminal and both count toward page completion; a dropped row is not left pending or retried on
the next poll. The page checkpoint (§Restart durability below) commits once every row in the page
has been handled, so the `ROWID` floor advances past dropped rows exactly as it does past
ingested ones. Acceptance property: a rejected row below the checkpoint floor never blocks or
re-selects on a later poll, so it can never stall the rows that follow it in `ROWID` order.

The `is_from_me` check exists because the adapter's own outbound sends (via `osascript`, above)
land in the same one-to-one conversation the inbound poll reads, and, being part of that
conversation, can carry the maintainer's own handle on the row. Without excluding
`is_from_me = 1` rows, the adapter would read back its own outbound sends as trusted inbound
maintainer input: for a chat channel feeding an autonomous loop, that is a self-echo path -- the
daemon replying to its own prior message and re-ingesting that reply in turn. The other checks
(conversation membership, handle match, service) do not catch this case, because an adapter-sent
row satisfies all three. Acceptance property: an adapter-sent row in the maintainer conversation
is never ingested as inbound.

The service check exists because `chat.db` records a service discriminator per message, and a
row carried over SMS, MMS, or RCS is not an iMessage-authenticated delivery: a forwarded SMS
message's sender number is spoofable in a way a delivery tied to a signed-in Apple ID is not.
Without this check, a spoofed SMS sender number matching the configured handle could be
attributed as trusted maintainer input. Rows carried over SMS, MMS, or RCS are dropped and
counted, never ingested -- the same disposition as the other three checks. This check also makes
structural a property this deployment model otherwise leaves incidental: the bridge Mac signs
into a dedicated bridge Apple ID that is email-only, with no phone number and no paired SMS
forwarding path (§Two-identity model below), so every row in the bridge's Messages database is
iMessage-service by construction -- there is no SMS-forwarding path through which an SMS message
could reach that database at all. The maintainer's own handle, by contrast, may be a phone number
or an email address; only whichever form is configured in `KHIVE_IMESSAGE_MAINTAINER_HANDLE` is
trusted. The service check therefore enforces a structural property of the bridge account, not an
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
- The checkpoint commits only after every row in a drained page has been handled -- ingested, or
  terminally dropped by one of the four sender-validation checks above (§Terminal disposition) --
  mirroring `channel_poll_loop`'s `cursor_get -> poll_page -> handle each -> cursor_commit`
  sequencing for email (§Amendment 2026-07-09), with dropped rows added as a second terminal
  outcome alongside ingestion. A row that fails to reach `comm.ingest` for a transport or storage
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
configured `KHIVE_IMESSAGE_SSH_TARGET` and the configured `KHIVE_IMESSAGE_DB_PATH`, in the
form `imessage-ssh:{ssh_target}:{db_path}`, mirroring the shape of the email adapter's
`imap+tls:{host}:{port}:{mailbox}:INBOX` source string (§Amendment 2026-07-09). Both components
are configuration values, not credentials, and the cursor row itself lives only in the daemon's
own local store, never on the bridge host. Because `source` is built from `KHIVE_IMESSAGE_DB_PATH`
directly, changing that variable changes the source identity and therefore selects a different
cursor row (§Source-keyed cursor history below), exactly as changing `KHIVE_IMESSAGE_SSH_TARGET`
does. `generation` starts at `1` when the cursor row is
first created for a given `(channel_kind, channel_slug, source)` and increments by one on every
checkpoint reset (below).

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
change that alters the source identity -- a different `KHIVE_IMESSAGE_SSH_TARGET` or a different
remote database path -- produces a different `source` string and therefore a new cursor row
keyed by that new source; the prior row is left intact, untouched and unreferenced, never
overwritten or migrated. Reverting to a previously used source therefore resumes that source's
own row at its own high-water mark, and the poll drains whatever backlog accumulated in that
source's database while a different source was active: no message received under a configured
source is ever skipped by switching away from it and back. Acceptance property: switching from
source A to source B and back to source A ingests the rows that arrived in source A's database
during the period source B was active.

**Identity guard and forward-only reset.** A `ROWID` alone is not a stable identity across a
replaced or migrated `chat.db`: a new database can reuse the same `ROWID` values for entirely
different messages. Guarding against this requires the explicit, additive schema extension named
above: a new optional opaque `anchor` text column on `comm_channel_cursor`, nullable, delivered
as a schema migration alongside the corresponding `comm.cursor_get`/`comm.cursor_commit` API
surface change to read and write it. The email and Telegram adapters leave this column null and
are otherwise unaffected; the iMessage adapter is the first caller to populate it, storing the
GUID of the last-ingested row at the checkpoint `ROWID` there (the same `chat.db` message GUID
used for dedup below, applied here to a different purpose: identity of the anchor row, not
overlap protection for retries). Before applying the stored high-water mark, each poll first
re-reads that `ROWID` from the remote `chat.db` and verifies its GUID still matches the stored
`anchor`:

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
applied once the GUID check fails.

**The widened cursor contract, precisely.** The shared checkpoint type
(`StoredChannelCheckpoint`, introduced in §Amendment 2026-07-09) gains one new field: `anchor:
Option<String>`, nullable for every existing adapter. The cursor read and commit operations
(`comm.cursor_get` / `comm.cursor_commit`) are widened to take `(channel_kind, channel_slug,
source)` rather than `(channel_kind, channel_slug)` alone, and both carry `anchor` on read and on
write. The `Channel` trait (§2) gains a source accessor -- a method returning the adapter
instance's own `source` string -- so the daemon loop can resolve which cursor row an adapter
reads and commits against before the first poll of a tick, not after. Stepwise, the daemon loop's
per-tick sequence is:

1. Resolve the adapter's source via the trait's source accessor.
2. Read the cursor by `(channel_kind, channel_slug, source)`.
3. Poll pages starting from the checkpoint the read returned.
4. For each page: handle every row -- ingest or terminal drop (§Terminal disposition above) --
   then commit the cursor, including any `anchor` update, once the whole page is handled.

This is the precise, API-level statement of the `cursor_get -> poll_page -> handle each ->
cursor_commit` sequencing described in §Restart durability above.

**Migration: owner and sequence.** The comm pack owns `comm_channel_cursor`, so this schema
change ships as a versioned pack-schema migration under the pack-scoped backend rules of ADR-028,
not a core `khive-db` migration. A `CREATE TABLE IF NOT EXISTS` statement cannot alter an
existing table's primary key, so widening the uniqueness key to three columns requires a full
table rebuild rather than an in-place `ALTER TABLE`. The migration: creates a new table carrying
the three-column primary key `(channel_kind, channel_slug, source)` and the new `anchor` column;
copies every existing row into it, backfilling `source` with each adapter's own constant source
value (the email adapter's `imap+tls:{host}:{port}:{mailbox}:INBOX` shape); drops the old table;
and renames the new table into its place. Acceptance property: an upgraded database preserves
every pre-existing cursor row's progress -- no adapter's high-water mark regresses or is lost
across the migration.

**Bounded drain per tick.** The poll query is anchored on `chat.db`'s own monotone insertion
key, the message table's `ROWID`: each poll fetches rows with `ROWID` strictly greater than the
committed checkpoint, ordered ascending, in pages of at most **200 rows** each. A tick drains
pages until either a page returns short of the page size (the signal that the poll has caught up)
or a fixed cap of **10 full pages** is reached -- at most **2000 rows per tick** -- whichever
comes first; both the 200-row page size and the 10-page cap are normative amendment text, not
configuration or a new environment variable. The checkpoint commits after every successfully
ingested page, not only at the end of the tick. When the 10-page cap is reached before catching
up, the task does not keep draining: it sleeps its normal `KHIVE_IMESSAGE_POLL_SECS` interval and
resumes on the next tick from the checkpoint the last page committed. This bounds each tick's
SSH/query/ingest work under sustained arrivals -- a conversation receiving messages faster than a
200-row page drains cannot pin the task in a continuous poll loop -- while the per-page commit
means no row is lost or re-skipped across the sleep boundary: the next tick continues from
exactly where the last page left off. No row between polls can fall outside a bounded window and
be skipped, because the query has no window -- only a floor -- and the 200-row-page/10-page-cap
bound (2000 rows per tick) bounds duty cycle, not correctness.

**Activation semantic.** On first activation for a given `(kind, slug)` -- no prior checkpoint
row exists -- the checkpoint initializes to the current maximum `ROWID` in `chat.db`, not to
zero, and `generation` initializes to `1` (§Cursor identity above). The channel is forward-only
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

| Variable                           | Required | Default                      | Description                                                                                                                                                                                                    |
| ---------------------------------- | -------- | ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KHIVE_IMESSAGE_SSH_TARGET`        | yes      | --                           | SSH target for the bridge host (`user@host`). Credentials resolve via the SSH client, not this config. Validated fail-closed at config load (SSH target validation above).                                     |
| `KHIVE_IMESSAGE_MAINTAINER_HANDLE` | yes      | --                           | The maintainer's own iMessage handle (phone number or email) -- the remote counterparty, never the bridge account's own handle (§Two-identity model above). Config-only; never appears in an envelope or note. |
| `KHIVE_IMESSAGE_MAINTAINER_SLUG`   | no       | `maintainer`                 | The slug in `imessage:<slug>` that maps to the maintainer handle.                                                                                                                                              |
| `KHIVE_IMESSAGE_DB_PATH`           | no       | `~/Library/Messages/chat.db` | Path to the Messages database on the bridge host. Part of the cursor `source` identity (§Cursor identity above): changing it selects a different cursor row.                                                   |
| `KHIVE_IMESSAGE_POLL_SECS`         | no       | `5`                          | Inbound poll interval against the bridge host's Messages database. Must parse as an integer >= 1 (validation rule below).                                                                                      |
| `KHIVE_IMESSAGE_INGEST_NAMESPACE`  | no       | `local`                      | Target namespace for ingested inbound messages (passed as `namespace` to `comm.ingest`).                                                                                                                       |

No secret material beyond what the SSH client already manages is configured here, and nothing
above is written to any khive store, matching §9.

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

### Acceptance properties (testable, transport mocked)

1. The outbound argv passed to the SSH/`osascript` invocation contains no message text --
   message text reaches the bridge host only via stdin or a temp file, never as an argv element
   or an interpolated shell/AppleScript string (the injection fence).
2. Simulating a restart mid-poll and re-delivering an already-seen `chat.db` row does not produce
   a duplicate inbound note; dedup is by GUID through `idx_comm_message_external_id`.
3. An unreachable bridge host degrades that one channel to a channel-down warning; other
   configured channels are unaffected and the daemon does not fault.
4. Every `chat.db` open the adapter performs is asserted read-only by the test double (no write
   flags requested).
5. A poll after a simulated restart resumes strictly above the committed `ROWID` checkpoint, not
   from an in-memory default; a `chat.db` row with `ROWID` at or below the checkpoint is never
   re-ingested and a row above it is never skipped, across a page-limit boundary.
6. On first activation for a `(kind, slug)` with no prior checkpoint row, the checkpoint
   initializes to the current maximum `ROWID`; a `chat.db` row inserted before activation is
   never ingested.
7. `KHIVE_IMESSAGE_POLL_SECS` set to `0`, a negative integer, or a non-integer string fails
   channel construction with a warning naming the variable; the channel does not start.
8. A `chat.db` row with `is_from_me = 1`, a sender handle that does not equal
   `KHIVE_IMESSAGE_MAINTAINER_HANDLE`, a conversation outside the maintainer conversation, or a
   service other than iMessage (SMS, MMS, or RCS), is not ingested; the drop is counted. An
   adapter-sent row in the maintainer conversation is never ingested as inbound.
9. Under sustained arrivals exceeding the bound of 10 pages of 200 rows each (2000 rows per
   tick), the inbound task sleeps its normal poll interval between ticks rather than draining
   continuously, and no row is skipped across that sleep boundary -- the next tick resumes from
   the checkpoint the last page committed.
10. When the GUID at the stored checkpoint `ROWID` no longer matches the remote database (row
    deleted, database replaced), the next poll resets forward-only to the current maximum
    `ROWID`, increments `generation`, and logs and counts the reset; a stale high-water mark is
    never applied once the GUID check fails.
11. `KHIVE_IMESSAGE_SSH_TARGET` values beginning with `-`, or containing whitespace or a control
    character, fail channel construction with a warning naming the variable and the channel does
    not start; every outbound `ssh` invocation additionally passes `--` immediately before the
    target so a value that reached the invocation unvalidated can still never be parsed as an
    `ssh` option.
12. Switching `KHIVE_IMESSAGE_SSH_TARGET` (or the remote `chat.db` path) from a source A to a
    source B and back to A resumes source A's own checkpoint row at its own high-water mark; rows
    that arrived in source A's database while source B was active are ingested on return, never
    skipped.

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
  configured bridge target and database path and, under the widened key, gives each source its
  own persistent row, so switching sources and back resumes the original source's backlog rather
  than losing it. Its `generation` tracks resets, and a GUID identity guard on the `anchor` row
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

> **Lifecycle amended 2026-07-17.** The `poll_all` sweep above is a design description, not the
> shipped mechanism. It is superseded by per-adapter loop pairs registered through
> `ChannelRegistry` and spawned individually by the daemon role. See
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
| `channel_kind` | string | no       | `"telegram"`, `"email"`. Stored in `properties.channel_kind`.                                                                                                                   |
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

> **Superseded 2026-07-17.** The single-loop, `poll_all`-driven model above is superseded by
> per-adapter loop pairs (`spawn_*_channel_loops`), one inbound task and one outbound task per
> configured channel, matching the shipped email and Telegram adapters. See
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

> **Superseded 2026-07-17.** The interval is unchanged in shape, but it now governs the sleep
> inside each adapter's own inbound loop rather than a shared `poll_all` sweep. See
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
