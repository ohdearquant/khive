# ADR-128 — Custody party-pairs and slot authenticity

Status: accepted 2026-07-25
Date: 2026-07-25
Supersedes: none
Amends: ADR-127, section "Key custody is the load-bearing constraint", on ONE point: the
criterion by which a custody home is evaluated. ADR-127 assessed the OS keychain on
readability; this record adds slot authenticity, on new measurement. Nothing else in ADR-127
is amended, qualified, or reversed.
Extends: ADR-127's custody rule to a party-pair ADR-127 did not decide. That part of this
record is **new policy on its own evidence, not a correction**.
Depends on: ADR-127 (authenticated actor and grant primitive)

## Context

ADR-127 made key custody the load-bearing constraint of its own design, evaluated the OS
keychain as a custody home, measured it rather than assuming, and ruled that **actor**
separation is a topological problem with an expiry date: co-location defeats it, distinct hosts
restore it, and a local broker that authenticates a **actor** by something other than file access
must not be built in the meantime.

That reasoning is sound, correctly scoped, and this record does not disturb it. ADR-127's
custody rule is stated in general terms; its remedy is expressly about actors and says so in its
own words. **There is no scope defect in ADR-127.** What follows is therefore two separate
things, and keeping them separate is the point.

The first is an **extension**. ADR-127's rule, applied to a second pair of parties, returns an
answer ADR-127 never recorded, because ADR-127 evaluated one pair and did not decide the other.
Deciding it is new work with its own premise and its own cost, not the repair of an omission.

The second is a genuine **amendment**, to the evaluation criterion. The keychain was assessed on
whether a stored secret can be read by the wrong party. That is not the only way a custody home
fails, and the other way is not covered by the repair a reader reaches for first. This part
changes how ADR-127's own question should have been asked.

## Finding 1 — applying ADR-127's rule to a second party-pair, which ADR-127 did not decide

ADR-127 states the test in deliberately general terms: **for any authentication design, name
which parties it distinguishes, then ask whether those parties can read each other's credential
material.** That rule is correct and this record adopts it unchanged.

ADR-127 then evaluates that rule against one pair, actor versus actor, and supplies a remedy for
that pair: co-tenancy under one OS principal defeats the separation, relocation restores it, the
arrangement is transitional, and local machinery to simulate the separation must not be built.
**That remedy is expressly about actors in ADR-127's own words** — it names actor separation, the
co-located actor arrangement, a broker authenticating an actor, and per-actor sandboxing. It is
correctly scoped and this record does not qualify it.

What ADR-127 does not do is instantiate its rule against a second pair: the **human principal**
and the **agent acting on that principal's behalf**. That is not an omission to be corrected;
evaluating a pair requires a threat-model premise about that pair, and ADR-127 supplies one for
actors and none for this. **This record supplies it, and owns it.**

### The premise, stated as this record's own

The premise is that **the agent holds the same credential as the human principal it acts for.**

This is asserted on this record's authority and is not inherited. ADR-127 describes actors
sharing a host user; it does not describe a human and its agent sharing a credential.

It would be convenient to go further and claim the pair is permanent — that an agent acting on a
principal's behalf necessarily runs with that principal's authority wherever it runs, so no
topology can separate them. **That claim is not made here**, because it would be an assumption
about a deployment presented as a property of agency.

A concrete counter-topology exists, and stating it is constructive rather than merely
self-limiting: a signing or approval credential held by the human on the human's own host, with
the agent running under a separate OS principal holding only a narrowly scoped, revocable
delegation. In that arrangement the agent never possesses the human's root authority and
relocation genuinely separates the pair. That is what the exit from this record's premise looks
like, which is worth knowing before anyone builds toward it.

Under the stated premise, ADR-127's rule returns its answer directly: the two parties can read
each other's credential material, so the authenticity claim between them is decorative. What
does **not** transfer is ADR-127's remedy, and not because that remedy is mis-scoped — because
it addresses a different pair. Relocating an agent to its own host separates it from other
actors, which is what ADR-127 measured, and says nothing about its relationship to the human
principal whose credential it holds by premise.

## Finding 2 — a custody ACL gates use of a secret, not its replacement

ADR-127's measured result was that a keychain item is readable non-interactively, with no
prompt, by a process running as the host user.

The repair a reader reaches for first is to stop the item being freely readable: attach an
access-control list so that an application which is not the depositor is refused. That repair
was measured. **It works, and the custody home fails anyway**, for a reason the original result
does not cover.

Two separately built binaries were used, one depositing the item and one not. The measurement
makes no claim about *what* the store uses to tell them apart; the operative fact is that the
store treated them differently, which the results below establish directly. All operations ran
with user interaction disabled, so any operation requiring confirmation returned an error rather
than prompting. A success therefore shows that the operation **completed without interactive
confirmation and without denial**. It does not by itself distinguish "no authorisation policy
applied" from "authorised automatically by policy or caller attribute" — the harness records the
returned status, not the store's internal decision. That distinction does not affect the finding
below, which turns on the operation completing at all.

| # | Step | Performed by | Result |
| --- | --- | --- | --- |
| 0 | Pre-state control: items under the service | — | none |
| 1 | Create the item | depositor | success |
| 2 | Read the secret | depositor | **success** |
| 3 | Read the secret | non-depositor | **refused**, authorisation failure |
| 4 | Delete the item | non-depositor | success |
| 5 | Absence check after deletion | non-depositor | **item not found** |
| 6 | Add an item at the same service and account | non-depositor | success |
| 7 | Read the new item | non-depositor | success |
| 8 | Read at the same service and account | **depositor** | **refused**, authorisation failure |

Each step of the argument is carried by a specific row, and the rows were chosen to close the
readings under which the store would have behaved correctly:

- **The refusal is identity-dependent, not a generic failure path.** Rows 2 and 3 are the same
  call against the same item moments apart, differing only in which binary made it. A locked
  keychain or an unrelated error path would have failed both.
- **The protected item was really deleted, and it was the protected item.** Row 0 establishes
  the service was empty beforehand and exactly one item was created, so there was no other item
  to select; row 5 establishes absence rather than inferring it from a success code. Without
  row 5, "delete returned success" is consistent with a no-op.
- **The replacement occupies the same retrieval identity.** Row 8 is the decisive one. After the
  replacement, the depositor is refused **at its own service and account** — present but
  refused, not missing. Its access is now governed by an access control it never set. The slot
  changed hands.

The result reproduced across runs, and cleanup was verified by planting a subject, confirming
an independent tool finds it, and only then confirming absence, so the final negative is a real
absence rather than a dead instrument.

**The protection is attached to reading a particular item, and an item is not an identity.**
Deleting an item and adding another is not an operation *on* the deleted item, so no access
control carried by that item is consulted. A consumer that fetches "the secret at service S,
account A" during start-up receives the replacement and cannot distinguish it from what it
deposited: the same location, the same retrieval call, the same success, no marker.

The general form: **a custody home must authenticate the slot, not merely gate the content.** An
evaluation that asks only "can the wrong party read what I stored" passes a store in which the
wrong party can decide what I read back.

### Scope of the measurement, stated so that it is not widened downstream

- It **measures** the platform's legacy credential store, which is what a command-line program
  or daemon obtains by default.
- It does **not** measure the platform's data-protection credential store carrying a
  user-presence or biometric access control. Creating such an item requires a real signing
  identity: from an unsigned binary, creation fails for a missing entitlement, and signing with
  an unbacked entitlement causes the process to be killed. That was verified against a control —
  the same binary signed without the entitlement runs normally and fails only at creation — so
  the obstacle is the signing requirement and not a defective probe.
- The argument that adding after deleting cannot consult the deleted item's access control is
  structural and applies to any store with that shape. **For the data-protection case it is an
  INFERENCE, not a measurement**, and it is recorded as an open question below.

## Open question, and what depends on it

Does the data-protection credential store, with a user-presence access control, gate deletion
and re-addition as well as reading? A signed harness would settle it in one run.

**Decision 4 below depends on the answer.** If that store gates deletion, then a local custody
home can hold an anchor the local agent cannot replace, and decision 4 is wrong. Naming an open
question and asserting independence from it are different acts; this record does the first and
not the second.

Findings 1 and 2 do not depend on it. Finding 1 is an argument about scope. Finding 2 is
measured for the store a daemon obtains by default, which is the store any implementation uses
before someone deliberately chooses otherwise.

## Decision

1. ADR-127's custody **rule** is adopted unchanged. Its **remedy** is already scoped to the
   actor pair in ADR-127's own words and is **not** qualified, narrowed, or corrected here. What
   this record adds is a decision about a pair ADR-127 did not decide, resting on a premise this
   record states as its own. A reader must be able to tell those apart, so: **decision 1 is not
   a documentation correction.** It is new policy, and if its premise is wrong the policy falls
   without touching ADR-127.
2. **Every custody claim in this project names the party-pair it is about.** A custody claim
   without a named pair is incomplete, because the same store is adequate for one pair and
   inadequate for another.
3. **A custody home is evaluated on slot authenticity, not only on content gating.** The
   evaluation question is "can an unauthorised local party replace what I stored and have me
   load it as mine," not only "can that party read what I stored." This decision is unconditional
   and rests on Finding 2 as measured.
4. **Conditional.** For the human-versus-agent pair, *in the deployment where the agent holds the
   same credential as the human*, and *unless the open question above resolves in favour of the
   data-protection store*, an anchor must be one the local agent cannot replace. This record does
   not design such an anchor and does not authorise building one. It records the requirement, its
   two conditions, and the fact that both are checkable.

## Consequences

**What does not change.** ADR-127's local-profile row remains as written: the property is
NOT-DELIVERED against a malicious peer actor. This record widens the reason rather than narrowing
it, so nothing downstream of that row needs revisiting. ADR-127's instruction not to build local
actor-separation machinery also stands unmodified, and this record does not instruct that
anything be built. Decision 4 concerns a different pair under a premise ADR-127 does not share,
so the two records issue no instruction about the same thing — and if decision 4's conditions
hold, the honest consequence is that no authorised mechanism exists yet, which is a state to
record rather than a licence to build.

**What changes for implementation.** For the human-versus-agent pair under the stated condition,
the custody step ADR-127 sequences first is not satisfied by depositing a key in the **legacy**
credential store, with or without an access control. An implementation that does so and reports
the step complete has produced a system that looks authenticated and is not, which is the
specific outcome ADR-127 exists to prevent. Whether the data-protection store satisfies it is
open, and is the cheapest next thing to measure.

**What changes for review.** A design proposing a custody home is reviewed against decision 3.
Demonstrating that a foreign party cannot read the stored material is not sufficient evidence,
and a review that accepts it as sufficient has tested one of the two failure modes.

**Cost of being wrong.** If the human-versus-agent pair turns out not to matter for the eventual
design, this record has cost one qualification and no mechanism, since it deliberately builds
nothing. If it matters and is not recorded, the first implementation inherits ADR-127's remedy as
though it settled a pair it was never tested against.

## Evidence

Two findings support this record. First: two binaries compiled from one source under different
conditional-compilation flags, each ad-hoc signed, are treated by the OS keychain as distinct
applications for access-control purposes — access granted to one does not extend to the other.
Second: a keychain item can be present and refused, distinguishable from absent, when the
requesting binary lacks a granted entitlement — the item is not silently deleted or masked, the
access attempt is denied.

Both findings hold regardless of which specific pair of collaborating processes is involved:
the custody model in this record depends only on process-identity discriminability, not on any
particular deployment topology.
