# NDJSON ⇄ SQLite sync (`src/sync.rs`)

`sync.rs` rebuilds the SQLite working database from the git-tracked NDJSON KG
export, and fetches/caches a remote KG archive over `git clone`. Both paths
share one invariant: nothing that is even partially written is ever made
visible as the "current" state.

## Local sync — `run_sync`

1. Read `<repo_root>/.khive/kg/entities.ndjson` and `edges.ndjson`.
2. **Validate-first gate** (`validate_ndjson_records`, issue #476): full
   ADR-020 structural validation — entity kind validity, entity/edge
   timestamp validity, entity/edge sort order, duplicate entity ids,
   duplicate edge ids, duplicate semantic edge triples
   `(source, target, relation)`, dangling edge endpoints, edge
   relation/weight validity — runs **before** the temp DB is created. A
   violation leaves the existing target DB completely untouched.
3. Build the working database in `<db_path>.tmp`.
4. Upsert entities and populate the FTS5 index. Vector embeddings are
   skipped — they're local-only derived state computed lazily via
   `kkernel kg embed` (ADR-035 §6).
5. Checkpoint the WAL (`PRAGMA wal_checkpoint(TRUNCATE)`).
6. Atomic rename: `<db_path>.tmp` → `<db_path>`. A crash before this step
   leaves the previous DB intact (all-or-nothing guarantee).

Records are converted and written in chunks of `SYNC_CHUNK_SIZE` (10,000 in
production, 5 in test builds) rather than all at once, so peak
converted-buffer memory is `O(SYNC_CHUNK_SIZE)`, not `O(records.len())`. The
`sync_chunk_boundary_round_trip` regression test writes 11 entities/edges
under the test chunk size, producing three chunks (5 + 5 + 1), and confirms
both the full count and the last record of the final chunk survive the
batching path intact.

## Remote sync — `run_sync_remote` (ADR-037)

1. `git clone --depth=1 --filter=blob:none` into a temporary staging
   directory.
2. Sparse-checkout `.khive/kg/entities.ndjson` and `.khive/kg/edges.ndjson`.
3. **Validate-first**: `build_kg_archive` parses all edge relations and
   returns an error if any relation is invalid — before any cache write.
4. Compute the `SnapshotId` over the validated archive (see
   `snapshot-hash.md`).
5. **Pin verification, fail-closed**: if `pin` is set and `repin=false`, a
   hash mismatch returns `VcsError::HashMismatch` before any cache file is
   written.
6. **Atomic cache publish** (`publish_remote_cache` + `atomic_replace_dir`):
   entities, edges, and `meta.json` are written to a staging directory (a
   sibling of the cache dir), then switched into place with one directory
   rename. If the target cache directory already exists, the old one is
   first renamed to a sibling backup, the new one renamed into place, and
   the backup removed only after the swap succeeds; if the second rename
   fails, the backup is restored. Both renames are individually atomic
   `rename(2)` calls, so a reader never observes a mix of old and new
   files — at every instant the cache dir is either the complete old
   directory, briefly absent, or the complete new directory.
7. `meta.json` records `fetched_at`, `git_ref`, `commit_sha`, `content_hash`
   — written even when no pin is present, for auditability.

### `RemoteName` — construction-time path-traversal safety

`RemoteName::parse` is the only constructor; there is no way to build a
`RemoteName` that fails validation, so a `RemoteConfig` can never carry an
unsafe name into `run_sync_remote`'s cache-directory join (VCS-AUD-002,
issue #474). The regression test
`run_sync_remote_cannot_be_constructed_with_invalid_name` confirms both the
construction-time rejection of names like `../evil` and `/tmp/evil`, and —
via a filesystem check — that nothing gets created at the traversal targets
or under `.khive/kg/remotes/`.

### Credential redaction in git error output — `redact_git_stderr`

git echoes credential-bearing URLs in its stderr on auth failure (e.g.
`fatal: Authentication failed for 'https://user:token@host/repo.git'`).
ADR-037 §157 prohibits leaking remote URLs in errors, so `redact_git_stderr`
strips two forms before any git stderr reaches a caller-visible error:

- `scheme://[user:pass@]host/path` — HTTPS and SSH scheme URLs.
- `user@host:path` — scp-style SSH remotes (e.g. `git@github.com:org/repo.git`).

Both are replaced with the literal token `<url-redacted>`, keeping the rest
of the diagnostic text useful while credentials and remote addresses never
reach logs. `is_scp_remote_start` distinguishes a real scp remote
(`user@host:path`) from `user@host: message text` by requiring the character
immediately after the colon to be neither whitespace nor another colon (the
latter would indicate an IPv6 address or a port already handled by the
`://` branch).

Two regression tests guard this: `public_error_redacts_https_credential_url`
confirms an HTTPS URL with embedded `user:pass` never appears in a clone
failure's public error string. `public_error_redacts_scp_style_remote`
confirms the sanitizer is wired into the error path even for failures where
git's stderr never echoed the URL at all (e.g. a DNS/SSH-level failure) —
the companion unit tests `redact_strips_scp_style_remote` and
`redact_strips_user_at_host_colon_path` verify the sanitizer itself directly;
this integration test verifies it's actually called on that path.

## FTS document consistency

`upsert_entities`' FTS5 document must be field-identical to
`entity_fts_document`'s output for the same entity — `sync_fts_document_matches_entity_fts_document`
is a standing regression test for this. Before it existed, `upsert_entities`
built a `TextDocument` inline with a slightly different field mapping than
the canonical helper, which silently produced a divergent FTS shape whenever
the canonical helper was updated but the inline copy wasn't.

## WAL checkpoint under the write queue

`run_sync`'s WAL truncate-checkpoint must go through the write queue's
`BEGIN IMMEDIATE` wrapping, not a plain `execute_script` call — the latter
was the old, broken call shape. The regression test
`wal_checkpoint_truncate_via_plain_execute_script_fails_with_write_queue_enabled`
is a revert-and-confirm-fails companion: it asserts the OLD call shape
*fails* under the write queue, which proves the paired "succeeds" test is
actually exercising the fix rather than passing vacuously (i.e. it isn't a
test that would pass regardless of which call shape were in use).

## Failure modes

| Scenario                        | Behaviour                                                |
| -------------------------------- | ---------------------------------------------------------- |
| Invalid edge relation in NDJSON | Error before any DB/cache write; previous DB intact       |
| Hash mismatch on remote pin     | `VcsError::HashMismatch`; no cache files written           |
| Git clone failure               | Error with remote name only (URL redacted from message)   |
| Non-UTF-8 staging path          | `Path` passed directly to `Command::arg`; no panic         |
| Non-finite edge weight          | `VcsError::Internal` from `edge_to_canonical_value`         |
| WAL checkpoint failure          | Error before rename; previous DB intact                    |

## Test coverage map

- Unit: `src/hash.rs` (hash correctness, edge cases), `src/types.rs`
  (`SnapshotId` validation, serde rejection), `src/sync.rs` (sync helpers,
  remote fetch, atomicity, FTS population).
- Integration: `tests/integration.rs` (cross-module composition, adapter
  pipeline, `VcsState` roundtrip).
