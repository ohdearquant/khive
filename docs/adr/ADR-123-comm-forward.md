# ADR-123: comm.forward — Provenance-Preserving Message Forwarding

**Status**: proposed\
**Date**: 2026-07-24\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-057](ADR-057-comm-actor-addressed-delivery.md) — Cross-actor messaging (dual-write delivery;
  the forward is a send and inherits its delivery semantics)
- [ADR-121](ADR-121-attachments-first-class.md) — Attachments (forwarded messages carry the
  original's renditions by `ContentRef`, never by byte copy)
- [ADR-122](ADR-122-email-outbound-delivery.md) — Email outbound (a forward addressed to an
  `email:*` recipient rides the same outbound path)

---

## Context

Relaying a message today means paraphrase. A sender writes to one actor; if that actor wants a
second actor to see it, they retype the gist into a new `comm.send`. Three things are lost in
the retyping:

1. **The original words.** A paraphrase compresses; tone, emphasis, and exact wording — often
   the point of escalation traffic — do not survive.
2. **Provenance.** The relayed message reads as the relayer's claim. The recipient cannot cite
   the original author or message id; "A said X" silently becomes "B said A said X". The
   provenance-of-claims discipline requires an originating message id on any attributed
   statement, and a paraphrase has none the recipient can reach.
3. **The relayer's own signal.** The relayer usually wants to add one line of steering on top
   ("this is wrong, fix it") — today that line and the original content compete for the same
   body text.

The motivating shape: an external sender emails actor A; A forwards the message to actor B with
a one-line note. B receives one message carrying A's note as its content and the original —
author, body, timestamp, thread identity — as structured, citable payload.

## Decision

Add one verb to the comm pack:

```
comm.forward(id, to, note?, subject?)
```

- `id` — a message in the caller's inbox or outbox (short prefix or full UUID, resolved the
  same way `comm.read` resolves).
- `to` — any address `comm.send` accepts: `lambda:*`, `email:*`, or any channel-kind address.
- `note` — optional text from the forwarder; becomes the delivered message's `content`. When
  absent, the content is an empty forward header (the embedded original still carries the
  substance).
- `subject` — optional subject for the delivered message. When absent it defaults to
  `Fwd: <original subject>` if the original carries a subject, and is omitted otherwise.

The subject default is not cosmetic. A forward addressed to `email:*` rides the outbound path
(ADR-122), where a message with no subject is delivered as untitled mail whenever a caller folds
the subject into the body instead of passing it. Deriving the subject from the original by default
means the common case cannot produce untitled mail, and an explicit `subject` remains available
when the forwarder wants to reframe.

### Delivered shape

The forward is an ordinary `comm.send` under the hood (dual-write, channel routing, outbound
delivery all inherited), with a structured `forwarded` block in the new message's properties:

```json
{
  "forwarded": {
    "from_actor": "email:sender@example.com",
    "content": "<original body, verbatim>",
    "subject": "<original subject, if any>",
    "sent_at": "<original RFC 3339 timestamp>",
    "thread_id": "<original thread id>",
    "message_id": "<author-side message id>",
    "message_id_kind": "khive" | "rfc822",
    "attachments": [ { "role": "...", "content_ref": "..." } ],
    "chain": [
      { "forwarded_by": "lambda:<actor>", "at": "<RFC 3339>", "note_present": true }
    ]
  }
}
```

Field semantics:

- `content` is embedded **verbatim** so the recipient needs no cross-namespace read to see the
  original. The embed is the delivery payload; the ids beside it are the citation path.
- `thread_id` is byte-identical across both copies of a dual-written message, so it points back
  to the source conversation for anyone who can reach it.
- `message_id` is the **author-side** id, and `message_id_kind` names the namespace it resolves
  in. Selection order, first match wins:

  1. the received copy's `outbound_ref` — the author-side khive note id of a message another
     actor dual-wrote. Kind `khive`.
  2. for a message the forwarder itself authored (`direction: "outbound"`), that copy's own note
     id, which is byte-identical to the `outbound_ref` the recipient's copy carries for the same
     message. Kind `khive`.
  3. the `wire_message_id` an ingested message carries — the RFC 822 `Message-ID` of an email.
     For a message from outside khive the author's namespace is the mail system, and this is the
     citation that resolves there. Kind `rfc822`.
  4. none of the above: the forward is **rejected**. The alternative is recording the
     forwarder's own private inbound-copy id as the original author's message id, which the
     recipient cannot resolve and cannot tell apart from a real citation. The block is worth
     citing only if the field means what it says, so refusing to forward is the smaller harm.
     The practical cost is that a message with no author-side identifier at all — a pre-ADR-057
     legacy row, or an ingest that supplied no `Message-ID` — is not forwardable.

  `message_id_kind` exists because the field now holds two identifier namespaces and a
  UUID-shaped khive id is not reliably distinguishable from an angle-bracketed `Message-ID` by
  inspection. A citation a consumer cannot classify is a citation it cannot resolve.
- `attachments` lists the original's role-keyed renditions by `ContentRef` (ADR-121).
  Content-addressed storage makes this zero-copy: forwarding never duplicates bytes, it
  propagates references. Until ADR-121 lands, the field is present and empty.
- `chain` records the hop this call performs, and only that hop: exactly one entry, always. See
  "Chain depth is the deliberate loss" below. Hop notes are NOT accumulated into the block (each
  hop's note is that hop's message `content`); `note_present` records only whether one existed.

### The block is trustworthy because it is derived, not because the field is protected

`forwarded` is constructed server-side by the `comm.forward` handler, on every call, from the
message that call resolved. Every field is derived at that moment: the author of the message in
hand, its citation, its content and subject, and this one hop. The handler reads no stored
provenance — a `forwarded` value already sitting on the resolved message is not read, not
validated, not appended to, not copied. **That** is what makes the block worth citing: each
field is true by construction, because none of it comes from state a caller could have written.

`forwarded` is also a reserved property: `comm.send`, `comm.reply`, `comm.forward` and
`comm.ingest` **reject** a request whose properties carry the key, with a named error rather
than a silent strip, so a caller who tried learns that they did instead of assuming it landed.

The reservation is a caller-facing guard on the verbs that create messages. It is **not** what
makes a block trustworthy, and this ADR previously said it was. Message properties are also
writable through the generic note-update path — comm messages are notes, and that path passes a
caller's `properties` map through without a key filter — so a caller can place a well-formed
`forwarded` object on a message it owns without going through any comm verb at all. It follows
that **no stored block can ever be evidence of its own origin**: a forgery and a handler-built
block are the same bytes, and nothing in the record distinguishes them. This is not a legacy
window for rows written before the reservation; it is open now and stays open.

That is why the handler derives instead of validating. Any validation of a stored block, however
thorough, would be checking a forgery's shape and then relaying it under server attestation —
laundering caller-written state into provenance, which is the precise harm this ADR exists to
prevent. Shape is not origin, and no amount of shape checking becomes origin. The reservation is
kept because it still does its own smaller job: it keeps the key out of the message-creating
verbs and produces a named error instead of silent acceptance.

**Contract change: the stored-block validation contract is removed, and with it the second
unforwardable class** (a message whose stored block carried no resolvable citation) that earlier
revisions of this ADR defined. Both described behaviour that no longer exists. Removing a shipped
contract is itself a contract change, so it is stated here explicitly rather than left to be
inferred from the code: a message carrying a stored `forwarded` block of ANY shape — well-formed,
malformed, scalar, null — is now forwardable, and the stored value has no effect on the outcome.
The named errors that rejected malformed stored blocks are gone. The remaining unforwardable
classes are the two that come from the resolved message itself: no author-side citation (case 4
of the selection order), and no identifiable original author.

### Chain depth is the deliberate loss

Because the handler never reads a stored block, a chain always has exactly one entry: the hop
being performed. Forwarding a forward yields a one-hop block describing **the message actually
forwarded** — whose author is the actor that forwarded it to you, whose `content` is that actor's
covering note, and whose `message_id` is the author-side citation of that actor's message. The
original sender two hops back is no longer named in the block; the recipient reaches them by
citing the message they were given and walking back a hop at a time, each link individually
attributed and individually checkable.

This is a real loss of convenience and it is the point of the change, not a footnote. The
alternative — carrying the origin across hops — requires reading the previous hop's block out of
stored properties, which is exactly the caller-writable state that cannot be trusted. A
multi-hop chain that could be forged at any hop asserts more than it can substantiate; a one-hop
chain asserts less and is true. Depth was worth having only while it was believed to be derived.

### Identifiers must name something; content may be empty

Every field of a `forwarded` block is one of two kinds, and the split governs what the
construction path may mint:

- **Identifiers** — `from_actor`, `message_id`, `message_id_kind`, `thread_id`, `sent_at`, and
  `chain[*].forwarded_by` / `chain[*].at`. Each must resolve to an actor, a record, or an instant.
  A value that is empty or whitespace-only resolves to none of those, so it is never minted.
  `""` is not a weaker identifier than
  a missing field; it is the same unusable value wearing a type that passes a string check, and it
  is worse than absence, because absence is visibly incomplete while `""` reads as filled in.
  Resolvability is checked, not merely non-blankness: a timestamp entering the block must parse
  as **RFC 3339**, on every path that can supply one. `comm.ingest` rejects a `sent_at` that does
  not parse rather than storing it, and a forward of a message whose stored `sent_at` does not
  parse is rejected rather than copying the value into the block. Non-blank is only the trivial
  non-resolving case; `"last tuesday"` is non-blank and names no instant. An **absent** send time
  stays absent (`null` in the block, ingest-time default on the row): not supplying a time is a
  different statement from supplying one that resolves to nothing.
- **Content** — `content` and `subject`. These reproduce text a human wrote, and emptiness is a
  legitimate value for them: a subject-only email has an empty body, a bodiless note has an empty
  subject. Both are real messages, and rejecting them would over-tighten in the name of provenance
  the block does not need.

Both kinds are checked on the construction path, because both are read out of the resolved
message's stored properties and that map is caller-writable through the generic note-update
path. For identifiers this means resolvability, per field: `thread_id` must be absent, `null`,
or a string that parses as a UUID, and every other value — blank, whitespace, a malformed
string, or a non-string of any JSON type — rejects the forward with a named error, exactly as an
unparseable `sent_at` does. A `thread_id` naming no thread is the `"last tuesday"` case on the
record axis: it would be attested in the block as the conversation the original belongs to,
byte-indistinguishable from a pointer that resolves. For content the check is narrower but not
absent: emptiness is legitimate, so no blank check applies, but content is TEXT, and a `subject`
stored as an object, a number, or an array is not a subject a person wrote. It is rejected
rather than cloned through, since the block would otherwise carry a structural value where a
subject line belongs — a claim the consumer cannot render and did not ask to parse.

Identifier is the default. A field added to the block later inherits the non-blank requirement
without anyone opting it in; the content list is the exemption. This is deliberate — the rule is
stated over the *kind* of a value rather than over a list of field names, because a per-field rule
leaves the same defect available on the next field.

On the construction path this yields the **second unforwardable class**, and it is case 4 of the
selection order restated on the actor axis: a message whose original author cannot be identified.
A message is forwardable only if its author can be named **both** as an actor (`from_actor`, or
the raw `from` label an ingested message carries) and by a citation (the selection order above).
Neither half may be defaulted. The handler must not fall back to an empty `from_actor`, because
the block's entire guarantee is that the *server* wrote that field rather than the caller — and a
server that writes `""` satisfies the letter of that guarantee while destroying its meaning,
producing a record that asserts the message came from nobody with the same authority as a real
attribution. As with case 4, the error names the fields that were looked for.

A missing identifier is reported as missing. It is not filled with a sentinel such as
`"unknown"`: a sentinel launders absence into a resolvable positive claim, producing a field that
reads as answered while naming nothing, which is the same defect as `""` wearing a friendlier
word. The handler either derives a value that means what it says or refuses to forward.

### Why the multi-hop path was deleted rather than hardened

The function that read, validated, and appended to a stored `forwarded` block cannot be hardened
into correctness: its job was to decide whether an untrusted record was genuine by looking at it,
and that question has no answer, so no amount of field-by-field validation produces a function
that answers it.

Deleting the path removes the question instead of continuing to answer it better. What remains
derives every field from the message in hand, and derived facts do not need to be validated.

### Threading

The forward starts a **new thread** between forwarder and recipient — it does not join the
original thread (the recipient may not be a participant there, and dual-write threading is
pairwise). The original `thread_id` inside the `forwarded` block is the pointer back;
`comm.thread` on it (where the caller has visibility) walks the source conversation.

### What forward is not

- Not a delegation or access grant: embedding the original body is an act of disclosure by the
  forwarder, exactly as retyping it would be. No namespace visibility changes.
- Not an edit surface: the embedded original is immutable at forward time. A forwarder who
  wants to annotate inline quotes in their `note`; the verbatim block is never modified.
- Not a broadcast primitive: one `to` per call, same as `send`. Fan-out is multiple calls.

## Consumers

Per the capability-consumption rule, a verb that nothing calls is not shipped. The day-one
consumers are named here, and the lane closes when a real forward has ridden each path — not
when the verb registers in `verbs()`.

1. **Inbound email escalated to a recipient.** An external sender mails an actor; that actor
   forwards the message to the recipient who owns the work, with a one-line note. This is the
   motivating shape and the first path to exercise: it covers `email:*` origin, a `lambda:*`
   recipient, and the subject default.
2. **A report forwarded outward with a covering note.** A recipient's report is forwarded to an
   `email:*` recipient with the forwarder's framing as the note. Exercises the outbound path and
   is where an untitled-mail regression would surface first.
3. **Ruling relay between recipients.** One recipient forwards a ruling it received to another
   rather than restating it. This is the path the provenance discipline exists for: the recipient
   can cite the originating author and message id instead of the relayer.

Each is one call, and each produces an artifact that can be read back. Verification is the
delivered message on the receiving side carrying a `forwarded` block whose `message_id` resolves
in the author's namespace, not a passing unit test.

## Consequences

- Escalation traffic carries evidence instead of paraphrase; recipients can cite the
  originating author and message id directly, closing the "relay ≠ authorship" gap for
  forwarded rulings.
- Email-origin messages (the primary inbound escalation channel) become forwardable to any
  recipient in one call, and recipient-generated reports become forwardable outward to email
  recipients with a covering note.
- The comm pack grows one verb and zero new tables: the `forwarded` block rides the existing
  message-note properties column. No migration.
- A block asserts only what the handler derived on the call that emitted it, so every field is
  auditable by construction. The cost is depth: a relay is attributed one hop at a time, and a
  recipient two hops downstream cites the message they were given rather than the original.

## Implementation notes

- Handler composes: resolve `id` → construct `forwarded` block → delegate to the existing send
  path with `note`-as-content, the resolved-or-defaulted subject, and the block merged into
  properties. Validation: forwarding a message the caller cannot read is rejected at resolution
  (no oracle: same not-found shape as `comm.read`).
- The reservation is enforced on the write path shared by every message-writing verb, not only
  in `comm.send`, so a future verb cannot reopen the hole by not knowing about it.
- `comm.read` marks the source message read iff it was inbound-unread, matching the act-then-
  read discipline; forward does not implicitly mark read.
- Verb registers in the comm pack vocabulary; `verbs()` reflects it; params follow the pack's
  existing `params.rs` conventions.
