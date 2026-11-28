# ADR-223: Forms Document Types

- **Status:** Accepted 2026-11-28 (#6939)
- **Date:** 2026-11-27
- **Grounding commit:** `838beb7e5ab97299416b44e3cae7eab18e78244a`
- **Serves:** issue [#6892](https://github.com/ohdearquant/khive/issues/6892) section 3 (part 2 of
  the forms work: document types)
- **Relates to:** [ADR-222](ADR-222-operation-graph.md) (section 14, question 2: the document type
  lives in the forms pack as the parent of `form`, in its own forms pull request),
  [ADR-221](ADR-221-entity-keys-and-aliases.md) (the content key a node is found by)

It does not change the status of any other record.

## Context and scope

Issue #6892 section 3 asks the forms pack to say what kind of document each form revision is,
where documents of that kind come from, which values a document of a kind carries printed on it,
and which documents a form asks to have attached. ADR-222 section 14 placed the document type in
the forms pack and left the rest to the forms pack, whose decisions it does not take (its context
section). This record decides how a document type is stored, what `forms.load` takes, and what
`forms.check` and `forms.lint` do with it.

Facts at the grounding commit:

1. Every forms node is a `concept` entity told apart by `entity_type` and found by a content key
   computed from its properties (`Graph::key` in `crates/khive-pack-forms/src/store.rs`). Export
   and replay walk the pack's list of types and that key, so a new type in both is exported and
   replayed with no other change.
2. A form item carries an `item_kind`. `forms.check` asks only items whose kind asks for a field,
   so an `attachment` item is never asked and no answer attaches a document.
3. `contains` is an existing relation with endpoint rules in the pack: a `form_issuer` contains its
   forms.

## Decision

### 1. A document type node

A tenth forms type, `document_type`, is a `concept` entity stored as the form node is, with
`document_type_id`, a name, an `origin` (`applicant`, `third_party` or `work`) or, in its place,
`system: true`, and an optional `issuer_class`. Its key is `document_type:<document_type_id>`, so a namespace holds one per id. A
document type contains its form revisions by the existing `contains` relation, under one added
endpoint rule (`document_type` contains `form`); no relation is added. A revision stays a `form`
node whatever its type, because renaming a stored type would break loaded stores.

### 2. Load declares the type or mints one

`forms.load` takes an optional `document_type` object, `{id, name, origin, issuer_class?}` or
`{id, name, system: true, issuer_class?}`. A declaration carries an origin, or `system: true` in its
place, as ADR-222 Amendment 1 A1.4 point 2 decides. A system type has no origin, and its lines only
state: a load or `forms.add_item` that gives a form of a system an asking or requiring line is
refused. A type minted for a loaded form is never declared a system, because its forms were loaded
as forms to fill in.

- When it is absent, the load mints a type whose id is the form id, whose name is the title, whose
  origin is `applicant`, and which carries `minted: true`. The origin is a guess, and the mark keeps
  the guess from reading as a declaration. A later revision of the same form id reuses the type.
- A declaration whose id names a minted type replaces the guess: the name, origin and issuer class
  become the declared ones and the mark is dropped.
- A declaration whose id names a declared type must agree with it in name (compared case- and
  space-folded), origin and issuer class. Otherwise the load is refused, and the message gives both
  sides. A load is one transaction, so a refused load writes nothing.

The mark is a property of the node, so export and replay carry it.

### 3. What a line does with its field

An item gains `relation`: `asks` (the default, and every item loaded before this record), `states`
or `requires`.

- A stating line is a value every document of the type carries printed on it, such as the
  holder's name on an identity card. It names the subject it is `about` and its field, and nothing
  more: a stating line that gives `filled_by`, `necessity`, `group`, `known_to` or `document_type`
  is refused at load, never read past, and so is one whose item kind asks for no field.
  `forms.check` never asks a stating line, and an answer keyed to one comes back unplaced with a
  reason that names the line as stating.
- A requiring line is an `attachment` item that names, in `document_type`, the id of the type of
  document it takes. It keeps `filled_by` and `necessity` as an asking line does, both validated at
  load. The type it names must already be loaded and must not be its own form's type (the declared
  type, or the form id when none is declared); otherwise the load is refused. Two types that
  require each other therefore cannot both load. That is chosen: a cycle of required attachments
  has no first document to load.
- An asking line that names a document type is refused.

`forms.check` reports nothing for a requiring line yet, because no answer attaches a document, so a
silent check on one is not a satisfied one. The missing and satisfied findings for it land with
#6892 part 3, where an instance can attach a document.

### 4. Lint

`forms.lint` lists `minted_document_types`, each with its id, and `forms_without_document_type`,
the form revisions no type contains (loaded before this record). Both list what was guessed or not
yet said, not what is broken, so neither counts against `clean`. `missing_source` treats a document
type as it treats an issuer: it is sourced when it contains a form. `missing_required` asks a
document type for its id and its origin, and asks a system for its id alone.

### 5. Left out

Mapping a field of one document type to another waits for ADR-222's build order item 5. The read
of which types state a field for a subject is ADR-222's (Amendment 2, A2.8), not this record's. No
confidence is stored on a type or a line.

## Migration

None. A store keeps its forms as they are. A form revision loaded before this record has no type;
`forms.add_item` treats it as its own type by its form id, and lint lists it.

## Acceptance

`crates/khive-pack-forms/tests/document_types.rs`, on a fixture with one type, an identity document
of origin `third_party`, whose form states the holder's legal name and date of birth and which an
application form requires attached:

- A declared type contains its revision by `contains`, and the export shows that edge.
- `forms.check` never asks the stating lines, and an answer keyed to one comes back unplaced with
  the stating reason; the same field asked on the application is placed.
- The requiring line keeps `filled_by` and `necessity` (required by default), and `forms.check`
  reports nothing for it.
- Refused at load, writing nothing: a requiring line naming a type not loaded, or its own form's
  type, declared or by form id; a stating line with `filled_by`, `necessity` or `document_type`; an
  asking line with a document type; a requiring line that is not an attachment; an unknown origin.
- A form that declares no type gets a minted one, which lint lists while `clean` stays true; a
  later revision that declares the type replaces it, with no second type node.
- A declaration that differs from the loaded type is refused and writes nothing; one that agrees
  reuses the type.
- A system declared with `system: true` has no origin, lint reads it clean, and an asking or
  requiring line on one of its forms is refused at load and at `forms.add_item`; a declaration with
  both `system: true` and an origin, or with neither, is refused, and so is declaring a minted type
  a system.
- Export then replay into an empty namespace gives identical nodes and edges, and the minted mark
  survives. An export with its types removed replays, and lint lists its forms.

## Not checked

- How #6892 part 3 represents an attached document, and so the shape of the answer that satisfies a
  requiring line.

## Amendment 1 (2026-11-28): a field narrows a broader field

ADR-222 A1.4 point 4 reads a field's parents to list the places that state a value in part. Today
the only field that contains another is a general wording that `forms.split` writes, so a parent
is always an ambiguous wording, never a broader field. This amendment lets a load declare a
broader field.

Facts at main `e0aaf4785e7fa4fe35980ee9bdf653ea22256be2`:

1. The pack already allows `contains` from a field to a field
   (`crates/khive-pack-forms/src/vocab.rs`). `forms.split` is its only writer, and the containing
   field it writes carries `general: true`.
2. `forms.neighbourhood` on a field lists, under `general`, every field that contains it, general
   or not, and under `narrower` every field it contains.
3. `forms.resolve` expands only a general field into the fields it contains; a match on any
   other field returns that field alone.
4. `forms.same` binds an item to one field, existing or new. It never makes two fields one.

### A1.1 What it declares

A `forms.load` field entry takes an optional `narrows`, the name of one field: every answer to
this field is also an answer to the named one, as a legal name is a name. It is stored as the
existing `contains` edge from the broader field to this one; no relation, property or node type
is added. The broader field stays an ordinary field: it can be asked, and it keeps its items,
values and comparison. A field narrows at most one field, and may be narrowed by many.

Inside the forms pack the edge is declared and read back, and nothing more. `forms.check` does
not take an answer to the narrower field as answering an item that asks the broader one, and
`forms.resolve` does not follow it. The only reader that draws the inference is ADR-222's
`stated_in_part` (A1.5).

### A1.2 What is refused

The load is refused and writes nothing when `narrows`:

- names a field that is neither loaded nor listed earlier in the same document;
- names the field itself;
- names a general wording;
- names a field of another value type, so that an answer to the narrower field is always a
  well-typed answer for the broader one;
- would close a cycle of narrows. The refusal is what keeps a walk up the chain (A1.5) finite;
- is given for a field that already narrows a different field.

Giving the same `narrows` again changes nothing, and a loaded field that narrows nothing may gain
one. `forms.add_item` and `forms.same` do not take `narrows` and refuse it, as they refuse
`known_as`. A choice field's values are not mapped to the broader field's values; that waits for
ADR-222's build order item 5, as section 5 says.

### A1.3 How a narrows goes away

This amendment gives no way to remove or change a narrows through a forms verb: once written, a
load can only repeat it. A wrong one is fixed by deleting the edge with the generic
`delete(id=<edge id>, kind="edge", hard=true)`. The forms pack does not guard it: `delete` on an
edge goes straight to the runtime's edge delete (`crates/khive-pack-kg/src/handlers/update.rs`).
The delete is hard because an edge is unique per source, target and relation, so a soft-deleted
row would still hold the pair. After it, the field narrows nothing, and a later load may give it
a narrows again, the same one included.

`forms.same` leaves the edge alone, because it never makes two fields one (fact 4). Binding an
item to a field moves no edge of the field; `new_field` creates a field that narrows nothing.
`forms.split` also leaves the edges alone: the renamed field keeps its narrows edges, and the new
field narrows nothing.

### A1.4 What forms.neighbourhood reads from it

On a field, `general` lists only the general wordings that contain it, so a broader field is
never reported as an ambiguous wording. A new `broader` lists the ordinary field it narrows, none
or one. `narrower` keeps its meaning: the fields this one contains, which for an ordinary field
are the fields that narrow it.

### A1.5 Who reads it

ADR-222's `stated_in_part` (A1.4 point 4) reads `broader` in place of `general`, and walks it up
the chain: with `legal name` narrowing `name` and `name` narrowing `label`, a line stating
`label` is listed for a required `legal name`, as is a line stating `name`. Each listed line names
the field it states. The nearest broader field comes first, then the next one up. The walk ends
because A1.2 refuses a cycle. That change, with its test moved from a hand-written edge to a
loaded `narrows`, lands in the same pull request as this amendment's forms code.

### A1.6 Lint, migration and the change log

A field that another field narrows counts as sourced, as a general field does. It stays in
`fields_without_items` when no item asks it, because that is still true.

No migration. A store whose field-to-field `contains` edges all come from `forms.split` reads the
same. In a store where an ordinary field was made to contain a field by hand before this
amendment, `forms.neighbourhood` lists the containing field under `broader` and no longer under
`general`. The change log entry says that `general` loses those entries, and the verb's
description says what `broader` holds.

### A1.7 Acceptance

`crates/khive-pack-forms/tests/narrows.rs`:

- Loading `name`, then `legal name` with `narrows: name`, writes one `contains` edge, and the
  export shows it. `forms.neighbourhood` on `legal name` gives `broader` `[name]` and `general`
  `[]`; on `name` it gives `narrower` `[legal name]`. Resolving `name` returns `name` alone.
- `forms.check` on a form that asks `name`, answered only with a `legal name` value, reports
  `name` unanswered.
- Each refusal in A1.2 writes nothing: a field not loaded, a field listed later in the same
  document, the field itself, a general wording, another value type, a cycle of two fields, a
  second different narrows. The same narrows given again changes nothing.
- After a hard `delete` of the edge, `legal name` has no `broader`, and a load may give it
  `narrows: name` again.
- A split field keeps its narrows edge; `forms.same` with `new_field` writes a field with no
  `broader`.
- Lint counts `name` sourced. Export, then replay into an empty namespace, gives identical nodes
  and edges.

In the operation pack: `stated_in_part` passes on a loaded narrows with no hand-written edge. With
three fields `legal name`, `name` and `label` chained, a line stating `label` is listed for
`legal name` after a line stating `name`. A general wording that contains `legal name` adds
nothing to `stated_in_part`.
