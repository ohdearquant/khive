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
`external_id`. As of #3380 (merged as eb48263c), generic message creation and
update both reject caller-supplied `external_id`, closing #3350's claim bypass.

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

## Amendment 3 (2026-09-28): bind outbound Message-ID to its own note

**Status: Accepted (2026-09-28).**

### Context

Accepted Amendment 2 persists `external_id` before SMTP and reuses any existing nonempty value verbatim. That protects at-least-once resend identity but treats the stored value as owner-claimed. Merged #3380 now rejects caller-supplied `external_id` at generic/wire message create and update. It does not prove the origin of pre-#3380 rows or of trusted in-process writes. On current main, the outbox reuses a nonempty value without checking it (`serve_outbox.rs:399-400`), `comm.ingest` searches outbound `$.external_id` for reply attribution (`handlers.rs:2410-2441`), and `comm.reply` derives outbound-parent `In-Reply-To`/`References` from it (`handlers.rs:1591-1597,3650-3662`). A caller could have copied a victim's Message-ID onto another outbound note before #3380. Matching only a plausible UUID or domain would not bind the ID to the row that presents it.

### Decision

1. For an outbound **email** message note with ID `N`, an accepted stored `external_id` has the exact canonical form `<N@D>`: `N` is that row's own canonical note UUID, and `D` is one of the selected sending channel's configured current or explicitly configured historical sending domains. The allowlist is configuration, never a hard-coded domain list or a domain inferred from the untrusted stored value. The current sender mailbox domain used to mint new IDs is included. No global `localhost` exception exists; the existing no-domain mailbox fallback is accepted only when the selected channel's derived/configured domain is `localhost`.
2. If `external_id` is absent or empty, the owner-only claim persists `<N@current-domain>` **before** SMTP as Amendment 2 requires. If it is nonempty, the outbox may reuse it only after the exact own-ID and configured-domain check. It neither normalizes nor overwrites an unverifiable value automatically. A mailbox-domain change preserves redelivery identity when the former domain remains explicitly configured as historical.
3. An unverifiable nonempty value is a **visible parked refusal**, not a send and not a silent skip. The owner path records a typed `external_id_unverifiable` hold on the message with a diagnostic reason, leaves it undelivered and out of automatic send selection, and emits one keyed, operator-visible non-message diagnostic note linked to the offending message. It never calls SMTP. If recording the hold or note fails, the current pass still refuses SMTP and reports that diagnostic-write failure; a later pass may retry the diagnostic. Clearing the hold or replacing the value requires explicit owner remediation under a separately reviewed path. No implicit terminal `delivery="failed"` stamp is used to hide an unresolved row.
4. `comm.ingest` may attribute a Message-ID reply to an outbound note only after validating that candidate row's own-ID-bound `external_id` and configured domain. Selection must validate before taking a one-row result; an unverified duplicate cannot shadow a valid owner row. When no verified outbound row matches, it does not adopt the unverified row's thread or actor. Existing UUID thread-root fallback is independent and remains subject to its own contract.
5. `comm.reply` must not put an unverifiable outbound parent's `external_id` into `In-Reply-To` or `References`. A requested reply that would do so returns a typed `external_id_unverifiable` refusal before creating the reply; absence of an `external_id` retains Amendment 2's existing no-header behavior. Inbound-parent `wire_message_id` semantics are unchanged.

This supersedes Amendment 2's unconditional reuse of **any** nonempty stored outbound email `external_id`, while retaining its mint-before-send value and at-least-once resend contract for verified values. The direct runtime/typed-store API remains a trusted in-process boundary; this amendment defines checks at the three sinks rather than asserting that old rows were retroactively provenance-marked.

### Evidence

A read-only review of delivered outbound email rows across deployments dated
2026-07-24 through 2026-08-01 found 58 nonempty `external_id` values whose UUID
was not the row's own ID. All 58 used the legacy
`<uuid_v5(fixed namespace, row's own ID)@khive.invalid>` form. Every other
nonempty value in that review had the canonical `<own-id@khive.ai>` form.
The retired `khive-component-email` writer at
`crates/khive-component-email/src/lib.rs:773-775` in commit `1c518c01d`
minted the legacy form. Its exact removal point remains unverified.

Those 58 values fail the own-ID check and are parked if their rows become
eligible for another send; earlier deliveries are not rewritten. Replies that
cite those Message-IDs cannot use `external_id` to claim an outbound row; only
the independent UUID thread-root fallback may apply. `comm.reply` with one of
those outbound rows as its parent refuses with `external_id_unverifiable` before
creating a reply.

### Acceptance arms

- Valid current-domain and configured historical-domain own-ID values are reused unchanged after a crash; an absent value is claimed before SMTP. An unconfigured domain, malformed ID, or other row's ID is parked with the typed refusal and one diagnostic note, and sends nothing.
- A victim note and a second outbound note carrying the victim's otherwise well-formed `<victim-id@allowed-domain>` are distinguished by **row ownership**. The second row is parked at send. An inbound reply using the victim ID attributes to the victim, never the second row even if it sorts first. `comm.reply` on the second row cannot emit that value as `In-Reply-To` or `References`.
- Independent mutation controls remove the own-ID comparison at each of the outbox, ingest-correlation, and parent-header sinks; each must turn its named acceptance test red. A test that checks only angle brackets, UUID parseability, or allowed domain does not satisfy this arm.
- Generic/wire `create` and `update` still reject caller-supplied `external_id` as #3380 requires; trusted `comm.ingest` and owner claim remain possible.

### Consequences and scope

The current behavior may have already delivered or correlated legacy rows. A3 does not rewrite past sends or claims cryptographic provenance. Historical sending domains must be explicitly configured to preserve otherwise valid own-ID claims after a mailbox-domain change. This amendment requires source changes at all three sinks and at the hold/diagnostic path in the same coherent implementation; a one-method runtime guard does not implement the decision.

## Amendment 4 (2026-10-05): refuse boot-known recipient policy violations before message creation

**Status: Proposed.**

### Scope

For `comm.send` and `comm.reply` addressed to `email:<address>`, a recipient
policy already known by the serving process must decide admission before a
fresh message is committed. A successful storage receipt must not conceal a
recipient rejection that the process can already determine.

This amends §1's transport-blind send rule only to permit this local policy
decision, and §2's asynchronous allowlist failure rule only for fresh
requests. Admission performs no transport I/O and does not promise delivery.
Section 2 continues to govern previously queued messages. The outcome
properties, retry classification, verified Message-ID ownership and
at-least-once ordering in the existing decision and Amendments 1–3 remain in
force. No verb, delivery-state value, note kind or storage schema is added.

### One immutable policy for admission and delivery

The host resolves the email recipient policy once during runtime
construction, before registering the comm runtime or starting delivery. The
comm runtime and its outbox use that same immutable policy. Requests,
runtime clones and outbox cycles must not independently reread environment
configuration. A forwarded request uses the serving process's policy.

Policy resolution keeps the existing selection order:

1. If `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS` is set, split it on commas, trim
   each value and discard empty values. Each remaining value must parse
   under the existing maintainer-address rules; a value that does not parse
   is a configuration error. A nonempty result is the configured recipient
   set.
2. If step 1 yields no address, because the variable is unset or because it
   holds only blanks and separators, and `KHIVE_EMAIL_MAINTAINER_ADDRESS` is
   set, its first address is the default recipient. This is the existing
   fallback: an explicit setting that yields no address restricts admission
   to the default recipient. Additional maintainer addresses do not
   implicitly become allowed outbound recipients.
3. The policy is **absent** only when neither variable is set. An explicit
   setting that yields no address while the maintainer variable is unset is
   a configuration error, and so is a value that cannot be read. Neither may
   become an unrestricted policy by being treated as absent. What this
   prevents is admission and queueing of mail the operator meant to
   restrict; delivery would not follow from the same process, because the
   delivery component requires a configured policy.

Whenever `KHIVE_EMAIL_MAINTAINER_ADDRESS` is set, it is read the way the
email connector reads it at start: split on commas, trim each value and
discard empty values; every remaining value must parse under the
maintainer-address rules, and at least one must remain. A maintainer value
that fails either test is a configuration error even when step 1 supplied
the recipient set, so resolution refuses exactly the maintainer values that
keep the delivery component from starting.

Explicit entries, the default recipient and the requested recipient (the
address after `email:`) are compared in one normalized form: the addr-spec
produced by the maintainer-address parser, without any display name or
angle brackets and lowercased. A recipient is allowed when its normalized
form equals a normalized entry; a recipient that does not parse matches no
entry. The policy stores its entries in this normalized form, and refusals
and log lines name addresses in it; a requested recipient that does not
parse has no normalized form and is named as requested. The message's
stored recipient is unchanged; normalization applies to comparison only.
Compared with the earlier exact match on explicit entries, this admits
case variants of a listed address and never a different address. This amendment adds no domain
wildcard. The policy distinguishes **configured** from **absent**; an absent
policy is not an assertion that every recipient is deliverable.

The host must resolve these public policy inputs independently of SMTP/IMAP
credentials and connector startup. A configured policy still applies when
delivery or polling is disabled, credentials are unavailable, or the outbox
has not started. Single-backend and routed multi-backend hosts install the
same policy on the comm runtime and the outbox that serves it. An embedder
constructing the comm runtime can supply a policy; without one, admission
uses the absent state.
Other authorization and read-only restrictions continue to apply.

When policy is absent, send/reply retain the existing ability to queue mail
without a running delivery component. That wanted backlog has no new age
cutoff. Starting a later process with configured policy applies that
process's defensive outbox check to the backlog.

### Fresh refusal and exact keyed replay

Existing request, mailbox, thread and parent-header validation remains in
force. For a fresh request whose resolved email recipient is excluded by a
configured policy, send/reply returns the existing typed permission-denied
form before creating either message copy, claiming an idempotency key,
writing attachment ownership, publishing message indexes or waking the
inbox. It creates no failed-delivery note and invents no outbound ID. The
refusal describes this request's lack of a commit; it does not claim that a
concurrent request can never commit the same key.

An exact keyed retry of an already committed message is a receipt lookup,
not fresh delivery admission. Its precedence is:

1. Look up the live holder of the existing actor/namespace-scoped message
   key. Validate the exact request, both committed message copies and their
   attachment identities under the existing replay rules. A mismatch or
   incomplete pair retains the existing conflict behavior.
2. A valid holder returns the original receipt even if the current policy
   would deny a fresh send. This does not create another pair, publish
   another wake, reset delivery state or request retransmission. A key
   never bypasses caller authorization or the existing replay checks.
3. If no holder exists, apply the current policy before creation. An
   allowed creator retains the atomic unique-key claim and reconciles a
   competing holder through the same exact replay validation.
4. A denied creator's refusal linearizes at its **final no-holder
   observation**. A holder observed before that observation must be
   validated and reconciled instead of being described as uncommitted. A
   competing holder committed afterward does not retroactively turn the
   denied request into a writer; a later retry can retrieve that receipt.
   The denied request itself never claims the key. The implementation
   provides a test-only seam at this observation, so a test can commit a
   competing holder immediately before or immediately after it.

### Defensive delivery checks and observable outcomes

The outbox retains its recipient check for historic queued rows and rows
created through other authorized writers. A denied queued recipient still
receives §2's permanent `delivery="failed"` outcome and an explanatory
`last_error`, without SMTP.

The delivery component requires a configured policy. Constructing it with
absent policy is a configuration error: the component fails with a
permanent component error, is not restarted, and no message leaves. This
removes the reading in which an empty recipient list means no recipient
check. Today the delivery component is constructed only by the host, which
always has a configured policy because the email connector requires a
maintainer address, so no current host configuration changes behavior. Any
constructor that later lets an embedder start delivery directly returns the
same configuration error when no policy is supplied; for such a caller this
is a breaking change from the empty-list meaning.

Amendment 3's stored Message-ID verification precedes this defensive
allowlist classification. An unverifiable nonempty `external_id` remains a
visible `external_id_unverifiable` hold; recipient rejection must not replace
that hold with a terminal failure that conceals the unresolved identity.
Verified IDs retain claim-before-send and send-before-delivery-stamp timing.
Transient failures retain backoff and are not promoted to terminal failure
by attempt count. Cancellation and credential-based AUTH classification are
unchanged.

Email delivery state belongs to the outbound message note's properties.
The sender can inspect that note through its caller-authored
`comm.inbox(box="sent")` view, including the `properties` field. A successful
SMTP handoff is represented by `delivery="delivered"`; permanent failures,
transient retry metadata and identity holds remain distinguishable through
their existing properties. This does not attest that the remote recipient
stored or read the message.

[`comm.transport_status` under ADR-105](ADR-105-cross-node-comm-transport.md)
reports sender transport records and verified cross-node receipts. It is
not an alias for these email note properties. Absence of such a record
remains `unknown`; this amendment neither synthesizes a sender transport
record for email nor maps SMTP acceptance to `recipient_stored`.

### Acceptance

The following cases use the real request and outbox paths with a mock
transport; no real email is required.

| Case                            | Required behavior                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| E1 — fresh refusal              | Both send and reply to a configured denied recipient refuse synchronously. Neither message copy, key claim, attachment owner, message index publication nor inbox wake is produced; the transport is untouched. The same request to an allowed recipient retains the original pair receipt and one wake.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| E2 — shared policy              | Single-backend, routed multi-backend and forwarded requests use the serving comm runtime's policy. Disabled polling, unavailable credentials and an unstarted outbox do not remove a configured refusal. Changing environment values after construction cannot change admission and delivery independently.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| E3 — configured versus absent   | Explicit recipients take precedence over the primary-maintainer fallback; trimming and first-maintainer selection remain stable. A mixed-case configured maintainer default admits a send to that address in any case, a case variant of an explicit entry is admitted, and a different address is refused. An explicit setting holding only blanks and separators falls back to a configured maintainer default and is a configuration error with the maintainer variable unset. A maintainer value with no address or with an entry that does not parse is a configuration error even beside a valid explicit set. An explicit entry that does not parse and an unreadable value each cause a configuration error rather than unrestricted admission. Absent policy (neither variable set) preserves the wanted backlog. A delivery component constructed with absent policy refuses to start and no message leaves. Refusal output names the recipient in normalized form, or as requested when it does not parse, and exposes neither credentials nor the complete recipient set. |
| E4 — replay and races           | An exact committed keyed replay after policy revocation returns the original receipt without writes, wake or retransmission. Altered payload and broken pair retain conflicts; a retry naming attachments is refused as before, since outbound channel addresses take none. Using the test-only seam at the final no-holder observation, a competing holder committed immediately before it is reconciled; one committed immediately afterward leaves the denied request uncommitted and is available to a later retry. Allowed competing creators still commit only one pair.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| D1 — sender-visible state       | Read the original outbound ID through the sender's sent view after successful delivery, historic-row policy failure, transient failure and an unverifiable-ID hold. Verify the corresponding existing properties and mailbox visibility. Separately exercise ADR-105 `pending`, `recipient_stored`, `recipient_quarantined`, `failed` and `unknown` results; SMTP success must not be reported as verified recipient storage.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| D2 — retained delivery behavior | Preserve single send across two ordinary poll cycles; a failure after SMTP acceptance but before the stamp may redeliver with the same verified claimed Message-ID. Post-auth permanent rejection fails only its message, retryable AUTH failures retain their restart behavior, cancellation retains in-flight settlement, and the configured comm backend receives the delivery updates.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |

This amendment addresses the email-admission portion of #1760. It does not
change thread retrieval, queue-page selection or the semantics of other
channel prefixes.

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
