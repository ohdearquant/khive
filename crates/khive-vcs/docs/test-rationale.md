# Test rationale — `src/sync.rs` regression tests

Background for maintainers reading the `#[cfg(test)] mod tests` block in
`src/sync.rs`. None of this is public API; it explains why specific
regression tests exist.

## `sync_chunk_boundary_round_trip`

Chunk-boundary round-trip: writes N > `SYNC_CHUNK_SIZE` entities and edges so
the batching path exercises at least two transaction boundaries, then
verifies the full count and spot-checks the last record of the final chunk.
In test builds `SYNC_CHUNK_SIZE = 5`, so N = 11 produces three chunks
(5 + 5 + 1) for both entities and edges.

## `run_sync_remote_cannot_be_constructed_with_invalid_name`

Issue #474: `run_sync_remote("../evil" | "/tmp/evil" | "safe/name")` must be
impossible to even construct — `RemoteConfig::name` is a `RemoteName`, and
`RemoteName::parse` is its only constructor — so these names never reach
`run_sync_remote`'s cache-directory join. Confirms both the construction-time
rejection AND (via filesystem check) that nothing was created at the
traversal targets or under `.khive/kg/remotes/`.

## `public_error_redacts_https_credential_url`

git echoes credential-bearing HTTPS URLs in its stderr on auth failure, e.g.:
`fatal: Authentication failed for 'https://user:token@host/repo.git'`. The
sanitiser must strip that before it reaches the caller; this test confirms an
HTTPS URL with embedded `user:pass` credentials never appears in the public
error string produced by a clone failure.

## `public_error_redacts_scp_style_remote`

Exercises the FAIL-before/PASS-after property of the scp-style credential
fix: the sanitiser is called on the raw git stderr, which git may populate
with lines like `fatal: Could not read from remote repository.` that do NOT
contain the URL — so for scp remotes that fail at DNS/SSH level the URL is
not re-echoed by git. What this test asserts is that the sanitiser IS wired
into the error path and that the rendered error does not include the scp
token (`git@host:org/repo.git`) from any source. The companion unit tests
`redact_strips_scp_style_remote` and `redact_strips_user_at_host_colon_path`
directly verify the sanitiser strips scp tokens from git stderr strings;
this test confirms the wiring.

## `sync_fts_document_matches_entity_fts_document`

Regression: the VCS sync FTS document must be field-identical to
`entity_fts_document` output for the same entity. Before this fix,
`upsert_entities` built a `TextDocument` inline with slightly different field
mapping than the canonical helper, which could produce divergent FTS shapes
when the helper is updated.

## `wal_checkpoint_truncate_via_plain_execute_script_fails_with_write_queue_enabled`

Revert-and-confirm-fails companion: the OLD (broken) call shape — plain
`execute_script`, which wraps the pragma in the WriterTask's `BEGIN
IMMEDIATE` — must fail under the write queue. This proves the paired
"succeeds" test is actually exercising the regression fix, not passing
vacuously.

Source: `crates/khive-vcs/src/sync.rs`.
