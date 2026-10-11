# ADR-200: External Email Policy: Sender Scope, Suppression, Unsubscribe and Screened Inbound Delivery

- **Status**: Proposed (2026-10-11)
- **Date**: 2026-10-11
- **Amends**: [ADR-122](ADR-122-email-outbound-delivery.md) (Amendment 4, recipient policy),
  [ADR-056](ADR-056-channel-transport-layer.md) (Amendment 2026-07-02, quarantine disposition)
- **Addresses**: [#5165](https://github.com/ohdearquant/khive/issues/5165)

## Context

The email channel was built for one correspondent, the maintainer.

- **Outbound.** ADR-122 Amendment 4 resolves one immutable recipient policy at start-up from
  `KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS`, defaulting to the maintainer address. Admission and
  delivery share it. Adding a recipient means editing the environment and restarting the daemon.
- **Inbound.** ADR-056's 2026-07-02 amendment attributes a message only when the sender's domain
  authenticates and the `From:` address is the maintainer. Every other message is quarantined:
  stored under the fixed `email:quarantine` sender, with no correlation key, waking nobody
  (`crates/khive-channel-email/src/channel.rs`, `gate()` and `quarantine_envelope()`). A reply
  from anyone else to a message we sent is therefore unthreaded and unseen.

An operator who corresponds with people outside the deployment (support, partners, outreach)
needs four things the channel does not provide: sending to recipients who are not known at
start-up, honouring opt-outs immediately and permanently, seeing replies in the inbox of the actor
that sent the original, and keeping text written by strangers from being read as instructions by
the agents that read the inbox.

External obligations this decision is shaped by, read at source on 2026-10-11:

- [RFC 8058](https://www.rfc-editor.org/rfc/rfc8058) one-click unsubscribe: `List-Unsubscribe`
  MUST contain one HTTPS URI; `List-Unsubscribe-Post` MUST contain the single pair
  `List-Unsubscribe=One-Click`; the message MUST carry a valid DKIM signature whose `h=` tag covers
  both headers, and receivers SHOULD NOT offer one-click without it (§3.1, §4). The POST carries
  no cookies or authorization, the sender MUST NOT answer with a redirect, and the URI SHOULD carry
  an opaque, hard-to-forge identifier that the server verifies (§3.1, §6). A `mailto:` URI MAY
  appear beside the HTTPS one.
- [Gmail sender guidelines](https://support.google.com/a/answer/81126): every sender keeps its
  reported spam rate below 0.3%; senders of 5,000 or more messages a day to personal accounts
  also need SPF, DKIM, DMARC with `From:` alignment, and one-click unsubscribe on marketing and
  subscribed messages.
- [FTC CAN-SPAM compliance guide](https://www.ftc.gov/business-guidance/resources/can-spam-act-compliance-guide-business):
  commercial messages carry a clear opt-out that keeps working for at least 30 days after sending,
  opt-outs are honoured within 10 business days without asking for more than the email address,
  each message carries a valid physical postal address, and business-to-business email is not
  exempt.

## Decision

### 1. Two recipient classes

A recipient is either a **policy recipient**, admitted exactly as ADR-122 Amendment 4 admits it
today (the configured recipient set or the maintainer default), or an **external recipient**:
any other address that parses under the existing normalization.

External recipients are disabled unless `KHIVE_EMAIL_EXTERNAL_SENDERS` is set (§2). With it unset
the channel behaves exactly as it does today, so no existing deployment changes.

A fresh `comm.send` or `comm.reply` to an external recipient is admitted only when every condition
below holds. A failure returns Amendment 4's existing typed permission-denied form before any
message copy, key claim, index entry or wake is created, and names the failed condition, never
the contents of any list:

1. the caller's resolved actor is in the external-sender set (§2);
2. the recipient is not suppressed (§3);
3. the daily external cap has room (§4);
4. the external footer is configured (§5.3);
5. when a remote suppression mirror is configured, its last successful refresh is fresh (§6).

Exact keyed replay keeps Amendment 4's precedence: a valid committed holder returns its original
receipt without consulting these conditions and without counting against the cap.

### 2. External sender scope

`KHIVE_EMAIL_EXTERNAL_SENDERS` is a comma-separated list of actor ids, resolved once at start-up
into the same immutable policy object Amendment 4 introduced and shared by admission and delivery.
An entry that does not parse as an actor id is a configuration error. Actors outside the set keep
policy-recipient sending only.

The check reads the caller's resolved actor, so it is exactly as strong as actor attribution on
the transport the request arrived on: credential-bound on an authenticated remote transport,
workspace configuration on a local one. A process that can act as an external sender can mail any
unsuppressed address up to the cap; §4 bounds that, §3 cannot be bypassed by it, and §8 keeps what
comes back from being trusted.

### 3. Suppression list

Suppression lives in the comm store, so an entry takes effect without a restart and can be written
by the channel itself. Each entry has a UUID id, a `scope`, a `kind`, a normalized `value`, a
`source`, the creating actor, a creation time and a free-text reason.

| Field    | Values                                                                                                                                              |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scope`  | `outbound` (do not send to this recipient) or `inbound` (do not deliver mail from this sender, §7)                                                  |
| `kind`   | `address` (normalized addr-spec), `domain` (ASCII-lowercased domain; matches that domain exactly, no subdomain wildcard), or `recipient_key` (§5.1) |
| `source` | `operator`, `mailto_token`, `one_click`, `reply_stop`                                                                                               |

An outbound recipient is suppressed when an `outbound` entry matches its normalized address, its
domain, or its recipient key.

Admission reads the store before creating a message. Delivery reads it again before handing a
queued external message to SMTP, extending Amendment 4's defensive delivery check: a recipient
suppressed after admission fails that message permanently (`delivery = "failed"`, `last_error`
naming suppression, never the entry). The store is the single source both points read, so they
cannot disagree about what the list means, only about when it was read, and the later read wins in
the direction of not sending.

Three verbs manage entries: `comm.suppression_add`, `comm.suppression_list` and
`comm.suppression_remove`. Adding and listing are open to the external-sender set and the
maintainer. Removal requires a reason and is audited. An entry whose source is an opt-out
(`mailto_token`, `one_click`, `reply_stop`) can be removed only by naming, as evidence, an inbound
message from that same address that authenticated under §7 and arrived after the opt-out. Opt-outs
are permanent until the person writes back to undo them.

### 4. Daily external cap

`KHIVE_EMAIL_EXTERNAL_DAILY_CAP` (a positive integer, required when external senders are configured;
a missing or invalid value is a configuration error) bounds fresh external admissions across all
actors in any trailing 24 hours, counted from committed outbound message notes with external
recipients. A new sending domain's reputation is shared by every sender on it, so the cap is global
rather than per actor.

### 5. Unsubscribe

#### 5.1 Recipient key and token

- The **recipient key** is `HMAC-SHA256(K_rk, normalized address)`. It identifies an address
  without revealing it, so a service that records opt-outs never needs to hold addresses. `K_rk` is
  long-lived; rotating it re-keys every recipient and requires re-deriving the stored keys.
- The **token** is the base64url encoding (no padding) of
  `0x01 ‖ key_id ‖ recipient_key ‖ tag`, where `key_id` is one byte, `recipient_key` is 32 bytes,
  and `tag` is the first 16 bytes of `HMAC-SHA256(K_tok[key_id], 0x01 ‖ key_id ‖ recipient_key)`.
  Token keys rotate by `key_id`; every retired key stays valid for verification, so a link in an old
  message keeps working (at least 30 days is the legal floor; the channel never expires a key on its
  own).
- Keys come from the environment (`KHIVE_EMAIL_UNSUBSCRIBE_RK_KEY`, `KHIVE_EMAIL_UNSUBSCRIBE_TOKEN_KEYS`
  as `id:key` pairs, the highest id minting). A missing key with external senders configured is a
  configuration error.

#### 5.2 Headers

Every external message carries:

```
List-Unsubscribe: <mailto:<mailbox>?subject=unsubscribe%20<token>>
```

When one-click is enabled (§6), the header also carries `<https://<base>/<token>>` and the message
carries `List-Unsubscribe-Post: List-Unsubscribe=One-Click`. One-click is enabled only after a probe
message through the deployment's sending boundary shows a DKIM signature whose `h=` tag covers both
headers (RFC 8058 §4). The channel does not sign; the boundary does, so this is a deployment fact
to measure, not one this code can assume. Without it, messages carry the `mailto:` form only.

Policy-recipient messages carry neither header.

#### 5.3 Footer

`KHIVE_EMAIL_EXTERNAL_FOOTER_FILE` names a text file appended to the body of every external message.
It must contain the placeholder `{unsubscribe}`, which the channel replaces with a plain sentence
giving the `mailto:` instruction (and the HTTPS link when one-click is enabled). The operator owns
the rest of the footer, including the sender identification and postal address the obligations
above require. External admission refuses while the footer is unset, unreadable or missing the
placeholder.

#### 5.4 Three entry points, one list

1. **Mailto.** An inbound message whose `Subject:` contains `unsubscribe` followed by a token whose
   tag verifies adds an `outbound` `recipient_key` entry (`source = mailto_token`). The token is the
   authority, so this runs before sender authentication (§7). The message is recorded as a machine
   record and is never delivered to an actor or screened. A token that does not verify is ignored
   and the message continues through §7 as ordinary mail.
2. **One-click.** A POST to the HTTPS URI is recorded by the remote endpoint and reaches the local
   list through the mirror (§6) as `source = one_click`.
3. **Reply.** An authenticated external message that screening labels as an opt-out (§8.4) adds an
   `outbound` `address` entry for its `From:` address (`source = reply_stop`) and is still delivered,
   marked, so a person sees it.

Entry points 1 and 3 take effect when their entry commits, before the next admission. Entry point 2
takes effect within one mirror interval.

### 6. Remote suppression mirror (one-click)

The one-click endpoint is a public HTTPS service, and no public service can reach this daemon. The
daemon therefore pulls. When `KHIVE_EMAIL_SUPPRESSION_MIRROR_URL` and its credential are configured,
the remote set of suppressed recipient keys is read and any missing keys are added locally as
`one_click` entries. The read returns the complete set with an `as_of` time; an incremental form is
a compatible later extension. The remote set is an entry point, not a second list: the local list
stays the one source that admission and delivery read, and nothing is written back to the remote.

- **At delivery** (the point where a message actually leaves): the delivery component refreshes the
  mirror before handing any external message to SMTP, at most once per delivery cycle. If the
  refresh fails, every external message in that cycle stays pending (transient, never a permanent
  failure) and none is sent. A message is never sent on a suppression read that did not succeed.
- **At admission**, which performs no network I/O (Amendment 4): the local list is read, together
  with the time of the last successful refresh. If that time is older than
  `KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS`, external admission refuses with "suppression
  mirror stale", so a long outage stops new external messages from queueing behind it.

An outage therefore delays sends; it never lets one through to an address that opted out.

This decision fixes the token format (§5.1) and the pull contract. The endpoint's own storage,
route and confirmation page belong to the service that hosts it and are specified there.

### 7. Inbound disposition order

This replaces the two-way attribute-or-quarantine decision of ADR-056's 2026-07-02 amendment with
an ordered disposition. Steps 1, 3 and 4 are that amendment's existing behaviour, unchanged.

1. Malformed or over the byte cap: quarantine (unchanged).
2. Unsubscribe token that verifies (§5.4 entry point 1): machine record, not delivered.
3. Domain authentication fails: quarantine (unchanged).
4. `From:` is the maintainer: attributed and routed as today (unchanged).
5. External senders not configured: quarantine as `off-allowlist` (today's behaviour).
6. An `inbound` suppression entry matches the authenticated `From:` address or its domain:
   quarantine with reason `blocked`.
7. Otherwise: screening (§8.3) decides between delivery, withheld delivery and quarantine.

A blocked sender is checked after authentication because an unauthenticated `From:` cannot
identify who is being blocked, and such mail is quarantined at step 3 regardless.

### 8. External inbound delivery

#### 8.1 Identity and marking

A delivered external message's sender is `email:<normalized address>`, never the maintainer actor.
Its metadata carries `origin = external` and `trust = untrusted`, plus the screening labels (rule
identifiers only, never the matched text). Read surfaces return those fields with the message, so a
consumer can fence the body as data. Domain authentication establishes that the sender's domain
vouches for the `From:` address. It says nothing about whether the content is safe.

#### 8.2 Routing

- A message whose `X-Khive-Thread-ID`, `In-Reply-To` or `References` resolves to an outbound message
  note owning that Message-ID under ADR-122 Amendment 3 is delivered to that note's sending actor,
  in that thread.
- Otherwise, `KHIVE_EMAIL_INBOUND_ROUTES` (comma-separated `address=actor` pairs) maps the first
  recipient address in `Delivered-To:`, `To:` or `Cc:` that matches a configured route to its actor.
  This lets one polled mailbox serve aliases such as a support address.
- Otherwise the message goes to `KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR` (required when external senders
  are configured).

Routing headers are sender-asserted. A forged one changes only which actor receives an untrusted,
screened message; it never changes the message's trust.

#### 8.3 Screening (deterministic, in the daemon)

Screening runs at ingest with no network and no model. Its rules are a versioned data file in the
email crate; each rule has a positive and a negative test fixture.

| Family                  | Matches                                                                                                                                                                                                                                 |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hidden_text`           | Unicode format characters (category Cf) outside emoji sequences; HTML text hidden by `display:none`, `visibility:hidden`, zero font size, or text colour equal to its background, when the plain-text part does not carry the same text |
| `agent_addressed`       | text addressed to an automated reader: instruction-override phrases, role claims, requests to reveal or change instructions                                                                                                             |
| `action_request`        | requests to run, install or execute something, pipe a download into a shell, connect to a server or tool endpoint, or supply a credential, token or password                                                                            |
| `executable_attachment` | an attachment whose declared or sniffed type is executable or a script                                                                                                                                                                  |
| `html_only`             | no text/plain part; delivered with text extracted from the HTML                                                                                                                                                                         |

Outcomes:

- no family, or `html_only` alone: **deliver**, marked;
- `hidden_text`, `agent_addressed` or `action_request`: **deliver withheld**: the message is
  delivered with its sender, subject and labels, its body is replaced by a placeholder naming the
  labels, and the original bytes are kept in the quarantine replay store for an explicit operator
  read; inbox notices never carry its body;
- `executable_attachment`, or two or more of the three families above: **quarantine** with reason
  `screened`.

#### 8.4 Opt-out label

A separate rule family, `opt_out`, matches explicit opt-out phrasing in the subject or the first
non-quoted line ("unsubscribe", "remove me", "opt out", "take me off", "stop emailing", "stop
sending"). It drives §5.4 entry point 3 and does not change the delivery outcome. Its errors run
toward suppression, which is the safe direction.

#### 8.5 Model screening (not decided here)

A model classifier would read untrusted text itself, so it could only run as a process with no tools,
and it puts a model call, its cost and its availability on the ingest path. If added later, it may
only tighten a deterministic outcome, never loosen one.

### 9. Phasing

**Before the first external send**: §1 to §4, §5 without one-click (mailto header, footer, mailto
entry point, reply entry point), §7 and §8.1 to §8.4. Without inbound routing a reply or an opt-out
written in prose is invisible, so outbound alone is not enough. The first send also waits on one
measured round trip: an external message to a test address, a reply from it, and that reply read in
the routed actor's inbox.

**Then**: §6 and the one-click headers, after the DKIM coverage probe passes and the remote endpoint
exists.

**Later, separately decided**: §8.5.

## Acceptance

Real request, outbox and ingest paths, with a mock transport and recorded raw messages.

| Case                        | Required behaviour                                                                                                                                                                                                                                                 |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A1 disabled                 | With `KHIVE_EMAIL_EXTERNAL_SENDERS` unset, every Amendment 4 acceptance case still holds and non-maintainer inbound mail is quarantined `off-allowlist`.                                                                                                           |
| A2 sender scope             | An actor outside the set is refused before creation for an external recipient and admitted for a policy recipient; an actor inside the set is admitted.                                                                                                            |
| A3 suppression at admission | An address, domain or recipient-key entry each refuses a fresh send before creation; a different address in the same domain is refused only by the domain entry.                                                                                                   |
| A4 suppression at delivery  | A message admitted, then suppressed before delivery, ends `failed` with suppression named and SMTP untouched.                                                                                                                                                      |
| A5 cap                      | With cap N, the (N+1)th fresh external admission in 24 hours is refused; an exact keyed replay of an earlier message is answered and not counted.                                                                                                                  |
| A6 footer                   | Unset footer, unreadable file and a file without `{unsubscribe}` each refuse external admission; policy-recipient sends are unaffected.                                                                                                                            |
| A7 headers                  | An external message carries the `mailto:` header with a token that verifies; one-click headers appear only with one-click enabled; a policy-recipient message carries neither.                                                                                     |
| A8 mailto opt-out           | An inbound unsubscribe with a valid token suppresses the token's recipient key, is not delivered, is not screened, and the next send to that address is refused. A tampered token is ordinary mail.                                                                |
| A9 reply opt-out            | An authenticated reply saying "please remove me" suppresses its `From:` address and is delivered with the `opt_out` label.                                                                                                                                         |
| A10 removal                 | Removing an opt-out entry without evidence is refused; with an authenticated later inbound message from the same address as evidence it succeeds and is audited.                                                                                                   |
| A11 mirror                  | A refresh failure at delivery leaves every external row in that cycle pending and SMTP untouched; a successful refresh releases them; a remote key suppresses its address before its row is sent; a refresh older than the maximum age refuses external admission. |
| A12 inbound order           | Each step of §7 is reached by a fixture that fails all earlier steps; a blocked authenticated sender is quarantined `blocked`.                                                                                                                                     |
| A13 routing                 | A reply to a verified outbound message reaches its sending actor's thread; a fresh message to a routed alias reaches that route's actor; anything else reaches the external inbound actor.                                                                         |
| A14 marking                 | A delivered external message reads back with `origin = external`, `trust = untrusted` and its labels, and its sender is never the maintainer actor.                                                                                                                |
| A15 screening               | Each rule family has a positive fixture that changes the outcome and a negative fixture that does not; a withheld message's notice and read omit the body; two families quarantine.                                                                                |

## Consequences

- External correspondence becomes possible without a restart per recipient, and an opt-out by any
  route stops the next send.
- Inbound mail from authenticated strangers now reaches an actor's inbox. Marking and the read-surface
  fields are the primary control; deterministic screening is a filter that a careful attacker can
  pass, so consumers must treat `origin = external` content as data, never as instructions.
- Any consumer that treats every `email:` sender as the maintainer must change to read attribution
  (`trust`, and the maintainer actor) instead of the address prefix.
- One table and three verbs are added to the comm pack; the configuration surface grows by the
  variables named above, all inert while `KHIVE_EMAIL_EXTERNAL_SENDERS` is unset.

## Alternatives considered

- **Keep the environment allowlist and add recipients per campaign.** Each addition is a restart, an
  opt-out cannot be written by the channel, and replies stay quarantined.
- **Suppression in configuration.** Machine-written opt-outs need a writable store.
- **Push opt-outs from the endpoint to the daemon.** The daemon is not reachable from a public
  service; pulling with a fail-closed staleness bound gives the same effect.
- **A model classifier on the ingest path now.** Deferred (§8.5) for latency, cost, availability and
  because it would read untrusted text with whatever tools it holds.
- **A second channel instance per mailbox.** Recipient-address routing (§8.2) serves aliases on one
  polled mailbox; separate mailboxes remain possible later.

## Open questions

- Sending from an alias address (for example a support address) needs send-as permission on the
  polled mailbox; the channel sends from one mailbox today.
- Hard-bounce processing into suppression is not covered here.
- Whether a forwarding rule (as opposed to an alias on the same mailbox) preserves an authentication
  result that passes §7 step 3 depends on the receiving boundary and is to be measured per deployment.
