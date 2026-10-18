# Inferred dependency kinds in link responses

On the local runtime path, for `depends_on`, khive infers a `metadata.dependency_kind` when the caller does
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

Bulk link responses add `inferred_count` when any successful entry inferred a
value. The default response includes only that count, so explanations do not
grow with the batch size. With `verbose=true`, the response also includes
`hints: [{"index": 0, "hint": "..."}]` and the same hint on each affected edge.
Indices refer to the original input array. Skipped duplicates, failed entries,
and explicit qualifiers do not contribute to the count or hints. Re-linking a
live edge without a qualifier discloses inference again. Empty hint and count
fields are omitted.

A singleton `create` with an `edges` list adds the same response-only `hint` to
each returned edge whose dependency kind was inferred. Its stored edge metadata
and events remain unchanged.

On multi-backend servers, a single link by UUID can use the coordinator route.
That route validates and stores caller-supplied metadata without inferring a
dependency kind, even when both endpoints resolve to the same backend. It
therefore returns no inference hint. Supply `dependency_kind` explicitly on
that route when the qualifier is needed.

CLI `--atomic` link results carry the same optional `hint`. Its provenance is
captured during preparation; a later response read cannot change which value
this operation inferred. Ordinary link responses use the writer-observed edge
without additional reads. No inference table, admission rule, storage schema,
or disposition behavior changes.
