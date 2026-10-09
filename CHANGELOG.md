# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking (Rust crates)

- `khive-fusion` adds `WeightedRrf { k, weights }`, positive-weight validation, and weighted-RRF
  fusion errors. This is source-breaking for consumers with exhaustive matches on
  `FusionStrategy`, `FusionStrategyError`, or `FuseError`; update those matches to handle the new
  rank-based strategy and its validation errors. Weighted RRF checks positive finite weights and
  source-count alignment, preserves empty source slots, and errors when a fused score exceeds the
  finite deterministic-score range.

## [0.11.0] - 2026-10-08

### Changed

- Breaking Rust API change: `khive_vamana::VisitedSet` (also `khive_vamana::graph::VisitedSet`) is
  now a re-export of `khive_types::vector::VisitedSet`, the visited-node tracker shared with the
  HNSW index. The path still resolves, but it names a different type than 0.10.0 did, so a consumer
  that implemented a trait for the old type or depended on its crate identity adjusts to the shared
  one.
- Breaking Rust API change: `khive_storage::StorageError::WriterTaskTerminated` and
  `khive_runtime::events_split::WireWriterTaskFailure::TaskTerminated` each gain a field,
  `sqlite_full_codes: Option<(i32, i32)>`. Neither enum is `#[non_exhaustive]`, so a downstream
  pattern that names either variant without `..`, and a literal that builds either variant, no
  longer compiles. Add `..` to the pattern. Build the storage variant with the new
  `StorageError::writer_task_terminated(request_state)`, which leaves the field `None`, or set the
  field in the literal. On the events socket the field is omitted when it is `None` and defaults to
  `None` when absent.
- Breaking Rust API change: `khive_storage::entity::EntityFilter` gains `property_equalities` and
  `tombstones`, and `khive_storage::event::EventFilter` gains `outcome` and `payload_equalities`.
  Neither struct is `#[non_exhaustive]`, so an external struct literal that lists every field no
  longer compiles; end it with `..Default::default()`. A serialized filter from 0.10.0 still
  deserializes, and the new fields default to the earlier behavior: live rows only, with no outcome
  or JSON restriction.
- The `kkernel` git annotation repair preview now reads commit notes and project links for the
  acknowledged SHAs in batches of up to 900 SHAs, where it ran two queries per SHA. The class
  assigned to each SHA and the preview hash are preserved; 2,000 SHAs take six lookup reads.
- Entity updates now distinguish an omitted `description` from an explicit `null`: omission
  keeps the stored value, `null` clears it, and an empty string stays a concrete value. Before,
  `description: null` was a no-op. Canonical and atomic updates also refuse a present `salience`
  or `decay_factor`, `null` included, on an entity or edge target, where it used to be ignored;
  the record is left unchanged and the refusal comes before any sibling field is applied.

### Added

- `khive_types::vector` adds explicit f32 byte codecs: `encode_f32_le` and `decode_f32_le` for
  portable little-endian bytes, `encode_f32_native` and `decode_f32_native` for the host-order
  layout that sqlite-vec stores, and `VectorCodecError`, which the decoders return when the input
  is not a whole number of 4-byte values. `khive-storage` re-exports all five.
  `VamanaIndex::to_bytes` now writes the vectors segment of its portable container as
  little-endian on every target; the bytes are unchanged on little-endian hosts.
- `khive_fs::fd_relative::FileIdentity` reports the device and inode of an open file
  (`FileIdentity::of`) or of an entry under a held directory (`FileIdentity::at`, which identifies
  a final symlink itself). Equality compares the objects seen by each call and does not guard
  against later replacement or inode reuse. Unix only.
- `khive_fs::fd_relative::unlink_at` removes a file or symlink entry under a held directory
  without following it and refuses a directory. `rename_at` renames an entry between two held
  directories with the kernel's ordinary replacement semantics. Neither syncs the directory. Unix
  only.
- `khive_fs::directory_walk::open_dir_nofollow` opens a directory read-only and refuses a symlink
  as the final component. `khive_fs::opened_file::open_regular_file_nofollow` does the same for a
  regular file, and refuses a FIFO or other non-regular file without waiting on it.
  `directory_walk::read_link_at` is now public, returns a `PathBuf`, and reports a target that
  fills the `PATH_MAX` buffer as `InvalidData`. Unix only.
- `khive_fs::fd_relative::open_file_at` opens one writable entry, write-only or read-write, under
  a borrowed directory descriptor. `OpenFileOptions` sets the creation policy through `Create`
  (`No`, `IfMissing` or `Exclusive`), an optional non-blocking flag, and the creation mode, which
  the umask filters. It always refuses a final symlink and the names `.` and `..`, sets
  close-on-exec, and never truncates existing contents. Unix only.
- Entity and event filters in `khive-storage` can match stored JSON and liveness.
  `EntityFilter::property_eq` and `EventFilter::payload_eq` require SQL JSON equality on the
  stored properties or payload for a `$.field[.subfield]` path, and the SQLite stores refuse any
  other path as invalid input. `EventFilter::outcome` matches one event outcome.
  `EntityFilter::include_tombstones` and `EntityFilter::tombstoned_only` select
  `EntityTombstones::All` or `EntityTombstones::Only`; the default stays live rows. `khive-db`
  adds `StorageBackend::list_namespaces(kind, NamespaceLiveness)`, which lists the sorted, distinct
  namespaces that hold records of one exact kind across entities, notes and events.
- `khive_storage::SqlStatement::new(sql, params)` builds a statement without a diagnostic label,
  and `SqlStatement::labelled(label)` sets or replaces the label.
- `khive_storage::SqlRow` adds typed accessors. `uuid` and `opt_uuid` read a native UUID or UUID
  text. `f64` and `opt_f64` read a float or convert an integer. Each returns `SqlColumnError` for
  an absent column, an invalid value or another SQL variant, and the `opt_` forms return `None`
  for SQL NULL. `text_or_none` and `i64_or_none` return `None` for anything that is not text or an
  integer.
- `SqlReader::count(statement)` runs a count statement and returns its nonnegative integer scalar
  as `u64`. A negative value, a missing row, NULL or a non-integer scalar returns
  `StorageError::Internal`. It is a provided method, so existing `SqlReader` implementors need no
  change.
- `khive_db::env` is a new public module of environment readers. `env_parse_or` parses a variable
  into any `FromStr` type and falls back to a default when the variable is missing, not Unicode or
  unparseable. `env_flag` reads a trimmed, case-insensitive boolean (`1`, `true`, `yes`, `on` and
  their opposites) with a default. `cached_env_flag` fixes a flag at its first read in a
  caller-supplied `OnceLock`. The pool settings and the drain timeout read their environment
  variables through it with the same parsing as before.

### Fixed

- `kkernel reindex` now walks entities and notes with a stable cursor instead of a numeric offset.
  A record written during the run is visited once, and the pass continues after a record is
  soft-deleted at a batch boundary.
- When SQLite reports a full disk (`SQLITE_FULL`) and the failure ends an admitted write
  transaction and retires the writer task, the error now carries `sqlite_disk_full` with the
  primary and extended SQLite result codes. It still reports `request_state`
  `side_effects_unknown`, `task_terminated: true` and `retryable: false`. Before, the same failure
  reported a generic `writer_task_terminated` with no SQLite codes. A commit failure whose rollback
  succeeds keeps its existing request-failed form.
- Bulk `link` now validates every entry, including one that repeats an earlier entry's source,
  target and relation. Before, such a repeat was counted as skipped without checking its weight,
  metadata or endpoint rules. In the default atomic mode an invalid repeat now refuses the whole
  batch. With `atomic=false` it is reported in `errors` and counted as failed, and an entry that
  failed no longer reserves its key, so a later valid entry for the same edge, such as one with
  `resurrect=true`, is applied.
- `kkernel kg validate` exits with status 2 when the rules file is malformed TOML or uses the
  unsupported YAML format (`.yaml` or `.yml`), as ADR-034 specifies. Before, both exited with
  status 1, the same as a graph rule violation. An unreadable rules file and graph rule violations
  keep status 1.
- `exec.tree_put` now rejects an edit whose `delete` is not a boolean with
  `edits[N].delete must be a boolean`, and publishes nothing. Before, any value other than `true`,
  such as the string `"true"`, counted as not deleting. An omitted or `null` `delete` still means
  no delete.
- A task update that sets `due` together with both `due_timezone` and `timezone` now validates
  both zone names, and a malformed one refuses the whole update. Before, a valid `due_timezone`
  hid an invalid `timezone` and the update succeeded. When both are valid, `due_timezone` still
  wins.
- `knowledge.upsert_atoms` no longer treats an atom as a domain mirror because one of its tags
  merely contains the text `type:domain`. Tags such as `type:domain-extra` or `prefix:type:domain`
  now allow both the slug upsert and the properties-only update, while an exact `type:domain` tag
  still refuses the update. Tag text that is not a JSON array of strings keeps the earlier
  substring check.
- Code source ingest (`code.ingest`) of a folder with no governing manifest, reached by a path
  that ends in `..`, now names the project after the folder it resolves to. Before, the project
  name came from the literal path text, so the same folder spelled `project/child/..` and
  `project` recorded a different `source_project`. Both spellings now agree.
- `schedule.schedule` now checks the `entity_kind` and `note_kind` of a scheduled singleton
  `create` action by the rule `create` itself applies. An empty string, or a value that is neither
  a string nor null, is refused with the same error `create` returns, even when the other kind is
  the one being created. Before, a non-string value was ignored when the action was scheduled. The
  refusal comes before any schedule is written.
- `git.digest` now refuses a `project` argument that is not a string, with
  `project must be a string when provided`, and writes nothing to the graph. Before, a non-string
  value was ignored and the digest ran against an automatically resolved or created project
  anchor. An omitted or `null` `project` still resolves the anchor automatically.
- `comm.thread` now refuses an `after` cursor that resolves to a note which is not a message, with
  the existing `does not resolve to a message` error. Before, any note id was accepted as a cursor.
- Errors from opening the database backend now name it. A failure to create the database
  directory, or to open the in-memory store, begins with `backend main:`.

### Restored

- The retrieval surfaces removed in 0.10.0 are back, unchanged from 0.9.x, as the base for
  their integration work: `KhiveRuntime::hybrid_search_with_strategy`;
  `khive_retrieval::hybrid::dual_index` (`DualIndexRouter`, `DualIndexConfig`,
  `DualIndexStrategy`); `khive_retrieval::query_ir` (`QueryNode`, `FuseStrategy`,
  `FilterPredicate`, `RerankMethod`); `khive_retrieval::metrics` (`MetricEvent`,
  `MetricValue`, `MetricsSink`, `NoopSink`, `RecordingSink`); and the `persist` feature with
  the `persist`, `replay` and `weights` modules (`RetrievalPersistence`, `PersistenceStats`,
  `PersistError`, `ShadowValidationConfig`, `ShadowValidationResult`, `ShadowMetrics`). A
  0.10.0 consumer that migrated away from any of these keeps working; the removal entries
  below describe 0.10.0 only.

## [0.10.0] - 2026-10-07

### Removed

- Five public `khive-runtime` methods that returned only a vector or a record and so hid
  embedding-input truncation are no longer public entry points. This is a source-breaking change
  for an out-of-tree Rust consumer. `KhiveRuntime::embed_document_with_model` is removed; migrate
  to `embed_document_with_model_outcome`. `KhiveRuntime::create_entity`,
  `KhiveRuntime::update_entity` and `KhiveRuntime::update_note` are no longer public; migrate to
  `create_entity_with_embedding_report`, `update_entity_with_embedding_report` and
  `update_note_with_embedding_report`. The `create_notes_atomic` re-export is dropped from the
  crate root; migrate to `create_notes_atomic_with_report`, which keeps the same all-or-none note
  write and adds the aggregate report.

### Changed

- Breaking Rust API change: `khive-channel-node::ReceiptRejection` now aliases
  `khive_channel::ReceiptRejectionReason`; local `PinUnavailable` and `SourceUnavailable`
  failures use `ReceiptVerification::Unhandled(ReceiptReadFailure)` instead of rejection.
  Poll/status callers must retain the receipt cursor for those read failures. Submission
  pin or outbound-record read failures return transient transport errors; retain the
  pending message and retry when local state is available.
- Breaking Rust API change: `khive-channel::SendOutcome::RecipientStored` and
  `RecipientQuarantined` now carry `VerifiedRecipientReceipt` instead of
  `DeliveryReceipt`. Verify with the pinned recipient key through
  `VerifiedRecipientReceipt::verify` and inspect the payload through `receipt()`.
  `VerifiedRecipientReceipt` and `ReceiptVerificationError` move from
  `khive_runtime::comm_transport` to `khive_channel`; update those imports.
- Legacy write methods that previously returned `Ok(record)` when the embedder bounded their input
  now return an error that carries `committed=true`, the committed `record_id` and the serialized
  truncation report, with `retryable=false`. The stored record is complete, so reconcile by
  `record_id` rather than retrying the mutation. The affected methods are
  `create_entity_with_attachments`, `create_note`, `create_note_with_embedding_content`,
  `create_note_with_decay`, `create_note_with_decay_for_embedding_model` and
  `update_entity_if_unchanged`. Input within the embedder limit keeps the previous return value.
  Report-aware counterparts return the record and the report together:
  `create_entity_with_attachments_and_report`, `create_note_with_embedding_content_and_report`
  (pass `None` for the default note path, which also replaces `create_note`),
  `create_note_with_decay_and_report`, `create_note_with_decay_for_embedding_model_and_report` and
  `update_entity_if_unchanged_with_embedding_report`.
- `embed_document`, `embed_document_batch_with_model` and `embed_document_batch` now return an
  error when the embedder bounds any input. Use `embed_document_outcome`,
  `embed_document_batch_with_model_outcomes` or `embed_document_batch_outcomes` to receive the
  bounded vectors with their byte counts.
- `memory.remember` retains its visibility token while disclosing bounded embedding input on a
  fresh keyed or unkeyed write. An identical keyed replay keeps the original fences and reports
  the truncation computed for that content. Receipt-only Rust entry points remain available, and
  report-returning entry points are added.

- Fresh, uncorrelated inbound email now defaults to the `channel:email` mailbox
  when `KHIVE_EMAIL_DEFAULT_ACTOR` is unset or blank. Deployments currently
  reading new mail from `local` must configure a `channel:email` serving actor
  with `[actor].mailbox_readers` and select that mailbox, or explicitly set
  `KHIVE_EMAIL_DEFAULT_ACTOR=local` to retain the previous routing. Correlated
  replies still route to the original sender.
- `khive-retrieval` no longer compiles `khive-hnsw`, `khive-bm25` or `khive-db` by default. The
  `khive-hnsw` and `khive-bm25` types it re-exports at the crate root (`HnswIndex`, `HnswConfig`,
  `Bm25Index`, `Bm25Config` and the rest) are now behind the new `hnsw` and `bm25` features, so a
  Rust consumer that uses them must enable the matching feature. `persist` implies both and
  `checkpoint` implies `hnsw`, so those consumers need no change. `khive-db` is now a
  dev-dependency only.

## [0.9.0] - 2026-09-27

### Removed

- `KindHook::prepare_note_update`, the trait method that sequenced a pack's
  `normalize_note_update` and `validate_note_update`. The registry now runs the two
  halves in that order at its single dispatch site, so a pack implements the halves
  and cannot express an order. This is a source-breaking change for an out-of-tree
  `KindHook` that overrode the removed method: such an impl no longer compiles, and
  the migration is to move its normalization into `normalize_note_update` and its
  checks into `validate_note_update`. There is no compatibility path, because an
  override of the sequencer is the thing being removed: it silently replaced the
  validator call along with the order.

### Added

- Chunked blob uploads: `blob.begin`, `blob.put_part`, `blob.commit` and
  `blob.abort` stage an object across sequential parts and publish it under the
  BLAKE3 reference the completed bytes hash to, so an object larger than one
  request can be stored without holding it in a single message. Staged uploads
  are bounded, 128 concurrently and 16 per actor by default, overridable with
  `KHIVE_BLOB_UPLOAD_MAX_ACTIVE` and `KHIVE_BLOB_UPLOAD_MAX_PER_ACTOR`; a caller
  past either ceiling is refused by name rather than allowed to accumulate
  staging entries.

- The `web` pack's verb surface (ADR-191, superseding ADR-175): `web.fetch`, `web.extract`,
  `web.ingest`, `web.search`, and `web.refresh` fetch, parse, and search live HTTP(S) content
  under an egress policy (address-class checks, an optional host allowlist, scoped credentials,
  a bounded request-header set, and byte/time/result-count ceilings), replacing the prior
  pack's local-manifest-only `web.ingest`. New `site`/`page`/`resource` entity subtypes and the
  `links_to` edge relation support the ontology; identity is deterministic by canonicalized
  address, so an unfetched link target and its later-fetched body are the same row.

- A new Python client package, `khive-py` (`python/`): a unified record interface
  (id/kind/namespace/properties/metadata/tags/lifecycle timestamps, with kind as
  the universal discriminator) over the daemon's Unix-socket wire, with a
  weighted-incidence model underneath it — an edge is a record plus N
  incidences, so a binary edge is the two-incidence special case rather than
  the schema, `link()` takes `source_weight`/`target_weight`, and `hyperlink()`
  takes an explicit member list. It also ships an `HttpTransport` /
  `AsyncHttpTransport` for a khive-cloud deployment with typed errors
  (`AuthError`, `RateLimited`, `BadRequest`, `ServerError`), an MCP session
  helper, an `AsyncSession` over the local daemon socket for callers already
  inside an event loop, a typed `Session.recall`, and a `khive-cloud` console
  script (`whoami`, `exec`, `tools`, `health`) whose credential comes only from
  `KHIVE_CLOUD_API_KEY` and is never echoed, including when a server response
  reflects it back.

- A new `tool` pack (ADR-180) and `exec` pack (ADR-181), both loaded by
  default: a policy-gated registry for tools, skills, plugins and verbs
  (`tool.suggest`, `tool.request`, `tool.grant`, `tool.check`, and more)
  joined to graph capability concepts by `implements` edges, with asynchronous
  grant approval and a fast path for a pre-granted tool; and sandboxed tool
  execution over content-addressed trees (`exec.tree`, `exec.tree_get`,
  `exec.tree_diff`, `exec.tree_put`, `exec.run`, `exec.receipt`, `exec.runs`,
  `exec.events`, `exec.identity`) under a seatbelt profile built from
  configured read roots and resource limits, recording a receipt for every
  outcome including refusals.

- Git dev-loop verbs (ADR-182): local, receipted `git.checkout`, `git.diff`,
  `git.branch`, `git.commit`, `git.receipts`, `git.reconcile`, `git.status`,
  `git.log`, `git.init`, and `git.update_ref` (a compare-and-swap move of a
  branch to an existing commit); guarded remote `git.push`, `git.pr_open`,
  `git.pr_review`, and `git.pr_merge`, which prove fast-forward ancestry,
  push with an explicit `--force-with-lease`, and write a durable receipt for
  every attempt including refusals. A `git_write.repositories` row may set
  `merge_refusals = ["opener", "last_pusher"]` to refuse a merge dispatched by
  the pull request's own opener or by the account that pushed the head being
  merged. An operator may name a configured git executable
  (`git_write.program`) for the write surface, and a repository mapping may
  target an explicit local push destination instead of a remote platform.

- Ordered streams and keyed, versioned notes (ADR-172, ADR-174): `stream.append`,
  `stream.read` and `stream.stat` give a run dense, gapless per-stream sequence
  numbers with a conditional `expected_seq` append and a lease fence against
  competing writers; `create`, `get`, `update` and `list` accept a caller-chosen
  `key` so a note can serve as a durable, addressable head, and `update` accepts
  `expected_version` for compare-and-set instead of last-write-wins; a write may
  also carry a fence naming another key, kind and expected version, checked in
  the same transaction as the write it guards. `stream.batch` appends in atomic
  or per-member modes.

- A new optional `telemetry` pack (`KHIVE_PACKS=kg,telemetry` or `--pack kg
  --pack telemetry`; not in the default set): an operator-declared `[telemetry]`
  channel table classifies event kinds onto a `durable` or `ephemeral` carrier
  with a `stop`/`gap` failure posture. `telemetry.emit` accepts a kind and an
  arbitrary JSON payload; `telemetry.read` counts and returns durable records;
  `telemetry.channels` reports the effective table.

- `gtd.repair(items=[{id, changes}], apply=false)`: an explicit, caller-reviewed
  way to correct a stored task's `created_at`, `updated_at` or `status` when it
  was written outside the lifecycle rules, defaulting to a dry run that returns
  the plan without writing.

- `gtd` task creation accepts an optional `timezone` (an IANA zone name) for
  anchoring a date-only `due` to a caller's own zone instead of the runtime's
  single configured display zone, and echoes the zone actually used back as
  `due_timezone` on every task carrying a `due`, including when the zone was
  defaulted rather than named explicitly.

- `gtd.tasks` accepts `tags`, `tag_mode` (`any`, the default, or `all`), and
  `context_entity_id` filters, applied before pagination and honored when
  computing the excluded-terminal-task hint.

- `comm.send` and `comm.reply` accept an optional caller key. A retried call
  with the same key and an unchanged payload returns the original message
  pair's ids, thread and timestamp with no new write, instead of creating a
  duplicate; a different payload under the same key is refused as a conflict.

- `comm.inbox` and `comm.thread` accept `mailbox_actor`. An owner can name
  trusted readers in a new `[actor] mailbox_readers` config, and a granted
  reader can inspect that owner's inbox or thread without changing caller
  identity, marking anything read, or gaining any right to reply.

- `comm.read` returns the message body (`subject`, `content`, `from`, `to`,
  `direction`, `created_at`) by default once the read mark succeeds, so reading
  a message no longer needs a follow-up `comm.thread` call. Pass `body=false`
  to keep the previous acknowledgement-only shape; `comm.mark_read` is
  unchanged in both modes.

- `knowledge.upsert_atoms` accepts `dry_run`: it runs every per-item input
  check, the secret-gate check, and the read-only-target checks the write path
  runs, over the whole batch, and returns a verdict per submitted item in
  submission order, without acquiring an atom writer or writing an atom row,
  index entry, or refusal event.

- `search` accepts `text_mode` (`all_terms`, the existing default, or
  `any_term`) for the lexical arm of KG entity and note search. KG search
  responses also carry `arm_participation`: per-arm (text and vector) evidence
  of whether each arm ran, was skipped, or errored, with bounded final
  candidate counts.

- A new admin capability moves records between namespaces, backed by a
  namespace schema census derived from the live composed registry rather than
  a fixed table list, so a migration that adds a namespace-bearing table is
  refused by name instead of being silently left unmoved by an older sweep.

- `kkernel entity-type-backfill --dry-run|--apply`: an admin subcommand that
  promotes an entity's legacy `properties.type` value into the `entity_type`
  column, classifying against the full composed runtime registry (the
  built-in table plus every loaded pack) rather than the built-in table alone.

- `schedule.remind` and `schedule.schedule` accept fixed intervals
  (`every:<N><s|m|h|d>`) and five-field cron expressions evaluated in UTC,
  alongside the existing `daily`/`weekly`/`monthly` aliases. One parser now
  serves both creation and the executor, so a stored repeat is always one the
  drain can advance; an impossible or malformed pattern is refused at
  creation rather than stored and silently never firing.

- `[gate].deny_writes_for`: actor-id patterns (`*` as the only wildcard)
  matched against it restrict a matching enrolled caller to explicitly
  reviewed read operations; writes and unclassified operation names fail
  closed for a matching actor.

- The daemon refuses to start a second `khived` instance on a socket a
  supervisor has already claimed, reading a marker file written beside the
  socket instead of losing the race to a client's on-demand spawn, which
  previously won by cadence and left the machine served by whichever process
  happened to call first.

- The `@khive-ai/cli` npm compatibility package is now published alongside the
  umbrella package on every release, with matching versions validated before
  publish.

### Changed

- KG and coordinated search publish `rank_score`, `rank_score_kind`, and retained
  component `signals`. The deprecated `score` field is an exact compatibility
  alias. `min_rank_score` applies an inclusive fixed-point floor before the final
  result limit, including time ordering; `min_score` remains a deprecated alias,
  and supplying both names is rejected. Agent presentation rounds the new score
  fields after filtering and omits an empty evidence object. Knowledge search
  and memory recall keep their existing scoring contracts.
- **Breaking**: runtime `SearchHit` and `NoteSearchHit` values now include
  `rank_score_kind` and typed `signals`. Struct constructors must identify the
  ordering strategy and explicitly retain available component scores; an absent
  signal uses `None`. `FusionExecutor` implementers must add
  `rank_score_kind()`, choosing `rrf`, `vector`, `keyword`, `weighted`, or `union`
  according to the executor's ordering strategy. The existing `score` field
  remains the canonical deterministic ordering value. Note search computes its
  salience weight with fixed-point arithmetic, so rounding at the smallest score
  increment can differ from the previous floating-point calculation.
- **Breaking**: the `web` pack no longer reads a `.well-known` application manifest or emits
  `machine_view`/`agent_tool`/`agent_skill` entities — see the `web.*` entries above and
  `docs/packs/web.md`.
- Standalone `stream.append` now consults the owning pack's `KindHook::prepare_create` before
  creating a caller-selected note kind, preserving pack-owned admission and field normalization.
  When the kind has a hook, an object record's fields are read as the create arguments the hook
  sees, so a pack normalizes or refuses the same input it would receive through `create`.
- `KHIVE_EMAIL_DEFAULT_ACTOR` now falls back to `local` when unset or blank,
  matching `KHIVE_EMAIL_INGEST_NAMESPACE` and the adjacent startup resolver,
  instead of a hard-coded identity with no meaning outside the deployment it
  was named for. Behaviour with the variable explicitly set is unchanged.
- Uncorrelated Telegram messages can be routed with
  `KHIVE_TELEGRAM_DEFAULT_ACTOR`; the unset or blank default remains the isolated
  `telegram:bot` inbox. Routing them to `local` requires an access policy that
  protects the local inbox from anonymous readers.
- **Breaking**: `gtd.complete` and `gtd.transition` to `done` now refuse with
  `reason=dependency_blocked` when the task's dependency edges are unresolved,
  naming each blocker, its state, and the count; a cancelled or missing
  blocker refuses on the same grounds, because neither establishes that the
  depended-on work happened. Cancelling a blocked task, and every non-terminal
  move, stays legal. Pass `ignore_dependencies=true` on either verb to keep
  the previous always-succeeds behavior.
- `memory.prune` accepts `min_effective_salience`, selecting on the same
  decay-adjusted salience value `memory.recall` already ranks with, instead of
  only the raw stored `salience` column. A memory whose effective salience has
  decayed near zero can now be selected for pruning even though its stored
  `salience` alone would not clear a `min_salience` floor; `min_salience` is
  unchanged and the two selectors union rather than replace one another.
- `active` and `waiting` tasks can move directly to `someday` instead of first
  requiring an intermediate `next` transition.
- `gtd.tasks`'s default listing now excludes a task whose stored status is not
  one of the canonical open states, the same way it already excluded terminal
  ones, instead of listing an unrecognized legacy status as open work; the
  empty-result hint gains `unrecognized_status` alongside `done` and
  `cancelled`.
- `brain.create_profile`'s `seed_priors` now rejects an unrecognized top-level
  key (for example a `relevance` shape the handler never read) with
  `InvalidInput` naming the key, before any profile is written, instead of
  silently ignoring it and creating the profile anyway.

### Fixed

- The events daemon no longer releases SQLite's advisory locks on its own
  database. Permission hardening of the `-wal`/`-shm` sidecars ran after the
  database was opened and closed a descriptor on each file, which under POSIX
  drops every lock the process holds on that inode; a backup or inspection
  connection closing afterwards then took itself for the last connection,
  checkpointed, and unlinked the sidecars while the daemon kept writing to the
  unlinked files. Hardening now runs before the open, the post-open check uses
  `lstat` only, and a cross-process test asserts the locks are held.
- The email channel stopped receiving mail for up to about an hour after the
  host woke from sleep: its OAuth token cache measured freshness on a
  monotonic clock that does not advance while the system sleeps, so a token
  fetched before sleeping read as fresh long after it had actually expired in
  wall time. The cache now checks both the monotonic and wall-clock deadline,
  and a token the server rejects is invalidated immediately so the next poll
  fetches a replacement.
- Inbound IMAP fetches are now capped per message
  (`KHIVE_EMAIL_IMAP_MAX_MESSAGE_BYTES`, default 25 MiB) and per page
  (`KHIVE_EMAIL_IMAP_MAX_PAGE_BYTES`, default 50 MiB); a message over the
  per-message cap is quarantined without its body instead of stalling the
  poll or exhausting memory, and later messages on the same page still
  ingest.
- The Telegram channel now honors a 429 response's `retry_after` interval,
  pausing outbound sending for that interval, instead of retrying a
  rate-limited channel at its normal cadence.
- A cron expression that can never occur on any real calendar date (for
  example `0 9 30 2 *`, February 30th) is now refused when a schedule is
  created, instead of being accepted and stored without ever firing.
- Daily and weekly schedule recurrence no longer panics when the next
  occurrence would fall outside the representable date range; it reports
  that no successor exists instead.
- A remote git URL with an `@` inside a path segment (for example
  `https://evil.example/x@github.com/org/repo`) no longer has its real host
  silently dropped when the git pack derives a GitHub `owner/repo` slug for
  policy checks.

- Knowledge-graph sync now refuses an entity carrying the reserved
  `khive:secret_gate` property before it can replace the database, stores an
  entity kind given as an alias or case variant under its canonical name, and
  hashes a symmetric edge (`competes_with`, `composed_with`) the same way in
  either endpoint order, so status no longer reports a change right after a
  clean sync.
- `neighbors` and `traverse` resolve a full-id anchor or root by id, the same
  way `get` does. An absent anchor or root is now a not-found error, naming the
  missing root for `traverse`, instead of an empty result; the edges and
  neighbor records returned stay scoped to the caller's visible namespaces.
- Remote issue and pull-request ingest no longer blocks a runtime worker on a
  slow or hung `gh` call: the probe and page fetches run as async child
  processes with a 60-second deadline and bounded output, and a failed page
  reports its typed reason (timeout, output limit, missing program). Moodboard
  raster decode, resize and encode run off the async workers.
- Pack schema ownership recognizes a table however its DDL spells the name
  (quoted, bracketed, `main.`-qualified, `TEMP`, or behind a comment), and
  every pack's tables are claimed before any DDL runs, so a collision between
  two packs fails boot by name with nothing applied. A repeated named-vector
  lookup reuses the verified store instead of re-scanning the vector table and
  taking the writer on every call.
- Brain profile accounting: the dispatch hook no longer holds a namespace lock
  across unrelated requests, and pending signals are bounded (drops are counted
  in `brain.state`). Feedback that names a served row must match that row's
  namespace, target and accounting profile. An archived profile no longer
  accumulates automatic updates, an explicit recall naming an archived profile
  is refused, and the default profile can no longer be deactivated or archived.

### Security

- Staged upload filesystem operations are race-free on Unix and are not on other
  platforms. The Unix paths open relative to a retained root descriptor and
  refuse to follow symlinks; the non-Unix paths validate and then resolve the
  same path again, so a local process able to write inside the blob root can
  substitute a directory, junction or reparse point in that window. The trust
  boundary is stated accordingly: the blob root, its contents and its ancestors
  must be writable only by trusted processes, including processes under the same
  account. A handle-based replacement is follow-up work and needs native coverage
  on the affected platform before it can be believed.
- `rustls` bumped to 0.23.45 for RUSTSEC-2026-0285: a TLS 1.3 handshake
  message could be accepted across an encryption level boundary in 0.23.42
  and earlier.
- The web pack's pinned egress HTTP client no longer picks up a proxy from
  the process environment or system configuration, and IPv4-mapped, NAT64,
  and 6to4 IPv6 address forms carrying a private or loopback IPv4 destination
  are now classified and refused instead of being read as ordinary public
  addresses.
- **Breaking**: the Telegram channel now accepts an inbound message only when
  both the chat id and the sender id match. A deployment whose configured
  chat is a group must set `KHIVE_TELEGRAM_AUTHORIZED_SENDER_ID`, or the
  channel refuses to start; previously a group chat needed no such setting,
  and a message from any member of that group was accepted and attributed to
  the maintainer.
- Secret-detection gate hardening across the release: a PEM private key is
  now matched by its body under the header rather than the header alone,
  lookup-key labels match whole rather than by suffix, masking is scoped to
  inline credential values, and a technical reference (a path or a hash)
  beside a credential word no longer triggers a false refusal.
- A generic `create` or update of a `message` note now refuses a
  caller-supplied `external_id`. The value is set only by inbound ingest and
  by the outbound delivery claim, so a caller can no longer choose an
  outgoing `Message-ID` or steer reply threading into another conversation.

## [0.8.0] - 2026-08-27

### Added

- Dedicated events daemon: the events plane is split from the main request path
  onto its own versioned transport, with independent routing and configuration
  (ADR-170).
- Content-addressed attachments: a new `attachments` table with durable claim
  fences and a phase-gated garbage collector that refuses to delete blobs still
  referenced by an active sweep (ADR-121, ADR-160).
- `BlobStore::get_bounded_verified` — blob reads that are both size-bounded and
  digest-verified on the read path (ADR-160 phase 1).
- `comm.inbox` bounded long-polling via `wait_ms`, `since`/`before` time-window
  filters, sender and recipient filters (`from_actor`, `from_prefix`,
  `exclude_from_actor`, `to_actor`), `subject_contains`/`content_contains`
  matching, projection via `fields`, and projected sent-message history via
  `box="sent"`.
- `comm.mark_read(ids=[...], atomic=false)` as the canonical bulk mark-read
  surface, with an opt-in all-or-nothing transaction and the released
  `comm.read(id|ids)` forms retained for compatibility. This closes #1387's
  residual scope after #1572 shipped best-effort bulk read marking and ADR-057
  superseded its original namespace/legacy-recipient assumptions (#1387).
- Deterministic GQL `SKIP` paging with structured `has_more`/`next_offset`
  continuation metadata for result sets beyond the query page bound (#1601).
- Semantic review workbench with a dedicated review contract, `khive.review.v1`
  (ADR-145).
- Service provenance and kind classification for entities (ADR-167),
  `person`→`org` and `org`→`org` edge pairs, and entity-tag filtering.
- Recall pipeline: `created_after`/`created_before` windows, multi-model vector
  fusion, a weighted feature-combination reranker on the main recall path,
  exposed `top_k`/`fusion_strategy`/`score_floor` knobs, and an
  `IdentityReranker` plus cross-encoder re-export seam.
- Dual embedding model registry, MiniLM alongside paraphrase (ADR-043), an ANN
  consumer-pending write log with a model-sequence index, and the
  `KHIVE_ANN_FRESH_TAIL` knob.
- Code pack L2 symbol-tier scanner and call-graph ingest, preserving ingest
  provenance and coverage.
- Remaining request-surface parameter additions, each optional:
  `brain.auto_feedback(target_id)`, the exact full UUID or compact id of the one
  result being judged, required when `signal` is supplied and required to occur
  exactly once in `results`; `search(source)`, filtering by exact retrieval
  source (`text` | `vector` | `both`) inside a bounded candidate window before
  the caller limit, where `both` means the hit received text and vector
  contributions; `list(session_id, observed, selected)` for `kind="event"`,
  filtering events by exact full session UUID and by events that observed or
  selected every listed exact full UUID, with short-prefix resolution rejected
  because it can miss or be ambiguous; `comm.thread(fields)`, the message-field
  projection already shared with `comm.inbox`, rejecting unknown fields;
  `code.ingest(tiers)`, selecting any of `l1` | `l1.5` | `l2`; and
  `update(entity_type)`, setting a registered entity type validated against the
  entity kind's closed vocabulary and reindexed.
- A precisely specified, supervisor-readable daemon exit-code contract (ADR-049
  Amendments 4-7). This contract is new rather than changed: v0.7.0 carried no
  exit-code table, no emitted set and no reserved set.
- Writer and storage observability: an append-only writer-timeout event sink
  with a liveness heartbeat, slow writer-stage timing sinks that record queued-
  write latency rather than only failures, and writer-contention plus
  WAL/checkpoint diagnostics.
- Recoverable `git.digest` receipts.
- Moodboard pack: visual-asset ingest and exact descriptor-space retrieval
  (ADR-148), plus actor-scoped calibrated pairwise preference learning
  (ADR-149) — actor-attributed randomized serve and judgment events,
  deterministic grouped logistic BCE training, temperature and tie calibration,
  and `lattice-fann` model serialization and inference. The pack registers seven
  verbs: `moodboard.ingest`, `.search`, `.serve`, `.judge`, `.preference`,
  `.train_preference`, `.model`. It is not in the default pack set and must be
  selected explicitly via `KHIVE_PACKS` or `--pack`.

### Changed

- `resolve_project_actor_id` (khive-runtime) now returns
  `ConfigError::ExplicitConfigMissing` when the explicit
  `--config`/`KHIVE_CONFIG` path does not exist, instead of resolving to
  `None` and falling through to discovery. This matches the explicit-tier
  contract of the database-anchored config loader (ADR-035).
- Daemon recovery is gated and fail-closed, and hung SQLite reads are now
  interrupted rather than blocking indefinitely.
- `query` takes `page_size` for the result-page bound, with a minimum of 1, a
  default of 500 and a hard cap of 10 000. The former `limit` is retained as a
  deprecated alias and is mutually exclusive with `page_size`; a `LIMIT` in the
  query text composes as the smaller of the two bounds.
- Schema migrations advance from V16 to V20, plus a coordinated V21 attachments
  cutover. V21 is deliberately not an ordinary versioned migration: it is
  applied through a coordinated path (`ATTACHMENT_CUTOVER_VERSION = 21`), so a
  database at V20 records V21 only once that cutover completes.

### Breaking (Rust crates)

Source-breaking for callers of the published crates. Neither is on a wire
format, so MCP clients are unaffected; only code compiling against
`khive-storage`/`khive-types` needs to change.

The version number carries this signal on purpose: under Cargo's semver rules a
`0.x` minor bump is the breaking bump for `0.x` crates, so `0.7 → 0.8` is what a
dependent's resolver reads as incompatible.

- `BlobStore::get` — removed, superseded by `BlobStore::get_bounded_verified`,
  which bounds the read and verifies the digest on the read path. Callers move
  to the new method; there is no deprecated shim.
- `Entity::with_content_ref` — removed builder method. The public `content_ref`
  field itself remains, so construction through the field is unaffected.

### Removed

An on-disk schema change, listed separately from the API breaks above because it
is a different kind of removal with a different consequence.

- The legacy `entities.content_ref` column, retired by the V21 attachments
  cutover. This one is on-disk rather than an API change, and readers are
  unaffected: `content_ref` was already computed as a subquery over the
  `attachments` table before V21 ran, so consumers were being served from the
  new table already. What it does change is rollback — see the migration note
  below.

### Migration note — V21 is forward-only

A database that has run V21 cannot be served by a v0.7.0 binary. Pin `v0.7.0`
before upgrading if you need to roll back.

V21 does not discard data it cannot account for. The drop is issued inside the
coordinated cutover transaction and only after a universal validation pass: any
entity carrying a `content_ref` without an exactly-matching attachment row
aborts the migration with an `InvalidData` error. The migration refuses rather
than dropping.

## [0.7.0] - 2026-08-02

This release was tagged without a changelog entry. The entry below records the
one item that was pending in `[Unreleased]` at the tag; for the full contents
see the `v0.6.0...v0.7.0` comparison.

### Added

- `whoami` verb (kg pack, bare name): reports the caller's actor reference,
  write namespace, and read-visible namespace set already resolved by the
  runtime for the current request.

## [0.5.0] - 2026-07-13

### Added

- BlobStore content-addressed blob capability (ADR-111, #922).
- GQL WHERE operators `CONTAINS`, `STARTS WITH`, `IN`, `IS NOT NULL` (#892).
- ADR-104 Stage C entity-anchored recall candidate extraction (#881).
- `resource.cost_unit` emission on runtime operations (ADR-103 Amendment 1, #927).
- Atomic proposal create plans in `khive-runtime` (#904, #928).
- Pack-declared entity-type subtype composition at boot (ADR-017, #925).
- Vamana ADR-110 Layer A feature-gated parallelism with deterministic serial fallback (#896).
- Request correlation id threaded across daemon frames and audit events (#948, #951).
- Slow-request and timeout logging for `knowledge` pack compose (#915).

### Changed

- Daemon strict mode now fails a request on fallback instead of degrading silently (#947, #949).
- Publish pipeline uses topological crate order with path-only cyclic dev-deps (#901).
- `lattice-embed` and `lattice-fann` dependencies bumped to 0.6.0 (#885).

### Fixed

- `gtd` preserves RFC 3339 due-date fidelity in agent-mode presentation (#956).
- `brain.event_counts` normalizes the actor filter and adds `counts_by_verb` (#943, #944).
- `pack-kg` exposes effective list limits (#894, #930).
- `memory.recall` emits `recall_executed` events (#866, #929).
- FTS5 metacharacters sanitized in `khive-db` query construction (#916, #932).
- Fired schedule reminders deliver to the creating actor's inbox (#897).
- ADR-091 Plank 1 background age sweep over `tx_registry` (#921).
- `memory.recall` bounded by a fail-soft deadline (#919).
- `pack-comm` health and probe honor the injected namespace (#914).
- `pack-schedule` preserves the ISO timezone offset in remind create-response rendering (#911).
- `pack-kg` create help schema includes the `resource` entity kind (#909).
- `pack-git` masking applied uniformly to all external-origin ingest fields (#910).
- Secret gate uses token-boundary trigger matching to stop path-slug false positives (#888).
- Batched `neighbors` results keyed by the requested node (#891).

## [0.4.0] - 2026-07-12

Published to crates.io on 2026-07-12; backfilled here as the tag and changelog entry did
not accompany that publish.

### Added

- Workspace entity kind with `contains` edges to git/gtd/session notes (#873, #874).
- `khive-pack-code` v0 admin-only code-ingest path (ADR-085 Amendment 3, #848).
- Daemon-resident schedule drain tick with missed-event policy (#782).
- `resolve_reference` capability and recently-referenced ring (#762).
- `git.digest` paged ingest with URL clone cache (ADR-088 Amendment 1, #761).
- ADR-104 Stage A/B serve-time profile projection and bounded per-entity posterior term in
  recall scoring (#743, #745).
- Auto-extraction of `entity_names` from the recall query when the caller omits them (#738).
- `brain.event_counts` windowed event-counts read verb (ADR-103 Stage 1, #737).
- Daemon audit `duration_us` and phase telemetry, plus `comm.health` resource self-report
  (ADR-103 Stage 1, #732).
- MCP bridge protocol mismatch self-heal via in-place re-exec (#731).
- `khive-changeset` op-list model and NDJSON-delta codec (ADR-101, #715), with envelope
  `batch_id` and field-scoped update preimage (#725) and a `kg commit` tier-2 primitive (#721).
- Five configurable rule classes for `kg validate` (#712).
- Lifecycle events, checkpoint pressure telemetry, severity ladder, and link-verb audit
  enrichment (#703).
- `khive-pack-git` v0: commit/issue/PR ingestion with provenance edges (ADR-088, #692).
- Store backup tooling (ADR-100, #677) with per-job retention knobs (#684).
- ADR-099 atomic CLI surface for `kkernel exec --ops-file` (B1-B3, #678, #680).
- Single-writer `WriterTask` core with a bounded write queue; all write paths route through it
  (ADR-067 Component A, #670).
- Per-request identity served over one warm daemon registry (ADR-096 Fork 1, #660).
- Read-only `comm.health()` verb with daemon-persisted heartbeat rows (#615).
- ADR-091 Plank 0/2: open-transaction registry, WAL checkpoint instrumentation, and WAL
  TRUNCATE escalation with rate-limited guards (#591, #593).
- `context` verb: entity-anchored graph context in one call (ADR-089, #588).
- Recall serve-time attribution wired into the serve ledger (ADR-081 §5, #583).
- ADR-081 retune-driver substrate: implicit weight, bounded-mass fold gate, serve ledger (#497).
- `khive-pack-session` T1 verb surface (store/list/resume/export) and a ChatGPT export mirror
  source (#411, #525).

### Changed

- `khive-runtime`, `khive-db`, and `kkernel` share canonical/atomic decision-step and DML code
  paths across the ADR-099 B3 series, closing duplication between the two execution modes.
- `begin_tx` retired; session ingest routes through `atomic_unit` (ADR-099 D5, #673).
- Edge-relation error hints now derive from installed pack `EDGE_RULES` (#621).

### Fixed

- FTS5 metacharacters sanitized in recall/search query construction (#880).
- Malformed-policy output masked from Gate deny reasons and audit logs (#853).
- Typed validation errors instead of panics on invalid `khive-quant` train/encode shapes (#854).
- `resource` entity kind accepted in JSON/NDJSON import adapters (#856).
- Every published crate declares `rust-version` (MSRV 1.91.0) (#855).
- Bounded ANN wait in `memory.recall` with lexical fallback (#859).
- Exact-name entity lookup checked before hybrid fallback in `resolve` (#852).
- Substrate node labels (`entity`/`note`) made satisfiable in GQL/SPARQL (#857).
- `pack-git` scratch-cache ENOENT race closed on the macOS flake family (#847).
- `link()`/`link_many()` guarded against concurrent hard-delete (#826).
- LIKE wildcards escaped in entity name-prefix resolution and Vamana snapshot invalidation
  (#834, #824).
- `FeedbackExplicit` signal observation decoded from `target_id` (#831).
- Credentials masked in `pack-git` issue titles and PR notes without dropping content
  (#835, #785).
- Daemon confirms a dead process before killing it, closing a recovery race (#838).
- `comm.probe` cursor made commit-order safe (#827).
- DSL container-nesting depth and input length bounded (#823).
- `gtd` next/tasks push status/assignee/priority filters into SQL (#825).
- Unreachable strong-count checkpoint exit replaced with a watch signal (#822).
- Stale ANN served during rebuild so the recall request path is not blocked (#812).
- Compact hex prefixes normalized before LIKE-scanning hyphenated ids (#816).
- Generation-check on ANN install closes a stale-build race (#815).
- `ensure_clone` refuses unowned cache-key directories (#788).
- IMAP UID cursor progress persisted for the email channel (#784).
- GQL result truncation warns at the 500-row cap (#802).
- Punctuated identifiers split correctly in FTS5 query sanitization (#790).
- RFC 3339 timezone offsets honored in relative-time display (#800).
- OAuth token refresh bounded by a timeout under the cache lock (#787).
- Over-cap commit embeddings truncated in `pack-git` (#789).
- Comm/GTD backlog burn: inbox sender filters, thread cursor pagination, message tags (#757).
- `brain.resolve` defaults the actor from the caller's dispatch identity (#742).
- Exactly-once forwarding, stale-daemon recovery, and cold-boot FTS guard in the daemon (#698).
- Tier-2 actor+namespace bindings resolved on the `brain` feedback path (#699).
- Multi-backend `annotates`→edge resolution, tier-3 config anchor, curation merge SQL
  unification (#695).
- Silent local-dispatch fallback eliminated; config_id topology parity with graduated
  fail-loud behavior (#698).

### Performance

- Shared MCP measurement client for the benchmark program (#865).
- Flagship coverage manifest and validator for benchmark tracking (#862).
- Throwaway readiness socket dropped and unknown-verb listing cached (#647).
- Neighbor queries for `direction=both` halved via a single `UNION ALL` expansion (#648).

## [0.3.0] - 2026-07-01

### Added

- Email channel transport (ADR-056): SMTP/IMAP adapter, app-only OAuth2
  (XOAUTH2) authentication, an outbound delivery loop with `Message-ID` and
  reply-to-actor routing, and an inbound round-trip (greeting, maintainer
  match, reply correlation).
- Session pack `khive-pack-session` (ADR-080): OSS session storage with a live
  daemon mirror of Claude Code sessions and Codex CLI transcript mirroring.
- Brain router seam: feature-gated lattice-fann router (M1), a
  `brain.register_adapter` integrity verb, `build_context_vector` reading live
  posteriors, and `router_state`/`adapter_set` snapshot persistence.
- ANN persistence (ADR-079): persist and warm-load v2 ANN segments so the
  daemon warm window is bounded by load cost rather than a full rebuild; ANN
  warming degrades to FTS-only instead of erroring.
- Output-format axis (ADR-078): `OutputFormat` (`json` / `auto` / `table`) with
  shape-aware rendering, orthogonal to presentation mode.
- Batch `create_many` for bulk entity creation; optional `entity_type` on
  `neighbors` and properties on `traverse`; property/tag filters on note search.
- Pack core-backend accessor (ADR-073) and a `SubstrateCoordinator`
  cross-backend link with federated search (ADR-029 Phase 2).

### Changed

- `kkernel exec` now defaults to `Verbose` presentation per ADR-045 §2 (the
  scripted / operator surface); the MCP `request` tool keeps the `Agent`
  default.
- Subhandler verbs are gated by wire origin rather than globally.
- Traverse performance: a recursive-CTE join-order fix yields a large speedup,
  and graph-traversal queries are batched to remove N+1 lookups.
- Namespace model (ADR-007 Rev 6): attribution-only namespaces, a per-actor
  episodic memory carve-out, and namespace-blind by-ID storage.
- Bumped `lattice-embed` to 0.4.2 and `lattice-fann` to 0.4.2.

### Fixed

- `knowledge`: `compose` reads resolved section posteriors; recall never
  returns a silent empty result while the ANN index is warming; a poisoned
  warming mutex is recovered instead of aborting the server.
- `retrieval`: property/tag predicates are pushed below result truncation.
- `runtime`: char-boundary-safe secret gate (no UTF-8/CJK panic); the
  configured actor is threaded into the gate request.
- `comm`: actor-addressed delivery (ADR-057) fixes cross-actor messaging; an
  anonymous inbox read leak is closed.
- `mcp`: the embedding-env warning fires only when a `[[engines]]` block
  overrides the `KHIVE_EMBEDDING_MODEL` / `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
  pair, not when that pair is the applied fallback.
- Storage hardening: WAL-checkpoint discipline, BM25 poisoned-lock recovery,
  and `expires_at` honored in recall with `memory.prune` / `memory.vacuum`.

### Docs

- Per-crate READMEs, a crate-README template, and a full configuration
  reference.
- Stale `kkernel call` references replaced with `kkernel exec`.
- New and updated ADRs: 067/068 (cloud topology), 069/072 (Subject model), 073,
  074, 075, 076 (relation-set calculability), 078, 079, and 080.

## [0.2.11] - 2026-06-13

### Fixed

- Cross-platform compile: `DaemonRequestFrame` and `compute_config_id` imports
  in `kkernel/src/exec.rs` gated with `#[cfg(unix)]` to match their declaration
  in `khive-runtime`

## [0.2.10] - 2026-06-13

Full crates.io publish (all 29 workspace crates).

### Fixed

- `khive-brain-core` added to publish dependency order — unblocks
  `khive-pack-brain` on crates.io
- All inter-crate version references bumped consistently

## [0.2.9] - 2026-06-11

GitHub release only — crates.io remains at 0.2.8.

### Added

- Write-time secret detection gate — credential plaintext is hard-blocked from
  content-bearing verbs with a masked reason (#76, #83)
- Type-differentiated salience + decay defaults for memory writes: episodic
  0.3/0.02, semantic 0.5/0.005 (#70, #84)
- `knowledge.get` `include_sections` param (#89); draft atoms excluded from
  knowledge search by default with `include_drafts` opt-in (#78, #90)
- `brain_profile` config knob with 3-tier feedback resolution: explicit →
  namespace-bound → global (#52, ADR-035)
- Vendored JSON/JSONL data-leak pre-commit + CI check (#61)
- Reindex progress bars and domain mirror backfill (#19)

### Fixed

- FTS coverage gap: reindex now backfills pre-existing notes (#88) and entities
  (#96) into FTS; new canonical `entity_fts_document` constructor shared by all
  entity FTS write paths
- Embed-intent prefixes wired across all call sites — instruction-tuned
  embedding models receive `query:`/`passage:` correctly (#95)
- Hard delete purges soft-deleted records (#82)
- `kkernel kg validate` enforces closed-taxonomy schema checks (#41)
- khive-merge compiles again and is hardened (#21, #42)
- `kkernel exec` routes through the warm daemon when available (#63, #64);
  ANN warm removed from stdio — daemon owns hot state (#20)
- Nondeterministic HashMap ordering + startup robustness (#45)
- FTS UPDATE triggers narrowed to indexed columns — stops WAL bloat from
  embedding updates (#19)

### Security

- serde boundaries reject non-finite/NaN and invalid values (#49)
- gate-rego: entrypoint trimmed and validated to avoid latent fail-open (#43);
  tracing dependency restored (#66)
- Remote URLs redacted from git clone error messages (#40)
- brain `section_signals` validated; replay rows quarantined (#46)

### Changed

- Schema DDL moved from inline Rust strings to `.sql` files per ADR-015 (#51)
- Workspace dependency discipline; unused deps removed; `#[allow]` REASON form (#53)
- Oversized production files split; long functions extracted (#35, #56)
- Crate-doc shape + rustdoc hygiene pass (#36, #55)
- ADR freshness pass: ADR-019/023/024/030/051 (#48)

## [0.2.0] - 2026-05-22

### Added

- **kkernel binary** — new Rust admin/management CLI (ADR-076). Subcommands:
  `kkernel sync` (build real SQLite DB from NDJSON), `kkernel pack list`,
  `kkernel pack handler <name>` (pack introspection)
- **81-issue sweep** — resolved 77 issues across 12 parallel plays via `/show`
- ADR-065 through ADR-077 (13 new ADRs covering plugin intent routing,
  cross-plugin workflows, marketplace adaptation, note merge, batch conflict
  detection, bulk link creation, remote entity resolution, sync content-hash
  verification, communication/schedule packs, KG swarm self-correction,
  agent-driven PR workflow, kernel/MCP split, binary packaging strategy)
- DispatchHook trait for brain event emission (issue #158)
- PackTunable for MemoryPack with 3 tunable parameters (#159)
- entity_kind and note_kind in search response (#160)
- Properties filter for search verb (#163)
- Neighbor/traverse enrichment with entity name + kind (#162)
- Memory plugin, GTD plan/process skills, KG agent improvements
- CHANGELOG.md, CONTRIBUTING.md, SECURITY.md
- 25+ regression tests across the audit-correction round
- Deno CLI: kg diff, kg log, kg stats, kg doctor commands

### Changed

- **neighbors/traverse response**: `node_id` → `id` on the JSON wire (#148).
  Internal Rust still uses `.node_id`. Legacy `node_id` accepted as input alias.
- FTS5 score normalization: linear rescaling within result set (0.05, 1.0]
  replaces the collapsed `1/(1+|rank|)` formula (#149)
- VCS crate restructured: superseded modules removed per ADR-048, foundational
  primitives (hash, types, error) retained
- CI script runs Deno tests from `cli/` directory (fixes import map resolution)
- Clippy enforced with `--all-targets` (catches test-only dead code)
- `khive kg sync` now shells out to `kkernel sync` for real SQLite DB build
  (replaces the dishonest JSON-as-DB stub)

### Fixed

- Flaky tracing test: global subscriber + unique gate_impl name filter (#161)
- MemoryPack::active_config was dead code — tuning had no effect (#159)
- Pagination offset hardcoded to 0 for entity/note list (#145)
- Contract tests: query row wrapping + field rename handling (#138)
- annotates edge source-must-be-note constraint documented in ADR-002 (#146)

## [0.1.4] - 2026-05-20

### Added

- Brain pack with event-driven auto-tuning (ADR-064)
- Configurable recall pipeline (ADR-062)
- Retrieval objectives for vector, text, and graph proximity scoring (ADR-061)
- Bayesian fold extensions: precision tracking and epistemic weight (ADR-059)
- Fold cognitive primitives crate (ADR-058)
- Dynamic pack loading with inventory-based self-registration (ADR-063)

### Changed

- Pack system now uses inventory-based self-registration; packs declare themselves
  at compile time and are discovered at runtime without manual wiring

## [0.1.2] - 2026-05-17

Maintenance release. Pack architecture documentation updates and workspace version alignment.

## [0.1.1] - 2026-05-16

Maintenance release.

## [0.1.0] - 2026-05-16

### Added

- Initial release
- Core crates: `khive-types`, `khive-score`, `khive-storage`, `khive-db`,
  `khive-query`, `khive-runtime`, `khive-request`
- Pack system with built-in packs: `kg`, `gtd`, `memory`
- MCP server (`khive-mcp`) exposing a single `request` tool that dispatches
  verbs through the loaded pack registry
- Deno CLI for git-native knowledge-graph operations
- Marketplace plugins for KG and GTD workflows

[Unreleased]: https://github.com/ohdearquant/khive/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ohdearquant/khive/compare/v0.2.11...v0.3.0
[0.2.0]: https://github.com/ohdearquant/khive/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/ohdearquant/khive/compare/v0.1.2...v0.1.4
[0.1.2]: https://github.com/ohdearquant/khive/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/ohdearquant/khive/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/ohdearquant/khive/releases/tag/v0.1.0
