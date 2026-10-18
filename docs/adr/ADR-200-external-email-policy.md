# ADR-200: External Email Policy: Sender Scope, Suppression, Unsubscribe and Screened Inbound Delivery

- **Status**: Accepted (2026-10-11)
- **Date**: 2026-10-11
- **Amends**: [ADR-122](ADR-122-email-outbound-delivery.md) (Amendment 4, recipient policy),
  [ADR-056](ADR-056-channel-transport-layer.md) (Amendment 2026-07-02, quarantine disposition)
- **Addresses**: [#5165](https://github.com/ohdearquant/khive/issues/5165)
- **Amendments**: Amendment 1 (proposed 2026-10-14), Amendment 2 (proposed 2026-10-16), Amendment 3 (proposed
  2026-10-16), Amendment 4 (proposed 2026-10-17) and Amendment 5 (proposed 2026-10-17), at the end of this record

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

## Amendment 1 (2026-10-14) -- Withheld originals are read through comm, and external routing is narrowed

**Status**: Proposed 2026-10-14.

**Trigger.** §8.3 keeps a withheld message's original bytes "for an explicit operator read" and names neither the
read nor its callers. The first implementation keeps the original as a blob, records its content reference in the
delivered message's properties, and documents `blob.get` as the read. `blob.get` reads any object whose reference the
caller holds and does not check the caller against a message, and inbox and thread reads return message properties.
Any actor that can list the recipient's inbox could therefore read text that screening withheld. Two further gaps were
found in the same review: a thread UUID asserted by an external sender can route to a thread that was never an email
conversation, and an opt-out inside a message that screening quarantines is not recorded.

**Decision.**

1. **One read for withheld originals.** `comm.withheld_body(id)` returns the original RFC 5322 bytes of a delivered
   withheld message, base64-encoded, with their size. It refuses an id that is not a delivered withheld external
   message, and it never returns a content reference.
2. **Callers.** The read is open to the message's recipient actor (its `to_actor`) and to the maintainer (the
   runtime's configured actor, the same check `comm.suppression_remove` uses). Any other caller is refused before the
   original is read, with the typed permission-denied form, and the refusal names neither the sender nor any content.
   Every call, allowed or refused, writes an audit event naming the caller, the message id and the outcome.
3. **No reference on the delivered message.** A delivered withheld message does not carry `quarantine_content_ref`
   or any other blob reference in its properties. The original is owned only by the message's `quarantine-original`
   attachment, which read surfaces already omit. A retried delivery of the same transport message verifies custody
   through that attachment. Inbox, thread, read and notice surfaces return no original bytes and no reference.
4. **Thread routing for external mail.** For an external message, the thread-UUID path of §8.2 (an
   `X-Khive-Thread-ID` that names a thread rather than a Message-ID) routes only when that thread holds an outbound
   email message whose Message-ID verifies under ADR-122 Amendment 3, the same proof the Message-ID path requires.
   A UUID naming any other thread falls through to recipient routes and then the external inbound actor. Maintainer
   mail keeps today's resolution.
5. **Opt-out independent of the outcome.** An authenticated external message that screening quarantines as
   `screened` and that carries the `opt_out` label still adds the §5.4 entry point 3 suppression for its `From:`
   address before the quarantine is acknowledged. §8.4 already says the label does not change the delivery outcome;
   this makes the suppression not depend on it either, in the direction §8.4 names as safe.
6. **Quarantine records are addressed to the maintainer.** At ingest, a message the channel quarantines, whatever
   its reason, is addressed to the maintainer (the runtime's configured actor) in place of the landing actor that the
   ingest handler's routing would choose (`khive-pack-comm/src/handlers/ingest.rs`). The recipient check that already
   governs every mailbox read (`permits_message_note`, `khive-runtime/src/mailbox_view.rs`) then keeps the record and
   its `quarantine_content_ref` out of every other actor's mailbox reads. The rule applies at ingest, so it covers
   records ingested once it is deployed: a quarantine record already stored keeps the `to_actor` it was written with,
   and a replay of the same message does not re-address it. A runtime with no configured actor has no maintainer, so
   its quarantine records keep today's landing actor and are visible to that actor.

**Unchanged.** Quarantine records keep `quarantine_content_ref` and their existing operator recovery path. The
screening rules, outcomes and labels of §8.3 and §8.4 are unchanged.

**Acceptance (added to the ADR's table).**

| Case                    | Required behaviour                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A16 withheld read       | The recipient actor and the maintainer each read a withheld original byte for byte; a third actor is refused before any read; each call is audited; an ordinary or quarantined message is refused.                                                                                                                                                                                                                                                                                                                                   |
| A17 no reference        | A delivered withheld message's properties, and every inbox, thread, read and notice result for it, contain no blob reference and no original bytes; a retried delivery of the same transport message is acknowledged through its attachment. A quarantine record ingested under Decision 6 is addressed to the maintainer: a non-maintainer landing actor finds it through no inbox, thread, read, get or search result, and the maintainer finds it through each; on a runtime with no configured actor it keeps its landing actor. |
| A18 thread routing      | An external message whose thread UUID names a thread with a verified outbound email routes to that thread's actor; one naming a thread with no such email routes by recipient alias or to the external inbound actor.                                                                                                                                                                                                                                                                                                                |
| A19 quarantined opt-out | An authenticated opt-out in a message quarantined as `screened` suppresses its `From:` address, and the next send to that address is refused.                                                                                                                                                                                                                                                                                                                                                                                        |

Realistic replies from developers that ask the reader to install, run or connect something are A15 fixtures, and the
change that adds them reports how many each screening family catches.

## Amendment 2 (2026-10-16) -- The opt-out window is the first non-quoted paragraph, and the phrase list names asks for no more email

**Trigger.** A recorded decline ends its first paragraph with "Please don't email me about this again", and the
sending client wraps that sentence onto a second line. §8.4 reads the subject and the first non-quoted line, so the
phrase is never seen, and the phrase list does not contain it. Under 15 U.S.C. 7704(a)(3) and (a)(4) a reply to the
sender's return address asking for no more mail is an opt-out request the sender has ten business days to honour,
whatever its wording, and a missed one is a second send to a person who asked us to stop. The two errors are not
symmetric: a false label suppresses a person who wrote "don't email me", which is never wrong.

**Decisions (amending §8.4).**

1. The opt-out window is the subject or the **first non-quoted paragraph** of the plain text: the lines from the
   first non-empty line that does not start with `>` up to the next empty or quoted line, joined with one space
   before matching. Quoted text and everything after the first blank line stay outside the window, so a top-posted
   reply above our own message, and a reply that quotes it, match only on what the person wrote. The HTML-only
   path reads the same paragraph of the extracted text.
2. The phrase list gains the plain asks for no more email: "stop contacting", "don't email me" (straight and curly
   apostrophe), "dont email me", "do not email me", "don't contact me" (both apostrophes), "do not contact me",
   "no more emails". The bare words "email me" are not a phrase.
3. Outcome and entry point are unchanged: the label still never changes the delivery outcome, and §5.4 entry
   point 3 still writes the suppression.

**Acceptance (added to the ADR's table).**

| Case                | Required behaviour                                                                                                                                                                                                |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A20 wrapped opt-out | A recorded decline whose "don't email me" sentence is wrapped onto the second line of its first paragraph is delivered marked with the opt-out label and no other family.                                         |
| A21 email-me decoy  | A recorded engaged reply containing "email me" twice ("can you email me the setup docs", "you can email me at") is delivered without the opt-out label; a widening to the bare words would turn this fixture red. |
| A22 quoted opt-out  | A reply whose only opt-out phrase is on a quoted line, followed by an unquoted line, is delivered without the label (existing fixture, unchanged).                                                                |

## Amendment 3 (2026-10-16) -- External inbound without external senders

**Status**: Proposed 2026-10-16.

**Trigger.** §7 step 5 ties external inbound to configured external senders: while `KHIVE_EMAIL_EXTERNAL_SENDERS` is
unset, every authenticated message from a sender other than the maintainer is quarantined as `off-allowlist`. A
deployment that has to receive mail from outside senders, such as service notices addressed to its mailbox, but must
not send outside its policy recipients has no setting for that: configuring external senders also enables outbound
external sending.

**Decision.**

1. `KHIVE_EMAIL_EXTERNAL_INBOUND=enabled` enables external inbound on its own. External inbound is enabled when
   external senders are configured or this setting is `enabled`. §7 step 5 is read as "External inbound not enabled:
   quarantine as `off-allowlist`", and steps 6 and 7 apply whenever it is enabled.
2. The setting changes nothing outbound. While external senders are unset, a send to an address outside the outbound
   allowlist is refused as before (§2), and no unsubscribe token verifies (§5).
3. A message admitted under it is delivered exactly as §8 describes: sender `email:<address>`, `origin = external`,
   `trust = untrusted`, screening per §8.3, routing per §8.2 and Amendment 1 decision 4.
   `KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR` is required whenever external inbound is enabled, and start-up fails while it
   is unset or empty.
4. Unset or empty means disabled. Any other value is a configuration error at start-up, so a misspelled value never
   reads as disabled.

**Acceptance (added to the ADR's table).**

| Case             | Required behaviour                                                                                                                                                                                                                                                                                                                                                                                           |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A23 inbound only | With external senders unset and the setting enabled, an authenticated message from a sender other than the maintainer is delivered to its routed actor marked external and untrusted, an inbound suppression entry still blocks it, a send to an address outside the outbound allowlist is refused, and start-up fails while the inbound actor is unset or empty or the setting holds an unrecognised value. |

## Amendment 4 (2026-10-17) -- The remote mirror's credential, its answer, and an idle refresh

**Status**: Proposed 2026-10-17.

**Trigger.** §6 names `KHIVE_EMAIL_SUPPRESSION_MIRROR_URL` and `KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS` but leaves
"its credential" unnamed, and it says nothing about an answer that is only partly well formed. Its refresh is also tied
to delivery: the mirror is read before an external message is handed to SMTP, and admission refuses once the last
successful refresh is older than the maximum age. A deployment with no external message pending therefore never
refreshes, passes the maximum age, and from then on refuses every new external message, so no external message is ever
pending to trigger the refresh that would reopen admission. [Source: §6, the "At delivery" and "At admission" bullets.]

**Decision.**

1. **Configuration.** The credential is `KHIVE_EMAIL_SUPPRESSION_MIRROR_TOKEN`, sent as a bearer token. The URL, the
   token and the maximum age are read only while external senders are configured, like the §5 variables. The URL must
   be `https://`. Setting some of the three but not all, or a maximum age that is not a positive integer, is a
   configuration error at start-up. No error repeats the credential.
2. **The read.** One `GET` under one deadline that covers connection, headers and body. Redirects are not followed, so
   the credential reaches only the configured URL. The answer is `{"as_of": "<RFC 3339>", "recipient_keys": [...]}`
   with an optional `count`; unknown fields are ignored, so the incremental form §6 allows can be added later. A status
   other than 200, a body over the size limit the implementation states, an `as_of` that does not parse, a `count` that
   differs from the number of keys, or any key that is not 64 lowercase hex characters fails the whole refresh. A partly
   valid set is never applied.
3. **Recording.** The missing keys are added before the refresh time is recorded, so a failure part way leaves the
   previous time, which can only make admission refuse sooner. The time is kept in the local store beside the list, so
   every process that admits external mail reads the same value.
4. **Idle refresh.** Besides the refresh before an external send, a delivery cycle refreshes when no successful refresh
   is recorded or the last one is at least half the maximum age old. It is the same refresh, still at most one per
   cycle, and it changes no other rule: a failed idle refresh records nothing, so admission keeps refusing until one
   succeeds.

**Acceptance (added to the ADR's table).**

| Case                | Required behaviour                                                                                                                                                                                                                                                                                                                                                                          |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A24 mirror contract | A partial or non-`https` mirror configuration refuses start-up without repeating the credential. A redirect, a non-200 answer, a count mismatch or one malformed key fails the refresh and adds no key. With nothing pending, a cycle with no recorded refresh reads the mirror and, once that read succeeds, external admission is open; a cycle under half the maximum age reads nothing. |

## Amendment 5 (2026-10-17) -- Occurrence-aware undo, complete mirror snapshots, and owned refresh

**Status**: Proposed 2026-10-17. This draft requires specification approval before dependent implementation lands. It does not change the Accepted status of the base ADR or mark Amendment 4 Accepted; Amendment 4 remains Proposed pending this decision.

**Scope**: Amend §3's removal of mirrored `one_click` entries and §6's mirror protocol, freshness and delivery-cycle rules. Replace Amendment 4 item 2's response shape and unknown-field compatibility rule; retain its credential, HTTP deadline, redirect, status, body-limit and whole-response refusal requirements. Clarify item 3's recorded time and item 4's ownership of idle refresh. Other suppression sources, sender policy, daily cap, recipient normalization, admission replay precedence, inbound disposition and SMTP checks remain unchanged.

**Addresses**: [#5630](https://github.com/ohdearquant/khive/issues/5630), [#5631](https://github.com/ohdearquant/khive/issues/5631), [#5632](https://github.com/ohdearquant/khive/issues/5632).

**Trigger.** A local removal can be undone by the next key-only mirror read. Ignoring unknown response fields can accept a partial page as the complete set. Awaiting the HTTP read in the outbox scan also delays policy-recipient mail during a mirror outage. Correcting these together requires an opt-out identity shared by the service and reader, an explicit complete response, and refresh ownership separate from scanning.

### Decision 1: remove remotely first, then undo that occurrence locally (§3)

An active remote opt-out has an `occurrence_id`, a random (version 4) UUID minted when the remote row is inserted; the service never issues the same occurrence twice. Insertion is insert-if-absent: another click while that row is active leaves its occurrence unchanged. A new insertion after removal has a different occurrence. Snapshot timestamps do not identify or order opt-out occurrences.

1. A local `one_click` row obtained from the mirror stores the `occurrence_id` with which it was read. The existing `UNIQUE(scope, kind, value, source)` identity remains unchanged: there is one `one_click` row per recipient key. When a non-revoked new occurrence arrives for a key whose row is still present, update that row's `occurrence_id` in place rather than create another suppression row or widen its identity. The row also records `occurrence_seen_at`: the local commit time of the snapshot application that stored its current occurrence. Insertion sets it, every in-place occurrence change sets it again, and item 6's legacy binding sets it at the binding application.
2. `comm.suppression_remove` retains §3's reason, authorized caller and evidence requirements: an authenticated inbound message from the same address that arrived after the opt-out. For a mirrored `one_click` row carrying an occurrence, "after the opt-out" means after the row's `occurrence_seen_at`, so evidence that predates the current occurrence never removes it. The row's original creation time stops bounding the evidence once its occurrence has changed. For a mirrored `one_click` row it also requires that the latest successfully applied mirror snapshot no longer contain the row's `(recipient_key, occurrence_id)`. The latest successfully applied snapshot is the applied snapshot with the highest generation (Decision 2), which the generation rule makes the last one applied. Missing or unverifiable snapshot state is not evidence of absence. If that occurrence remains in the snapshot, refusal tells the operator to remove it at the one-click service first. Remote removal belongs to the operator through the service's administrative removal contract. The daemon makes no remote write.
3. Successful local undo removes the row and records its audit in one `SqlAccess::atomic_unit`. When the row has an `occurrence_id`, that same unit also records the revoked `(recipient_key, occurrence_id)` durably. Revocations are separate records keyed by that pair, not additional suppression rows. Revalidate the row, its occurrence and the snapshot precondition in the unit so a racing refresh cannot make a stale removal succeed. A failure does not leave a successful removal without its required revocation and audit. The legacy case in item 6 never invents a missing occurrence or revocation pair.
4. Applying a later snapshot never reinstates a revoked occurrence, by insertion or in-place replacement, including from a stale or out-of-order read. A different, non-revoked occurrence for the same key is a new opt-out and must suppress again, updating an existing row as item 1 requires or inserting one if no row remains. Undo of an older occurrence cannot remove that new suppression. Occurrence UUIDs are identities, not sortable generations; do not substitute UUID or wall-clock ordering for equality and revocation checks.
5. Source independence remains: removing `one_click` does not remove or weaken `operator`, `mailto_token` or `reply_stop` entries. Absence from a remote snapshot alone never deletes a local opt-out; evidence-backed local removal is still required.
6. A legacy `one_click` row without an occurrence receives the occurrence carried for its key by the first applied v2 snapshot. If its key is absent from that snapshot, leave the row without an inferred occurrence. If a later applied v2 snapshot first carries its key, item 1 binds that occurrence in place. A legacy row whose key is absent from the latest successfully applied v2 snapshot satisfies item 2's absence precondition; the existing reason, authorization, later authenticated inbound evidence and atomic removal/audit requirements still apply. No occurrence is inferred or invented, and unknown snapshot state does not prove absence.

A local-only override is insufficient. While the remote row remains active, another real opt-out click is absorbed by insert-if-absent, so the reader cannot distinguish it from the occurrence it previously ignored. Whether the daemon should eventually perform routine remote undo is a separate service decision, outside this amendment.

### Decision 2: accept a closed, complete v2 response (§6 and Amendment 4 item 2)

The daemon requests the configured endpoint with the query selector `format=complete-v2` (`GET ...?format=complete-v2`). The response body must declare `"format": "complete-v2"`. Its top-level JSON object has exactly these required fields:

| Field        | Required meaning                                                                                                                                                |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `format`     | Exactly the string `"complete-v2"`; any other value refuses.                                                                                                    |
| `as_of`      | The service snapshot time, a valid RFC 3339 string.                                                                                                             |
| `generation` | A nonnegative integer the service increases with every committed change to the opt-out set and never decreases, including across service restarts and failover. |
| `count`      | A nonnegative integer equal to the number of entries.                                                                                                           |
| `entries`    | An array of objects with exactly the required `recipient_key` and `occurrence_id` fields.                                                                       |

Each `recipient_key` is 64 lowercase hexadecimal characters. Each `occurrence_id` is a UUID in its canonical lowercase hyphenated spelling: 36 characters in the `8-4-4-4-12` grouping. Any other spelling refuses, including uppercase, unhyphenated, braced or URN forms; the reader must not silently normalize one of those inputs into an accepted entry. A duplicate recipient key refuses even when both entries carry the same occurrence. Missing, null, mistyped or unknown fields at either object level, malformed values, unsupported formats and count mismatch refuse the whole refresh before any application or success stamp. The complete empty set is valid with `count = 0` and `entries = []`.

The selector and literal above define this proposed contract; they are not a claim that the service already deploys v2. Producer and reader must use the same approved spelling before rollout. An incremental or paged answer requires a different format and its own reader. The v2 reader refuses it rather than treating its first page as the complete set. This replaces Amendment 4 item 2's sentence allowing unknown fields for future incremental compatibility. A count match checks consistency with the declared complete snapshot; it cannot independently prove the service's database contents.

One deadline still covers connection, headers and body. Redirects are not followed; the bearer credential reaches only the configured endpoint. Non-200 status, oversize body and every existing transport/configuration failure remain failed refreshes. Errors do not disclose the credential or list contents.

**Order and read guarantee.** The service answers with a snapshot that reflects every opt-out change it committed before it received the request, and carries that snapshot's generation. The reader persists the highest generation it has applied, in the same unit as that snapshot's local writes. An answer with a lower generation is a failed refresh: nothing is applied, the stamp does not move, and the undo precondition keeps reading the higher-generation snapshot. An answer with the same generation must carry exactly the applied entries. A different set at an equal generation refuses as an inconsistent producer. Generations order snapshots; they do not order occurrences (Decision 1 item 4). A reader with no applied generation (first start, or lost state) accepts the first valid answer. For that first answer the read guarantee is the only content bound, which is why rollout step 1 makes it a producer conformance item.

The `as_of` window below checks clocks; it does not bound how old a snapshot's content is. Content age is bounded by the read guarantee together with Decision 3 item 3's refresh-after-selection witness. A batch is selected before its qualifying request is sent, so any opt-out the service committed before the selection was committed before the service received that request, and it is in the answer that releases the batch. No clock comparison is involved.

For freshness, record two local wall-clock timestamps for the GET: its send time `S` and completed receive time `R`. Reject a remote `as_of` earlier than `S - 300 seconds` or later than `R + 300 seconds`; the boundary values are accepted. The recorded local successful refresh time is **S**, written only after the accepted snapshot's required local writes have committed, not the later apply-completion time. Remote `as_of` remains distinct metadata. At admission, a missing timestamp, a timestamp later than local now, or an age exceeding `KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS` is stale and refuses. Admission remains network-free. These wall-clock checks establish freshness only; occurrence equality and durable revocation establish opt-out identity.

### Decision 3: one owned refresher, no HTTP wait in the scan (§6 and Amendment 4 item 4)

1. The delivery component owns one refresher task per configured mirror. That task performs both idle and demand refreshes, allows at most one request in flight, and is cancelled and joined by the component lifecycle. No unowned refresh continuation survives shutdown.
2. The outbox scan never awaits HTTP. Policy-recipient scanning and delivery continue while a mirror request is pending or failing. Idle refresh retains Amendment 4's trigger: no successful refresh, or a recorded refresh at least half the maximum age old. Demand and idle work share the same task and retry discipline.
3. An external delivery batch records the instant at which its messages were selected. It may proceed to SMTP only after a refresh that **started strictly after that selection** has succeeded and its required local writes have committed. Compare local scheduling events with a monotonic ordering; neither remote `as_of` nor a young persisted stamp substitutes for this witness. Under Decision 2's read guarantee the qualifying answer reflects every remote opt-out committed before its request was received, which includes every one committed before selection. A request already in flight when selection occurs cannot release that batch, even if it finishes later. A later-selected batch also cannot reuse an earlier refresh result. A subsequent refresh may satisfy waiting batches whose selections all precede its start.
4. A failed or cancelled refresh releases no external batch. External messages stay pending unless a separate existing terminal rule applies. Retry delays start at 5 seconds, double after successive failures and cap at 300 seconds; polling and new messages cannot bypass backoff or start parallel requests. Success ends the failure sequence.
5. Cancellation observed before apply begins applies nothing. Once the synchronous store unit has begun, let it settle, then withhold the result from delivery if cancellation was requested. This does not claim to roll back already committed storage or to interrupt arbitrary synchronous I/O. The task remains owned until settlement and join. Network I/O never runs inside a writer transaction.
6. Immediately before each external SMTP handoff, reread the local suppression list as §3 already requires. A successful refresh is necessary, not permission to bypass a subsequently added suppression, claim ownership, recipient policy or any other delivery gate.

“Delivery cycle” in §6 now means the selected external batch and its qualifying refresh, rather than every pass of the short outbox scan loop. Keeping a batch pending across scan passes does not grant send permission.

### Alternatives considered

- **Remote wins forever:** importing the same key after every local undo makes §3's undo temporary and does not resolve #5630.
- **Local generation override without remote removal:** insert-if-absent absorbs a second click while the old remote row remains active; a local reader cannot recover the lost event distinction. Remote-first operator removal is mandatory.
- **Permanent key override, TTL override, or clearing an override after absence:** these respectively hide future opt-outs, expire valid undo arbitrarily, or miss remove-and-re-add between polls. None replaces occurrence identity and remote-first undo.
- **Optional count and ignored continuation fields:** a consistent first page can still omit suppressed recipients. A closed complete format is required; pagination needs a separately negotiated reader.
- **A tighter `as_of` window instead of a generation:** narrows how stale an accepted answer can look but cannot prove a fresh snapshot, and wall-clock order cannot rank two answers whose snapshots the service took in a different order. Ordering needs the producer's generation and the read guarantee.
- **Background TTL cache as send permission:** a previously young stamp must not authorize a batch whose qualifying refresh failed or started before selection. It weakens fresh-before-send even if admission remains bounded.
- **Cancellation alone or a parser-only first slice:** useful partial changes cannot remove normal scan blocking and cannot provide durable occurrence-aware undo. They do not constitute completion of this cluster.
- **Routine remote mutation by the daemon:** deferred to the one-click service's administrative contract; this amendment preserves the pull-only daemon.

### Rollout and implementation sequence

1. Reconcile the proposed `GET ...?format=complete-v2` selector, `"format": "complete-v2"` response and canonical lowercase hyphenated UUIDs with the producer before publication. The producer also commits to the read guarantee and to a generation that never decreases across restart and failover, and proves both with its own conformance tests before the reader ships. Any jointly agreed spelling change must update this contract and both sides together. The service deploys v2 beside v1 first. Existing v1 callers remain supported by that service; the new daemon requests only v2 and refuses a v1 answer.
2. Obtain specification approval on this amendment before dependent source lands. After signature and live v2 availability, one grouped implementation PR covers all three issues: occurrence-aware local state and atomic undo, the strict reader/freshness checks, and owned refresh/delivery coordination. The service's v2 deployment is a separate service-owned change.
3. Keep the suppression row's natural key unchanged and store occurrence revocations separately. Apply Decision 1 item 6 to legacy rows: attach only the occurrence actually carried by a valid applied v2 snapshot; absent keys remain without an invented occurrence and require the normal evidence-backed undo. Retain suppression/audit evidence and prove restart behavior. Migration must not infer an unknown occurrence's revocation or release mail merely to complete an upgrade. An older reader cannot safely enforce this contract; rollback must keep external sending closed until a compatible reader and state are restored.
4. Retain the base Accepted text and proposed amendment history. Integrate this as a scoped amendment; do not silently rewrite Amendment 4's status or unrelated decisions.

### Acceptance and negative controls

These are required host witnesses, not results obtained by writing this draft. Exercise real dispatch, storage, refresher and outbox paths with isolated loopback HTTP and mock SMTP. Synchronize races with actual request/apply/selection witnesses rather than sleeps alone. Run the same storage cases through direct and queued file-backed writers where supported.

| Case                                                   | Required behavior                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | Must-red control                                                                                                                                                                                                  |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A25 remote-first undo                                  | Later authenticated matching inbound evidence cannot remove an occurrence still present in the latest applied snapshot. After operator removal and a complete snapshot proving absence, local removal, separate pair-keyed revocation and audit commit together. A legacy row with no occurrence may satisfy the precondition only when its key is absent from the latest applied v2 snapshot; remove and audit atomically without inventing a UUID. After an in-place occurrence change, inbound evidence that arrived after the previous occurrence's `occurrence_seen_at` but before the current one's cannot remove the row. | Bypass snapshot precondition, fabricate a legacy occurrence, commit removal without its required revocation/audit, or keep the row's creation time as the evidence cut-off after an occurrence change.            |
| A26 durable occurrence identity                        | Replaying a revoked occurrence before and after restart cannot insert it or replace a newer row with it. A new non-revoked occurrence suppresses again; if the row remains, its occurrence changes in place with the same row ID and one row under the unchanged natural key. Revocation records for earlier occurrences remain independent. Another click while the remote row is active does not change its occurrence.                                                                                                                                                                                                        | Ignore revocation on insert/update, widen the natural key, create a second suppression row, or permanently override the key.                                                                                      |
| A27 source and race isolation                          | Other sources remain effective. Interleaved refresh and undo revalidate actual state; an old undo never deletes a new occurrence. Inject failure in each local undo write and verify rollback and persisted audit/state.                                                                                                                                                                                                                                                                                                                                                                                                         | Delete across sources, omit the enlisted precondition, or split undo's atomic unit.                                                                                                                               |
| A28 closed complete response                           | Observe the request selecting `format=complete-v2` and require body `format="complete-v2"`. Empty/nonempty sets and canonical lowercase hyphenated UUIDs succeed. Uppercase, undashed, braced, URN or malformed UUIDs; a missing, negative or non-integer `generation`; missing/null/mistyped/unknown fields; v1/paged formats; duplicate keys; malformed keys; and count mismatch apply nothing and leave the stamp unchanged. Existing transport controls remain.                                                                                                                                                              | Omit the selector, accept another format, normalize noncanonical UUID input, restore optional/unknown fields, or skip duplicate/entry validation.                                                                 |
| A29 bounded clocks                                     | Deterministic clocks test both exact 300-second boundaries and just-outside values. Persist GET send time after committed apply. A future local stamp refuses admission; age exactly at maximum retains the existing allowed boundary.                                                                                                                                                                                                                                                                                                                                                                                           | Persist completion time, remove either skew bound, or accept a future local stamp.                                                                                                                                |
| A30 scan independence and strict ordering              | A witnessed stalled request does not block policy mail or scan progress. With a young prior stamp, external batches stay pending until a successful committed refresh started strictly after their selection. A pre-selection or equal-order refresh cannot release them.                                                                                                                                                                                                                                                                                                                                                        | Await HTTP in the scan or release using completion time, an earlier request or the cached stamp.                                                                                                                  |
| A31 idle and retry ownership                           | Idle success restores admission. Idle and demand share one in-flight request. Failure intervals follow 5, 10, 20, 40, 80, 160, 300, 300 seconds; polling cannot bypass them. Lifecycle cancellation joins the owned worker.                                                                                                                                                                                                                                                                                                                                                                                                      | Disable idle trigger, single-flight, backoff or lifecycle join.                                                                                                                                                   |
| A32 cancellation and settlement                        | Cancel before apply: no application, stamp or send. Cancel after apply admission: witness store settlement and joined worker, but release no batch. Storage/commit failure also yields no success permission. No network runs under the writer unit.                                                                                                                                                                                                                                                                                                                                                                             | Drop the task handle, treat timeout as settlement, grant success on cancelled/failed apply, or stamp before required writes commit.                                                                               |
| A33 rollout, legacy binding and final suppression read | A v2 request receiving v1 refuses. First valid v2 application binds a present legacy key to the exact carried occurrence in place; an absent legacy key retains its unbound row and is eligible only for evidence-backed undo. Restart preserves this state, the unchanged natural key and separate revocations. A newly added local suppression before SMTP still blocks sending, and restart requires a qualifying new refresh.                                                                                                                                                                                                | Accept v1 fallback, invent a legacy occurrence, erase an absent legacy row automatically, create a second row on binding, omit the final suppression read, or restore delivery permission from a persisted stamp. |
| A34 snapshot order                                     | After a snapshot at generation g lists (K,Y), an answer at a lower generation that omits Y applies nothing, leaves the stamp unchanged, and cannot satisfy Y's undo precondition, before and after restart. An equal-generation answer with different entries refuses. The highest applied generation commits in the same unit as the snapshot's writes.                                                                                                                                                                                                                                                                         | Accept a lower generation, order answers by `as_of` or arrival, accept a different set at an equal generation, or persist the generation outside the apply unit.                                                  |
| A35 content age at release                             | With a loopback producer that honors the read guarantee, an opt-out committed after a batch's selection and before its qualifying request, and one committed before selection, both appear in the releasing answer and block the batch. The answer's `as_of` may lie anywhere inside the accepted window. The producer's own conformance tests show it never serves a snapshot that predates a committed change it received the request after, and never lowers its generation across restart.                                                                                                                                   | Release on an answer from a request sent before selection, or a producer that answers from a pre-request cache or resets its generation on restart.                                                               |

Acceptance evidence includes exact source head, commands and features, pre-fix failure where applicable, post-fix results, independently applied mutation results, HTTP request and SMTP call counts, store/audit snapshots across restart, and worker settlement/join witnesses. Structural or source inspection alone does not close these issues.
