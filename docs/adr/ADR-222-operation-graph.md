# ADR-222: Operation Graph

- **Status:** Proposed
- **Date:** 2026-11-27
- **Grounding commit:** `fe344bd5789a84f5255100b494b6b8df103fa814`
- **Relates to:** [ADR-221](ADR-221-entity-keys-and-aliases.md) (entity keys carry the names of
  type-level nodes), [ADR-208](ADR-208-charter-workflows.md) (the charter pack, whose definition and
  evidence rows this pack follows without depending on it)

It does not change the status of any other record.

## Context and scope

Three issues ask for one new pack.

- [#6892](https://github.com/ohdearquant/khive/issues/6892) asks, in the table of its section 0, for
  "the operation graph: operation types, operation instances, one read verb `frontier`", in "a new
  pack", and sets it out in section 4. Its parts 1 and 2 (subjects, the keyed check, document types)
  belong to the forms pack and are not decided here.
- [#6893](https://github.com/ohdearquant/khive/issues/6893) asks that "the graph is the base; a
  contract is a view of it" and that "the graph is expected to learn": requirements, one predicate
  shape, repeating steps, `by` and `via`, recorded and inferred acts, a contract projection with a
  digest per part, approval by part, replay, proposals with a measure, conformance and untyped
  operations.
- [#6894](https://github.com/ohdearquant/khive/issues/6894) asks that "an existing body of step
  definitions can live in it": step types as their own versioned definitions, `requires` and
  `produces` with derived order, freshness, gap codes, variants, rule records, named conditions,
  names in several vocabularies, a `stated` source, observed sources with their base, views with
  replay against expected cases, and bundles that round-trip with fixtures shipped as data.

The three are read as one design. Where a later issue changes an earlier one, the later one
governs and the change is named below.

Facts at the grounding commit:

1. No pack owns operations. The workspace has 22 `khive-pack-*` crates and none of them is an
   operation pack.
2. The edge vocabulary is closed: "The closed [`EdgeRelation`] enum is not extensible"
   (`crates/khive-types/src/pack.rs` line 475). A pack adds endpoint rules, not relations.
3. A change set holds six kinds of operation: `Create` (of an entity or a note), `Link`, `Update`
   (of an entity, a note or an edge), `Delete`, `Restore` and `Merge`
   (`crates/khive-changeset/src/op.rs` lines 17-23 and 40-42). None of them writes a row of a
   pack-owned table, so `propose`, `review` and `withdraw` reach entities, notes and edges only.
4. A linked edge in a change set carries properties: `LinkOp` has
   `properties: BTreeMap<String, PropertyValue>` (`op.rs` lines 85-92). The forms pack's own
   `link` passes an empty map every time (`crates/khive-pack-forms/src/store.rs` lines 551-559).
   So an edge a pack writes can carry a role in its properties; whether GQL filters on edge
   properties was not checked.
5. Charter definitions are keyed `(policy_domain, charter_id, version)`, carry
   `definition_digest`, `definition_bytes` and `source_bytes`, and three triggers refuse a second
   insert, any update and any delete with "charter definitions are immutable"
   (`crates/khive-pack-charter/sql/001-charter-definitions.sql`). The same table requires
   `template_id`, `gate_registry_digest` and `action_contract_version`, which belong to the
   charter engine.
6. Charter evidence rows have a dense per-run `sequence`, `producer`, `source_identity`,
   `source_event_id`, an observation window, `completeness`, a payload digest and bytes, and
   `supersedes_sequence`, unique per producer, kind and source
   (`crates/khive-pack-charter/sql/005-charter-evidence.sql`).
7. `charter_subjects` carries `forge`, `repository_id` and `pull_request_id`
   (`crates/khive-pack-charter/sql/002-charter-subjects.sql` lines 7-9).
8. An entity key is unique per `(namespace, kind, key)` over held rows, and a caller that wants a
   key per type puts the type in the key. An alias may sit on several entities; aliases have no
   vocabulary (ADR-221 sections 1 and 2).
9. The forms pack stores every node as a `concept` entity told apart by `entity_type`.

Out of scope: rendering a projection into any contract language; inducing operation types from
traces; running external checks; a charter phrase that waits on a frontier; and the later items of
#6892 section 6 point 5 (computed lines, channel, form-to-form mapping).

## Decision

### 1. A new pack, `operation`

The pack is `khive-pack-operation`, verbs `operation.*`, `REQUIRES = ["kg", "forms"]`. It records
and derives; it performs nothing, opens no task and runs no check. It has four layers:

- the **working type graph**: entities and edges in a type namespace, which `define` writes and
  kg change sets can change;
- **published definitions**: immutable rows in a pack-owned table, one per version;
- **operation records**: append-only rows in pack-owned tables, in an operation namespace;
- **derived reads**: `frontier`, `check`, `contract`, `replay`, `conform`, `trace` and `enter`,
  computed on every call and never stored, except the scoreboard row of section 12.

This answers #6892 section 5:

> Question 4: A new pack, or grow charter? Lean: "A new pack. Charter is procedural and already carries one
> domain's columns; the operation graph is declarative. Share the evidence row design."

Decided as the lean. The domain columns are fact 7; the evidence row design is shared in section 9.

### 2. The type graph, versions and pins

Every type-level node is a `concept` entity in the type namespace with an `entity_type` from:
`operation_type`, `variant`, `slot`, `step_type`, `requirement`, `rule`, `named_condition`,
`gap_code`, `view`, `act_type`, `status_word`, `purpose`, `confidence`, `vocabulary` and
`frontier_kind`. Relations between them are existing relations with endpoint rules this pack
declares; where one pair of node types needs more than one meaning (a step type requires a field,
or produces it), the meaning is a `role` in the edge's properties (fact 4). Each node holds the
ADR-221 key `operation/<entity_type>/<canonical name>`, so a reload finds the node it loaded
before.

Publishing compiles one operation type or one step type, with everything it refers to, into
canonical definition bytes and writes one row of `operation_definitions`: namespace, definition id,
kind (`operation_type` or `step_type`), version, digest, definition bytes, source bytes, the part
digests of section 11 and the time. The key is `(namespace, definition_id, version)`. Three
triggers refuse a second insert, any update and any delete, as the charter triggers do (fact 5).

An operation type's definition bytes pin the id, version and digest of every step type it lists.
Publishing a new version of a step type changes no operation type until that operation type is
published again pinning it. Every derived read works from published bytes only, never from the
working graph, so an edit to the working graph changes nothing an operation sees until it is
published.

This answers #6893 section 6:

> Question 1: Approval and versioned definitions: in the operation pack, or by extending the charter pack?
> Lean: "Reuse the charter pack's definition identity (domain, id, version, digest) and its refusal
> rules. One place for 'a published version is immutable'."

Decided: the identity and the refusal rules are reused as a design, in this pack's own table, with
the namespace in the place of the policy domain. This departs from "one place": the charter table
requires three columns that only the charter engine can fill (fact 5), so writing operation types
there would make this pack depend on charter and fill those columns with placeholders. The rule
itself stays one rule, stated by the same three triggers in both tables.

### 3. Step types

A **step type** is its own node and its own published definition. It has a name, a version, an
effect (a term from a list the caller loads), the system or party it acts on, an act type, an
optional document type, the slot that owes the act (`by`), an optional slot whose system carries it
(`via`), its `requires`, its `produces`, its gap codes and its citations.

- `requires` is a list of (field, per which slot, value type, places it may be found). Places are
  document types and systems in order of preference, each with a citation.
- `produces` is a list of (field, per which slot, class), the class one of `read`, `computed` or
  `stated`.

Order is derived: a step that requires what another produces comes after it. Where producer and
consumer name the field differently, the operation type says `supplies: {from step, field, as
field}`. `after` (#6892's "another step is done") remains for steps between which nothing flows.
`define` refuses a cycle and names the steps on it. A step that a rule skips is not asked, and a
step that comes after it comes after that step's own predecessors instead: in the #6893 fixture,
with `submit` skipped online, `issue` comes after `sign`.

A step declared on a repeating slot is expanded per bound identity of that slot, and per pair when
it names two repeating slots. The quantifier is `all_of` or `at_least_one`. A frontier item carries
the identities it was expanded for.

Every frontier item names its supplier, the slot that owes the act (`by`) and, when the step
declares one, the slot whose system carries it (`via`). `frontier(group_by=...)` groups the items by
`supplier`, the default, by `by` or by `via` (#6892 section 4.5, #6893 section 1.4).

This answers #6894 section 6, which gives no lean:

> Question 1: Whether a step type is a new node type or the existing step grown; and whether part 1's in-type
> step stays as shorthand for a step type used once.

Decided: a new node type, and the in-type step stays as shorthand. No step exists in code yet
(fact 1), so nothing is grown. An in-type step compiles to a step type with the id
`<operation type>/<step name>`, published with that operation type and listed by it alone. Every
derived read sees step types only, so the shorthand adds no second code path.

### 4. Predicates, named conditions and rules

One predicate shape serves every condition: a rule's premise, a requirement, a variant's selector
and a named condition. A predicate is a conjunction of terms, each one of the six #6893 section 1.2
lists (a fact present or absent for a slot and field; a fact equal to a value or one of a set; two
facts compared under the field's comparison; a step done or not done; an observation with a status
word; a named external check with a recorded result), plus two more: "named condition C holds",
and "an operation of type U has completed for the same identities", written
`{operation: U, match: {<slot here>: <slot there>}}`, which reads the operations in the token's
visible namespaces. The last is the third precondition kind of #6892 section 4.2.

A **named condition** is a name and a predicate, defined once and referred to by name; `define`
refuses a cycle between named conditions. `frontier` and `replay` report, per operation, which
named conditions held.

A **rule** is a record `(step, action, owner, when, unless, citation, status)`. The action is
`activate`, `skip`, `hold` or `require`; the status is `proposed`, `active` or `retired`, and only
`active` rules are evaluated. `when` and `unless` are each a list of conjunctions, any one of which
holds. A step's `activated_by` and `unless` (#6893 section 1.2) compile to rules, so there is one shape.

This answers #6894 section 6, which gives no lean:

> Question 3: Whether top-level disjunction in a rule's premise is acceptable, against part 2's "no
> disjunction".

Decided: accepted, at the top of a rule's `when` and `unless` only; `define` refuses it anywhere
else. #6893 refused disjunction so that "every term can be traced to a record"; each alternative is
still a conjunction whose terms each name the record they read, and a fired rule reports the index
of the alternative that held. One rule with several premises is the same as several rules sharing
one citation and one owner, so nothing becomes expressible that was not before.

This answers #6893 section 6:

> Question 4: External checks: recorded results only, or a callable registry? Lean: "Recorded results only.
> Who runs the check and how is outside the pack; the charter vocabulary already has the idea of a
> registered producer."

Decided as the lean. A check result is an observation of a declared check name, with its producer.

### 5. Requirements and freshness

A **requirement** on an operation type has a name, a predicate, a level (`hard`, `soft` or
`advisory`), the slot that resolves it, and optionally a freshness limit on each fact it reads. The
frontier reports three distinct items for it:

- `requirement_unknown`: a fact it reads is absent, so somebody has to look;
- `requirement_unmet`: the predicate does not hold, citing the facts it read and the supplier;
- `fact_stale`: a fact it reads is older than its limit at the as-of time; the fact stays readable,
  and the item names the read step that produces it and the slot that performs that step.

This answers #6894 section 6, which gives no lean:

> Question 2: Whether freshness belongs on the requirement (asked here) or on the field.

Decided: on the requirement. A field is role-free and shared by every form and step that asks for
it (#6892 section 2.1). How old a value may be depends on what it is used for, and a limit on the
field would bind every use to the strictest one. A read step's produced facts carry when they were
observed and from which source, and nothing else.

### 6. Variants and gap codes

An operation type may have **variants**, each a list of step types and a selecting predicate.
Shared step types are named once. `frontier` resolves the variant first. While a deciding fact is
missing it returns one `variant_undecided` item, naming that fact and the places it may be found,
with the steps common to every variant. The resolved variant is a record of the operation; a change
of variant is a later record, never an edit.

A **gap code** on a step type has a stable name, a selector (one of the step's inputs or
requirements, and a state among `unknown`, `unmet`, `stale` and `conflict`) and the party class that
resolves it. A frontier item that matches a selector carries the code. An item that matches none
carries an empty code and is counted in the frontier's `uncoded` total.

A slot may name the operation type that produces what it binds (`opened_by: U`). While such a slot
is unbound and no completed operation of type U binds an identity for it, the frontier reports one
`needs_operation` naming U in place of `slot_unbound`; opening that operation is the caller's
decision.

An observation can undo a done act. The operation type declares, per status word, the steps that
word reopens (`reopens: {<status word>: [<step>, ...]}`). An observation carrying such a word marks
every done act of those steps observed before it `void`, citing the observation, and the steps are
owed again. The acts stay in the history (#6892 section 4.9 row 7).

### 7. One node, names in several vocabularies

Every type-level node has one canonical name and any number of `known_as: (vocabulary, name)`. Each
name is held as an ADR-221 key `operation/<vocabulary>/<entity_type>/<name>` on the node. A second
node taking the same name in the same vocabulary for the same node type is refused by the key's
uniqueness, which is the clash #6894 section 2.1 asks to refuse, and resolving a name is a key
lookup. ADR-221 aliases are not used for this: an alias may sit on several entities (fact 8), the
opposite of what is asked.

The clash is read per node type: one vocabulary may use the same word for a step type and a field,
because every name a read returns says which node type it names.

Published bytes hold canonical names only. Every read verb takes `vocabulary=` and renders names in
it at read time, falling back to the canonical name with `name_fallback: true`. The same frontier
read in two vocabularies therefore differs in names only.

Fields are forms-pack nodes. Their names in a vocabulary are written by the forms pack, which owns
field entities: `forms.load` takes `known_as` on a field and holds each name as the same key on the
field entity, and this pack only reads them. That forms change lands with the names pull request.

### 8. Sources

Every slot, requirement, step type, rule and precondition carries one or more sources of three
kinds:

- `declared`: a document type or form revision, a page, the quoted text and the source digest;
- `observed`: a count, the base it was counted over, a reference to that set, the window, the date
  and the digest of the replay that produced it, never the instances. A later observed source may
  name the one it supersedes, which stays readable;
- `stated`: who (an opaque reference), when, in answer to what, and a confidence.

`operation.check(type=..., mode="sources")` lists the lines whose kinds disagree: the necessity a
declared source prints, the share an observed source counts (against a threshold the call passes)
and the necessity a statement gives. That list is the question sheet #6894 section 2.2 asks for.

This answers #6894 section 6, which gives no lean:

> Question 5: Whether `stated` confidence is a list the caller loads or a fixed three.

Decided: a list the caller loads, ordered from strongest to weakest, with at least one term. Act
types, status words and gap codes are loaded the same way, and a fixed three would put one
domain's words in the pack.

### 9. Operations and their records

Two pack-owned tables hold the instance side.

- `operations`: namespace, operation id, the definition id, version and digest it was opened under
  (all null for an untyped operation), the parent operation, and when it was opened.
- `operation_records`: namespace, operation id, a dense `sequence` per operation, kind (`binding`,
  `document`, `fact`, `act`, `observation`, `variant` or `typing`), a caller key unique per
  operation, the payload digest and bytes, when it was observed, when it was recorded, and
  `supersedes_sequence`. Triggers refuse a replace, an update and a delete and enforce the dense
  sequence, as charter evidence does (fact 6).

An identity is an opaque reference (label, kind, external key), optionally with an entity id when
the deployment wants it searchable. A fact's value may be a digest and an external reference in
place of the value. State changes of an operation (`completed`, `cancelled`, `superseded`) are
observations like any other. Recording the same caller key twice writes one row.

Every record carries two times: `observed_at`, when it happened as the caller states it (its
recorded time when none is stated), and `recorded_at`, when the store took it. An as-of read applies
the records observed at or before the as-of time, in order of observed time and then sequence.
`basis="recorded"` applies instead what the store held at that time, in sequence order. Records
recovered after the fact carry their observed time, so for them sequence order is not time order.

Acts are derived as `recorded` or `inferred`: when a later step is recorded done, each step it comes
after that has no record is reported done, `inferred`, with the record it was inferred from.

An operation may be opened with no type. Nothing is derived for it except its trace:
`operation.trace` returns the multiset and partial order of (act type, document type, role of the
actor) with no identity and no value, and `operation.traces(filter)` returns traces with counts. A
`typing` record binds it to a type and version later; its earlier records are not rewritten.

This answers #6892 section 5:

> Question 3: Operation instances: entities and notes, or pack tables? Lean: "Pack tables, append-only with a
> dense sequence, as charter evidence. Identities may be ordinary entities (`person`, `org`,
> `artifact`) when the deployment wants them searchable, and opaque references when it does not."

Decided as the lean.

### 10. Isolation

Operation records go only to the caller's write namespace, and `open` and `record` refuse when that
namespace is the one holding the pinned definition. Instance rows live in pack tables, which no kg
verb reads, so a token that sees only the type namespace receives no instance row from any kg verb;
every `operation.*` read filters by the token's visible namespaces. Nothing from an instance flows
into the type graph except the scoreboard row below.

`operation.replay` runs with a token whose write namespace is the type namespace and whose visible
set includes the operation namespaces it reads. It reads their records and writes one scoreboard
row: counts, the population digest, the definition digest and the date.

This answers #6893 section 6:

> Question 3: Does the pack run replay over operations in other namespaces? Lean: "It runs in the operation
> namespace and writes only the scoreboard row (counts, digests) to the type namespace. That is the
> one sanctioned flow from instances to types, and it carries no instance content."

Decided with a departure from the lean, which has replay run in the operation namespace. A token
writes to one namespace and the scoreboard row lands in the type namespace, so the replaying token
writes the type namespace and sees the operation namespaces. #6892 section 4.7 point 1 refuses
visibility from the type side into instances; that holds for every type-only token, and the
replaying token is a third kind, held by whoever a deployment trusts to measure.

A stated limit: what the runtime gives a token today. A call that names no namespace writes to
`local` and reads `local`, the namespaces configured as visible to the caller and the actor's own
namespace. A call that names a namespace gets a token for that one namespace, for reading and for
writing, with nothing visible beside it. So the pack publishes types with the type namespace named,
and opens and records operations with no namespace named, by a caller whose visible set includes the
type namespace. A caller that names the type namespace reads no operation at all: `frontier` and
`record` refuse the operation as not found, which is how #6892 section 4.9 row 13 is tested. The
replaying token above, which writes the type namespace and reads operation namespaces, is not one
the runtime can issue today, so `operation.replay` waits on a runtime change that lets one token
write one namespace and read others; its pull request names that change.

### 11. Contract projection and approval by part

`operation.contract(type, version, vocabulary?)` returns the published type as an ordered
structure: slots, requirements, steps with who performs each and what each comes after, the
outcome, and the sources of every line. Steps are in topological order of the derived order, ties
broken by declaration order. The parts are the requirements, the steps performed by each slot, and
the outcome; the result carries a digest of the whole and one per part. It is computed from the
published bytes, so one version always yields the same bytes.

Approvals are rows of `operation_approvals`: namespace, definition id, the version approved, part,
part digest, actor, role and time. They are matched by part digest, not by version; the version
column records which version the approver read. A new version whose part keeps its
digest therefore keeps that part's approvals with nothing copied, and only changed parts are asked
again. The operation type declares which roles may approve each part. A version is approved when
every one of its parts has an approval by an allowed role for that part's digest. An operation's
frontier and check use the version it was opened under, and both report its digest.

This answers #6893 section 6:

> Question 2: Is a contract projection also a charter source, so the charter engine can run it? Lean:
> "Probably yes later: the projection's ordered steps and requirements are close to phases with
> `require` and `await`. Not needed for the first version; the structure and digests are."

Decided as the lean: not in this record.

### 12. Measurement and proposals

`operation.replay(type, version, operations, label)` walks each operation's records in observed
time order (section 9) and, at each label observation, compares the frontier as of that
observation's time with the label. It
returns counts only: moments, agreed, agreed exactly, per label value, and the misses grouped by
(label, frontier) pair. A second run over the same population returns the same counts. It refuses
a definition any of whose predicates reads the label kind, naming the term.

Replay also takes **expected cases**: facts and bindings in, an expected view record out, with no
history. It reports, per field of the view record, agreement, **false clear** (nothing owed where
something is expected) and **discrimination** (two cases with different expected records must not
get the same output).

A **view** is data: a verdict chosen by ordered predicates over the frontier item kinds present,
lists of items filtered by party class or kind, and the list of applicable step types.
`operation.frontier(view=...)` returns that record.

This answers #6894 section 6, which gives no lean:

> Question 4: Whether a view is data the pack interprets, or left to the caller with only the replay measures
> in the pack.

Decided: data the pack interprets. Agreement, false clear and discrimination are measured per field
of the view record, so the pack has to produce that record to measure it; with the view in the
caller, replay would measure only what the caller reports.

`operation.conform(operation)` lists how a history departs from its type: acts with no step, steps
done before a precondition held, required steps neither recorded nor inferred, and requirements
unmet when a later step was done. Over a population it returns the departures with counts, largest
first, and no instance content.

`operation.propose(type, change, population?)` builds a kg change set over the working type graph,
which is made of entities and edges and so is in reach of change sets (fact 3). It compiles the
changed type without publishing it, replays the current and the changed version over the same
population, and submits the change set through kg `propose` with both scoreboard rows attached.
Approval applies the change set and publishes the new version in one transaction; the approvals of
unchanged parts stand by section 11 and the changed parts await approval. A rejected proposal stays
as history with its measure, and `propose` refuses the same change against the same base again
unless its evidence differs.

### 13. Bundles and fixtures

`operation.define` loads a directory bundle: one JSON file per definition, read in byte order of
path. The canonical form of a file is sorted keys, two-space indentation, LF line ends and one final
newline. A published row keeps each file's source bytes, and `export` writes the bundle back. For a
bundle in canonical form, `define` then `export` returns the same bytes. For any other file,
`define` reports `canonical: false` with its path, and export writes the canonical form, so a
deployment commits the canonical files once.

The pack ships its acceptance fixtures as data under `crates/khive-pack-operation/fixtures/`: the
#6893 section 4 fixture, the three domains of #6892 section 4.8 and the permit example of #6894,
each as types, records, as-of times and expected frontiers. A test in the pack runs every fixture,
and a caller can run the same files against a stand-in of its own. The #6893 fixture lands with
the first pull request and the others land as data in the second, before the verbs that run them;
a test runs every fixture whose verbs exist and names each one still waiting.

Operations export apart from types (#6892 section 4.7 point 4). `operation.export_records(filter)`
writes operations and their records with sequences, caller keys and both times, and a value given by
reference as its digest and reference only. `operation.import_records(document)` loads such a
document into a store holding the same published definitions, and refuses, writing nothing, when
any operation in it is already present. Importing an export into an empty store and exporting again
gives an equal document and equal frontiers.

### 14. What the pack ships, and what it leaves to the caller

This answers the remaining questions of #6892 section 5:

> Question 1: New relations under a closed enum: new endpoint rules on existing relations, role in edge
> metadata, properties, or more relations? Lean: "Role in edge metadata on one existing relation
> per pair of node types, if pack-written edges can carry metadata (not verified). A property
> holding a subject id is the fallback and matches how `filled_by` is stored now."

Decided as the lean. A pack-written edge can carry properties (fact 4), so the working type graph
puts a role in edge properties (section 2). The forms pack's subjects keep the fallback, a
property holding a subject id, as they are being built.

> Question 2: Does `document_type` live in the forms pack or beneath it? Lean: "In the forms pack, as the
> parent of `form`. A separate pack for one node type costs more than it gives."

Decided as the lean. It lands as its own forms pull request. Until it lands a step type omits its
document type, and `define` refuses a document type it cannot resolve.

> Question 5: Does the pack ship any act types or status words? Lean: "`fill` and `sign` at most. Everything
> else is loaded."

Decided: it ships none, which is within the lean. The only names the pack owns are its frontier
kinds, because they are its output.

> Question 6: Is `frontier` allowed to open tasks? Lean: "No. It returns the grouped list; a caller or a thin
> verb turns groups into tasks."

Decided as the lean.

> Question 7: Induced operation types: in scope? Lean: "Storage and the `declared` / `observed` mark, yes. The
> induction itself, no."

Decided as the lean, with `stated` added as the third mark (section 8).

### 15. Entry by text

`operation.enter(text?, purpose?)` returns candidate operation types, ranked, each with why it
matched and the forms it touches. Text resolves to purposes through their canonical names and their
names in vocabularies (section 7), and to forms through `forms.resolve`; a purpose leads to the
operation types and the forms that declare it (#6892 section 2.2 point 14). It writes nothing, and
the query text is not stored (#6892 section 4.7 point 3).

## Migration

None. The pack is new and its tables are created by its own schema plan. Loading it changes no
existing table, and a store without it is unchanged.

## Build order

One pull request per row group, in this order. Each names the acceptance rows it passes.

1. The pack, `operation_definitions` with its triggers, `define` and publish for operation types
   with slots and in-type steps (`by`, `after`, rules over fact and step terms), repeat expansion,
   `open`, `bind`, `record` (bindings, facts, acts, both times), and `frontier` with `act_owed`,
   `act_blocked`, `slot_unbound`, as-of and grouping by supplier; the #6893 fixture as data. Step
   types already exist here: an in-type step compiles to a step type that the operation type pins,
   so every derived read sees step types from the start and nothing of this pull request is rebuilt
   later. `define` here writes the published row only, and `REQUIRES` is empty: nothing in this
   pull request reads kg or forms. Rows: #6893 1 and 2; #6892 section 4.9 rows 13 and 14.
2. The fixtures as data, before the verbs that run them: the three domains of #6892 section 4.8,
   the permit example of #6894 and the whole #6893 section 4 table, each with expected frontiers.
   The test runs those whose verbs exist and names the rest; each later pull request turns on the
   ones it passes.
3. Step types published on their own, `requires`, `produces`, `supplies`, derived order and
   `input_missing`; the working type graph of section 2, written by `define` with each node's key;
   names in vocabularies, with the forms change for field names. `REQUIRES` becomes `["kg",
   "forms"]` here. Keys are written through the runtime's entity key operations, as kg `create`
   writes them; a change set carries no key operation, so how `propose` changes a key is settled
   in item 9. Rows: #6894 1, 2, 3 and 9.
4. Requirements and their levels, observations and reopened steps, external check results,
   freshness, `via` and inferred acts. Rows: #6893 3 to 6; #6894 4; #6892 section 4.9 row 7.
5. Variants, rule records with any-of premises, named conditions and gap codes. Rows: #6894 5 to 8.
6. Sources of three kinds, the disagreement list, and `contract` with part digests. Rows: #6893 7;
   #6894 10 and 11.
7. `operation_approvals`, and versions pinned by operations. Rows: #6893 8 and 9.
8. `replay`, scoreboard rows, views and expected cases, and `conform`. Rows: #6893 10, 11 and 16;
   #6894 12.
9. `propose` with a measure, and approval that publishes. Rows: #6893 12 and 13.
10. Untyped operations, `trace`, `traces` and `typing`. Rows: #6893 14 and 15.
11. Preconditions across operations and `needs_operation`; documents, conflicts, outcomes and
    values by reference, once #6892 part 2 has landed. Rows: #6892 section 4.9 rows 1 to 6 and 8 to
    12.
12. Bundles and `export`, and `export_records` with `import_records`. Rows: #6894 13 and 14; #6892
    section 4.9 row 15.
13. `enter`. #6892 gives it no acceptance row; its pull request adds one.

## Acceptance

The acceptance tables of #6892 section 4.9, #6893 section 4 and #6894 section 4, row for row, each
in the pull request the build order above names, with these readings:

- #6893 row 13 "version 2 published": publication happens when the proposal is approved, in the
  same transaction (section 12).
- #6894 row 13 "byte for byte": for a bundle in canonical form (section 13).
- #6894 row 9 "an alias clash is refused at `define`": a clash within one node type (section 7).
- #6892 section 4.7 point 1 says "no instance row may be written into a namespace that a type-only
  token can read". Which tokens can read a namespace is set per token by configuration, which the
  pack does not see, so the pack enforces the part it can check: `open` and `record` refuse when the
  write namespace is the one holding the pinned definition.
- "As of" (#6892 section 4.5, row 12) and replay's "in time order" (#6893 section 3.1) read the
  observed time of each record (section 9).

## Not checked

- Whether GQL can filter on edge properties.
- Whether kg `review` can publish a pack definition inside the transaction that applies a change
  set, and whether `propose` carries extra payload such as scoreboard rows. If it cannot, PR 8
  decides between a pack hook on apply and an operation verb that applies the change set and
  publishes in one transaction, and says which.
- The charter engine's own evaluation code; the reuse in sections 2 and 9 is of its tables only.
