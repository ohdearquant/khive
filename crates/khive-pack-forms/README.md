# khive-pack-forms

A typed graph of printed forms for khive. It stores who issues a form, what kind of document each revision is, the form's sections and items, the field each item means, the allowed values of a choice field and the rules printed on the form, and keeps one field per meaning however many forms print it and however they word it.

The pack is linked into the server and selected by name (`--pack forms`, beside `kg`).

## Verbs

| Verb | What it does |
| --- | --- |
| `forms.load` | Load one form from a document in one transaction. |
| `forms.add_item` | Add an item to a loaded form and report which fields its label matches. |
| `forms.same` | Record that an item means an existing field, or a new one. |
| `forms.split` | Record that one label names two things: rename, add, and a general field over both. |
| `forms.resolve` | Find the field a wording means, from the exact wording down to its initials and its words; a shared wording resolves as ambiguous, and a miss names the nearest fields. |
| `forms.neighbourhood` | Read one node with its typed edges. |
| `forms.terms` | List the loaded kinds, roles, relations and counting qualifiers, each with its definition and aliases, and whether a relation carries. |
| `forms.subjects` | List the subjects one form revision declares; the revision loaded last unless one is named. |
| `forms.check` | Check a set of forms and answers against the printed rules. |
| `forms.lint` | List nodes that break the store's invariants. |
| `forms.export` | Export the forms graph keyed by content. |
| `forms.replay` | Write an export into another namespace. |

## Types and edges

All ten types are `concept` entities: `form_issuer`, `document_type`, `form`, `form_section`, `form_item`, `field`, `field_value`, `form_rule`, `form_subject` and `form_term`. The pack adds typed endpoint rules for `contains`, `variant_of`, `links_to`, `competes_with`, `composed_with`, `precedes`, `depends_on` and `enables` between them; `depends_on`, `precedes` and `links_to` between concepts are open only for these pairs.

## Document types

Every form revision belongs to a document type, which contains it. A document may declare its type (`document_type`: `id`, `name`, `origin` of `applicant`, `third_party` or `work`, and an optional `issuer_class`). A form that declares none gets a type minted from its form id and title, with origin `applicant` and marked `minted`; a later revision that declares the type under that id replaces the guess, and a declaration that disagrees with a declared type is refused. `forms.lint` lists minted types and forms that no type contains, and neither keeps the graph from being clean.

An item says what it does with its field (`relation`): it `asks` for it (the default), `states` it, or `requires` an attached document. A stating line is a value every document of the type carries printed on it; it names its subject and field and nothing else, `forms.check` never asks it, and an answer keyed to it comes back unplaced. A requiring line is an `attachment` item naming the id of the type it takes (`document_type`), with a writer and a necessity like an asking line; the type must already be loaded and must not be the form's own, so two types that require each other cannot both load. `forms.check` does not yet report a requiring line, because no answer attaches a document. The decisions are recorded in [ADR-223](../../docs/adr/ADR-223-forms-document-types.md).

An outside system that holds values about a subject, such as a permit portal, is declared the way a document type is, with `system: true` in place of an `origin`: its record is loaded once as a form whose subjects and stating lines say which fields it holds for whom. Every line of a system's record states its field; a document that gives one an asking or requiring line is refused at `forms.load` and at `forms.add_item`, a declaration with both `system` and `origin`, or with neither, is refused, and a type minted for a form already loaded is never declared a system. The mark comes from [ADR-222](../../docs/adr/ADR-222-operation-graph.md) Amendment 1, A1.4 point 2.

## Subjects and lines

A field is role-free: `legal name`, never "the applicant's legal name". Whose value a line asks for is a subject the form revision declares: a party or a thing, with a kind and a role from term lists the caller loads (`terms` in the document), an optional qualifier, an optional row count (`repeats`, from 1 to 100) and named relations to other subjects of the same form. A subject belongs to its revision and is never merged with another form's.

A term may carry aliases (`{name, definition?, aliases?}`). A name or an alias resolves to its term, so a later document that names a loaded term by any of its wordings means that term and adds its further wordings as aliases; a term whose wordings name two loaded terms of one list is refused. Subjects are stored under the canonical names. Two more lists sit beside kinds, roles and relations: `counting_qualifiers`, the qualifiers that only count ("first", "second", "additional"), compared after folding case and spaces; and `carrying_relations`, the relations by which one subject carries another ("belongs to"), each a relation listed in the same document or loaded before it. `forms.neighbourhood` on a term shows its aliases and whether it carries.

On a form that declares subjects, each item that asks for a field says which subject it is `about` and which subject writes it (`filled_by`); `forms.load` refuses an item that names an undeclared subject. An item may also carry a `state` (a former and a new value of one field), its own `repeats`, a `necessity` (`required`, the default on such a form, `optional`, `one_of` a named `group`, or `at_least_one_row`), and `selects`, the subjects its value may name. A printed rule may enable a subject as well as a section or an item.

An item may say which subject already holds its value apart from the one who writes it (`known_to`): a declared subject, or `{"relation": ...}` for the subject its own subject names under that relation, so a line about an account held at a receiving party is known to that party. A form may declare the purposes it serves (`purposes`), each a term from the `purposes` list; the purpose term lists its forms in `forms.neighbourhood`.

A form may hold two of its lines to match or to differ (`constraints`: `must` is `match` or `differ`, `lines` names two lines by subject, field and state, `text` is optional). Each field compares its answers one way (`comparison`): `exact`, `folded` (case and spacing ignored), `digits` (only the digits count, and a pair where an answer holds no digit is not compared), or `key`, where each answer carries a caller-supplied `key`. A field that states no comparison compares exactly. `forms.load` refuses a constraint whose lines are not printed on the form or compare differently, and a field reused with another comparison than the one it was loaded with, no comparison counting as `exact`, so a later form never changes what an earlier form's constraint means.

`forms.check` takes answers keyed by subject, field, row and state, fills only the lines they belong to, and reports each line a necessity leaves open as a `missing` finding. An answer that names no subject fills only lines about none; where the checked forms ask for its field only of subjects, it comes back unplaced. A line whose subject, section or item is enabled by a rule whose premise does not hold is not asked, and counted apart. An answer that fills only lines that are not asked comes back under `answers_not_asked` and holds no premise, so it brings nothing else into play. This holds for forms loaded without subjects as well: an answer to a line that a printed rule brings into play, given while the rule's premise does not hold, makes the check inconsistent. A required line that is bound to no field yet is reported as `line_without_field`. A constraint whose two lines are answered and asked, and whose answers break it, is reported as `constraint_unmet` naming both lines; one that cannot be compared, such as a `key` comparison with an answer that gives no key, comes back under `constraints_unchecked`. A `missing` finding names who writes the line and who already holds it. Answers that fit no subject or line come back with the reason.

Two limits hold for now. A `one_of` group counts a choice line by its field, so the options of one choice cannot sit in different groups. A line repeats at most 100 times, because a check reports each unfilled row.
