# ADR-195: Actor Trust Classes and Per-Pair Message Policy for comm

- **Status**: Proposed (2026-09-23)
- **Date**: 2026-09-23
- **Depends on**: [ADR-127](ADR-127-authenticated-actor-and-grant-primitive.md) (authenticated actor
  and grant primitive, assurance classes, action-seam obligations),
  [ADR-143](ADR-143-store-held-caller-grants.md) (store-held caller grants, subactor identity),
  [ADR-057](ADR-057-comm-actor-addressed-delivery.md) (actor-addressed delivery),
  [ADR-105](ADR-105-cross-node-comm-transport.md) (cross-node comm transport, amended 2026-09-14)
- **Amends**: ADR-057 (its "no allowlist check" clause, for sends and replies under an active policy);
  ADR-127 (states how the comm policy decision fits invariant 4)
- **Relates to**: [ADR-007](ADR-007-namespace.md) (namespace is attribution, the Gate is the
  boundary), [ADR-018](ADR-018-authorization-gate.md) (single dispatch site),
  [ADR-063](ADR-063-comm-principal-model.md) (comm principal model; its view filter is not a security
  boundary), C-ADR-033 in the hosted service's repository (pair consent between deployments)

## Context

comm is how agents on a deployment coordinate: resident agents address one another, bounded
agents report to the agent that started them, and the hosted messaging profile (ADR-105, amended
2026-09-14) extends the same verbs to agents on other people's deployments. An operator wants to
say who may message whom, and for what purpose. For example, an announcement may reach resident
agents but not the bounded agents working under them, and an internal agent may report to its
owner but must never report to an external party.

Nothing in the send path can express that today. Read at `c160b2f2`:

- **The recipient is never checked.** `comm.send` (`crates/khive-pack-comm/src/handlers.rs:301`)
  validates the label's shape and refuses self-addressing unless `self_send` is set. No check reads
  the recipient. ADR-057 records this as intended: "No allowlist check is performed" (ADR-057 line
  229). The only per-caller gate is the verb-level enrollment gate over the sender's identity,
  consulted at dispatch (`crates/khive-runtime/src/pack.rs:2889`).
- **The email allowlist is a transport check, not a send check.** The outbound email allowlist
  (`KHIVE_EMAIL_SEND_ALLOWED_RECIPIENTS`, `crates/khive-mcp/src/serve.rs:578-585`) is enforced in
  the outbox loop. A miss marks the stored message failed ("recipient ... not in outbound
  allowlist", `serve.rs:1683`) after `comm.send` has already returned success.
- **The sender's identity is claimed, not proven.** The sender is `token.actor().id`
  (`handlers.rs:324`). Through the daemon the client puts its own resolved actor on the frame. The
  daemon admits any peer with the same effective user id (`crates/khive-runtime/src/daemon.rs:492`)
  and does not authenticate which agent is speaking. ADR-127 and ADR-143 define authenticated
  actors, assurance classes and subactor identity; their core types are not yet in the source tree.
- **There is no message kind.** `SendParams` (`crates/khive-pack-comm/src/params.rs:11`) has no kind
  field and denies unknown fields, so a client sending one today is refused at deserialization.
- **There is no contact record.** The knowledge graph has `person` and `org` entities, but no
  convention for an agent's addresses, class or policy. Any caller the gate admits to graph writes
  can edit those entities.
- **The hosted service's consent carries no class or kind.** Between deployments, the hosted
  service keeps a contact grant per ordered pair of agents, with a generation that revocation
  bumps. A grant carries no class and no message kind. C-ADR-033 leaves "trust classes and per-pair
  policy" to the runtime.

(Corrected by Amendment 2. The list above is the record as read at `c160b2f2`. At
`8f2f867df06c8fe0909fd6d2c4d1bc60f220fbe6`, `comm.send` is `handle_send`
(`crates/khive-pack-comm/src/handlers.rs:119`), the sender is read at `crates/khive-pack-comm/src/handlers.rs:142`, and
the verb-level gate is consulted at `crates/khive-runtime/src/pack/dispatch.rs:724`. Email is no longer only a transport
check: `dual_write_message_with_identity` (`crates/khive-pack-comm/src/message.rs:434`) applies the effective email
policy before either message copy is written, and `prepare_email` (`crates/khive-mcp/src/serve_outbox.rs:618`) checks
queued mail independently; neither depends on this ADR's mode. The daemon's same-user check is `uid_is_permitted`
(`crates/khive-runtime/src/daemon/locks.rs:252`), which still does not distinguish agents sharing that user. Some
ADR-127 types now exist, for example `GrantAdmin` (`crates/khive-capability/src/durable/permission.rs:286`), but none
authenticates comm's claimed sender or implements D9 administration. The node channel has moved with the hosted service
(`docs/adr/ADR-105-cross-node-comm-transport.md:8`). The other points still hold.)

The mistakes this has to stop have already happened: a bounded agent mailing an address outside
the operator's organization, and a broadcast meant for resident agents reaching the agents working
under them. A policy that catches these is useful now. It cannot stop a process that deliberately
claims another agent's identity, because at this revision nothing can. This ADR is designed so the
same policy becomes an access-control boundary when authenticated identity lands, without another
redesign.

## Decision

### D1. Every decision records its assurance; the first release is a cooperative guard

Each policy decision records the assurance of the sender identity it was made on. At this revision
the only value is `claimed`: the identity the caller's configuration or frame asserted. `claimed` is
this ADR's marker for "no verification ran". It is not one of ADR-127's classes and ranks below both
of them. When ADR-127's verification exists, the runtime supplies its two classes, `DaemonBearer`
and `ActorSignature`, from the verification result. A request can never state its own assurance.

A policy rule may name a minimum assurance. A rule requiring `DaemonBearer` or `ActorSignature` never
matches a `claimed` decision, so an operator can write the stricter rules ahead of time and they
take effect as identity is authenticated. A rule meant to hold against another local agent
impersonating the sender names `ActorSignature`: ADR-127's property table gives sender authenticity
against a peer actor only to that class (ADR-127 line 377). (Superseded by Amendment 2, Decision 1.)

Until then, the policy is a cooperative guard inside the existing trust boundary, the set of
processes that can reach the daemon as the same user. It stops honest misrouting. It does not stop
a process that claims an allowed identity, and it is not evidence of who sent a message. Anything
that relies on proving the sender, including a recipient's out-of-band confirmation before acting
on a consequential request, stays in place until authenticated identity is enforced. Admitting a
message never authorizes acting on its contents.

### D2. Classes and policy live in a runtime-owned store, linked to the graph

The runtime owns three records:

- **Actor record.** The actor id, its class (D3), its parent actor if it is a subactor, its addresses
  (local label, email address, hosted address `khive1:<realm>/<agent_id>`), and an optional link to
  the `person` or `org` entity that describes its owner.
- **Pair rule.** A sender (class or actor), a recipient (class or actor), a kind (D4) or any, an
  effect (`allow` or `deny`) and an optional minimum assurance (D1).
- **Policy state.** The mode (D7) and a revision number that every decision records.

These are not properties on graph entities. Generic graph writes (create, update, merge, import,
delete) cannot change a class, an address binding or a rule. This follows the pattern already used
for transport-owned message fields, which the guarded note store refuses on generic writes
(`crates/khive-runtime/src/runtime.rs:695-712`; citation corrected by Amendment 2 to
`crates/khive-runtime/src/runtime.rs:1255`, `KhiveRuntime::notes`, and
`crates/khive-runtime/src/note_store_guard.rs:344`, `PolicyEnforcingNoteStore::upsert_note`).

The graph entity describes an owner. One owner can have several actors, and one actor several
addresses. A contact view presents one record per actor: identity, class, addresses, the linked
entity's descriptive fields, and the rules that name it. Deleting or merging the linked entity
removes or moves the link and never grants or transfers authority.

### D3. Four classes, assigned explicitly

| Class              | Meaning                                                                                |
| ------------------ | -------------------------------------------------------------------------------------- |
| `resident`         | A resident agent acting for this deployment's operator                                 |
| `bounded`          | A bounded agent working for a parent actor, with a recorded parent                     |
| `trusted_external` | An agent of another operator that this operator has chosen to trust for named purposes |
| `external`         | Any other address, including every email address unless classified otherwise           |

A class is assigned explicitly on the actor record and is never inferred from the spelling of a
label. A label with a separator in it proves neither residency nor subordination. Parentage is
recorded structurally: as ADR-143's subactor identity (a parent principal and a label) once that
exists, and as the actor record's parent field until then.

An actor with no record is `unclassified`. `unclassified` is treated as `external` wherever a rule
protects an external destination, and it matches no allow rule except an exact, reviewed legacy
route (D7). A class check always reads the runtime's own record. A remote agent's self-description
cannot claim a local class.

`trusted_external` is still external for any rule that prohibits external destinations. Trust
widens what a pair may exchange; it does not move an actor inside the operator's organization.

### D4. Message kind is optional, declared intent

`comm.send` gains an optional `kind` field with values `announce`, `report` and `ask`. Omitting it
records `unspecified`. `reply` is not a value a caller can send: it is derived only from `comm.reply`
acting on a verified parent message, and a reply is evaluated as a message in its own right, never
as inheriting permission from the thread. (Superseded by Amendment 2, Decision 5, item 1.) Unknown kinds are refused.

A kind is the sender's declaration of purpose, not a classification of the content. A sender can
label a report as a question. For that reason every prohibition that must hold absolutely, such as
"never to an external recipient", is written as a rule over any kind, so relabeling cannot bypass
it. Once a message travels between deployments, its kind is bound to the authenticated envelope
bytes: that prevents alteration in transit, not dishonesty at the source.

### D5. Evaluation: default deny, deny overrides

A decision takes the sender, the canonical recipient (resolved to an actor record), the kind and
the sender's assurance, and evaluates every matching rule:

1. Any matching `deny` refuses, whatever else matches.
2. Otherwise any matching `allow` whose minimum assurance the decision meets admits.
3. Otherwise the message is refused (default deny).

Because deny overrides, an actor-specific allow cannot punch through a class-level deny. An
operator who needs an exception to a class rule narrows the class rule instead. Each decision
records the policy revision, the rules that matched, the outcome, the assurance and a stable reason
code.

### D6. Where the decision is made

**At send and reply, before anything is written.** After the recipient is resolved to its canonical
actor, and before either copy of the message is written, the runtime evaluates the pair. For
`comm.send` the recipient is the request's `to`. For `comm.reply` it is the parent message's other
party (`handlers.rs:1521`; citation corrected by Amendment 2 to `crates/khive-pack-comm/src/handlers/reply.rs:249`,
`handle_reply`), which is known only after the parent message is read. A refusal
returns a named error with a stable reason code (`policy_denied`, or `policy_unavailable` when the
policy store cannot be read) and writes no message copy. The decision is audited either way.

**How this fits ADR-127 invariant 4.** ADR-127 keeps "Handlers never authorize. Pack handlers must
not perform authorization; the dispatch site is the sole enforcement point" (ADR-127 line 640). It
also defines a second, bounded seam: "The action seam discharges an obligation the Gate issued; it
is not a second ambient identity resolver" (ADR-127 lines 198-208). The comm policy uses that seam.
At dispatch, the Gate issues a comm-policy obligation for `comm.send` and `comm.reply`. The comm
action seam discharges it once the recipient is resolved, by asking the runtime's policy evaluator,
before it writes. The handler supplies the action descriptor (sender, canonical recipient, kind)
and cannot proceed without a discharged obligation. It holds no rule logic of its own. This amends
ADR-127 to name the comm-policy obligation beside the action-seam obligations it already allows.

**At every transport attempt.** A message that was admitted may leave through a transport later:
the email outbox, the Telegram outbox, or the node channel to another deployment. Every attempt
re-evaluates the pair against the current policy. A refusal at that point holds the message with
reason `policy_denied`: it is neither delivered nor failed, and it is visible to the sender. (Superseded by Amendment 2,
Decision 5, item 2.) A change
of policy revision re-queues every message held with `policy_denied` for one re-evaluation; nothing
else retries a held message. (Superseded by Amendment 2, Decision 5, item 3.) A message already handed to a transport
cannot be recalled.

**The email allowlist stays.** The outbox allowlist remains an independent transport restriction.
When the policy is in `enforce` mode, `comm.send` also checks the effective allowlist for an email
recipient and refuses at send time with the same named error. (Superseded by Amendment 2, Decision 3.) The outbox
check still runs, so a
change in either one never widens delivery.

### D7. Rollout: off, shadow, enforce

The policy mode is one of:

- `off`: no evaluation. Today's behaviour, and the default for an existing deployment at upgrade. (Corrected by
  Amendment 2, Decision 3: `off` skips pair-policy evaluation only; fresh email admission and the outbox check still
  apply.)
- `shadow`: every send, reply and transport attempt is evaluated and the would-be refusals are
  audited, but messages are delivered as today. (Corrected by Amendment 2, Decision 3: in `shadow` pair policy blocks
  nothing, and email admission still can.) Shadow evaluates rules only. It never bypasses the
  email allowlist or the hosted service's consent.
- `enforce`: refusals take effect.

The first rules come from a census of the (sender, recipient) pairs a deployment has actually used.
The census proposes rules; it does not approve them. Each proposed rule is reviewed against the
intended classes before it is written, and a pair nobody can justify becomes a deny, not an allow.
The operator records the cutover criterion, for example a shadow window with no unreviewed would-be
refusal, before switching to `enforce`. Exceptions kept for existing callers (legacy routes) carry
an expiry. Moving from `enforce` back to `shadow` never widens a rule. (Superseded by Amendment 2, Decision 5, item 4.)

Rolling out the `kind` field is server-first: because `SendParams` denies unknown fields, a server
must accept `kind` before any client sends it. A client that never sends it keeps working, and its
messages are `unspecified`.

### D8. Between deployments: one vocabulary, two independent gates

(The node-channel obligations in this section move to the hosted service with the channel: Amendment 2, Decision 4.)

A recipient on another deployment is reached through the node channel and the hosted service. Its
hosted address is bound to a local actor record by the operator, and that record carries its class.
The local policy evaluates such a send exactly as D6 does, at send time and at every submission. The
hosted service's pair consent remains a separate, mandatory gate on every submission. Neither
overrides the other: a message goes only when both allow it.

On the receiving side, the runtime evaluates the pair (remote sender's local class and assurance
from the verified transport, local recipient, kind) inside verified-recipient ingest, before
ordinary ingestion. A refusal stores the message as quarantined and never drops it silently, so the
signed receipt tells the sender `recipient_quarantined`. (Superseded by Amendment 2, Decision 4 and Decision 5, item
6.) ADR-127's separate quarantine and
recipient-signal obligations for authentication failures are unchanged.

The hosted service's delivery contract (C-ADR-033 D5) is amended in its own repository to match
these three points: a send refused by local policy writes no local message and no transport record,
a transport record may be held for policy between submissions, and a recipient-side policy refusal
yields the `quarantined` disposition.

### D9. Administration

Only an operator-authorized administrator, scoped to the rules and actors it may change, can create
or change an actor record, a class, an address binding, a rule or the mode. Owning a contact entity
or holding graph-write access is not enough. Each change and its audit record commit in one
transaction, recording what changed from what. A subactor cannot administer, following ADR-143's
rule that subactors are never grantors. (Qualified by Amendment 2, Decision 2: the refusal is cooperative until
authenticated administration exists.)

Until ADR-127's authenticated administration exists, administration is a trusted-host operation:
anyone who can act as the operator's user on the host can change the policy. The documentation
states this plainly.

## Amendments

**ADR-057.** Its rule "No allowlist check is performed" (line 229) holds while the policy mode is
`off`. (Superseded for email by Amendment 2, Decision 3.) In `shadow` and `enforce` modes, `comm.send` and `comm.reply`
evaluate the pair policy (D5,
D6) before either copy is written. Both copies still stay in the caller's namespace, and addressing
is unchanged.

**ADR-127.** Invariant 4 (line 640) is read together with the action-seam obligations (lines
198-208): the comm-policy decision is an obligation the Gate issues at dispatch and the comm action
seam discharges after resolving the recipient. The evaluator belongs to the runtime. Handlers
supply the descriptor and never decide.

## Consequences

- An operator can express who may message whom, per purpose, and see every refusal named at the
  sender instead of discovering a failed delivery later.
- Email refusals under `enforce` move from "accepted, then failed in the outbox" to "refused at
  send". Callers that relied on `comm.send` always succeeding see a new error, and only after the
  operator switches to `enforce`. (Superseded by Amendment 2, Decision 3: fresh email admission already refuses at
  send in every mode; `enforce` adds pair-policy refusals.)
- Every message path gains one evaluation at send and one per transport attempt. (Superseded by Amendment 2, Decision
  5, item 5.) The evaluation
  reads a small runtime table. It never reads message content.
- Until authenticated identity is enforced, the policy stops mistakes, not impersonation. The
  documentation and every decision's recorded assurance say so.
- Temporary legacy routes can outlive their purpose. The expiry on each and the census review are
  the counterweight. A route with no expiry is refused at write time.

## Implementation fences

- MAY reuse the Gate's dispatch path, the grant architecture of ADR-127 and ADR-143, and the
  guarded-store pattern for protected fields.
- MAY NOT infer a class from a label, let a request state its assurance, let graph edits change
  classes, bindings or rules, add a handler-local access list, treat admission as authorization to
  act on content, or retire any out-of-band sender confirmation before authenticated identity is
  enforced.
- Kind travels inside the encrypted envelope between deployments. The hosted service never sees it.

## Acceptance

Each arm names the control that must fail:

1. A forged sender, kind or administrator is refused where a rule says so, and every decision
   records `claimed`. (Narrowed by Amendment 2, Decision 1.) Control: a request that states its own assurance is
   rejected.
2. Generic graph create, update, merge, import and delete cannot change a class, binding or rule.
   Control: each of the five against a protected field.
3. `comm.send` and `comm.reply` both refuse before either copy is written. Control: the message
   store has no new row after a refusal.
4. Deny overrides: an actor allow under a class deny is refused.
5. A parent's rule change applies to a bounded agent's queued message at its next transport attempt: the
   message is held with `policy_denied`, and no retry runs while held. (Superseded by Amendment 2, Decision 5, item 3.)
6. Under `enforce`, an email recipient outside the allowlist is refused at send, and the outbox
   check still refuses a message admitted before an allowlist change. (Superseded by Amendment 2, Decision 3.)
7. The hosted service's refusal is independent: local allow plus hosted denial means no delivery. (Moved to the hosted
   service by Amendment 2, Decision 4.)
8. A recipient-side refusal yields `recipient_quarantined` at the sender. (Moved to the hosted service by Amendment 2,
   Decision 4.)
9. `shadow` delivers and audits would-be refusals, and never bypasses the email allowlist or hosted
   consent. (The hosted-consent portion moves to the hosted service: Amendment 2, Decision 4.)
10. A decision records the policy revision and matched rules, and replaying an accepted
    idempotent send returns the original acceptance with no new writes.

## Alternatives considered

- **Wait for authenticated identity before any policy (A2).** Cleaner, with one assurance mode.
  Rejected because the misrouting it would leave unaddressed has already happened, and D1's recorded
  assurance makes the later tightening a rule change instead of a redesign.
- **Class and policy as properties on the `person` or `org` entity (one physical row).** This reads
  as "the contact record is the actor record" most literally. Rejected because it puts authority on
  records that every generic graph write path can reach. Each of those paths would need the same
  protected-field guard and audit, and one owner with several agents would need several classes on
  one entity. D2's contact view gives one record at the surface that is read, with authority kept in
  the runtime.
- **Class and policy in operator configuration.** Rejected: it changes only on reload, anyone who
  can edit the file can promote an actor, and it keeps no attributable history.
- **Required `kind` on every send.** Rejected: every existing caller would break at once. With D4,
  an omitted kind is `unspecified`, which D5 admits only where an allow rule matches it: a rule for
  any kind, or a reviewed legacy route. Everything else stays under default deny.
- **Replacing the hosted pair consent with the local table.** Rejected: the consent is the other
  owner's decision, and the local table cannot speak for them.
- **Most-specific rule wins.** Rejected: an absolute prohibition could then be overridden by any
  narrower allow.

## Amendment: held-message state, unreadable policy, receive-side errors, wire kind and recorded assurance (2026-09-24)

This amendment aligns D1, D4, D6 and D8 with the hosted service's delivery contract (C-ADR-033 D5, amended in its
own repository to say the same). It replaces one sentence of D6 and adds rules; everything not named here is
unchanged.

1. **A hold records the policy state it was refused under (D6).** A message refused at a transport attempt records
   the mode (D7) and the revision it was evaluated under. It is evaluated again, once, whenever the current policy
   state differs from the recorded one, whether the change was published before or after the hold was written. A
   mode change is a state change. Under `off` or `shadow` a held message is released to its transport; under
   `shadow` the release is audited as a would-be refusal when the current rules still refuse it. A message refused
   again records the new state. This replaces "A change of policy revision re-queues every message held with
   `policy_denied` for one re-evaluation; nothing else retries a held message."
2. **An unreadable policy store makes no attempt (D6).** When the policy state or rules cannot be read at a transport
   attempt, no attempt is made and the message stays pending under the transport's own retry backoff. It is not
   held, because a store that recovers publishes no new state and the hold would never be evaluated again, and it
   is not sent unevaluated, because that fails open.
3. **A resubmission after admission is an attempt (D6).** (Moved to the hosted service by Amendment 2, Decision 4.) On
   the node channel, a message the hosted service admitted
   is resubmitted under ADR-105 Appendix A.8 (600 seconds after `admitted_at` without a verified receipt). That
   resubmission is a transport attempt and is evaluated. A refusal stops the resubmission and recalls nothing: the
   earlier admission may still deliver, a verified receipt for it ends the hold, and the message stays shown as
   possibly charged. This is the case "A message already handed to a transport cannot be recalled" covers.
4. **Receive side (D8).** (Moved to the hosted service by Amendment 2, Decision 4.)
   (a) When the recipient's policy store answers with an error, nothing is committed and no receipt is sent, as for a
   failed write, and the runtime reports it where it reports a failed write. An actor with no record is
   `unclassified` (D3), which is a decision, not an error. A store holding no policy state is in mode `off`.
   (b) A delivery of a message already committed is answered from its replay identity and is not evaluated again,
   so a later policy change never turns a stored message into a quarantined one.
   (c) Under `shadow` a receive-side refusal is audited and the message is stored.
5. **Kind on the wire (D4).** A sending deployment writes `kind` on the wire only as `announce`, `report` or `ask`,
   and otherwise omits it; it never writes `unspecified`, `reply` or null. The receiving deployment records an
   omitted kind as `unspecified`. A message whose `in_reply_to` names a verified parent is evaluated as `reply`
   whatever kind it declares. A verified parent is a message committed in the recipient's store that the current
   recipient sent to the current sender. A follow-up to a message the sender itself sent is not a reply for policy
   and is evaluated by its declared kind. Local `comm.reply` applies the same rule when it evaluates the pair: the
   verb still accepts a parent addressed to or from the caller, and a reply to the caller's own message is
   evaluated by its declared kind.

6. **What advances the revision (D2, D9).** The revision advances on every D9 change, a removal included: an actor
   record, a class, an address binding, a rule or the mode. A decision reads all of them (D5), so a change to any of
   them is a change of policy state for item 1, and correcting a class releases the messages it held.
7. **Receive-side keeping and claim (D8).** (Moved to the hosted service by Amendment 2, Decision 4.)
   (a) A policy-refused delivery is kept with its parsed plaintext under a bound per sender, separate from the bound
   for undecryptable or held deliveries, so one sender's refusals never evict another sender's items.
   (b) The replay identity is claimed inside the commit transaction, on one identity shared by the message note and
   the quarantine record. Two concurrent arrivals of one message, with the policy changing between their
   evaluations, commit once, and both acknowledgement entries carry that one disposition.
8. **Assurance where no request is present (D1, D6, D8).**
   (a) A message records the assurance of its sender identity when it is sent, in every mode. Every transport
   attempt, and every evaluation under item 1, evaluates that recorded value and never derives one from the
   context of the transport, so a message sent `claimed` is never admitted later by a rule that requires
   `DaemonBearer` or `ActorSignature`.
   (b) (Moved to the hosted service by Amendment 2, Decision 4.) On receipt, an envelope opened under ADR-105 Appendix
   A.5 is evaluated at `claimed`. It proves possession
   of the sender device key pinned for that contact, which is neither of ADR-127's classes, and HPKE Auth mode
   does not resist key-compromise impersonation. A rule requiring `DaemonBearer` or `ActorSignature` therefore
   never admits a delivery from another deployment until a later decision names an assurance for one.

Acceptance arms added (each names the control that must fail):

11. A hold written after a revision was published is evaluated under that revision. Control: a re-queue keyed on
    the publish event leaves it held.
12. Moving from `enforce` to `off` or `shadow` releases held messages. Control: a revision-only trigger leaves them
    held.
13. With the policy store unreadable at an attempt, no attempt is made, no hold is written and the message stays
    pending. Control: an attempt made without a decision.
14. A recipient-side policy store error commits nothing and sends no receipt. Control: a quarantine written on the
    error.
15. A redelivery of a committed message returns the original receipt with no evaluation, after a policy change
    between the two deliveries. Control: an evaluation that turns stored into quarantined.
16. No message on the wire carries `kind` as `unspecified`, `reply` or null. Control: a sender that serializes the
    local kind value unchanged.
17. Under a pair rule that allows `reply` and refuses `ask`, a reply to a message the recipient sent is admitted and
    a follow-up threaded on the sender's own admitted message is refused, locally and on receipt. Control: a
    self-threaded follow-up evaluated as `reply`.
18. A class changed and changed back, with no rule edited, advances the revision twice and releases a message held
    under the first change. Deleting a deny rule advances the revision and releases a message it held. Controls: a
    revision that advances only on rule changes; one that advances only on a create or a change.
19. Two concurrent arrivals of one message, with a policy change between their evaluations, commit one record and
    both acknowledgements carry its disposition; a flood of refused deliveries from one sender evicts none of
    another sender's kept items. Controls: a claim taken outside the commit transaction; one shared bound.
20. With a test assurance source that reports a stronger assurance at the attempt than at send, a message sent
    `claimed` and held under a rule requiring `ActorSignature` stays held at the next attempt; a message sent while
    the mode is `off` carries its recorded assurance to an attempt under `enforce`. Control: an attempt that takes
    its assurance from the transport's context.
21. A delivery from another deployment, under a pair rule that allows it only at `DaemonBearer` or above, is
    quarantined, and its decision records `claimed`. Control: a receive path that maps envelope authentication to
    `ActorSignature`.

## Amendment 2 (2026-10-23) -- Assurance limits, email admission and policy ownership

**Status**: Accepted 2026-10-23. It does not change the status of the base ADR or of earlier amendments.

**Addresses**: [#5598](https://github.com/ohdearquant/khive/issues/5598).

**Trigger.** At `8f2f867df06c8fe0909fd6d2c4d1bc60f220fbe6`, sender transport records have assurance and policy-state
fields, but `PolicyMode` explicitly does not enable runtime policy
(`crates/khive-db/src/stores/note/transport/mod.rs:120`). Email admission already checks the effective policy
(`crates/khive-pack-comm/src/message.rs:434`), and the node client has moved with the hosted service
(`docs/adr/ADR-105-cross-node-comm-transport.md:8`). The earlier text conflates these implemented mechanisms with
proposed pair-policy enforcement and overstates the local identity guarantee. This amendment defines their boundaries
and reconciles the base decision with the 2026-09-24 amendment, identified here as Amendment 1.

### Decision 1 -- Claimed assurance does not prevent local impersonation

D1 specifies only a cooperative policy with `claimed` sender assurance. A stronger minimum assurance cannot match a
`claimed` sender. `ActorSignature` is not delivered as protection against peer-actor impersonation in the local profile:
ADR-127 states both that the current symmetric kernel cannot express it and that shared local key custody defeats that
property (`docs/adr/ADR-127-authenticated-actor-and-grant-primitive.md:182`,
`docs/adr/ADR-127-authenticated-actor-and-grant-primitive.md:313`,
`docs/adr/ADR-127-authenticated-actor-and-grant-primitive.md:377`). Naming a stronger assurance in a rule does not
supply verification or separate custody. D1's former local impersonation promise is superseded.

The runtime currently overwrites sender-envelope assurance with `Claimed` on creation and confirmed re-encryption
(`crates/khive-runtime/src/comm_transport.rs:66`, `crates/khive-runtime/src/comm_transport.rs:82`). The enum's stronger
variants describe recorded values, not available comm authentication. Amendment 1 item 8(a) still requires assurance
captured at send in every mode and preserved at subsequent evaluations. The message writer does not yet capture that
field on every ordinary send (`crates/khive-pack-comm/src/message.rs:538`). Base acceptance arm 1 therefore tests
rejection of caller-supplied assurance and minimum-assurance mismatch, not resistance to a process claiming an allowed
local actor. This preserves the useful misrouting guard without claiming an identity boundary it cannot provide.

### Decision 2 -- Subactor administration refusal is cooperative

D9's refusal remains cooperative until ADR-127 authenticated administration and ADR-143 structural subactor identity are
available on the comm administration path. In that phase the check resolves the claimed caller's D2 actor record and
refuses administration when its parent field identifies a subactor, as D3 already records parentage; it does not infer a
parent from a label separator. This check cannot prevent a process from claiming a different actor or directly changing
data as the same host user. D9's trusted-host qualification therefore applies to its subactor sentence too.

The authenticated form uses the structural principal's subactor field, as specified in
`docs/adr/ADR-143-store-held-caller-grants.md:203` and `docs/adr/ADR-143-store-held-caller-grants.md:218`. The authority
service already refuses a caller whose `principal.leg` is present
(`crates/khive-capability/src/authority_store/current.rs:58`); this is evidence for that service's check, not evidence
that comm administration uses it. The daemon's same-user check remains `peer == daemon_euid`
(`crates/khive-runtime/src/daemon/locks.rs:252`). Keeping those boundaries explicit avoids turning cooperative parentage
into an unsupported authentication claim.

### Decision 3 -- Email admission is independent of pair-policy mode

The effective email policy applies to fresh `comm.send` and `comm.reply` before message creation in every pair-policy
mode. It consists of ADR-122 Amendment 4's boot-resolved recipient policy and ADR-200's external-sender conditions
(`docs/adr/ADR-122-email-outbound-delivery.md:294`, `docs/adr/ADR-200-external-email-policy.md:61`). Both verbs call
`dual_write_message_with_identity` (`crates/khive-pack-comm/src/handlers.rs:196`,
`crates/khive-pack-comm/src/handlers/reply.rs:285`). That writer checks recipient membership, external-sender
membership, suppression, cap, footer and configured mirror freshness (`crates/khive-pack-comm/src/message.rs:434`). A
recipient outside the configured set is not automatically refused if ADR-200 admits it as external mail.
`KHIVE_EMAIL_EXTERNAL_SENDERS` is a separate actor-id set, not an assignment of D3 trust class
(`docs/adr/ADR-200-external-email-policy.md:77`). Likewise, inbound `origin = external` describes message provenance,
not a D2 actor record (`crates/khive-pack-comm/src/handlers/ingest.rs:444`).

| Pair-policy mode | Pair-policy effect on fresh send/reply                       | Effective email admission |
| ---------------- | ------------------------------------------------------------ | ------------------------- |
| `off`            | No pair evaluation.                                          | Still applies.            |
| `shadow`         | Evaluate and audit; no pair-policy refusal blocks the write. | Still applies.            |
| `enforce`        | Refuse a denied pair before either message copy.             | Still applies.            |

The same separation applies at transport attempts. `off` and `shadow` do not bypass email delivery restrictions;
`enforce` adds pair-policy refusal. The outbox retains its independent check
(`crates/khive-mcp/src/serve_outbox.rs:618`). An absent email policy retains ADR-122's embedded admission semantics:
`OutboundEmailPolicy::allows` returns true when no recipient set exists
(`crates/khive-runtime/src/outbound_email_policy.rs:208`), while email delivery requires a configured policy
(`crates/khive-mcp/src/serve_outbox.rs:300`). Changing pair-policy mode neither configures nor disables that policy.

A valid committed keyed replay returns its original result before fresh email admission, with no new message writes
(`crates/khive-pack-comm/src/message.rs:405`, `docs/adr/ADR-200-external-email-policy.md:72`). The existing validation
and replay requirements remain in force. The earlier enforce-only email sentence, the email interpretation of ADR-057's
no-allowlist clause, and base acceptance arm 6 are superseded by these mode-independent rules.

ADR-105 Appendix A.8's present-tense ADR-195 obligations are conditional on acceptance of ADR-195 and its applicable
amendments (`docs/adr/ADR-105-cross-node-comm-transport.md:857`). ADR-105's accepted status does not itself accept this
proposed pair policy. Its dated 2026-10-05 relocation note defines where the node channel now belongs
(`docs/adr/ADR-105-cross-node-comm-transport.md:8`); Decision 4 applies that boundary here. Independent accepted email
admission remains effective while ADR-195 is proposed.

### Decision 4 -- Node-channel obligations follow the hosted service

D8's node-channel obligations, Amendment 1 item 3 and item 8(b) belong to the hosted service with `khive-channel-node`.
The move is recorded by #4131, commit `f089845ad`, and `docs/adr/ADR-105-cross-node-comm-transport.md:8`. These
obligations are removed from this repository's acceptance for #5598. This transfers responsibility; it does not assert
that the hosted implementation satisfies the contract.

The following mapping also covers dependent receive-side and wire clauses so that a transferred D8 obligation is not
reintroduced as an acceptance arm. Local send/reply, local policy-state changes and retained transport persistence stay
in scope. Existing runtime recipient storage remains in the repository, including atomic replay and acknowledgement
records (`crates/khive-db/src/stores/note/recipient/mod.rs:657`), but storage alone is not a working node channel.

| Earlier clause                     | Scope after this amendment                                           |
| ---------------------------------- | -------------------------------------------------------------------- |
| D8; base arms 7 and 8              | Hosted service.                                                      |
| Amendment 1 item 2; arm 13         | Retained transport-attempt behavior.                                 |
| Amendment 1 item 3                 | Hosted resubmission, admission and receipt behavior.                 |
| Amendment 1 item 4; arms 14 and 15 | Hosted receive errors, replay and shadow behavior.                   |
| Amendment 1 item 5; arms 16 and 17 | Local kind/parent semantics retained; wire and receipt cases hosted. |
| Amendment 1 item 6; arm 18         | Retained policy revision behavior.                                   |
| Amendment 1 item 7; arm 19         | Hosted receive retention and concurrent-arrival behavior.            |
| Amendment 1 item 8(a); arm 20      | Retained send-time assurance and later evaluation behavior.          |
| Amendment 1 item 8(b); arm 21      | Hosted receipt assurance.                                            |
| Arms 11 and 12                     | Retained state-change re-evaluation and release behavior.            |

The hosted-consent portions of base arm 9 and D6's node-submission wording follow the same transfer. This repository's
acceptance must not require a hosted endpoint, a node wire encoder or a node receipt loop.

### Decision 5 -- Reconcile the base rules with Amendment 1

The following sentences supersede the corresponding earlier wording; they do not change Amendment 1's intended mode,
state or parent rules.

1. **Kind (D4).** A verified parent sent by the current recipient to the current sender derives `reply` locally or on
   receipt, while a follow-up to the sender's own message uses its declared kind and never inherits thread permission.
2. **Attempt refusal (D6).** Only `enforce` turns a pair-policy refusal at a transport attempt into a `policy_denied`
   hold; `shadow` audits and permits the attempt subject to independent restrictions, and `off` skips rule evaluation.
3. **Held retry (base arm 5).** A policy hold suspends ordinary retry, but a difference in recorded mode or revision
   triggers one reconsideration under Amendment 1 item 1, including when the change precedes the hold write.
4. **Mode rollback (D7).** Moving from `enforce` to `off` or `shadow` leaves stored rules unchanged but can widen
   delivery by releasing policy holds, subject to independent restrictions.
5. **Evaluation coverage (Consequences).** Pair evaluation covers fresh send/reply, transport attempts and changed-state
   holds in evaluating modes, plus fresh hosted receipt; `off` releases policy holds without evaluating rules.
6. **Receive refusal (D8).** An enforced receive-side refusal quarantines, while `shadow` audits a would-be refusal and
   stores ordinary delivery, subject to the unchanged independent authentication requirements.

These distinctions are observable in the existing boundaries: `handle_reply` permits either participant and selects the
other party (`crates/khive-pack-comm/src/handlers/reply.rs:136`, `crates/khive-pack-comm/src/handlers/reply.rs:249`);
recipient storage derives reply kind from a verified outbound parent
(`crates/khive-db/src/stores/note/recipient/mod.rs:693`,
`crates/khive-db/sql/comm-recipient-live-outbound-parent-exists.sql:1`). Sender storage preserves mode and revision on a
hold (`crates/khive-db/src/stores/note/transport/mod.rs:631`, `crates/khive-db/sql/comm-sender-hold-update.sql:1`), and
its pending query excludes held rows (`crates/khive-db/src/stores/note/transport/mod.rs:324`). These mechanisms support
the contract but do not supply its missing pair evaluator or changed-state scheduling. ADR-105 records the same mode and
state distinctions at `docs/adr/ADR-105-cross-node-comm-transport.md:857` and
`docs/adr/ADR-105-cross-node-comm-transport.md:878`, subject to Decision 3's acceptance dependency and Decision 4's
scope.

### Decision 6 -- Store ownership and the #5598 acceptance contract

The D2 policy store is owned by `khive-runtime`: actor records, parentage, classes, address bindings, pair rules and
policy state are available through its policy boundary. The evaluator belongs to that runtime boundary as D6 already
requires. `khive-pack-comm` supplies the resolved action descriptor and must not own an independent rule table. Generic
graph writes do not administer policy. The existing protected-note pattern is a reference for that separation
(`crates/khive-runtime/src/runtime.rs:1255`, `crates/khive-runtime/src/note_store_guard.rs:344`). Existing transport
persistence uses `khive-db` through the runtime (`crates/khive-runtime/src/comm_transport.rs:29`); migration 041 stores
transport outcomes, not D2 actor records or pair rules (`crates/khive-db/sql/041-sender-transport.sql:1`). This
amendment does not select a new table layout, persistence interface or migration number.

#5598's replacement scope must name that store owner, Decision 2's cooperative parent-field refusal and the complete
Amendment 1 items 2-8 and arms 11-21 mapping in Decision 4. Revision handling is state comparison, not a
revision-publish notification alone. No dedicated census verb is required by #5598; D7's census and explicit approval of
proposed routes remain rollout requirements, with no new public verb specified here. This keeps the required behavior
explicit without inventing an interface absent from D7.

### Acceptance

The following are required controls. Existing source references identify mechanisms or assertions, not a claim that
these controls have passed. Named `required` controls are acceptance identifiers to implement, not existing test names.
Hosted cases in Decision 4 are excluded from this repository's executable acceptance.

| ID  | Statement                                                                                                    | Evidence                                                                        |
| --- | ------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| A1  | Runtime sender envelopes record `claimed`; no local impersonation guarantee is asserted.                     | `verified_receipt_uses_bound_backend_and_token_attribution` (E1); Decision 1.   |
| A2  | A caller cannot assert assurance; stronger minimum rules reject `claimed`.                                   | Required `assurance_claim_refused`; E2.                                         |
| A3  | A caller with recorded parentage cannot administer; same-user spoofing is outside this check.                | Required `subactor_admin_refused`; E3.                                          |
| A4  | Graph create/update/merge/import/delete cannot change D2 authority.                                          | Required `policy_graph_writes_refused`; E4.                                     |
| A5  | Send and reply enforce deny overrides before either copy; shadow only audits; off skips pair evaluation.     | Required `pair_modes`; E5.                                                      |
| A6  | All modes retain fresh email admission and independent outbox restrictions.                                  | Required `email_pair_modes`; E6.                                                |
| A7  | Valid committed email replay keeps the original result with no new writes.                                   | `committed_email_send_and_reply_replay_after_revocation_without_mutation` (E7). |
| A8  | A late-written hold is reconsidered once when stored mode or revision differs.                               | Required `late_hold_state_change`; E8.                                          |
| A9  | Enforce-to-off/shadow releases policy holds without bypassing email restrictions.                            | Required `policy_mode_release`; E8.                                             |
| A10 | Unreadable policy at an attempt sends nothing, writes no policy hold and stays pending on backoff.           | Required `policy_unavailable_attempt`; E8.                                      |
| A11 | Local reply to a verified incoming parent derives `reply`; a self-threaded follow-up does not.               | Required `local_reply_kind`; E5.                                                |
| A12 | Every D9 mutation, removal included, advances revision; class changes twice advance it twice.                | Required `policy_revision_mutations`; E4.                                       |
| A13 | Send records assurance even in off mode; later attempts cannot upgrade it from transport context.            | Required `send_assurance_preserved`; E1, E2.                                    |
| A14 | Node-specific items and arms are assigned as in Decision 4 and omitted from local execution.                 | `docs/adr/ADR-105-cross-node-comm-transport.md:8`; Decision 4's table.          |
| A15 | #5598 names runtime ownership and the subactor check, maps items 2-8/arms 11-21 and requires no census verb. | `ADR-195-comm-actor-trust-classes.md`, Decision 6; #5598 scope.                 |
| A16 | ADR-105's ADR-195 obligations require ADR-195 acceptance.                                                    | `docs/adr/ADR-105-cross-node-comm-transport.md:857`; Decision 3.                |

Evidence locations:

- **E1:** `crates/khive-runtime/src/comm_transport.rs:225`, `verified_receipt_uses_bound_backend_and_token_attribution`,
  asserts normalized assurance at lines 236-246.
- **E2:** `crates/khive-pack-comm/src/params.rs:10` and `crates/khive-pack-comm/src/message.rs:538` are the send input
  and stored-message construction seams; `crates/khive-db/src/stores/note/transport/tests.rs:650`,
  `sender_assurance_round_trips_and_exact_retry_compares_it`, covers the persisted envelope only.
- **E3:** `docs/adr/ADR-143-store-held-caller-grants.md:203` specifies the structural restriction;
  `crates/khive-capability/src/authority_store/current.rs:58` supplies the authority-service check. The required comm
  control must exercise the D2 parent field through D9 administration.
- **E4:** `crates/khive-runtime/src/runtime.rs:1255` and `crates/khive-runtime/src/note_store_guard.rs:344` identify the
  existing guarded-accessor pattern; the required controls must exercise the separate D2 store, not note guards alone.
- **E5:** `crates/khive-pack-comm/src/handlers.rs:119` and `crates/khive-pack-comm/src/handlers/reply.rs:51` identify
  the local send/reply paths.
- **E6:** `crates/khive-pack-comm/src/email_admission_tests.rs:151`,
  `email_fresh_send_refuses_before_any_note_key_index_attachment_or_wake`, and
  `crates/khive-pack-comm/src/email_admission_tests.rs:183`,
  `email_fresh_reply_refusal_preserves_parent_and_validation_precedence`, assert refusal without mutation;
  `crates/khive-pack-comm/src/email_admission_tests.rs:719`,
  `external_sender_scope_keeps_policy_recipients_available_to_other_actors`, covers external-sender scope.
  `crates/khive-mcp/src/serve_outbox.rs:618` identifies the independent delivery check.
- **E7:** `crates/khive-pack-comm/src/email_admission_tests.rs:378`,
  `committed_email_send_and_reply_replay_after_revocation_without_mutation`, asserts unchanged snapshots at line 424.
- **E8:** `crates/khive-db/sql/041-sender-transport.sql:33`, `crates/khive-db/sql/comm-sender-hold-update.sql:1`, and
  `crates/khive-db/src/stores/note/transport/mod.rs:324` identify state persistence and the ordinary pending query; the
  required controls must additionally exercise runtime scheduling and failure handling.
- **E9:** `docs/adr/ADR-105-cross-node-comm-transport.md:8` and #4131 record the node-client transfer.
- **E10:** `docs/adr/ADR-105-cross-node-comm-transport.md:857` is the conditional policy-hold obligation.
