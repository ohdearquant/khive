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
against a peer actor only to that class (ADR-127 line 377).

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
(`crates/khive-runtime/src/runtime.rs:695-712`).

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
as inheriting permission from the thread. Unknown kinds are refused.

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
party (`handlers.rs:1521`), which is known only after the parent message is read. A refusal
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
reason `policy_denied`: it is neither delivered nor failed, and it is visible to the sender. A change
of policy revision re-queues every message held with `policy_denied` for one re-evaluation; nothing
else retries a held message. A message already handed to a transport cannot be recalled.

**The email allowlist stays.** The outbox allowlist remains an independent transport restriction.
When the policy is in `enforce` mode, `comm.send` also checks the effective allowlist for an email
recipient and refuses at send time with the same named error. The outbox check still runs, so a
change in either one never widens delivery.

### D7. Rollout: off, shadow, enforce

The policy mode is one of:

- `off`: no evaluation. Today's behaviour, and the default for an existing deployment at upgrade.
- `shadow`: every send, reply and transport attempt is evaluated and the would-be refusals are
  audited, but messages are delivered as today. Shadow evaluates rules only. It never bypasses the
  email allowlist or the hosted service's consent.
- `enforce`: refusals take effect.

The first rules come from a census of the (sender, recipient) pairs a deployment has actually used.
The census proposes rules; it does not approve them. Each proposed rule is reviewed against the
intended classes before it is written, and a pair nobody can justify becomes a deny, not an allow.
The operator records the cutover criterion, for example a shadow window with no unreviewed would-be
refusal, before switching to `enforce`. Exceptions kept for existing callers (legacy routes) carry
an expiry. Moving from `enforce` back to `shadow` never widens a rule.

Rolling out the `kind` field is server-first: because `SendParams` denies unknown fields, a server
must accept `kind` before any client sends it. A client that never sends it keeps working, and its
messages are `unspecified`.

### D8. Between deployments: one vocabulary, two independent gates

A recipient on another deployment is reached through the node channel and the hosted service. Its
hosted address is bound to a local actor record by the operator, and that record carries its class.
The local policy evaluates such a send exactly as D6 does, at send time and at every submission. The
hosted service's pair consent remains a separate, mandatory gate on every submission. Neither
overrides the other: a message goes only when both allow it.

On the receiving side, the runtime evaluates the pair (remote sender's local class and assurance
from the verified transport, local recipient, kind) inside verified-recipient ingest, before
ordinary ingestion. A refusal stores the message as quarantined and never drops it silently, so the
signed receipt tells the sender `recipient_quarantined`. ADR-127's separate quarantine and
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
rule that subactors are never grantors.

Until ADR-127's authenticated administration exists, administration is a trusted-host operation:
anyone who can act as the operator's user on the host can change the policy. The documentation
states this plainly.

## Amendments

**ADR-057.** Its rule "No allowlist check is performed" (line 229) holds while the policy mode is
`off`. In `shadow` and `enforce` modes, `comm.send` and `comm.reply` evaluate the pair policy (D5,
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
  operator switches to `enforce`.
- Every message path gains one evaluation at send and one per transport attempt. The evaluation
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
   records `claimed`. Control: a request that states its own assurance is rejected.
2. Generic graph create, update, merge, import and delete cannot change a class, binding or rule.
   Control: each of the five against a protected field.
3. `comm.send` and `comm.reply` both refuse before either copy is written. Control: the message
   store has no new row after a refusal.
4. Deny overrides: an actor allow under a class deny is refused.
5. A parent's rule change applies to a bounded agent's queued message at its next transport attempt: the
   message is held with `policy_denied`, and no retry runs while held.
6. Under `enforce`, an email recipient outside the allowlist is refused at send, and the outbox
   check still refuses a message admitted before an allowlist change.
7. The hosted service's refusal is independent: local allow plus hosted denial means no delivery.
8. A recipient-side refusal yields `recipient_quarantined` at the sender.
9. `shadow` delivers and audits would-be refusals, and never bypasses the email allowlist or hosted
   consent.
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
3. **A resubmission after admission is an attempt (D6).** On the node channel, a message the hosted service admitted
   is resubmitted under ADR-105 Appendix A.8 (600 seconds after `admitted_at` without a verified receipt). That
   resubmission is a transport attempt and is evaluated. A refusal stops the resubmission and recalls nothing: the
   earlier admission may still deliver, a verified receipt for it ends the hold, and the message stays shown as
   possibly charged. This is the case "A message already handed to a transport cannot be recalled" covers.
4. **Receive side (D8).**
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
7. **Receive-side keeping and claim (D8).**
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
   (b) On receipt, an envelope opened under ADR-105 Appendix A.5 is evaluated at `claimed`. It proves possession
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
