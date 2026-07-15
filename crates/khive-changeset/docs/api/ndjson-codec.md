# NDJSON Change-Set Codec

`ChangeSet`, `to_ndjson`, and `from_ndjson` define the in-memory NDJSON-delta boundary. The format is line-oriented, strict about schema and fields, and preserves operation order exactly.

## `ChangeSet`

A change-set contains one `Envelope` and an ordered `Vec<Op>`. Order is semantically load-bearing: a later link may reference an ID minted by an earlier staged create. `ChangeSet::new` stores both values without sorting, validation, I/O, or execution.

## Wire layout

Line 1 is the envelope. Each following line is one internally tagged operation in stage order:

```text
{"schema_version":1,"producer":"agent:x",...}
{"op":"create",...}
{"op":"link",...}
```

`to_ndjson` appends `\n` after the envelope and after every operation, including the last. It returns an in-memory `String`; persistence and transport belong to the caller.

## `to_ndjson`

Encoding serializes the envelope first and then visits `ops` in slice order. Any serde failure becomes `ChangeSetError::Serialize`. No partial output escapes on error because the function returns only after completing the string.

## `from_ndjson`

Decoding requires an envelope line, checks its `schema_version` against `CURRENT_SCHEMA_VERSION`, then decodes every remaining line as one `Op`. Lines are not reordered or skipped.

A blank or whitespace-only operation line is malformed rather than ignored. This keeps the invariant “every post-header line is an operation” exception-free. Serde `deny_unknown_fields` mirrors reject extraneous or misspelled envelope, operation, patch, and embedded-preimage keys.

Malformed JSON reports a one-based physical line number. An unsupported schema reports both the found and expected versions.

## `ChangeSetError`

| Variant                    | Meaning                                                                 |
| -------------------------- | ----------------------------------------------------------------------- |
| `Empty`                    | no envelope line exists                                                 |
| `MalformedLine`            | the one-based line is invalid JSON or violates its strict wire contract |
| `UnsupportedSchemaVersion` | envelope version differs from the only accepted version                 |
| `Serialize`                | envelope or operation serialization failed                              |

All variants arise from in-memory input; the codec never performs filesystem or network I/O.
