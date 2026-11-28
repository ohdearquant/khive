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
`gap_code`, `act_type`, `status_word`, `purpose`, `confidence`, `vocabulary` and
`frontier_kind` (`view` is withdrawn by Amendment 1, A1.7). Relations between them are existing
relations with endpoint rules this pack declares; where one pair of node types needs more than one
meaning (a step type requires a field, or produces it), the meaning is a `role` in the edge's
properties (fact 4). Each node holds the ADR-221 key `operation/<entity_type>/<canonical name>`,
so a reload finds the node it loaded before.

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

Published bytes hold canonical names only. Rendering a read in a vocabulary (`vocabulary=` on every
read verb) is withdrawn by Amendment 1 (A1.7); names in vocabularies are still taken at `define`.

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

Expected cases and views (#6894 section 3.1, and its section 6 question 4) are withdrawn by
Amendment 1 (A1.7): the caller that asked for them designs its graph fresh and does not need a view
in its own shape. Replay over histories, its scoreboard rows and `conform` stay.

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
   in item 9. Rows: #6894 1, 2, 3 and 9, row 3 as Amendment 1 (A1.4) derives the places and orders
   them, and row 9 for its alias clash only (the rendering in two vocabularies is withdrawn).
4. Requirements and their levels, observations and reopened steps, external check results,
   freshness, `via` and inferred acts. Rows: #6893 3 to 6; #6894 4; #6892 section 4.9 row 7.
5. Variants, rule records with any-of premises, named conditions and gap codes. Rows: #6894 5 to 8.
6. Sources of three kinds, the disagreement list, and `contract` with part digests. Rows: #6893 7;
   #6894 10 and 11.
7. `operation_approvals`, and versions pinned by operations. Rows: #6893 8 and 9.
8. `replay`, scoreboard rows and `conform`. Rows: #6893 10, 11 and 16. (#6894 row 12, expected
   cases through a view, is withdrawn by Amendment 1.)
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

## Amendment 1 (2026-11-27): #6904, bindings are computed, not written

#6904 withdraws the form-to-form mapping row (#6892 part 1, section 3 point 4): two forms' subjects
meet through the slot each resolves to. It changes how a slot meets a form's subjects (its section
2), where a required value may be found (its section 3) and how an outside history records an act
(its section 4), and it no longer asks for four requests of #6894 (its section 6). Its section 7
leaves four decisions to this pack, made in A1.6. Its section 5, facts about the operation itself,
asks nothing yet and stays undecided here.

### A1.1 One role list, in the forms pack

Roles, subject kinds and relations are the term lists a caller loads with `forms.load` (`terms`),
shared by every form of a namespace. Three things are added to them, all in the forms pack:

- A term may carry aliases. A name or an alias resolves to its term; an alias that would resolve to
  two terms of one list is refused at `forms.load`.
- `counting_qualifiers`: the qualifiers that only count ("first", "second", "additional"). Any other
  qualifier may change the meaning. Qualifiers are compared after folding case and spaces.
- `carrying_relations`: the relations by which one subject carries another ("belongs to"). Each is a
  relation term.

A slot names its kind, its role and the relation it is defined by from these lists, as a step names
a field (section 7). `define` refuses a slot whose kind, role or relation is not a term of the
namespace by name or alias (`unknown_term`). From the pull request that builds this, `REQUIRES`
holds `forms`.

### A1.2 A slot is given one of two ways

- By kind and role, with an optional qualifier:
  `{"name": "co_applicant", "kind": "person", "role": "applicant", "qualifier": "additional"}`.
- By a relation to another slot of the same type:
  `{"name": "parcel_owner", "defined_by": {"relation": "owns", "slot": "parcel"}}`.

`define` refuses as `slot_shape`: a kind without a role or a role without a kind, a qualifier
without them, a slot given both ways, a slot defined by a relation to itself, and two slots of one
type with the same kind, role and qualifier (a party that appears more than once is one slot that
repeats). A `defined_by` naming no slot of the type is `unknown_slot`, and a chain of `defined_by`
that comes back to where it started is `slot_cycle`. A slot given neither way is bound only by
`bind`. The new fields are left out of a slot's definition bytes when absent, so the digest of a
type that uses none of them does not change.

### A1.3 `resolve` and `decide`

`operation.resolve(type, version?, form, revision?)` is a read verb. It reads the published type
(the latest version unless one is given), the form revision (the latest unless one is given), the
namespace's terms and the decisions below, and writes nothing. For each subject of the form it
returns the first of these that yields a slot, with how it was found:

1. `decision`: the newest decision naming this type, this form and this subject that still applies
   (below). It gives a slot, or `not_this_type`.
2. `role`: the slots whose kind and role are the subject's, compared as terms. A subject with no
   qualifier takes the slot of its kind and role that has none. A qualifier equal to a slot's takes
   that slot. A counting qualifier that no slot carries takes the slot of its kind and role that
   has no qualifier. Any other qualifier blocks the match: the subject is `undecided`, with the
   slots of its kind and role as candidates.
3. `relation`: a slot defined by relation R to slot S, where the subject carries R to a subject that
   resolved to S.
4. `carried`: the subject carries a carrying relation to a subject that resolved to a slot. It needs
   no slot of its own and is returned with the slot of the subject that carries it.

Rungs 3 and 4 repeat until no subject changes, so a chain of relations resolves. Where a rung
yields more than one slot, or a slot that does not repeat is reached by two subjects, those subjects
are `undecided` with the candidates, and later rungs are not tried for them. A subject no rung
reaches is `no_slot`. Resolve never guesses. The answer names the type version and the form
revision it read.

`operation.decide(type, form, subjects, slot | not_this_type, reason)` records a decision. It
refuses, writing nothing, a subject the form's latest revision does not declare and a slot the
type's latest version does not have. Its rows are in `operation_slot_decisions`: namespace,
decision id, definition id, form id, the subject ids, the type version and form revision decided
against, the slot or `not_this_type`, the reason, the actor, the time, and the ids of the earlier
rows it supersedes (each one that named one of its subjects for this type and form). One decision
may cover several subjects. Resolve reads decisions per subject: for each subject the newest row
that names it decides, so a row that a later row supersedes for one of its subjects still decides
its other subjects. Three triggers refuse a second insert of a decision id, any update and any
delete, as on `operation_definitions` (section 2). Bundles export decisions beside the types
(section 13).

A decision is keyed by definition id, form id and subject id, not by version or revision. It keeps
applying to a later version of the type and a later revision of the form while the subject and the
slot it names both still exist there; where one of them is gone, resolve returns undecided or
no_slot and names the decision that lapsed. In detail: a subject still declared whose decided slot
the version no longer has is returned `undecided`, with the candidates rungs 2 and 3 would offer, or
`no_slot` where there are none, and its `lapsed` field names the decision. No rung binds it
automatically, because the decision was written for a reason the rungs do not see. A decision whose
subject the revision no longer declares is listed under the answer's `lapsed`. A `not_this_type`
decision lapses only with its subject. Nothing is rewritten: a new decision supersedes the lapsed
one.

Arms:

- A1.3-A, #6904 section 2, municipality A. The form's subjects are _applicant_, _additional
  applicant_, _parcel_, _parcel owner_ (owns: parcel), _applicant's mailing address_ (belongs to:
  applicant) and _permit being revoked_. The type's slots are `applicant`, `co_applicant`
  (qualifier additional), `parcel`, and `parcel_owner` defined by "owns `parcel`". Resolve returns
  role, role, role, relation, carried, and `no_slot` for the last; after a decision of
  `not_this_type` for it, the last is `decision`.
- A1.3-B, #6904 section 2, municipality B. A second form with other subject names and the same
  roles resolves the same with no decision written.
- A1.3-C, the lapse rule. The A decisions still apply after the type is published as version 2 and
  the form gets revision 2 with the same subjects. A revision that drops _permit being revoked_
  lists its decision under `lapsed`. With a decision binding a subject to `co_applicant`, a version
  that drops `co_applicant` returns that subject `undecided` or `no_slot`, naming the lapsed
  decision.

### A1.4 Where a required value may be found

This replaces the per-step "where it may be found" of #6894 section 1.2 in section 3's `requires`.

1. Places are derived from `states` (#6892 part 1, section 3 point 2). A (field, slot) a step
   requires may be found in every document type that states that field for a subject resolving to
   that slot by A1.3.
2. An outside system is declared the way a document type is: a record schema with subjects and
   lines naming fields, loaded once in the forms pack and marked as a system. Its subjects resolve
   as a form's do, so one declaration serves every form of every type.
3. `input_missing` still carries the places, now derived. The order between them is the
   deployment's, not a step's: a namespace publishes one `source_order` definition (a row of
   `operation_definitions` with that kind), a list of document types and systems. Places come in its
   order, then those it does not list, in name order. The frontier names the `source_order` version
   it read.
4. Stated in part. Where the field stated is the parent of the field required in the forms pack's
   field hierarchy, the input is reported `stated_in_part`, with both fields and the place, so the
   caller asks only for the difference.

Places are the one derived input not taken from the type's published bytes (section 2). They are a
reading of the forms pack at the time of the read, as resolve is, so a form loaded after a type is
published adds its places without the type being published again. The frontier names the form
revisions it read. Places and `stated_in_part` are fields of the `input_missing` item, never an
item of their own, so they do not change which items a frontier holds or their kinds. Replay
(section 12) compares item kinds and the steps and slots they name, not the places an item lists,
so a form loaded later cannot move replay's counts.

Arm A1.4, #6904 section 3, the portal's six lines. The portal's schema states _structure use_
(residential or commercial). Form A asks _structure use by occupancy class_, a child field.
`frontier` reports that input `stated_in_part` by the portal, and six portal lines written once
answer both municipalities' forms.

### A1.5 Recording an act without naming a step

`record` accepts an act as an act type and a party class or a slot, in place of a step. It looks
for the step among the steps of the operation's type and resolved variant that take that act type
from that party. Where several fit, it records the act on those with no activating condition
(section 4) and reports that it chose, and which. Where every step that fits has an activating
condition, or none fits, it refuses (`act_ambiguous`, `act_unplaced`) and writes nothing. An outside
system's declaration (A1.4 point 2) maps its own wording to an act type and a party class, or to a
status word (section 6), and to nothing else.

Arm A1.5: #6904 gives no case, so the pull request that builds this adds one. A history line
"signed, by the applicant side" on an operation whose type has two signing steps for that side, one
of them conditional, records the act on the other step and reports the choice.

### A1.6 The four decisions of #6904 section 7

1. Resolve is a read verb of its own, not part of `define`. A resolution depends on the form and on
   the decisions recorded so far, and both change after a type is published, while a published
   definition never changes (section 2). Inside `define`, a form loaded later or a decision
   recorded later would not be seen until the type was published again.
2. Decisions live in the store, in the append-only table of A1.3, written by `operation.decide` and
   exported in bundles. Resolve reads them on every call, and the forms pack keeps the decisions of
   `forms.same` and `forms.split` in the store the same way; a store that only indexed the caller's
   files would answer with whatever the files held when they were last loaded. The key is
   definition id, form id and subject, so a decision keeps applying to a later version of the type
   and a later revision of the form while the subject and the slot it names both still exist
   there; where one of them is gone, resolve returns undecided or no_slot and names the decision
   that lapsed.
3. The forms pack owns the role list, with the counting qualifiers and the carrying relations
   (A1.1). With one owner, the role a subject carries and the role a slot asks for are the same
   node, so the join is a node lookup. A third vocabulary that both packs read would have to be
   loaded before either and kept in step with both.
4. What undoes an act belongs to the operation type: the `reopens` of section 6. One outside system
   serves several operation types, and what a rejection voids may differ between them; kept in the
   system's declaration, it would be one table per type again, which #6904 removes.

### A1.7 Withdrawn, and kept

Withdrawn:

- The form-to-form mapping row (#6892 part 1, section 3 point 4).
- Rendering a read in a vocabulary (`vocabulary=` on every read verb, #6894 section 2.1). Names in
  vocabularies stay at `define` (section 7), and #6894 row 9's alias clash is still refused there.
- Views and replay against expected cases (#6894 section 3.1, and its section 6 question 4), with
  the `view` node type. #6894 row 12 is not built.

Kept, because the pack uses them by itself:

- Gap codes as names the caller loads (section 6). The loading from an older vocabulary that #6894
  section 1.4 asked for was never part of this record.
- The rule record of section 4. Section 12's proposals are rules in `proposed`, and the any-of
  premise was decided there on its own terms.
- Replay over histories, its scoreboard rows and `conform` (section 12), which measure proposals.

### A1.8 Where it lands in the build order

- A1.1 to A1.3 are one pull request after item 1 and the forms subjects (#6892 part 1): the slot
  shape, term aliases and the two lists in the forms pack, `resolve`, `decide` and
  `operation_slot_decisions`. Arms A1.3-A, A1.3-B and A1.3-C.
- A1.4 lands with item 3, once the forms pack has document types (#6892 part 2), with
  `source_order`. Arm A1.4.
- A1.5 lands with item 4, beside the status words it maps to. Arm A1.5.
- Decisions in bundles land with item 12.

Not checked:

- A1.4 point 4 reads the forms pack's field hierarchy as a general field containing the fields it
  generalizes (`contains` between two field nodes, read today by `children_of_field` in
  `crates/khive-pack-forms/src/store.rs`). Whether every form a deployment loads places its fields
  in that hierarchy was not checked.
- How much of a form resolves without a decision depends on how normalized the role list is; #6904
  section 8 reports six of eleven rows with one wording and ten with another. Nothing here measures
  the counting-qualifier list either.

## Amendment 2 (2026-11-27): item 3, step types, inputs and names

Serves #6894 rows 1, 2, 3 and 9 and A1.4 (#6904 section 3). Building item 3 left questions that
section 3, section 7 and A1.4 do not settle; this amendment settles them. It changes one sentence of
section 7 (A2.5) and where A1.4 point 3 stores a source order (A2.6), and none of section 3. It
also specifies the two forms reads that A1.3's `resolve` calls and two additions to
`forms.neighbourhood` that A1.4's places read (A2.8), which no record specified before.

### A2.1 A step type published alone, and pinned

`define` takes `step_types` beside `operation_types`. Either list may be empty, not both
(`invalid_document`). A step type given there is compiled and published as its own row of kind
`step_type` (section 2), in the same atomic unit as the document's operation types. It carries `id`,
`version`, `name` (default: the id), `act` (default: the name), `by`, an optional `quantifier`,
`requires` and `produces`. These are the fields item 3 builds. Section 3 gives a step type more: an
effect, the system or party acted on, a document type, `via`, gap codes and citations. Each of those
keeps the item that builds it, and this list is not a new definition of a step type. A step type's
`by`, and the slots its values are about, are slot names: an operation type that pins it must
declare those slots (`unknown_slot`).

An operation type's step either pins a step type, `{name, step_type: {id, version}}`, or is the
in-type shorthand of section 3. A pin resolves against the document's own step types and those
already published in the namespace. Where neither holds it, the pin is refused
(`unknown_step_type`). The digest pinned is read from that row; the caller never gives one.

A pinned step gives no `by`, `act`, `quantifier`, `requires` or `produces` of its own
(`pinned_step_shape`), so what the step is has one definition. `via` is part of what the step is as
well: once an item builds it, a pinned step that gives `via` is refused the same way. A pinned step
keeps `after`, `activated_by`, `unless`, `outcome` and `supplies`, which say how this operation type
uses it. A step type pinned on a repeating slot must carry a quantifier (`quantifier_missing`).

Publishing again an id and version already published with the same digest writes nothing. With
another digest it is refused (`version_conflict`). A document that gives one step type, or one
operation type, twice is refused (`duplicate_type`).

### A2.2 Requirements and products name loaded fields

`requires` and `produces` name a forms-pack field by its canonical name. `define` refuses a field
the forms pack has not loaded in the namespace (`unknown_field`), as it refuses an unknown slot term
(A1.1). A requirement's `value_type` must equal the field's (`value_type_mismatch`), because a
second value type for one field would be a second source of truth for what the field holds. The
check runs at `define`, and the published bytes keep the value type it passed with (section 2).
`define` reads a field's value type from the `field` part of `forms.neighbourhood`'s reply, one read
per distinct field the document names; a field the forms pack does not hold is `unknown_field`.

### A2.3 Order derived from values

As section 3 says, a step that requires (field, slot) which another step of the type produces comes
after that step, and a step with `supplies: {from, field, as}` comes after `from`. A supply's `from`
must be a step of the type (`unknown_step`). Its `as` must be a field the step requires, and `from`
must produce `field` (`unknown_supply`), about the slot the requirement is about
(`supply_slot_mismatch`). A step supplied from itself is a `derived_cycle` of one step.

An edge that `after` already states is not derived again. Cycles are checked in this order: through
`after` alone (`after_cycle`), then through rules (`rule_cycle`), then through derived edges
(`derived_cycle`, naming the steps). A document with cycles of more than one kind is refused with
the first. The published bytes carry each step's derived predecessors (`derived_after`) beside
`after`, so the frontier reads order from bytes only (section 2). Section 3's sentence on a skipped
step holds for derived edges as written.

### A2.4 `input_missing` and `act_owed`

A step whose predecessors are done or bypassed is checked input by input. For a requirement (field,
slot), the fact sought is that field about the identity bound to the slot, or about the operation
when it names no slot. For a supplied requirement, it is the supply's `field`.

A producer that is not yet done holds the step back as `act_blocked`, as today, because it is a
predecessor by A2.3. Where the producer is done and left no fact, or no step produces the field, the
frontier gives one `input_missing` item per missing input, and the step is not `act_owed` while any
input is missing. The item carries: the step; the field and its value type; `by` and the supplier;
the slot and the identity it is about; `produced_by` when a producer exists; and the places and
`stated_in_part` of A1.4. On a repeating slot it is expanded per identity, as section 3 expands
steps. It groups like the step's other items.

### A2.5 A field's names in a vocabulary: a change to section 7

Section 7's fourth paragraph said the forms pack holds a field's names "as the same key". That
sentence changes. The forms pack holds them under its own prefix, `forms/<vocabulary>/field/<name>`,
on the field entity. `forms.load` takes `known_as` on a field and writes them; this pack reads them
and writes none. `operation/<vocabulary>/<entity_type>/<name>` stays for this pack's own nodes.

A key's prefix then names the pack that writes it, so a reader knows whose uniqueness refused a
clash, and neither pack writes under the other's prefix. A field name and an operation node name in
one vocabulary never clash, which is section 7's per-node-type reading already. #6894 row 9's clash
between two fields is refused by the same key uniqueness, inside forms.

A vocabulary may not be named after one of section 2's node types (`reserved_vocabulary`). The
shorthand step type `<operation type>/<step>` has the canonical key
`operation/step_type/<operation type>/<step>`, which a vocabulary named `step_type` could otherwise
collide with.

### A2.6 `source_order`

`define` takes `source_order: {version, places}`, one id per namespace, versioned as section 2
versions definitions. Each place names a document type or system loaded in the forms pack. `define`
refuses a name the forms pack has not loaded (`unknown_place`) and a name given twice
(`duplicate_place`).

A source order is stored in its own append-only table, `operation_source_orders`, keyed by
namespace and version, with the canonical bytes and their digest, and refusing an update, a delete
and an insert over an existing key. It is not a row of `operation_definitions`, as A1.4 point 3
said. A pack's tables are created with `CREATE TABLE IF NOT EXISTS` at every start, so a store
that already holds `operation_definitions` would keep its `kind` check unchanged, and a third kind
would be refused there.

So a source order loads after the document types and systems it names. A deployment that loads a
new document type publishes a new version that names it, and until then that type's places come
after the listed ones, in name order (A1.4 point 3). The frontier reads the newest published version
in the namespace and names it.

### A2.7 A key added to a live entity

Section 2's canonical key and section 7's names are keys on nodes that already exist. ADR-221
assigned a key only at keyed create. ADR-221 Amendment 1 adds one runtime method that adds a key to
a live entity under keyed create's rules. Section 2 and section 7 write through it. It is a runtime
method, not a verb. It answers a key another live entity holds with that entity's id, whatever the
caller may read; the pack that calls it reports `key_conflict` with `existing_id` only when the
caller may list the holder, as ADR-221 section 1 says of a keyed create.

### A2.8 Two forms reads

A1.3's `resolve` reads the namespace's terms and one form revision's subjects from the forms pack,
through two read verbs the forms pack gains with A1.1 to A1.3. Neither writes anything.

- `forms.terms` takes no argument. It returns the term lists `forms.load` takes (`kinds`, `roles`,
  `relations` and `counting_qualifiers`, A1.1), each term with its name, its definition, its aliases
  and `carries`, which is true for a relation by which one subject carries another.
- `forms.subjects(form, revision?)` returns the subjects one form revision declares, in name order,
  each with its id, kind and role and, where it has them, its qualifier, row count (`repeats`) and
  relations. `form` is a form id or `form_id@revision`. With no revision named, it reads the
  revision loaded last. The reply names the revision it read and lists every revision of the form
  in load order. A form or revision that is not loaded is refused, naming it, and so is a revision
  named both in `form` and in `revision`.

A1.4's places and A2.6's `unknown_place` check read two additions to `forms.neighbourhood`, a
read that exists today. Both add output and write nothing.

- On a field, the reply gains `stated_by`: one row per line of the field that states it, each
  `{document_type, system, form, revision, subject, role}`.
- It takes `document_type:<id>` as a node, as it takes `kind:<name>`. The reply's node has type
  `document_type`, with `system` and the forms the type contains. An id that names no loaded
  document type is refused, naming it.

Each changes a published read's output, so each carries a CHANGELOG line and an update to the
verb's description.

### A2.9 Admission rows, and where it lands

No operation verb is added by this amendment. Of Amendment 1's verbs, `operation.resolve` is a read
and joins `VerbRegistry::ADMISSION_DEGRADE_SAFE_VERBS` and ADR-103 Amendment 3's list;
`operation.decide` writes and does not. `forms.terms` and `forms.subjects` (A2.8) are reads and join
both lists. All three land with the A1.1 to A1.3 pull request.

Item 3 lands as four pull requests, after ADR-221 Amendment 1:

- 3a: forms, `known_as` on fields (A2.5), the system mark of ADR-223 and the two
  `forms.neighbourhood` additions (A2.8).
- 3b-1: operation at `define`, A2.1 to A2.3.
- 3b-2: operation at the frontier, A2.4 and A2.6, with A1.4's places and `stated_in_part`.
- 3c: operation, the working type graph with section 2's canonical keys and section 7's names.

Each refusal code this amendment adds has an arm, in the pull request that adds the code; the
refusal arms of one section may be rows of one table test.

- A2.1: a pinned step type shared by two operation types. Refused: a pin neither the document nor
  the namespace holds (`unknown_step_type`); a pinned step that gives `by` (`pinned_step_shape`); a
  step type with no quantifier pinned on a repeating slot (`quantifier_missing`); an id and version
  published again with another digest (`version_conflict`), beside the same digest writing nothing;
  one step type given twice in a document (`duplicate_type`); a pinned step type whose `by`, or a
  slot one of its values is about, the pinning operation type does not declare (`unknown_slot`).
- A2.2: a requirement naming a field the forms pack has not loaded (`unknown_field`), and one whose
  value type is not the field's (`value_type_mismatch`).
- A2.3: a cycle through derived edges, refused as `derived_cycle` naming its steps; a supply whose
  `from` is not a step of the type (`unknown_step`); a supply whose `as` the step does not require
  (`unknown_supply`); a supply whose `from` produces the field about
  another slot (`supply_slot_mismatch`); a document with an `after` cycle and a derived cycle,
  refused as `after_cycle`.
- A2.4: the #6893 fixture's `issue`, which waits on a value `sign` produces.
- A2.5: a field clash refused in one vocabulary and accepted across two; a vocabulary named
  `step_type` (`reserved_vocabulary`).
- A2.6: an unknown place (`unknown_place`) and a place given twice (`duplicate_place`).
- A2.8: `forms.terms` returns a term's alias and a carrying relation's `carries`; `forms.subjects`
  reads a named revision and, with none named, the revision loaded last. Refused by
  `forms.subjects`: a form that is not loaded, a revision that is not loaded, and a revision named
  both in `form` and in `revision`. `forms.neighbourhood` on a field lists each stating line in
  `stated_by`, and on `document_type:<id>` names the type, its system mark and its forms; an id
  naming no loaded document type is refused.
