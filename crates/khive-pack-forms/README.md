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

All seven types are `concept` entities: `form_issuer`, `form`, `form_section`, `form_item`, `field`, `field_value` and `form_rule`. The pack adds typed endpoint rules for `contains`, `variant_of`, `links_to`, `competes_with`, `composed_with`, `precedes`, `depends_on` and `enables` between them; `depends_on`, `precedes` and `links_to` between concepts are open only for these pairs.
