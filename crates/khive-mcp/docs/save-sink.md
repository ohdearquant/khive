# Save-sink design — manifest and destination policy

`save_sink` (`src/save_sink.rs`) backs the `request` tool's `save_to` param
and the `kkernel exec --save-file` CLI path: it writes op results as JSONL
and returns a self-describing manifest instead of the raw results.

## Why the manifest matters

A sink that self-reports null counts catches bulk export corruption (e.g.
`content=null` across 10,000 rows) in one second rather than after a
downstream agent fleet has graded blind. `JsonlSaveSink`/`write_and_manifest` compute
`per_column_null_counts`, a `schema_fingerprint` (SHA-256 of sorted field
names), and a file `checksum` so a caller can sanity-check a large export
without re-reading it. The manifest also carries the dispatch `summary` and,
when any row failed, a compact `failures` projection containing `op_index`,
`tool`, the unchanged `error` payload, and an optional stable `reason`. This
keeps `kkernel exec --save-file` machine-classifiable from stdout while the
complete per-op rows remain in the JSONL file. The refusal vocabulary is
defined in `crates/kkernel/docs/usage.md`. For `kkernel exec --strict
--save-file`, the exec dispatch seam attaches otherwise-unclassified
`strict-op-failure` reasons before this sink serializes anything, so the
manifest projection, canonical JSONL rows, and checksum all describe the same
classified data.

For an atomic ops-file envelope, `write_envelope` also copies the complete
top-level `atomic` object into the stdout manifest. This unit-level commit and
retry contract cannot be reconstructed from independent JSONL rows. In
particular, a manifest for a committed-but-degraded unit retains
`atomic.committed=true`, `atomic.status="committed_degraded"`, its typed
`atomic.degradations`, and `atomic.retryable=false` unchanged.

`write_and_manifest` derives that projection from its in-memory envelope.
Incremental callers pass their already-bounded `summary.failures` projection to
`finish`; the sink promotes exactly those entries to top-level `failures`
without re-buffering or expanding the streamed rows.

## Why the destination policy matters

`save_to` is a client-supplied string reaching the filesystem. Without a
root + traversal + symlink check, a client could request
`../../etc/cron.d/x` or overwrite an existing symlinked file outside any
sandbox. `validate_destination` enforces three things before any write:

1. No `..` traversal components anywhere in the requested path.
2. The resolved parent directory must stay inside the export root — checked
   by walking up to the deepest _existing_ ancestor and canonicalizing that,
   proving containment before any directory is created (an as-yet-missing
   suffix can only descend further beneath an already-contained ancestor).
3. An existing symlink at the destination itself is rejected outright (no
   follow-and-overwrite).

`export_root()` defaults to `~/.khive/exports`, overridable via
`KHIVE_SAVE_TO_ROOT` (used by tests to scope each case to its own temp
directory). Every `save_to` request from the MCP wire path must resolve to a
path inside this root. The trusted operator CLI path
(`kkernel exec --save-file`, `restrict_to_export_root = false`) skips this
check entirely and may write anywhere the operator points it — that is
documented CLI behavior, not an oversight.

## Why the incremental sink uses a random temp file

`JsonlSaveSink` uses `tempfile::Builder::tempfile_in` instead of a
predictable `path.with_extension("tmp")` sibling. This closes the
symlink-following / predictable-path race the previous sibling-tmp approach
was open to, and the temp file always lives in the same directory as the
destination so the final rename is same-filesystem and atomic.

Rows are serialized directly to that sibling while the SHA-256 checksum,
schema field set, and null counts are accumulated. `finish` flushes and
renames only after every row is present; dropping an unfinished sink removes
the temp and leaves any prior destination intact. `write_and_manifest` uses
this same incremental implementation, so large exports do not require a
second complete JSONL buffer in memory.

For `kkernel exec --ops-file --save-file`, destination validation and temp-file
creation happen before the first operation chunk is dispatched. Each validated
ordered result row is then written before the next chunk begins. Only final-file
publication is atomic: non-atomic database effects commit incrementally by
chunk, and file I/O plus database commits cannot form one cross-resource
transaction.

Once dispatch begins, every termination prints a reconciliation manifest.
Success retains the ordinary manifest shape and publishes the complete JSONL.
A post-dispatch error that prevents the manifest from being finalized instead
prints `status="aborted"`,
`file_published=false`, the confirmed `committed_chunks`, and, when its response
could not be verified, `dispatched_chunk`. Its `summary` covers confirmed rows
only; `unconfirmed_ops` accounts for the remainder without falsely classifying
them as aborted. That dispatched chunk can have database effects even though it
is not listed as confirmed. The incomplete temp file is discarded and any old
destination remains unchanged.

Policy exits are different and keep the ordinary manifest. When the batch runs
to completion and the manifest is published, a non-zero exit from `--strict` or
from an all-failed file happens after that publication. The outcome of every op
is known in those cases, so the ordinary manifest is the correct reconciliation
record and no aborted manifest is emitted. Callers reconcile
the manifest before applying the per-op/idempotency contracts to a retry;
atomic ops-files retain their separate all-or-nothing database contract.

Atomic execution also has a distinct post-commit sink-failure boundary. If a
row write, flush, or final rename fails after the database unit committed, the
CLI cannot return a manifest for an unpublished file. It instead prints the
original result envelope augmented with a `save_file_publish` degradation and
`retryable=false`, then exits non-zero for the sink failure. That non-zero exit
does not roll back or make the already-durable atomic unit safe to replay.
