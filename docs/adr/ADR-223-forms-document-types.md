# ADR-223: Forms Document Types

- **Status:** Proposed
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
`document_type_id`, a name, an `origin` (`applicant`, `third_party` or `work`) and an optional
`issuer_class`. Its key is `document_type:<document_type_id>`, so a namespace holds one per id. A
document type contains its form revisions by the existing `contains` relation, under one added
endpoint rule (`document_type` contains `form`); no relation is added. A revision stays a `form`
node whatever its type, because renaming a stored type would break loaded stores.

### 2. Load declares the type or mints one

`forms.load` takes an optional `document_type` object, `{id, name, origin, issuer_class?}`.

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
type as it treats an issuer: it is sourced when it contains a form.

### 5. Left out

Mapping a field of one document type to another waits for ADR-222's build order item 5. The read
of which types state a field for a subject lands with ADR-222's build order item 3, and so does the
mark that declares an outside system the way a document type is declared (ADR-222 Amendment 1,
A1.4 point 2); no origin here means a system. No confidence is stored on a type or a line.

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
- Export then replay into an empty namespace gives identical nodes and edges, and the minted mark
  survives. An export with its types removed replays, and lint lists its forms.

## Not checked

- How #6892 part 3 represents an attached document, and so the shape of the answer that satisfies a
  requiring line.
