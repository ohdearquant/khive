# Pending source references

L2 stores `l2_file_pending` on each file-module entity. Its object keys are the
lowercase hex digest of the file's canonical absolute path, computed with the
digest source content hashes use; the path itself is not stored. Keys are
separate from module UUID identity. Each value contains the accepted
`content_hash`, `declaration_ids`, the `scanner_version` that wrote it, and
`references`: `{declaration_id,module_path,segments,evidence}` tuples. Successful
parses replace only their producer entry, including `references: []`, together
with coverage. A reference the secret gate refuses is reported when it is
recorded and left out of the entry, so it cannot refuse the file's coverage.
Fresh row rebases preserve other producer entries.

Previously, declarations carried an append-only `l2_unresolved_references` array
of `{segments,evidence}`. That history is still written and read for append
membership checks; it never authorizes late edges. The remaining direct reader
is a test helper. Late resolution uses only this invocation's accepted physical
producer entries, including unchanged producers with matching accepted hashes.
Missing, malformed, mismatched or other-version entries force a real parse on their
next encounter; no historical declaration union is migrated. An accepted empty entry
can then reuse.

Failed parses and refused coverage writes retain prior producer history without
accepting it for this invocation. Late cleanup preserves newer accepted hashes
and concurrent additions, and cannot restore tuples absent from fresh state.
The unresolved counter deduplicates within each producer in scan order; different
producers remain independent. Entity/edge/FTS work retains its committed-prefix
behavior and does not become a transaction or serialization between invocations.
