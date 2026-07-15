# Save-sink design — manifest and destination policy

`save_sink` (`src/save_sink.rs`) backs the `request` tool's `save_to` param
and the `kkernel exec --save-file` CLI path: it writes op results as JSONL
and returns a self-describing manifest instead of the raw results.

## Why the manifest matters

A sink that self-reports null counts catches bulk export corruption (e.g.
`content=null` across 10,000 rows) in one second rather than after a
downstream agent fleet has graded blind. `write_and_manifest` computes
`per_column_null_counts`, a `schema_fingerprint` (SHA-256 of sorted field
names), and a file `checksum` so a caller can sanity-check a large export
without re-reading it.

## Why the destination policy matters

`save_to` is a client-supplied string reaching the filesystem. Without a
root + traversal + symlink check, a client could request
`../../etc/cron.d/x` or overwrite an existing symlinked file outside any
sandbox. `validate_destination` enforces three things before any write:

1. No `..` traversal components anywhere in the requested path.
2. The resolved parent directory must stay inside the export root — checked
   by walking up to the deepest *existing* ancestor and canonicalizing that,
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

## Why `write_atomic` uses a random temp file

`write_atomic` uses `tempfile::Builder::tempfile_in` instead of a
predictable `path.with_extension("tmp")` sibling. This closes the
symlink-following / predictable-path race the previous sibling-tmp approach
was open to, and the temp file always lives in the same directory as the
destination so the final rename is same-filesystem and atomic.
