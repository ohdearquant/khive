# `findings.json` ingestion

`ingest_findings_json` is a pure, fail-closed mapper from one audit document to deterministic
project, finding-note, and annotation-edge records. Persistence remains the caller's responsibility.

## Input and options

The root must be an object containing `audit` and a `findings` array. `CodeIngestOptions` supplies
the KG namespace, an observation timestamp, and an optional stable `source_run`. When no explicit
non-empty run is supplied, the function derives `audit.date:audit.commit`; missing components are
`CodeIngestError::MissingSourceRun`.

`observed_at` becomes record creation/update time but is excluded from identity, so retrying the
same audit later reproduces the same IDs.

## Validation boundary

The audit object and every finding are parsed into validated intermediate values before any output
record is constructed. A malformed finding therefore returns one error and no partial batch.

Governed values are severity (`critical`, `high`, `medium`, `low`, `info`) and confidence (`high`,
`medium`, `low`). Medium, high, and critical findings require a non-empty `failure_scenario`.
Evidence accepts null/absence, one string, one object, or an array of strings/objects; strings become
`{"description": ...}` and empty strings or other shapes name both finding and evidence indexes.

Categories, standard, references, priority, producer status, impact, recommendation, and
verification are deliberately ungoverned: their JSON shape is preserved without coercion, and null
is treated as absence. Unknown finding keys are preserved under `raw`.

## Deterministic identity

All IDs are UUIDv5 values under `CODE_INGEST_NAMESPACE`. Object keys are recursively sorted before
identity serialization; array order remains significant content. The project tuple includes
namespace, repository, and scope. A finding tuple includes source run, normalized title, all
validated/preserved content, and raw extensions, but excludes observation time. Any substantive
content change creates a new ID rather than overwriting the prior finding.

## Output records

A successful `CodeIngestBatch` contains one project entity, one `finding` note per finding, and one
weight-1.0 `annotates` edge from each note to the project. The note's `kind_status` maps producer
`fixed` to `resolved`, `false_positive` to `invalid`, and everything else to `open`; the raw producer
value is also retained as `audit_status` because it is not the governed lifecycle field.

String impact renders verbatim in note content, other JSON uses canonical JSON text, and absent/null
impact renders empty. The batch is ready for existing storage/runtime paths but is not committed by
this function.

## Error taxonomy

`CodeIngestError` distinguishes invalid roots, missing fields, wrong types, invalid governed values,
missing failure scenarios, invalid indexed evidence, unavailable source-run identity, and JSON parse
failures. Messages include the accepted value set or shape so callers can repair input directly.
