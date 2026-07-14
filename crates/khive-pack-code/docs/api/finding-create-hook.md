# Finding create hook

`FindingHook` validates and normalizes the generic create path when `note_kind = "finding"`.

## Title, content, and properties

Create input must provide a non-empty `title` or `name`. The hook stores that value as canonical
`name`; absent content defaults to the title. Properties must be an object or null/absent. Top-level
severity, confidence, categories, source run, standard, evidence, references, and lifecycle status
are copied into the properties object.

## Governed values

`kind_status` defaults to `open` and must be one of `open`, `resolved`, `wontfix`, or `invalid`.
Severity, when present, must be `critical`, `high`, `medium`, `low`, or `info`; confidence must be
`high`, `medium`, or `low`. Evidence, when present, must already be an array.

The hook validates the generic create request but performs no post-create mutation.
