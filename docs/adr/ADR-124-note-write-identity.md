# ADR-124: Note-Write Identity — Deriving Pack-Owned Identity Properties at the Write

**Status**: proposed\
**Date**: 2026-07-24\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-001](ADR-001-entity-kind-taxonomy.md) — Kind taxonomy (which pack owns which kind)
- [ADR-046](ADR-046-event-sourced-proposals.md) — Event-sourced proposals (the apply path is
  one of the note-write sites)
- [ADR-057](ADR-057-comm-actor-addressed-delivery.md) — Actor-addressed delivery (`from_actor` /
  `direction` / `sent_at` on `message` notes)

---

## Context

A note's `properties` column holds per-kind metadata. For most kinds that metadata is
descriptive: it says what the record is about. For a pack-owned kind, some of it is
**identity**: it says who produced the record. `message` notes are the clearest case —
`from_actor` names the authoring actor and drives inbox rendering, reply routing, and thread
participation checks.

The pack that owns a kind knows which of its properties are identity and where they come from.
`comm.send` already resolves `from_actor` from `token.actor().id`. But `properties` is a
general-purpose column reached by several runtime write paths that are kind-agnostic by design:

1. `create` on the shared CRUD surface, which materialises a `Note` from caller arguments.
2. Proposal apply (`prepare_add_note`), which builds its arguments itself and dispatches no
   pack hook at all.
3. `update`, which merges a caller `properties` patch into the stored object.
4. `merge`, which folds two already-written notes' properties together per a strategy.

There is also a specialized direct-ingest path: `KhiveRuntime::try_create_note` uses
`INSERT OR IGNORE` for transport-level deduplication and intentionally does not route through the
generic create validator. `comm.ingest` constructs the inbound message identity and routing
properties before calling it. That exception matters: "direct Rust caller" below means a caller of
the `create_note*` family, not every Rust function capable of inserting a note.

Each of those paths stores what it is given. Identity that is a function of the token on one
path and a function of caller input on another is not identity; a stored value is only evidence
of authorship if every path that can write it derives it the same way.

The runtime already solves this shape twice. `entity_type_validator` is a pack-installed
function called by `create_many` so entity-type validation is active at the runtime layer for
write paths **including direct callers that bypass the handler**. `note_mutation_hook` is a
pack-installed function for note mutations arriving through a **different pack's verb**. Both
exist because a rule that lives in one pack's verb handler is not a rule about the record.

## Decision

**1. A third pack-installed function slot: the note-write validator.**

`KhiveRuntime` gains `note_write_validator`, sibling to `entity_type_validator` and
`note_mutation_hook`, installed at pack registration through
`PackRuntime::register_note_write_validator`. It receives `(note_kind, actor_id,
caller_properties)` and returns the properties to store. It is invoked by the generic
caller-supplied-property sites: `create_note_inner` (the `create` verb and direct Rust callers of
the `create_note*` family) and `prepare_add_note` (proposal apply). It is deliberately not invoked
by `try_create_note`, whose channel-ingest caller constructs transport identity before the
deduplicating insert. Kinds the installing pack does not own are returned unchanged; the slot is
single-occupancy, like `note_mutation_hook`.

`khive-pack-comm` installs one that derives a `message` note's `from_actor` from the
authorization token, using the same `token.actor().id` resolution `comm.send` already performs,
so there is one source of truth for the field rather than two.

The bound on single-occupancy, stated so it is recognised rather than rediscovered:
`install_note_write_validator` is a plain assignment into the slot
(`crates/khive-runtime/src/runtime.rs`, `*guard = Some(f)`), and
`call_register_note_write_validators` invokes every registered pack against that one slot. A
second pack implementing this hook would therefore be **discarded by registration order, with no
error and no warning** — the surviving validator would return the other pack's kinds unchanged and
that pack's identity would silently stop being derived. This is safe today only because
`khive-pack-comm` is the sole implementer. It is documented rather than fixed here: widening the
slot to a list is a change to the pack-runtime contract, not to note-write identity, and adding a
second implementer is the event that should force it.

**1a. A pack-installed slot has as many install sites as the process has boot paths, and every
missed site fails open silently.**

An absent validator is indistinguishable from a permissive one: the empty slot returns the
caller's properties unchanged, and no write site can tell that apart from a validator that
approved them. So the slot is installed on every path that builds a registry from a freshly
constructed runtime — `khive-mcp`'s `serve.rs` (multi-backend) and `server.rs` (single runtime),
and `kkernel`'s `atomic_apply.rs` (the `--atomic` proposal-apply path, which is the path that runs
`prepare_add_note`) — and on **every runtime** each of those paths builds, not only the default
one. In the multi-backend path the per-pack runtimes are constructed independently by
`build_pack_runtime`, so they share none of the default runtime's slots, and `core()` clones the
secondary's own slots rather than the default's; a runtime holding the pack-owned kind list but no
validator enforces half the rule while reading as complete.

This is the same defect class the note-mutation hook is exposed to at `atomic_apply.rs`, where
`--atomic` builds its own registry without the install and the hook would be a guaranteed no-op
for the whole process. A documented startup sequence is not itself enforcement, so the
requirement here is mechanical rather than documentary: every slot of this family lands with an
install-site **assertion** — a test over the runtime a documented startup sequence produced,
asserting the slot is occupied — never a test that installs one itself. A hand-installed fixture
tests the validator and proves nothing about whether production installs it, which is precisely
the condition that fails.

**1b. Which runtime a pack installs onto is a per-slot decision, and the two existing slots answer
it differently from this one.**

`register_entity_type_validator_with_types` and `register_note_mutation_hook` both ignore the
runtime they are handed and install onto `self.runtime`, the runtime the pack owns
(`khive-pack-kg/src/dispatch.rs`, `khive-pack-memory/src/pack.rs`; the kg implementation says so
in a comment at the line). Their call sites in `serve.rs` correspondingly pass `&default_runtime`
with no per-pack loop, and that is correct rather than an oversight — both slots are
owner-scoped. kg's entity-type validator guards `create_many`, kg's own verb running on kg's own
runtime; memory's mutation hook notifies memory's own ANN cache. The enforcement point coincides
with the owner, so `self.runtime` reaches it.

The note-write validator is the first slot of this family that is **not** owner-scoped. A note of
a pack-owned kind is materialised by `create_note_inner` and `prepare_add_note`, which run on
whichever runtime the write was routed to, not on the owning pack's. So
`register_note_write_validator` takes the caller-supplied runtime and `serve.rs` loops it over
every per-pack runtime; installing onto `self.runtime` would guard the owning pack's own backend
and leave every other one open.

The two conventions are distinguished in source only by whether the parameter carries a leading
underscore. A slot author choosing by copying the nearest neighbour has even odds of picking the
one that fails open, and the failure is silent in both directions — an owner-scoped slot looped
everywhere is merely redundant, while a substrate-wide slot installed on `self.runtime` alone
reads as installed and enforces nothing off that backend. The rule: a slot enforcing an invariant
at sites the owning pack itself executes installs on `self.runtime`; a slot enforcing an invariant
at sites **any** runtime can execute installs on every runtime the boot path builds. Decide which
kind the slot is before choosing, and state the choice at the implementation.

**2. Owned identity properties are `from_actor`, `direction`, `sent_at`.**

These three answer "who produced this record" rather than "what does it say". Of the three,
only `from_actor` is a function of the token, so only `from_actor` is derived at the write.
`direction` and `sent_at` are supplied by legitimate callers that the runtime cannot second-guess:
`dual_write_message` writes the inbound copy of a send with `direction="inbound"` under the
sender's own token, and `comm.ingest` carries a transport-supplied `sent_at`. Deriving either
would overwrite a value the owning pack must set. All three are still owned for the purposes of
rules 3 and 4 below.

**3. `update` refuses the owned identity fields, by name, on a pack-owned note kind.**

The owned identity properties are set once at the write and are not patchable afterward. The
refusal is scoped to a patch that **names** one of them: naming is the exact test, because the
patch is folded with `prefer_from`, so a named key would overwrite the stored value while an
unnamed one leaves it untouched. Refusing named keys is therefore exactly sufficient, and the
error names the offending key so a caller can drop that key rather than guess.

Every other property key still merges normally. This matters because it is the only path for
some of them: a pack's own verb surface covers the state that pack transitions, not arbitrary
metadata on an existing record. `gtd`'s entire parameter surface is
`title`/`status`/`priority`/`assignee`/`due`/`depends_on`/`context_entity_id`/`tags`
(`khive-pack-gtd/src/vocab.rs`), with `gtd.transition` taking `id`/`status`/`note` and
`gtd.complete` taking `id`/`result`/`status` — no gtd verb writes an arbitrary key such as a
`blocked_on` annotation onto an existing task. Refusing the whole `properties` object on
pack-owned kinds would remove that workflow's only path and offer none in its place; refusing a
class to close a specific hole is a broader change than the hole justifies.

The refusal lives at the runtime layer in `prepare_update_note_from_snapshot`. Two call sites
reach it: the `update` verb through the guarded
`update_note_from_snapshot_with_embedding_report` path, and the atomic seam in
`atomic_prepare`. Placing the refusal in the kg handler instead would leave the atomic seam open.
No proposal-borne note update converges there, because `ProposalChangeset` carries no note-update
variant at all (`khive-types/src/event.rs`); should one be added, it must route through this
guarded snapshot update path rather than around it. `name`, `content`, `salience` and
`decay_factor` remain patchable on every kind.

The pack-owned kind set is derived from the packs' `NOTE_KINDS` constants
(`PackRegistry::pack_owned_note_kinds`): every note kind declared by a pack other than the
generic-CRUD pack, whose kinds are the general-purpose ones the shared verbs exist to serve.
Nothing is hardcoded but the name of the generic pack, so a pack adding a kind moves the set
with it.

**3b. Naming is the exact test only for object patches. A non-object patch is refused on a
pack-owned kind.**

The sufficiency argument in rule 3 rests on one clause: an unnamed key "leaves the stored value
untouched". That is a property of merging one object into another, and a `properties` patch is not
always an object. A non-object patch — a string, array, number or boolean — names no key, so the
named-key refusal never fires, and the `prefer_from` fold then applies the value directly,
replacing the entire stored property object rather than merging into it. Every owned identity key
is erased by a patch that named none of them. Rule 3's "exactly sufficient" therefore does not
hold as written, and this sub-rule supplies the missing case rather than reinterpreting the one
rule 3 already states.

A JSON `null` is deliberately absent from that list, and saying so is worth the sentence because
the opposite is the natural assumption. `UpdateParams.properties` is an `Option<Value>`, so serde
collapses a literal `null` to `None` at the deserialize boundary: `properties=null` is
indistinguishable from omitting `properties` and is a no-op. The atomic seam reads raw JSON and
replicates that collapse on purpose (`optional_properties` in `atomic_prepare`, which documents
it). Both routes therefore agree, and neither can deliver a `null` to the fold. The same struct
separates the two cases where it wants them separated: `salience` and `decay_factor` sit
immediately above `properties` with a `tri_f64` deserializer, and the absent/null/value
distinction that gives them is documented and asserted for both fields in the `UpdateParams` tests
in `khive-pack-kg/src/handlers/tests.rs`. `properties` has no such deserializer, so it does not
draw that distinction. A `Value::Null` reaches the guard only from an
in-process `NotePatch` constructor, so refusing it is defence in depth against a future
in-process caller rather than a change in caller-facing behaviour.

Rule 4 already recognises the same shape problem on the merge side. Rule 3 was not given the
parallel treatment; this is it.

**The rule: on a pack-owned note kind, a non-object `properties` patch is refused, whether or not
the stored row carries an owned identity property.** The test is the shape of the patch, not its
named keys and not the stored row's contents.

Scoping the refusal to rows that happen to carry identity was considered and rejected. It closes
attribution loss and leaves the same mechanism intact everywhere else, so a pack-owned row holding
only the pack's own bookkeeping stays erasable by a patch that names nothing. That is a narrower
rule that is harder to state and harder to reason about — the caller's error is identical in both
cases, and only the stored row decides whether it is refused. Two further consequences of the
narrow form are worth naming because they are invisible at the call site: a row whose `properties`
is `None`, and a row whose properties object is empty, both carry no owned key, so both take the
permissive path and are replaced wholesale. Neither is a considered affordance; both are the
fall-through of a value-shape arm. A rule whose behaviour turns on stored state the caller cannot
see is a rule callers cannot follow.

The wider form also answers a question the reserved-key direction cannot. If the fixed
three-key owned set is later replaced by pack-declared reserved keys, a reserved-key guard still
answers "no reserved key named" for a non-object patch, because a non-object names nothing at all.
Patch **shape** and patch **content** are independent axes, and no rule about which keys are
protected can subsume a rule about which shapes are legal. Deciding both here keeps rule 3 a single
coherent statement instead of two overlapping amendments to the same rule.

What this costs, stated rather than discovered later: wholesale replacement of a pack-owned row's
property container ceases to have any caller-facing expression. An empty object does not substitute
for it — under `prefer_from` an empty patch merges nothing and leaves every stored key in place. A
caller may still set any key to `null` by naming it. That is the intended shape of the contract:
on a pack-owned kind the property container belongs to the pack, and callers mutate it key-wise
rather than replacing it. Non-pack-owned kinds are unaffected by this sub-rule entirely.

The error names the shape rather than a key, since there is no offending key to name, and says what
to send instead: an object containing the keys the caller intends to set.

**3a. Owned-field membership: derived-at-write and immutable-after-write are different sets.**

- `from_actor` is derived at the write **and** immutable after it.
- `direction` and `sent_at` are derived **nowhere** — they are legitimately caller-set at the
  write (`dual_write_message` sets `direction="inbound"`; `comm.ingest` carries a
  transport-supplied `sent_at`) — yet are still immutable **after** the write, through both
  `update` and `merge`.

The second claim rests on there being no legitimate writer of those fields after creation, which
holds at source. Comm's post-write read bookkeeping does not go through the `update` verb: both
`handle_read` and `handle_reply` call the store-level `set_note_property` with the literal key
`read`. That single-key path prevents unrelated concurrent metadata from being lost, but it does
not make identity mutation legitimate. No production path writes `from_actor`, `direction`, or
`sent_at` after creation; every other occurrence is a creation-site build or a test fixture.

The bound, stated so it is recognised rather than rediscovered: both
`update_note_properties` (whole-document replacement) and `set_note_property` (one-key atomic set)
are store-level paths that bypass the owned-field check entirely. They are safe only while they
have no caller-facing verb surface and pack code limits them to its own mutable bookkeeping keys.
Giving either a caller-facing surface, or using either to write an identity field, reopens the hole
this ADR closes; no identity check belongs in the kind-agnostic storage layer.

**4. A merge cannot rewrite who sent a message.**

On a pack-owned note kind, the surviving note's owned identity properties are restored after the
property fold, under every strategy including `prefer_from`. This is field preservation, not
refusal: everything else still merges per the requested strategy, so legitimate flows —
deduplicating memories, folding duplicate findings — keep working. A key the surviving note did
not carry is dropped rather than inherited, so a record with no attribution does not acquire one
by being merged into.

A property fold does not always yield an object. `merge_json` applies a non-object `from` value
directly under `prefer_from`, replacing the surviving note's whole property object with that value —
a scalar, but equally an array or `null`. None of them can hold the owned identity keys, so
restoration has nothing to write into and the attribution is erased rather than preserved.
Preservation therefore keeps the surviving note's properties in that case instead of the folded
value. Nothing the fold intended is lost,
because a scalar contributes no key that could coexist with the identity it would be replacing.
The guard is scoped to notes that actually carry an owned identity key, so a pack-owned note
without one still folds per the requested strategy.

## What `from_actor` means, precisely

The derived value names **the actor whose token performed the write**. On the proposal-apply
path that token is the applying caller's, threaded in by the apply worker — not the proposer who
composed the changeset. So on that path `from_actor` is honest about who wrote the row and does
not identify who composed the content. Any consumer reading `from_actor` as "who authored this
text" is over-reading it for proposal-applied rows. Closing that gap needs proposer identity
carried on the changeset itself and is out of scope here.

## Consequences

- On the generic create family and proposal apply, a `message` row's derived identity is a
  function of the authorization token, including direct Rust callers of `create_note*`.
  `try_create_note` is the explicit exception: `comm.ingest` supplies transport-derived inbound
  identity before that deduplicating insert, and the generic validator does not run there.
- A bare runtime with no packs installs neither the validator nor the kind set, so all four
  rules are inert there and embedded/unit-test callers keep their current behaviour.
- Packs owning other identity-bearing kinds can install their own derivation without a new
  extension point, subject to the single-occupancy bound stated in 1: the slot holds one
  validator and a second implementer is discarded by registration order with no error, so adding
  one means widening the slot rather than relying on it (same note as
  `install_note_mutation_hook`).
- `update`'s properties patch keeps working on pack-owned kinds for every key except the three
  owned identity fields, so storing working metadata on a task or a message is unaffected. A
  caller that names an owned field gets an error naming that field.
- A `properties` patch on a pack-owned kind must be an object (rule 3b). A caller sending a string,
  array, number or boolean gets an error naming the shape. This is a narrowing of previously
  accepted input: such a patch used to be applied, replacing the whole stored property object.
  `properties=null` is not part of that narrowing and never was, on either the `update` verb or
  the atomic seam: it collapses to "field absent" before reaching the runtime, so it is a no-op
  after this rule exactly as it was before. No caller-facing way to replace the property object
  wholesale remains, deliberately — an empty object merges nothing, and a caller wanting to blank
  a single key names that key with a `null` value, which stores a null under it rather than
  removing the key.
- A pack owning identity-bearing kinds inherits the install-site requirement in 1a: adding a
  boot path, or a runtime inside one, means installing the slot there and asserting it. It also
  inherits the scope decision in 1b: a new slot is owner-scoped or substrate-wide before it is
  anything else, and the two are told apart in source only by a parameter's leading underscore.
