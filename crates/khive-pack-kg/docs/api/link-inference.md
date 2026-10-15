# Inferred dependency kinds in link responses

For `depends_on`, khive infers a `metadata.dependency_kind` when the caller does
not supply one. A successful link response adds a top-level `hint` naming that
inferred value. The hint is response information; it is never stored in the edge
or its event payload.

All existing defaults receive the hint:

| Source kind | Target kind        | Inferred value |
| ----------- | ------------------ | -------------- |
| project     | project            | build          |
| service     | service            | runtime        |
| service     | dataset            | data           |
| service     | artifact           | artifact       |
| artifact    | project or service | tooling        |
| document    | document           | normative      |

The normative hint also says to use `links_to` for references that are not
normative. Inference and stored data keep the rules in
[ADR-002](../../../../docs/adr/ADR-002-edge-ontology.md) and
[ADR-191](../../../../docs/adr/ADR-191-web-pack-ontology-and-operations.md).

An explicit `dependency_kind`, either in metadata or as a top-level parameter,
suppresses the hint even when it equals the default. A metadata qualifier keeps
precedence over a competing top-level value. A null metadata argument is treated
as omitted; `metadata.dependency_kind: null` remains invalid. Relations other than
`depends_on` and endpoint pairs without a default receive no hint.

Bulk link responses add `hints: [{"index": 0, "hint": "..."}]` when any successful
entry inferred a value, including when `verbose` is false. Indices refer to the
original input array. Skipped duplicates and failed entries receive no hint.
Verbose edge objects carry the same hint. Empty hint fields are omitted.

CLI `--atomic` link results carry the same optional `hint`. Its provenance is
captured during preparation; a later response read cannot change which value
this operation inferred. Ordinary link responses use the writer-observed edge
without additional reads. No inference table, admission rule, storage schema,
or disposition behavior changes.
