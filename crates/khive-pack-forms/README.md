# khive-pack-forms

A typed graph of printed forms for khive. It stores who issues a form, the form's sections and items, the field each item means, the allowed values of a choice field and the rules printed on the form, and keeps one field per meaning however many forms print it and however they word it.

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
| `forms.check` | Check a set of forms and answers against the printed rules. |
| `forms.lint` | List nodes that break the store's invariants. |
| `forms.export` | Export the forms graph keyed by content. |
| `forms.replay` | Write an export into another namespace. |

## Types and edges

All nine types are `concept` entities: `form_issuer`, `form`, `form_section`, `form_item`, `field`, `field_value`, `form_rule`, `form_subject` and `form_term`. The pack adds typed endpoint rules for `contains`, `variant_of`, `links_to`, `competes_with`, `composed_with`, `precedes`, `depends_on` and `enables` between them; `depends_on`, `precedes` and `links_to` between concepts are open only for these pairs.

## Subjects and lines

A field is role-free: `legal name`, never "the applicant's legal name". Whose value a line asks for is a subject the form revision declares: a party or a thing, with a kind and a role from term lists the caller loads (`terms` in the document), an optional qualifier, an optional row count (`repeats`, from 1 to 100) and named relations to other subjects of the same form. A subject belongs to its revision and is never merged with another form's.

On a form that declares subjects, each item that asks for a field says which subject it is `about` and which subject writes it (`filled_by`); `forms.load` refuses an item that names an undeclared subject. An item may also carry a `state` (a former and a new value of one field), its own `repeats`, a `necessity` (`required`, the default on such a form, `optional`, `one_of` a named `group`, or `at_least_one_row`), and `selects`, the subjects its value may name. A printed rule may enable a subject as well as a section or an item.

`forms.check` takes answers keyed by subject, field, row and state, fills only the lines they belong to, and reports each line a necessity leaves open as a `missing` finding. An answer that names no subject fills only lines about none; where the checked forms ask for its field only of subjects, it comes back unplaced. A line whose subject, section or item is enabled by a rule whose premise does not hold is not asked, and counted apart. An answer that fills only lines that are not asked comes back under `answers_not_asked` and holds no premise, so it brings nothing else into play. This holds for forms loaded without subjects as well: an answer to a line that a printed rule brings into play, given while the rule's premise does not hold, makes the check inconsistent. A required line that is bound to no field yet is reported as `line_without_field`. Answers that fit no subject or line come back with the reason.

Two limits hold for now. A `one_of` group counts a choice line by its field, so the options of one choice cannot sit in different groups. A line repeats at most 100 times, because a check reports each unfilled row.
