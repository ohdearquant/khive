# ADR-125: Reserved property keys on pack-owned note kinds

- Status: Proposed
- Date: 2026-07-25
- Supersedes: none
- Amends: none. ADR-124 keeps its status; this record widens what ADR-124's machinery protects and does not change how it protects it.
- Related: ADR-007 (namespace as attribution), ADR-017 (pack-owned vocabulary), ADR-124 (note-write identity)

## Context

ADR-124 established that a caller may not write a record's identity properties: the substrate derives
them from authenticated context on creation and preserves them across later mutation. The mechanism
works. This record is about the set it is applied to.

### The defect is the shape of a set, not a missing guard

Enforcement converges. One constant,
`OWNED_IDENTITY_PROPERTIES` at `crates/khive-runtime/src/curation.rs:2497`, is read by every
enforcement site: the patch-time refusal helper at `:2509`, the merge-time restore at `:2538` and
`:2555`, and the regression test at `:2856`. Creation converges too, on
`derive_note_write_properties`, which generic create reaches through its kind hook and proposal
apply calls directly at `crates/khive-runtime/src/atomic_prepare.rs:509`.

So there is no unguarded door. There is one guard, applied consistently, holding a set that is three
keys wide:

```rust
pub(crate) const OWNED_IDENTITY_PROPERTIES: &[&str] = &["from_actor", "direction", "sent_at"];
```

That set was assembled by asking which properties identify the record's sender. Every member answers
that question and the name says so. A key that changes trusted behaviour without identifying anyone
is outside the set by construction, and nothing in the code or the tests will say so.

This failure mode is worth naming because it is invisible to the audit that would normally catch it.
A missing guard shows up as an absence: some path has no check. A convergent guard holding an
under-shaped set shows up only if you ask what the set **contains**, and an audit organised around
the same concept as the set will confirm the set. Identity-shaped review of an identity-named
constant returns clean, correctly, and tells you nothing about the class of key it was never
looking for.

**The diagnostic that generalises: for any shared policy constant, ask what class of key its NAME
excludes.**

### The class this record must cover

A property key written into a record by trusted code and read back by trusted code to decide the
record's **structure** — which logical object it belongs to, how it is grouped, where it routes,
whether it is visible — while identifying no actor.

Such a key is not protected by an identity-shaped set, and a forged value does not corrupt the
record it sits on. It changes how a _different_ set of records reconstructs. The record stays
intact and readable; what degrades is the assembled view. Detection is therefore not a matter of
finding a damaged row, because there is no damaged row.

A measured instance exists in the tree and is recorded in the evidence section below.

### Enumerating the protected set: both sides, and why either alone under-covers

A key belongs in the protected set if **either**:

1. **Trusted production** — the value is established by owning-pack or substrate code under an
   authority the caller does not have; or
2. **Semantic read dependence** — some code path reads the value to decide identity, grouping,
   routing, lifecycle, visibility, authorization, deduplication, or membership in a rendered
   result.

Neither test alone is sufficient, for reasons that are structural rather than empirical.

Read dependence alone cannot define **creation** semantics. A read site can establish that a key
matters; it cannot say whether generic creation should derive the value, refuse it, or accept it
once and freeze it. Those are properties of how the value comes to exist, and the reader does not
know them.

Trusted production alone misses keys that acquire behavioural weight after they are written, and it
cannot see a consumer in another crate that has begun depending on a value the producer considers
incidental.

The instance that motivated this record does **not** discriminate between the two tests. It was
discoverable from either side: written by owning-pack code under owner authority, and read by
owning-pack code to select a grouping key. It cannot be cited as evidence that one test suffices,
and this record does not cite it that way.

### What this record does not decide

Patch **shape** — whether a non-object patch is refused on a pack-owned kind — is a separate axis
already settled elsewhere (ADR-124 rule 3b, implemented at `crates/khive-runtime/src/curation.rs:1116`).
A reserved-key policy does not subsume it, and folding the two together would let a shape decision
ride in on a vocabulary decision. They stay separate.

## Decision

### 1. One declared policy, two enforcement modes

Enforcement continues to converge, and the convergence is the asset. What changes is that the
policy stops being a flat list of identity names and becomes a pack-declared classification.

Two modes, because creation and mutation are different operations:

- **Creation** derives or refuses a protected value before the note is constructed. Generic create
  and proposal apply consult the same installed runtime policy; both already reach it.
- **Mutation** refuses protected keys on update, and on merge preserves the surviving `into`
  value.

There is no single physical guard shared by create, update, proposal apply, and merge, and this
record does not invent one. Sharing one _declaration_ across four seams prevents policy drift
without pretending that derivation and preservation are the same operation.

### 2. The owning pack declares its protected keys, with a typed policy

Each pack declares, adjacent to its `NOTE_KINDS`, a policy for every property key that pack code
uses to affect record semantics:

- **`Derived`** — the substrate or owning pack establishes the value from trusted runtime context
  at creation. Caller input cannot determine what is stored, and later generic mutation cannot
  change it.
- **`OwnerOnly`** — only an owning-pack write path may establish the value. Generic create,
  proposal add-note, update, and merge-from cannot introduce or replace it.

Absence from the declaration means generic metadata, writable by callers. There is deliberately no
`Free` variant: an explicit third state would let an unreviewed key look classified.

The declaration has a proven shape rather than a hypothetical one, checked against the trait rather
than assumed: `Pack` already carries three structured, pack-declared associated consts that default
to empty and are collected at boot — `EDGE_RULES`, `ENTITY_TYPES`, and `NOTE_KIND_SPECS`, the last
of which is per-note-kind spec data declared by the owning pack. A property policy is the same
shape as `NOTE_KIND_SPECS` and inherits its additive, empty-default contract, so no pack is forced
to change to adopt this record.

The declaration is also the source of typed property-key tokens for pack semantic reads. Pack code
should not reach a behaviour-deciding property through a raw string lookup, and the coverage
mechanism that enforces this is the load-bearing unresolved question (see Risks).

### 3. Reject the whole patch, name every protected key

An object patch naming one or more protected keys is refused in full. The error names every
protected key present and the owning kind. No part of the patch is written.

Silent stripping is rejected: the measured exploit already returns success with no marker, and
preserving that shape would conceal both attacks and accidents. Rejecting only on a differing value
is rejected: it makes behaviour depend on stored state the caller cannot see, and buys nothing any
workflow needs.

### 4. Merge preserves `into`

On a merge of two notes of a pack-owned kind, protected keys retain the `into` note's value under
every strategy including `prefer_from`. A protected key absent from `into` stays absent even if
`from` supplies one. Unprotected properties continue to follow the requested strategy.

Blanket merge refusal is rejected because deduplication is a designed workflow. The existing
identity-key restore at `curation.rs:2191` is the precedent to generalise; it already does exactly
this for the three-key set.

### 5. Write-forward, with no synthetic legacy signal

Existing records receive no inferred trust marker, and nothing here can fail daemon startup on
legacy data.

Storage cannot distinguish an owner-written historical value from a caller-written one. A read-time
"predates enforcement" marker would therefore assert provenance the system does not have, which is
the same error as the original defect wearing the opposite sign. Refused writes emit the normal
named error and the existing audit event. A separately authorised audit and repair lane may inspect
high-impact keys; it is not a startup prerequisite and is out of scope here.

## Excluded class, named rather than covered

Applying this record's own diagnostic to its own decision: what does per-pack declaration exclude?

**Keys whose producer and whose store are different packs.** If each pack declares the keys it owns,
a key written by pack A into a note kind owned by pack B is unowned by construction — the same
defect one level up, with better types.

The class is real. **No instance of it exists in the tree at the time of writing**, verified rather
than assumed: for the motivating key, the producer, the note-kind owner, and every reader are the
same pack; the only out-of-pack consumer reads properties and does not write them; every remaining
apparent cross-pack site is a runtime test fixture.

This record therefore **names the class and fences it** instead of choosing a mechanism to cover a
case that does not occur:

- The first property key whose producer pack and store pack differ forces the declaration-authority
  question — who classifies it, and who may write it — **before that key ships**, not after.
- A pack may not declare policy for another pack's note kind under this record.
- A consumer pack may not tighten an owning pack's write contract indirectly.

**This fence resolves permissively, and a reader must not mistake fenced for detected.** Absence
from a declaration means caller-writable, and no pack may declare for another pack's kind, so a
cross-pack-written key is caller-writable **by default** the moment it ships. Running this record's
diagnostic on its own fence: what does the fence emit when such a key appears? Nothing. It is a
rule, not a check, and its trigger is a person noticing. That is acceptable only while the class has
no instance, which is the condition under which it was written.

Choosing a mechanism now for a case with no occurrence would mean designing from a plausible model
of the code rather than the code, which is precisely the failure this record's method exists to
avoid.

## Acceptance test, stated as falsification

The claim under test is: _a set enumerated by trusted production and semantic read dependence
catches keys that a meaning-based enumeration misses._

A retrospective check cannot test this. Every key currently in the tree is already visible to this
record's author, so any "held-out" set drawn from existing keys is contaminated by knowledge of the
motivating instance, and a list that already contains it cannot falsify a claim that was written
after observing it.

**The held-out set is therefore prospective, and named concretely:**

- **Which keys**: every property key introduced into the write path of a pack-owned note kind after
  this record merges, until ten such keys exist.
- **Chosen when**: at the time each key is introduced, by work not yet planned.
- **By whom**: the author of the introducing change, who applies the two-part rubric in this record
  and records a classification in the same change.
- **Audited by**: a reviewer who is not that author, checking whether the classification matches
  what the code does with the key.

The claim is **falsified** if, across those ten keys, any key whose mutation changes trusted
behaviour was classified as generic metadata by an author applying the rubric in good faith. One
such key means the rubric does not transfer to people who did not witness the original defect,
which is the only property that matters.

**This means the completeness claim is unvalidated at merge time**, and stays unvalidated for ten
keys. That cost is accepted knowingly. The alternative on offer is a criterion that can only ever
confirm itself.

## Risks and unresolved questions

- **The coverage mechanism is UNRESOLVED and load-bearing.** A Rust type or lint that reliably
  separates semantic property access from display-only access is not demonstrated. Bulk
  deserialization, indexed access, pattern matching, helper wrappers, and aliases must all be
  covered or the by-construction claim fails and must be withdrawn.

  **The withdrawal has a trigger, not only a condition.** Verify-by 5 is the demonstration. If
  verify-by 5 cannot be made to pass reliably across every access form named above, the implementing
  change does not proceed: an amendment to this record striking the by-construction claim, and
  stating that completeness rests on author review, merges **before any part of the policy ships**.
  The owner is the author of the implementing change; the point in time is the moment verify-by 5 is
  abandoned or narrowed, not a later review.

  A stated fallback with no mechanism to reach it is the fail-open shape this record exists to
  correct, one level up.

  **Resolved 2026-07-26 — the demonstration was executed and the claim did not survive.** The
  trigger named above fired: the by-construction claim is struck and verify-by 5 is narrowed. See
  Amendment 1, which merged before any part of the policy shipped, as this bullet requires.
- **`OwnerOnly` presumes an authorization distinction** between an owning-pack write and a generic
  note write. The creation seam exists; that a capability token distinguishing the two exists is
  not established. UNRESOLVED.
- **Cross-pack readers** need the owning pack's typed tokens to be importable without importing
  write authority. The crate dependency direction is UNRESOLVED.
- **Legacy values stay ambiguous** and continue to affect reads. The prevalence of already-forged
  values was not established; the search method used returned nothing, which is a statement about
  the method.

## Implementation fences

### MAY

- Reuse the installed runtime note-write validator for create and proposal apply.
- Generalise the existing merge preservation helper from the three identity keys to the declared
  policy.
- Generate typed key tokens and policy tables from one pack-local declaration.

### MAY NOT

- Deny all property patches on pack-owned kinds. Arbitrary metadata round-tripping through update
  is a merged contract with a regression test.
- Deny generic creation of pack-owned kinds; at least one pack depends on shared `create`.
- Silently strip protected keys.
- Permit `merge(prefer_from)` to introduce or replace a protected key on the surviving note.
- Infer a legacy trust marker from timestamps, ids, creation history, or current values.
- Fold the non-object patch-shape contract into this record.
- Claim closure of the motivating defect because its key appears in a reviewed list. Closure
  requires the key to be unreachable through all four mutation paths, demonstrated.

### Verify by

1. A table-driven test enumerating every pack-owned note kind from installed pack vocabulary
   (eleven at the time of writing, across the code, comm, git, gtd, memory, schedule, session, and
   template packs) and applying each declared policy through generic create, proposal apply,
   update, and every merge strategy.
2. Arbitrary unrelated metadata still round-trips through update, preserving the existing
   regression contract.
3. The motivating key is refused through generic create, proposal add-note, update, and merge-from,
   while the owning pack's own write path can still establish it.
4. The recorded reproduction, re-run, shows no degradation of the assembled view after every
   attempted caller mutation.
5. A compile-fail or lint fixture introduces a new raw semantic property read and fails until the
   key is classified, including through bulk deserialization and a helper wrapper. _Narrowed by
   Amendment 1: the fixture covers the five type-reachable access forms; bulk deserialization and
   two further forms discovered during execution are demonstrated bypasses, not covered forms._
6. Merge tests prove a missing protected key on `into` stays missing and an existing value is
   unchanged under every strategy.
7. A rejected patch names every protected key it contained, and no unprotected sibling key was
   written.

## Evidence

The class was not hypothesised. It was measured against a scratch database, with a pre-state
control and a bounding negative. The measured shape: a forged value in a caller-writable
property collapsed two logically distinct records into one grouping key. The assembled view
lost an entry. Every underlying row remained present, readable, and individually correct. The
operation returned success with no marker.

## Amendment 1 — 2026-07-26: verify-by 5 executed; the by-construction claim is struck

The Risks section made verify-by 5 the gate on the by-construction completeness claim and named
the consequence of failure: an amendment striking the claim merges before any part of the policy
ships. The demonstration was executed. This is that amendment.

### What was demonstrated

A sealed newtype with a private inner and typed-token accessors — the strongest of the candidate
mechanisms, and a type rule rather than a name rule — was prototyped and driven with one
compile-fixture per access form. It **fails the build, by construction**, for every access form
that passes through a pack type:

- direct map get (`E0616` private field, `E0308` bare string where a token is required),
- indexed access (`E0608`),
- pattern matching (`E0425` — the match cannot be written without declaring a token, and declaring
  one is classification),
- helper wrappers (`E0425` at the leaf, regardless of wrapping depth),
- aliased re-exports (`E0616`/`E0308` — a type rule is immune to renaming).

Three access forms bypass it with a clean compile, each for a structural reason no better type can
remove:

1. **Bulk serde deserialization.** Field names bind to map keys inside a `derive` expansion, where
   no token value can be interposed. The `Serialize`/`Deserialize` impls this rides are not
   optional: the properties map is read from a JSON column and echoed verbatim into responses, a
   merged contract this record's own MAY-NOT fence protects. Sealing and the round-trip contract
   are in direct conflict; closing this form would break the fence.
2. **Raw-SQL property reads.** `json_extract(notes.properties, '$.<key>')` inside SQL string
   literals — a form the original Risks list did not enumerate. On the order of seventy such sites
   exist across more than twenty files today. The key is a substring of an opaque `&str` consumed
   by SQLite; no Rust type or lint can see it.
3. **Untyped `Value` reads.** The properties map reached as an anonymous child of a generic JSON
   blob decoded from a SQL row, with no pack type anywhere in the path. There is nothing to seal.

A name-based lint was also executed and failed in both directions on one fixture: it flagged a
display-only read it should have permitted and missed a `Map::get` semantic read it should have
caught. The semantic-versus-display separation the Risks section requires is exactly what a name
rule cannot express.

### What this record now claims

- **The by-construction completeness claim is struck.** No mechanism demonstrated here, or
  assessed and argued against, makes an unclassified semantic property read fail the build across
  all access forms present in the tree.
- **Verify-by 5 is narrowed** to compile-fail fixtures over the five type-reachable forms. Those
  fixtures are worth shipping: for the forms they cover, `error: cannot find value` until the key
  is classified is precisely the behaviour the original item asked for, and it is where name-based
  approaches fail that the type rule holds.
- **Completeness of the protected set rests on author review** under the two-part rubric, audited
  by a non-author reviewer, with the prospective ten-key acceptance test in this record as its only
  validation. That is a human process with a known false-negative mode, not a mechanism.
- **The uncovered forms emit no signal when they appear.** Applying this record's own diagnostic
  to its amended state: a new bulk-deserialize, raw-SQL, or untyped-`Value` semantic read compiles
  clean and tells no one. Readers must not treat the typed-token API as a completeness guarantee.
  It is a convenience and a partial guard, and the partiality is silent.

### Consequence for the implementing change

The policy mechanism (declaration, creation derivation, update refusal, merge preservation) is
unblocked and unchanged — none of it depended on the struck claim. The typed-token accessor ships
as the recommended read path for the five covered forms, with its fixtures. The two forms
discovered during execution join bulk deserialization in the named-uncovered class, and any future
mechanism claiming to cover them must demonstrate against all three before the claim enters this
record.

A forged write that returns ordinary success is the reason patch rejection in this record is
total rather than a silent strip: the failure already looks like success, and adding a second
silent success would make it permanent.
