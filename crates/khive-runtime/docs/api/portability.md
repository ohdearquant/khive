# KG portability

`khive_runtime::portability` exposes `KgArchive`, `ExportedEntity`, `ExportedEdge`
and `ImportSummary`. `KhiveRuntime::import_kg` imports an archive through the
authorized namespace token and returns a summary only on success. `import_kg_json`
uses the same path after decoding the archive. Export preserves retained edge
identities, properties and timestamps.

## Skipped edges

`ImportSummary::edges_skipped` counts edges skipped for missing endpoints or
endpoint-contract refusal. Each newly successful import also returns
`skipped_edges: Vec<ImportSkippedEdge>`, and its count equals that vector's length.
Every entry includes the archive `edge_id`, `source`, `target` UUIDs, the canonical
`relation` and a typed `ImportEdgeSkipCause`:

| JSON cause | Meaning | `error` |
| --- | --- | --- |
| `missing_source` | The initial source-entity lookup did not find the source. | Absent |
| `missing_target` | The source exists, but the initial target-entity lookup did not find the target. | Absent |
| `endpoint_contract` | The later endpoint validator returned `InvalidInput` or `NotFound`. | Exact original `RuntimeError` display text |

The source check runs first. An edge whose two endpoints are absent therefore has
one `missing_source` entry. A later validator `NotFound` remains an
`endpoint_contract` entry; reporting does not change resolution or race semantics.
Entries retain archive order, including repeated identities and duplicate
endpoint/relation triples. No reporting cap truncates the list. Successfully
written edges have no entry and keep their original identity and metadata.

For example, an entity-sourced `annotates` edge can appear as:

```json
{
  "edge_id": "00000000-0000-0000-0000-000000000103",
  "source": "00000000-0000-0000-0000-000000000001",
  "target": "00000000-0000-0000-0000-000000000002",
  "relation": "annotates",
  "cause": "endpoint_contract",
  "error": "invalid input: annotates source 00000000-0000-0000-0000-000000000001 must be a note"
}
```

`kkernel kg import` prints the complete summary, so a nonempty `skipped_edges`
list is included automatically in CLI JSON. Existing tracing warnings are retained.
The BibTeX adapter's additional summary metadata remains independent of these
runtime edge details.

## JSON compatibility

An empty `skipped_edges` list is omitted during serialization. All-valid and empty
imports keep the previous summary shape, including `edges_skipped: 0` and
`embedding_truncation`. Deserialization defaults an omitted list to empty. Historical
JSON can consequently retain a positive `edges_skipped` count without individual
details: those details were never recorded and are not invented on decode. The
count/list equality describes newly successful imports, rather than arbitrary
deserialized or caller-constructed summaries.

The three cause spellings are a closed snake-case enum. The optional `error` field
is omitted for missing endpoints and preserves the existing diagnostic verbatim
for endpoint-contract skips.

## Failure semantics

Reporting does not broaden endpoint legality or change deterministic archive,
property, secret, namespace or embedding checks. Errors other than the existing
skip verdicts still abort the import. Store failures do not become skipped-edge
entries or a successful summary. Entity and earlier valid-edge writes may already
have committed when a later operational failure occurs; this summary does not add
atomic rollback or describe a failed import's partial effects.
