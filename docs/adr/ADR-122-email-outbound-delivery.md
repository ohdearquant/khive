# ADR-122: Email outbound delivery — outbox contract and supervised delivery component

- Status: Accepted (2026-07-29)
- Date: 2026-07-23
- Amends: [ADR-119](ADR-119-daemon-component-supervision.md) (Amendment 2's Phase 2 delivery status),
  [ADR-056](ADR-056-channel-transport-layer.md) (replaces the removed in-core
  `spawn_email_channel_loops` / `channel_outbox_loop` topology)
- Relates to: [ADR-119](ADR-119-daemon-component-supervision.md) (daemon component supervision),
  [ADR-057](ADR-057-comm-actor-addressed-delivery.md) (dual-write messaging)

## Context

The inbound half of the email channel runs as a supervised daemon component
(ADR-119): it polls the mailbox and ingests messages through `comm.ingest`.
At proposal time, the outbound half was missing:
`comm.send(to="email:<addr>")` stored the outbound message note, and nothing
transported it. The accepted implementation adds that half as the separately
supervised `email-outbound` component specified below.

`handle_send` is deliberately transport-blind. It dual-writes the message
note with `from_actor`/`to_actor` labels and knows nothing about channels.
At proposal time there was no written contract for how a delivery loop found
undelivered mail, how delivery outcomes were recorded, or what redelivery
after a crash meant. The removed loop had working answers (poll
channel-prefixed outbound notes without a delivered stamp; stamp after send;
at-least-once), but they existed only as code. This ADR records the accepted
contract and restores the loop as a second supervised component.

## Decision

### 1. Outbox contract (query-side, transport-blind send)

`comm.send` stays transport-blind: it continues to write the outbound
message note with no delivery marker. The outbox is a **query contract over
message notes**, not a new note kind:

A message note is **pending email delivery** when all of:

- `direction = "outbound"`
- `to_actor` starts with `"email:"`
- `properties.delivery` is absent

The delivery component records outcomes by patching note properties (by-ID
update):

| Outcome           | Properties written                                                          |
| ----------------- | --------------------------------------------------------------------------- |
| Delivered         | `delivery = "delivered"`, `delivered_at` (RFC 3339), `transport_message_id` |
| Permanent failure | `delivery = "failed"`, `failed_at`, `last_error`                            |
| Transient failure | `delivery_attempts` (incremented), `last_error` — note stays pending        |

`delivery` is written only on terminal outcomes, so the pending predicate is
simply "no `delivery` key". Transient failures leave the note pending and
are retried on later cycles, under the retry policy below.

#### Transient-retry policy

Retrying at raw poll cadence would be an unbounded per-message retry at
seconds scale: a sustained soft failure (greylisting, mailbox-full 4xx)
would hammer the transport and rewrite `last_error` every few seconds. The
policy is therefore **per-message backoff derived from `delivery_attempts`**:
after a transient failure the component records `delivery_attempts` and
`next_attempt_at` (exponential in the attempt count, from the poll interval
up to a bounded ceiling on the order of tens of minutes), and poll cycles
skip a pending note until `next_attempt_at` has passed. A successful send
clears both fields on the terminal stamp.

There is deliberately **no promotion to `failed` after N transient
attempts**. This channel carries operator-configured recipient mail; converting a
still-valid recipient's message to a terminal failure because the transport
was greylisted for an afternoon silently drops exactly the mail this ADR
exists to deliver. A message leaves pending only through a successful send
or a genuinely permanent classification (configuration, authentication,
allowlist), never through attempt count.

Messages written while no delivery component was running match the pending predicate and
are delivered when the component starts. That backlog is wanted mail; there is no age
cutoff.

Messages written while no delivery component was running (including the
window this ADR closes) match the pending predicate and are delivered when
the component starts. That backlog is wanted mail; there is no age cutoff.

### 2. Recipient allowlist failures are recorded, not silent

Skipping non-allowlisted recipients with only a daemon log line leaves the note pending
forever and the caller sees `ok: true` with no signal. Under this contract a
non-allowlisted recipient is a **permanent failure**: `delivery = "failed"` with
`last_error` naming the allowlist rejection. The allowlist itself is environment-configured
with an operator-configured default recipient.

### 3. Idempotency: at-least-once with a deterministic Message-ID

The component stamps `delivery` **after** a successful transport send. A
crash between send and stamp therefore redelivers — the same at-least-once
ordering the inbound side uses (cursor commit after ingest, never before).

To make redelivery harmless at the receiver, the SMTP `Message-ID` is minted
**deterministically from the note UUID** (UUIDv5 over the note id, formatted
as a Message-ID). A redelivered message carries the same Message-ID as the
original, so receiving mail systems deduplicate it. `transport_message_id`
records the minted value.

### 4. Delivery loop as a second supervised component

Outbound delivery is a **separate ADR-119 component** (`email-outbound`) in
the email component crate, not a second loop inside the inbound component:

- Independent restart budget and health row: an SMTP outage degrades
  outbound without restarting the inbound poll, and vice versa.
- Same configuration source as inbound (the channel's environment config);
  both components independently treat missing configuration as a clean stop.
- Error taxonomy per ADR-119: configuration and definitive authentication
  errors are component-level `Permanent`; network, token-endpoint pressure,
  SMTP 4xx, and other transient transport errors are `Retryable`. SMTP is
  classified at explicit connection, AUTH, and post-auth delivery stages:
  a definitive AUTH rejection stops visibly for operator action, while a
  post-auth per-message 5xx records `delivery = "failed"` only on that note
  and continues draining other recipients.
- Poll cadence matches the removed loop (short fixed interval, seconds);
  heartbeat recorded every cycle.
- Cooperative cancellation between messages: a drain-time cancel finishes
  the in-flight send, stamps it, and stops.

### 5. Behavioral test

The component's suite must include: `comm.send` to an `email:` recipient
with a mock transport at the connector seam, asserting (a) the note is
delivered exactly once across two poll cycles, (b) `delivery`/`delivered_at`/
`transport_message_id` are stamped, (c) a non-allowlisted recipient ends
`failed` with the allowlist named, (d) a transport error leaves the note
pending with `delivery_attempts` incremented, (e) a post-auth permanent SMTP
rejection terminally fails only that note, and (f) the minted Message-ID is
stable across a simulated redelivery.

Ratification is gated on a serialized run of the component library suite:

```bash
cargo test -p khive-component-email --lib -- --test-threads=1
```

The evidence is deliberately behavioral at the connector seam, not a static
registration claim. In particular:

- `outbound_delivers_exactly_once_across_two_cycles_and_stamps_delivery`
  executes `comm.send`, a mock transport acceptance, the durable delivery
  patch, and a second outbox scan;
- `outbound_permanently_fails_non_allowlisted_recipient_naming_the_allowlist`
  proves the allowlist gate never reaches the transport and terminates the
  note with an operator-readable reason;
- `outbound_transport_error_leaves_pending_with_incremented_attempts_and_is_skipped_next_cycle`
  proves transient retry state and immediate backoff eligibility;
- `outbound_post_auth_permanent_rejection_terminally_fails_only_the_note`
  proves a definitive per-message SMTP rejection leaves the component
  available to drain other recipients and never retries the rejected note;
- `outbound_redelivers_after_send_before_stamp_with_the_same_message_id`
  faults the durable stamp after transport acceptance, proves the note is
  selected again, and proves both sends carry the same deterministic
  Message-ID; and
- `supervisor_records_separate_inbound_and_outbound_health_rows` drives the
  actual link-time registrations through the ADR-119 supervisor and proves
  `email-channel` and `email-outbound` remain separately addressable health
  identities.

## Amendment 1 (2026-09-15): classify an AUTH failure by its credential, not by its stage

The error taxonomy above classifies "definitive authentication errors" as component-level
`Permanent`, which under [ADR-119](ADR-119-daemon-component-supervision.md) is terminal `Unhealthy`
with no restart and no backoff. ADR-119 defines `Permanent` as a condition that **cannot change
within the process lifetime**. Those are two different questions, and in this channel they come
apart.

The credentials are read once at boot: `client_id`, `tenant_id` and `client_secret` come from
`std::env::var` in `EmailChannelConfig::from_env` (`crates/khive-channel-email/src/config.rs:227`).
Those are process-lifetime constants, and a failure attributable to them answers ADR-119's question
with "cannot change". The access token is not: it is cached with an expiry and refreshed in process
(`crates/khive-channel-email/src/oauth.rs:150`, `:171`), so a rejection of a token that was
successfully minted describes a condition a refresh can change without restarting anything.

So the classification keys on which credential failed:

| Failure                                                  | Cannot change in-process? | Classification                                                    |
| -------------------------------------------------------- | ------------------------- | ----------------------------------------------------------------- |
| Token endpoint refuses the configured client credentials | yes                       | `Permanent`                                                       |
| SMTP AUTH rejects a token that was successfully minted   | no                        | `Retryable`                                                       |
| Post-auth per-message 5xx                                | —                         | unchanged: `delivery = "failed"` on that note, draining continues |

Nothing else moves. A retryable AUTH failure consumes restart budget like any other retryable
failure, and budget exhaustion is terminal `Unhealthy` that MUST NOT hot-loop, which is what bounds
an authentication that keeps failing. No probe entrypoint and no rearm call is added: ADR-119's
restart budget already is the bounded retry, and a second mechanism for it would be the surface
nobody exercises. No credential reload path is added either — that would change what `Permanent`
means for every ADR-119 component, which is a larger decision than this one.

Component status stays operator-local structured logging and metrics. ADR-119 requires a separate
additive decision for any public introspection surface, and this amendment does not make one.

### Acceptance

Three arms differing only in which credential fails, so an implementation that keeps the
stage-based classification fails two of them:

- An AUTH rejection of a minted token consumes one restart-budget unit, and delivery resumes after
  a successful refresh with no daemon restart.
- An AUTH rejection a refresh cannot fix exhausts the budget and the component goes terminal
  `Unhealthy` without hot-looping.
- A token-endpoint refusal of the configured client credentials is `Permanent` and terminal on the
  first occurrence, with no budget consumed.

## Amendment 2 (2026-09-25): claim the outbound Message-ID before sending

**Status: Accepted (2026-09-26).**

This amendment supersedes the Message-ID derivation in §3 and clarifies the
property timing in §1 and the behavioral assertions in §5. The original §3
describes UUIDv5 and names only `transport_message_id`; the implementation
uses the outbound note's UUID directly. For note ID `note_id` and the
configured sender mailbox's domain, the wire header is
`<{note_id}@{domain}>` (using `localhost` if the mailbox has no domain).
It is not a UUIDv5 value.

Before the SMTP send, the outbox component claims that exact header value in
the outbound message note's `external_id` property through the owner-only
runtime path. A later attempt uses an existing nonempty `external_id` verbatim,
including when the sender mailbox's domain has changed. The claim therefore survives
a send that succeeds before the delivery stamp is persisted. A failed claim
does not proceed to SMTP, and caller-facing note updates cannot set
`external_id`. A caller-supplied external_id at creation currently bypasses the claim; see #3350.

The §1 outcome table is amended for outbound email as follows:

| Stage or outcome  | Properties written or retained                                                                                               |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| Before send       | Claim `external_id = "<{note_id}@{domain}>"` while the note remains pending.                                                 |
| Delivered         | Write `delivery = "delivered"`, `delivered_at` (RFC 3339), and `transport_message_id = external_id`; retain `external_id`.   |
| Permanent failure | Write `delivery = "failed"`, `failed_at`, and `last_error`; retain any earlier `external_id` claim.                          |
| Transient failure | Increment `delivery_attempts` and write `last_error` and `next_attempt_at`; retain `external_id` and leave the note pending. |

Accordingly, §5's delivery and redelivery assertions must check that the
SMTP `Message-ID` equals the claimed `external_id`, that a successful delivery
stamps the same value in `transport_message_id`, and that redelivery reuses the
claim rather than deriving a new ID.

## Consequences

- Operator-configured-recipient email delivery works, including the backlog written
  while no delivery component existed.
- The outbox predicate is written down; any future channel (telegram,
  webhook) can adopt the same `delivery` property vocabulary with its own
  actor prefix.
- Silent allowlist parking is gone: every outbound email reaches a terminal
  recorded state or is visibly pending.
- At-least-once delivery is unchanged from the removed loop, but redelivery
  is now receiver-deduplicable via the deterministic Message-ID.
- A crash exactly between transport accept and the property patch can still
  produce a duplicate send; the deterministic Message-ID bounds the blast
  radius to mail systems that ignore Message-ID deduplication.
- Two delivery components running simultaneously (during a process restart or handoff)
  can both send the same pending note; the deterministic Message-ID is the mitigation
  for that case too — both copies carry the same Message-ID and deduplicate at the
  receiver.

## Alternatives considered

- **Stamp `delivery = "pending"` at send time.** Makes the marker explicit
  but breaks send's transport-blindness (the comm pack would need to know
  which actor prefixes are channel-addressed) and grandfathers nothing: the
  dark-window backlog carries no marker. Absence-based pending covers both.
- **A dedicated outbox note kind.** Heavier: duplicates the message content
  or adds a join, and the note-kind set is closed by design. Property
  vocabulary on the existing message note is sufficient and queryable.
- **Second loop inside the inbound component.** Fewer moving parts, but
  couples the restart budgets: an SMTP-only outage would restart (and
  eventually exhaust) the component that also owns inbound polling.
- **Exactly-once via stamp-before-send.** Inverts the loss mode: a crash
  after stamp but before transport silently drops mail. At-least-once with
  receiver dedup is strictly better for this channel.
