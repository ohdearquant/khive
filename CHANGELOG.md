# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.15.0] - 2026-10-25

### Added

- `embedding_truncation_report` replies may now include `discarded_tokens` and
  `token_reporting_unavailable` counters. Both new counters are omitted when zero.
  Document embeddings report provider token cuts alongside the existing byte limit.
- `khive_runtime::KhiveRuntime::prepare_link_metadata` applies the shared
  dependency-kind defaults to caller-resolved entity kinds without I/O.
- Writer contention diagnostics now report per-pool FIFO admission depth, its
  high-water mark, capacity rejections and bounded lifetime duration histograms.
  Snapshots identify the FIFO stage and report unavailable call/rotation metrics
  explicitly. Reading them never starts a writer or changes admission routing.
- `db_diagnostics` reports a process-wide `code_map_vfs` block: `quarantine_occupancy`,
  `quarantine_cap`, `registrations` and `registration_cap`, each read under its ledger's
  lock without retiring entries. The block is observe-only and has no stability contract.
- Add the CLI operator `kg.proposal_cleanup(older_than="30d")` to archive old
  applied and rejected proposals in the selected namespace. The duration uses
  positive whole seconds, minutes, hours or days, and retention follows the last
  projection activity. Ordinary lists hide archived proposals; proposal identity,
  lookups, parent references and lifecycle event history remain available.
  The operator follows normal write policy and is unavailable through MCP or
  multi-operation atomic requests. Migration V58 follows V57 from #5185.
- `khive_query::compile_with_mailbox` compiles a GQL or SPARQL query with a caller-derived
  `QueryMailboxScope`, filtering every note source, synthetic observation target and graph
  edge in the compiled SQL by the note store's mailbox rule. `khive_query::compile` keeps
  its unscoped behavior for callers that supply no scope.
- Add `stream.truncate` to remove a stream prefix atomically while preserving its sequence
  floor. `stream.read` and `stream.stat` report the floor, and empty streams retain their
  sequence position.
- KG archive imports now support explicit conflict handling, a dry-run preflight and forced
  skipping of invalid edges. Dry runs report the planned counts without writing; force only
  relaxes endpoint validation.
- Change sets add `RestoreOp` for active entities, notes and edges, and soft-delete inversion
  now produces restore operations. Restore cannot yet be applied, and schema-version 1
  change sets no longer decode.
- `attach(id, role, bytes, media_type?)` stores a role-keyed attachment on an existing entity or
  note on the main backend, replacing any earlier attachment in that role. Representations a pack
  protects stay with that pack, and inline input keeps the 64 MiB limit. `get` returns role-keyed
  attachment metadata and hydrates only the roles a caller requests, with verified base64 bytes;
  an unknown role is refused. Soft deletion keeps attachment rows and hard deletion removes them.
- `RuntimeCloseOwner` closes a quiescent single-backend runtime slot explicitly. A successful close
  waits for the audit tasks, drains the writer and joins the final store's timeout sink. Progress
  survives cancellation, and while other holders keep pool, backend or audit roots alive the close
  reports `Incomplete` with the observed strong counts so the host can release them and retry.
  Runtimes with embeddings, ANN, blob hydration, split events, host background loops or several
  backends are refused at preflight. Ordinary `Drop` remains best effort.
- Add `charter.*` verbs to compile and publish workflow definitions, start and advance
  runs from recorded or sampled observations, inspect status, and cancel open runs.
  Definitions use registered vocabularies; `request_id` makes writes safe to retry.
  Phase revisits include new evidence, and only declared triggers advance generation.
- New crate `khive-capability`: sealed process-local grants, authenticated durable grant
  envelopes and attenuating capability delegation. With its `authority-store` feature,
  authority records, typed change history and replay receipts are stored in `khive-db`
  transactions, and bootstrap state is written atomically.
- `khive-db` records edge-governance decisions and invalidations in a new dormant
  schema. The tables preserve decision history and native edge changes, without
  activating governance checks or changing ordinary edge writes.
- Failed dispatch audit events carry a typed `error_class`. `brain.event_counts`
  groups recognized classes and counts missing, malformed or unknown classes as
  unclassified, without changing caller errors or retry behavior.
- Audit success retention supports `reduced` and `all` through
  `KHIVE_AUDIT_SUCCESS_ROWS`. Reduced mode omits or samples successful rows for
  selected trusted handlers; `db_diagnostics` reports the decisions and
  `brain.event_counts` includes stored sampling metadata.

### Changed

- **Breaking:** `exec.events` returns every event `id` as a string: a UUID for new
  lifecycle events and a decimal string for legacy rows. New lifecycle events use
  the shared event store with the caller's canonical actor attribution. Legacy
  rows remain visible in the same oldest-first list, ordered by timestamp and
  string ID before applying the requested limit.
- The knowledge suggest verb now admits unspaced Chinese, Japanese and Korean queries by counting
  every two supported letters as one word, while retaining the existing whitespace-piece
  count for other scripts.
- Successful search audit records include the retrieval arms actually started, lexical match
  mode, effective limit and result count, including for empty results.
- `kkernel reindex` reads `[embed].batch_size` from the selected config file when the option
  is omitted; `--batch-size` takes precedence and the configured value is bounded to 1 to 500.
- Quarantined email records now retain the parsed To recipients in `quarantine_claimed_to`.
  Empty recipient lists and malformed-message envelopes omit the field.
- `comm.thread` renders only the selected visible message owners when building message JSON
  and previews, avoiding rendering discarded rows.
- Atom weights are applied to knowledge search results after embedding blending and status
  normalization. Missing weights use 1.0, and unavailable weight reads preserve results with
  a degradation reason.
- Event-count cursor reads now preserve typed `Timeout` and `AdmissionTimeout` refusals from
  storage, including failures while checking page-boundary completeness.
- When `KHIVE_SEARCH_PROFILE` is enabled, deadline phase records now carry the same search
  call and backend identifiers as their profile records, distinguishing timeout and late
  completion observations.
- Legacy quarantine cleanup now has a partial index for its typed quarantine and blank-slug
  filters, while keeping the existing expiry-or-creation ordering.
- MCP instructions now direct clients to the `request` tool's verb list for linked packs
  that are not loaded.
- MCP server instructions no longer repeat the verb catalog already present in the `request`
  tool description.
- Entity vector searches exclude knowledge atom vectors before candidate limits are applied,
  so nearby atoms cannot crowd entities out of entity search results. Other vector searches
  retain their existing populations.
- SQLite vector batch searches now use one admitted reader and one read transaction per batch,
  so every query sees the same snapshot. Per-query failures remain isolated.
- Writable cached SQL queries on standalone writers and pools with the write queue disabled
  now take write admission immediately before execution and retain the connection through
  settlement. Queued writer queries retain their existing read-only behavior.
- Standalone database writes now use consuming write owners that keep the connection and
  volume lease through settlement, including when an async observer is cancelled.
- Malformed active tool-grant rows now return an authorization error naming the row and
  invalid column, instead of falling through to policy or the default decision.
- Windows code-map pools can now obtain the native handle for an opened main database file,
  allowing the post-open identity check to succeed without reopening the path.
- Event-daemon supervision retains a child handle after transient exit-poll errors, allowing
  a later poll to reap it and preventing a second daemon from starting beside it.
- On Unix, dropping a volume lease explicitly unlocks its file before closing it, so a
  duplicated descriptor cannot keep the old lease locked.
- Refusal diagnostics now mask credential-shaped values before including them in error messages:
  brain profile lifecycle and feedback signals; comm inbox box and status values and thread order;
  scheduled-action `kind`, `entity_kind` and `note_kind`; session export formats, resume and export
  ids, list timestamps and search sources and timestamps; memory types, fusion strategies and
  feedback signals; knowledge feedback signals and section keys; knowledge import `chunk_strategy`
  and `format`; and tool argument values. Ordinary invalid values, accepted-value lists, validation
  order and error variants are unchanged.
- Invalid KG search `text_mode`, `source` and `order_by` values are now named in the diagnostic,
  masked when credential-shaped, and an invalid tool string-list argument identifies the refused
  element.
- `kkernel reindex` warns once when the selected configuration sets `embed.batch_size` above the
  500 cap, naming the file, the configured value and the effective value. An explicit
  `--batch-size`, an in-range value, a validation-only run and single-record repair do not warn.
- Bounded FTS ranking (`RankWithinCap`) materializes the bounded candidate rows before ranking,
  avoiding a second id lookup, and duplicate ids outside the exact filters no longer expand the
  gather cap. Default ranked search is unchanged.
- The default `kkernel` and `khive-mcp` builds no longer link the agent, telemetry and web
  packs; enable `pack-agent`, `pack-telemetry` and `pack-web` to include them.
- S3 blob storage is linked only when the `blob-s3` feature is enabled. A build
  without that feature refuses S3 configuration during load.
- `kkernel kg import` reads native khive archives without extra features. Enable
  `kg-adapters` for JSON, NDJSON, CSV, TSV or BibTeX imports; unsupported formats
  are refused before the source is read.
- Published `kkernel` binaries strip symbol tables; panic file and line information
  remains, while backtrace frame names are omitted.
- Refusal diagnostics mask credential-shaped caller values across `gtd`, `blob`,
  `memory`, `runtime`, `git`, `comm`, `schedule`, `brain`, `kg` and `knowledge`.
  Invalid values still receive the existing validation result without echoing a
  credential-shaped value.
- `knowledge.suggest` and `knowledge.compose` now share the same query admission
  counting rules.
- `khive-fs::read_link_at_bounded` returns complete symlink target bytes within a
  caller-supplied cap, or an explicit too-large result. `khive-pack-exec` uses this
  shared bounded read when capturing symlink targets.

### Performance

- `khive-fold`: the diversity selector keeps one indexed queue per category (#5856).
  `selector/diversity_scale` grows 7.1x from 1,000 to 5,000 candidates (scaling exponent
  1.22, previously 2.07), with output identical to the previous selection loop.
- `khive-db`: edge inserts and updates are faster (#5962). Against the same tree with the
  change reverted, `edge_mutation/bulk_insert_100` takes 0.20x the time, `bulk_insert_1000`
  0.28x, `update_weight_metadata` 0.43x and `update_endpoint` 0.44x, and the dormant
  edge-governance triggers no longer add a measurable cost to inserts and updates (#5922).
  Interleaved A/B, 8 rounds per arm, x86-64 Linux.

### Fixed

- `neighbors` now declares its `include_entity_type` parameter, so `verbs()` and `help` list
  it. The handler already accepted it; only the declaration was missing.
- `query` now applies the caller's mailbox view inside the compiled SQL, before matching,
  ordering and pagination, as `get`, `search`, `list`, `neighbors` and `context` already
  did, so another actor's message copies and quarantine records can no longer be read
  through a query. An edge whose endpoint is a hidden message is dropped.
- Idle ANN checkpoint tasks now release their runtime and storage handles when the final
  ANN owner is dropped, without waiting for the checkpoint interval.
- Updates rustls-webpki and webpki-roots to patched releases addressing RUSTSEC-2026-0381
  and RUSTSEC-2026-0382.
- Knowledge search recovers eligible rows from the namespace index before checking FTS
  membership when the bounded global FTS window contains foreign or ineligible rows.
- `brain.resolve` now returns a successful absence result when a loaded consumer has no
  eligible profile, without adding an error audit row.
- Ordinary runtime builds no longer retain per-generation audit history that is only read by
  instrumentation; cumulative counters and health metrics remain available.
- Non-Unix builds no longer compile Unix-only event forwarder and server exports.
- `RankWithinCap` text search keeps every exact filter when a candidate id occurs in more than one
  namespace.
- `kg.restore` refuses tool registry records, preserving the registry's retired
  entries from restoration through the KG update path.
- Coordinated `link` requests validate the ordinary link parameters after admission
  and before the coordinated write. Invalid weights, resurrection types and unknown
  arguments receive the same refusal as a direct link request.
- Vamana medoid selection now averages the sampled rows, so ordered inputs do not
  bias the graph's starting point toward early rows.
- Event lookup by identifier now searches across namespaces while preserving the
  caller's visibility rules.
- Entity `create` refuses the note-only `content` field and directs callers to use
  `description`; note content remains accepted.
- Cached single-statement writes reject transaction-control SQL before choosing a
  writer route. Use `atomic_unit` for statements that must run in one transaction.
- Updates `ctutils` to 0.4.4 to address RUSTSEC-2026-0385.
- Updates `tokio` to 1.53.3 to address RUSTSEC-2026-0386.

### Removed

- `khive_storage::graph::CommitAnnotationCursorValue`,
  `khive_storage::graph::CommitAnnotationGuard`, and
  `khive_storage::graph::CommitAnnotationInsertOutcome`.
- `khive_storage::GraphStore::insert_commit_annotation_if_absent` and
  `khive_runtime::KhiveRuntime::link_commit_annotation_if_absent`.
  Git reconciliation uses pack-owned predicates and the existing generic
  create-only graph insertion instead.
- `khive_db::stores::graph::GraphMutationRequest::CommitAnnotation` and
  `khive_db::stores::graph::GraphMutationOutcome::CommitAnnotation`.

### Breaking (Rust crates)

- `khive_runtime::DocumentEmbeddingOutcome` adds `provider_token_count`; direct
  struct literals must set it. `EmbeddingTruncationReport` adds `discarded_tokens`
  and `token_reporting_unavailable`; literals must set them or use `Default`.
- `khive_db::diagnostics::WriterContentionDiagnostics` adds `writer_admission` and
  `writer_admission_unavailable_reason`. Direct struct literals must set them.
- `khive_db::diagnostics::DbDiagnostics` adds `code_map_vfs`. Direct struct literals must set it.
- `khive_db::WriterGuard` no longer hands out its connection: `conn`, `conn_mut` and the
  `Deref`/`DerefMut` impls to `rusqlite::Connection` are removed, and `transaction` now
  takes the guard by value. Write through the consuming `WriterGuard::autocommit` or
  `WriterGuard::transaction`, which admit the write before it runs and settle the
  connection before the guard is released.
- The public standalone writer opener is removed. Use the consuming
  `standalone_autocommit` or `standalone_transaction` operations, or the restricted
  `open_standalone_checkpoint` handle.
- `khive_changeset::Op` adds `Restore`, and `khive_changeset::InvertError` adds
  `InverseRestore`, which also moves the implicit discriminant of `InverseUpdate`.
  Exhaustive matches on either enum must handle the new variant.
- `khive_runtime::KhiveConfig` adds `embed`, the `[embed]` table that carries the
  reindex batch size. Direct struct literals must set it or use `Default`.
- `khive_runtime::RuntimeError` adds `CanonicalUnsupportedOperation`. Exhaustive
  matches must handle it.
- `kkernel::kg::ImportArgs` adds `on_conflict`, `force`, `dry_run` and
  `continue_on_error`. Direct struct literals must set them.
- `khive_types::EventKind` adds `GrantChange`; downstream exhaustive matches must handle it.

### Upgrade note

- Change sets now use schema version 2. Change sets stored at schema version 1 no longer decode,
  so a canonical proposal stored before this release must be proposed again.
- `cargo install kkernel` now builds without the agent, telemetry and web packs, S3 blob
  storage and the JSON, CSV and BibTeX KG import formats. To keep them, install with
  `--features pack-agent,pack-telemetry,pack-web,blob-s3,kg-adapters`.
- This release adds database migrations V58 to V62: the proposal archive, the legacy
  quarantine index, the stream truncation floor, the edge-governance tables and the
  authority store.

### Known issues

- Edge deletes and product merges still carry the dormant edge-governance trigger cost,
  about 13 to 15 percent on `edge_mutation/delete_hard` and `delete_soft` and 11 to 12
  percent on the 8-edge product merges (#6118).

## [0.14.0] - 2026-10-18

### Added

- `NoteStore::try_patch_note_properties` applies guarded partial property writes,
  forward-only expiry and a monotonic update timestamp in one storage operation.
  Its provided default returns `Unsupported`; existing implementations remain
  source-compatible. Comm duplicate-quarantine repair now uses this typed operation
  through a channel-ingest capability, preserving its property guards and writer
  admission while removing the pack's repair SQL.
- `KHIVE_EMAIL_EXTERNAL_INBOUND=enabled` receives authenticated mail from outside senders
  (delivered as untrusted external messages and routed as with external senders configured)
  without setting `KHIVE_EMAIL_EXTERNAL_SENDERS`, so outbound external sending stays disabled.
  Start-up fails while it is enabled and `KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR` is unset or
  empty, and on any other non-empty value.
- With `KHIVE_EMAIL_SUPPRESSION_MIRROR_URL`, `KHIVE_EMAIL_SUPPRESSION_MIRROR_TOKEN` and
  `KHIVE_EMAIL_SUPPRESSION_MIRROR_MAX_AGE_SECS` set, the email outbox reads the complete set of
  remote one-click suppressions and records every missing key in the local suppression list
  before an external message is sent (ADR-200 section 6, Amendment 4). A failed read keeps
  external messages pending, and external admission refuses with `suppression mirror stale`
  once the last successful read is older than the maximum age. Each outbox cycle also reads
  the mirror when its stamp is half the maximum age old, so an idle deployment stays fresh.
- Configured embedding engines may name custom pack providers. MCP, ingest and downstream
  host composition register providers before binding ordered peers and their declared
  dimensions, without loading models. Missing providers fail startup; explicit no-embedding
  runtimes keep no configured default while explicit manual named access remains available.
- `khive-lion-core` copies Apache-2.0 `lion-core` 0.4.0 and adds `Kernel::with_key`
  for caller-supplied seal keys. All-zero keys are refused; initialization starts
  at epoch zero with no previous key and the same empty, deny-all state.
- Add `schema()` for loaded vocabulary and attributed endpoint declarations, including
  entity subtypes, counts and a stable content hash. The proposed ADR-084 contract
  remains pending acceptance and conformance validation.
- `apply_changeset` applies a complete canonical NDJSON change-set as one bounded, ordered
  writer transaction. Staged create and link identifiers survive application, updates
  compare the captured values of the fields they touch, and deletes compare the whole
  captured record except `updated_at`. `kkernel kg apply` submits the delta through the
  daemon, and reviewed canonical proposals use the same path. Merge is refused.
- When a coordinated kg search hits its deadline, the `backend search task timed out` and
  `backend search task completed after the shared request deadline` warnings add
  `in_flight_phase`, `phase_elapsed_ms`, `active_phases` and `completed_phases`, naming
  what the backend search was doing. `khive-storage` gains
  `request_context::SearchPhaseTrace`. Deadlines, budgets, retries, ordering and results
  are unchanged.
- `comm.health` reports an outbox scan health snapshot for the email and Telegram
  transports: completion and success times of the pending-row query and observations of
  the bounded eligible page. A failed scan keeps the previous success time.
- Explicit KG imports honor `[schema].strict`, which defaults to true. Strict mode refuses
  an unknown entity kind before the destination is set up; relaxed mode keeps its spelling
  and warns once per distinct kind. Local VCS sync degrades unknown kinds to `concept` and
  records `khive:degraded_kind`.
- `code_map_root` and `[[code_maps]]` entries (`name`, `path`, `source_root`) in
  `config.toml` are validated entry by entry and loaded once into a runtime registry of
  code maps (ADR-139). A malformed entry is rejected by name without stopping the
  configuration from loading.
- `code.list`, `code.search` and `code.deps` read operator-registered code maps: list and
  search paginate deterministically with a cursor bound to the map registration and
  filters, search ranks exact, then prefix, then interior name matches, and dependency
  traversal returns nodes breadth-first with incoming normal, dev and build dependencies,
  accurate node-limit truncation and every dependency edge among the returned nodes. The
  three verbs are classified as reads.
- `link` responses disclose a `dependency_kind` the server inferred, with a hint for each
  inferred value; a caller-supplied `dependency_kind` suppresses it. Bulk hints keep the
  caller's input indices across skips and failures, and an atomic link response carries
  the hint captured while the plan was prepared.
- The code pack's source ingest records the literal source-file inputs a module names:
  `include!`, `include_str!` and `include_bytes!` arguments, `#[path]` attributes,
  `#[doc = include_str!(...)]`, and filesystem read calls whose argument is a string
  literal.
- Guarded create-only edge insertion: `khive-storage` adds `GraphStatementGuard`,
  `GraphStatementGuardFailure` and `EdgeInsertIfAbsentOutcome`; `khive-db` evaluates each
  read-only guard statement inside the edge's writer transaction; `khive-runtime` adds
  `link_if_absent_with_guards`.
- `khive_types::app` declares a khive app as plain data (ADR-203): name, version, signer,
  the verb inventory with a rights class per handler, and the kinds it contributes.
  `AppManifest::validate` and `AppManifest::admit` refuse incomplete or conflicting
  manifests, naming the conflicting owner.

### Changed

- `RetrievalPersistence::new` now accepts `Arc<dyn SqlAccess>`. Snapshot writes and
  namespace clears use the backend writer route; loads and statistics use its
  reader. Core migrations install the snapshot table and namespace index. The
  existing `init_schema` compatibility bootstrap now uses the admitted writer.
- `khive-retrieval` weight updates and batch reads now accept `&dyn SqlAccess`
  instead of a raw SQLite connection. Pass the migrated backend's SQL access handle;
  updates and audit rows commit together through its writer route, and read-only
  handles refuse updates. Storage failures use `PersistError::Storage`.
  Core migrations now install `atom_weights` and `weight_events`; the standalone
  `init_weight_schema` bootstrap remains available for compatibility.
- Edge-weight validators use the shared `khive-types` predicate,
  preserving numeric boundaries and validation diagnostics.
- With a configured runtime actor, a message the email channel quarantines, whatever its
  reason, is addressed to that actor (the maintainer) instead of the routed landing actor, so
  the mailbox recipient check keeps the quarantine record and its content reference out of
  every other actor's inbox, thread, read, get and search results. This applies to records
  ingested after the upgrade: a quarantine record already stored keeps the landing actor it
  was written with, and nothing re-addresses it. A runtime with no configured actor keeps the
  landing actor.
- The email opt-out label (ADR-200 Amendment 2) now reads the first non-quoted paragraph of a
  reply rather than its first line, so a phrase a mail client wrapped onto the second line still
  counts, and the phrase list gains the plain asks for no more email ("don't email me",
  "do not contact me", "no more emails" and their spellings). The bare words "email me" are not a
  phrase: a reply asking us to email the docs is delivered without the label.
- Email quarantine distinguishes `auth-untrusted`, a message whose authentication results
  headers were present but not trusted, from `auth-absent`. The record carries the anchor
  mode, the header family and the identifier as an escaped string of at most 128 bytes.
- Bulk `link` responses report `inferred_count` instead of repeating an inference
  explanation for every edge; `verbose=true` keeps the indexed hints and per-edge
  explanations. Singleton `create` edge results disclose inferred dependency kinds.
- `context` charges anchor-to-anchor edges against the response budget and returns an
  anchor edge only when both of its anchors were admitted (ADR-140 Amendment 1).
- A best-effort mark-read failure carries `mark_error_details` (typed code and stage, and
  the native SQLite write stage and result codes where present) beside `mark_error`.
- On Linux with glibc, the runtime returns the transient heap of a native embedding
  model load to the operating system once the built-in embedder has loaded.

### Fixed

- SQLite volume-lock refusals name the directory or lock file that could not be used and the
  underlying I/O error, and name both ways to choose another directory: `KHIVE_VOLUME_LOCK_DIR`
  for the default directory resolver, or the caller-supplied lock directory setting. Headless and containerized deployments require a writable home
  directory or an absolute, writable lock directory shared by all cooperating khive processes
  of the deployment; an unusable directory still refuses the open rather than falling back to
  a database-specific directory.
- An authenticated opt-out in a message that screening quarantines as `screened` now records
  the reply-stop suppression for its sender before the quarantine is acknowledged, as a
  delivered opt-out already did. No other quarantine reason records an opt-out, and ingest
  refuses one claimed on them.
- A registered code map whose main database file has more than one link is refused at
  registration and on every read, and the guarded read-only file system checks the link
  count on the file SQLite reads before any row is read (ADR-139 Amendment 2). A
  wrong-typed `code_maps` or `code_map_root` value fails the configuration load with the
  key named. `khive_db::file_identity::file_link_count` reads the link count from an open
  file.
- `db_diagnostics` collects `size_composition` only when the read snapshot holds at most
  8,192 pages and 32 MiB, checking both in the same snapshot; a larger store gets no
  composition and `size_composition_error` names the observed footprint and the limits.
  The limits bound the work admitted, not elapsed time. `size_composition=false` still
  skips the walk.
- The write-path secret gate treats `head=<40-hex>`, `head <40-hex>` and the equals form
  of the other revision markers (`sha=`, `commit=`, `revision=`, `rev=`) as revision
  labels instead of refusing them as credential tokens.
- The web pack's disk ingestion encodes each file name as a URL path segment, so `#`, `?`,
  `%` and `\` in a name no longer turn into a fragment or query and collapse two files onto
  one document.
- `khive-pack-brain` builds without the `lattice-router` feature no longer fail clippy
  with unused imports.

### Breaking (Rust crates)

- `khive_runtime::KhiveConfig` adds `schema`, `code_map_root` and `code_maps`, and
  `khive_runtime::RuntimeConfig` adds `code_map_root`, `code_maps` and
  `code_map_protected_database_paths`. Direct struct literals must set them.
- `khive_pack_code::CodeSourceIngestReport` adds `source_reads`,
  `khive_types::ProposalAppliedPayload` adds `preimage`, and `kkernel::kg::ImportArgs` adds
  `config`. Direct struct literals must set them.
- New enum variants: `khive_runtime::AtomicOpFailure::{OwnerSnapshotConflict,
  PreimageMismatch}`, `khive_runtime::AtomicOpPlan::Canonical`,
  `khive_runtime::RuntimeError::CanonicalAdmission`, `khive_runtime::ConfigError::SchemaParse`,
  `khive_types::ProposalChangeset::Canonical` and `kkernel::kg::KgCommand::Apply`. None of
  these enums is `#[non_exhaustive]`, so downstream exhaustive matches must handle them.
  `ProposalChangeset::Canonical` precedes `Compound`, whose implicit discriminant moves from
  6 to 7.
- `khive_runtime::AtomicOpPlan` is no longer `UnwindSafe` or `RefUnwindSafe`.
- `khive_pack_brain::persist::PersistenceTracker` no longer has the public
  `increment_dirty` and `reset_dirty` methods.

## [0.13.0] - 2026-10-15

### Documentation

- Accept ADR-200, the external email policy: sender scope, suppression, unsubscribe and
  screened inbound delivery.
- Accept ADR-106 Amendment H, a per-event missed-schedule policy in which `catch_up_once`
  requires an explicit `max_lateness`.
- Accept ADR-140, the context graph payload budget rule; ADR-089 now points to it.
- Accept the ADR-102 and ADR-046 2026-10-14 amendments: a staged change-set lands through one
  atomic `apply_changeset` verb, and canonical change-set proposals land through the same path.
  Neither is implemented yet.

### Added

- `knowledge.lint` inspects the caller-visible graph against the thirteen built-in hygiene
  rules and returns typed violations, with statistics taken before any fix. Requested fixes
  soft-delete unchanged dangling edges, propose questions about duplicate groups and mark
  unchanged superseded entities deprecated, each checked inside its own transaction; the
  receipt keeps earlier committed fixes when a later one conflicts or fails. The gate
  classifies the verb as Write. Custom rules (`knowledge.lint_config`) remain deferred.

- With `KHIVE_EMAIL_EXTERNAL_SENDERS` set, authenticated mail from outside senders passes
  deterministic screening and routes to a verified thread owner, a configured recipient alias
  or a fallback actor. Delivered messages read back marked `origin: external` and
  `trust: untrusted` with the rule IDs that matched. A screening rule can withhold the body:
  the message is delivered with a rule-ID placeholder, and its `quarantine-original` attachment
  owns the original MIME bytes, which the recipient actor or the configured maintainer reads
  through `comm.withheld_body`. A reply asking to stop records an address suppression
  before the message is acknowledged. With the variable unset, startup and mail handling are
  unchanged.

- `schedule.remind` and `schedule.schedule` accept `misfire_policy` (`skip` or
  `catch_up_once`) and `max_lateness` (positive seconds, required with `catch_up_once`). An
  overdue `catch_up_once` event dispatches its latest due occurrence once when that occurrence
  is no older than `max_lateness`; older occurrences are recorded as missed and never
  dispatched. Events without a policy keep the existing skip behaviour and the
  `KHIVE_FIRE_GRACE_SECS` window.

- `kkernel kg review` reads Tier 1 eligibility from the same rules snapshot it validates
  with. An optional `[tier]` section configures the operation kinds, link relations, mutable
  entity fields and minimum link weight eligible for Tier 1. Empty lists narrow eligibility,
  and judgment relations, destructive operations, existing-edge updates, low weights and
  error findings still require Tier 2. `khive-rule-evaluator` now depends on
  `khive-changeset`.

- `comm.withheld_body` reads the original RFC 5322 bytes of a delivered withheld external
  message, base64-encoded with their size. Only the message's recipient actor and the
  configured maintainer may read it; any other caller is refused before the original is read,
  and an id that is not a delivered withheld message is refused. Every call writes an audit
  event naming the caller, the message id and the outcome. The response never carries a
  content reference.

- `EntityPosteriors::fold_evidence` applies weighted `supports` and `refutes` evidence
  to a claim's posterior, rejecting invalid relations and weights before changing values
  or eviction order. Either signed zero is a mutation-free no-op.

- Inbound email follows one disposition order. A malformed or oversized message is quarantined
  first. A subject of the form `unsubscribe <token>` whose token verifies records an outbound
  `recipient_key` suppression entry (source `mailto_token`) and the message is neither delivered
  nor quarantined. With external senders configured, an inbound suppression entry matching the
  authenticated sender address or its exact domain quarantines the message with reason `blocked`.

- `KHIVE_EMAIL_AUTHSERV_ID` accepts `!topmost:<authserv-id>` (`TrustAnchor::TopmostWithAuthservId`).
  Only the topmost `Authentication-Results` header is considered, as with `!topmost-no-authserv-id`,
  and it is trusted when it carries the configured `authserv-id` or none. This covers Exchange
  Online, which stamps `mx.microsoft.com 1` on its own header for some senders and no `authserv-id`
  for others. An empty or malformed id after the prefix fails configuration. Adding the variant to
  the public `TrustAnchor` enum is a breaking change for exhaustive matches.

- With `KHIVE_SEARCH_PROFILE` set, each search the coordinator runs across configured backends
  writes one JSON line per stage to stderr (`backend`, `merge`, `fanout`, `finalize`, `hydrate`
  and `search`), grouped by a call id that a nested search shares with its outer call. A
  `backend` line names the backend, its hit count and its outcome (`ok`, `error` or `timeout`).
  With the variable unset nothing is written and no stage timer reads the clock.

### Changed

- With no configured floor, the filesystem blob store's free-space floor is 2% of the volume
  holding the blob root, never less than 5 GiB, instead of a fixed 100 GB. A configured
  `floor_bytes` is used as given, including `0`. `FsBlobStore::DEFAULT_FLOOR_BYTES` remains,
  deprecated.
- Event counts keep the caller's request-read budget (`KHIVE_REQUEST_READ_TIMEOUT_SECS`)
  instead of the fixed 10-second RPC timeout, and typed read and admission timeouts cross the
  events wire intact. The events protocol version is now 6, so clients and daemons must be
  upgraded together.
- Configurable graph rules share the pure `khive-rule-evaluator` crate between
  `kg validate` and `kg commit`, using captured NDJSON inputs and explicit citation
  time. Runtime and offline checks share the canonical endpoint table and matcher.
- Name the distinct query admission policies `khive_text::is_meaningful_text_query`
  and `khive_pack_memory::scoring::is_meaningful_recall_query`, preserving their
  existing behavior. Both `is_meaningful_query` functions remain as deprecated
  compatibility wrappers for one release; callers should use the replacement names.

### Fixed

- A daemon that supervises an events daemon now holds each request until that child's socket
  answers for the first time, instead of dispatching while the child is still starting. A write
  sent in that window could commit its domain change and then report a failed audit commit. If the
  socket has not answered within 10 seconds of startup, requests are refused before dispatch with
  `events_daemon_unavailable`, `retryable: true` and `domain_disposition: not_committed`, naming
  the socket, until it answers. Probe frames are answered without waiting.

- The email poll loop no longer holds its IMAP page without bound. Every cause that holds a page,
  a `comm.ingest` refusal or a quarantined message's original that cannot be stored, now counts
  toward one bound per message: the message is recorded as quarantined on its 60th failed poll
  while the store refuses it as retryable, and on its 5th for every other cause. If the original
  cannot be stored, or the record that references it is refused, the record is written without it
  and names the reason. `comm.health` reports `consecutive_held_polls`, `held_messages`,
  `last_held_at` and `last_quarantined_after_hold_at` per channel.

- External inbound email no longer routes on an unproven thread UUID. A sender-asserted
  `X-Khive-Thread-ID` that names a thread routes only when that thread holds an outbound email
  whose Message-ID verifies under the configured mailbox domains, the same proof the
  `In-Reply-To`/`References` Message-ID path already requires. A UUID naming any other thread
  falls through to the recipient aliases and then the external inbound actor. Maintainer mail
  keeps its existing resolution.

- A new quarantine record's `comm.ingest` receipt now reports whether the record kept its original:
  `quarantine_original_retained` (a boolean) and, when it is false,
  `quarantine_original_not_retained_reason` naming the stored reason. A non-quarantine message and a
  deduplicated acknowledgement carry neither. The success heartbeat gains `originals_not_retained`
  (a running total of records written without their original, with `original_not_retained_reason`),
  `original_retention_state` (`retained` or `failing`), `notices_failed` (a running total) and
  `notice_config_error` (cleared by a later success that omits it); a failure heartbeat refuses all
  of them. `comm.health` reports each per channel, `null` for a writer that does not report them and
  for quarantine-only entries.

- Brain verbs report storage timeouts and admission refusals as typed storage errors instead
  of internal errors, so callers keep the retryable classification.

- A note update whose model reindex fails after the commit returns the typed committed
  degradation error, naming each failed model and stage, instead of reporting success.

- A verified mailto opt-out received while the email host has no comm store is kept for retry
  instead of being consumed as already recorded.

- With `KHIVE_RECALL_PROFILE=1`, each memory recall labels its stage records with its own call
  ID and writes exactly one `total` record on every exit, including empty results, errors and
  cancellation.

- Events store directory refusals name the database path and the ownership, permission or
  location remedy.

### Performance

- Reindex writes each page of notes or entities to the full-text index as one batch, keeping
  successful documents when one in the page is refused.
- Vamana insertion reuses pooled visited-node scratch instead of allocating it per insert.

### Breaking (Rust crates)

- `khive_channel::ChannelEnvelope` adds `inbound_delivery`, the host-verified routing for an
  inbound message (never serialized). Direct struct literals must set it, usually to `None`.
- `khive_runtime::events_split::EventsRequest::CountEvents` and `CountEventsGrouped` add
  `operator_read_budget_secs`, and `EventsResponse::Error` adds `read_failure`. Direct
  constructors and exhaustive destructurings of these variants must account for them.
- `khive_runtime::RuntimeError` adds the `LintFix` variant, which keeps a lint fix's typed
  cause and its confirmed receipts. The enum is not `#[non_exhaustive]`, so downstream
  exhaustive matches must handle it.
- `khive_channel_email::config::TrustAnchor` adds the `TopmostWithAuthservId` variant for
  `!topmost:<authserv-id>`. The enum is not `#[non_exhaustive]`, so downstream exhaustive
  matches must handle it.

## [0.12.0] - 2026-10-13

### Documentation

- Accept the ADR-048 graph lint contract for #4813: thirteen built-in rules, typed
  reports and statistics, and guarded fix receipts. The verb remains unimplemented.
- Accept the `kg.proposal_cleanup` operator contract: hide old applied or rejected
  proposals using an archive marker while retaining their identity and event history.
  Retention follows last projection activity. The handler remains unimplemented.
- Accept ADR-037 configuration-bound namespace and project-root rules for remote
  cached-entity reads, preserving origin and refusing remote mutations. The read
  remains unimplemented.
- Accept ADR-035 embedding pipeline configuration, legacy-key compatibility and
  import strictness rules, with an ADR-001 companion distinguishing explicit import
  from VCS snapshot degradation; edge relations remain closed. The configuration
  remains unimplemented.

### Added

- Weighted memory reranking recognizes a precomputed `graph_proximity` feature.
  `RerankFeatures` gains the public field and a zero-valued `Default`; direct struct
  constructors must supply it. Recall handlers supply zero pending graph gathering,
  default weights remain empty, and rerank provenance records the recognized feature.
- `khive-brain-core` exposes evidence-only posteriors with explicit event/read clocks,
  checked half-lives, decay-before-add and a fixed read-time prior. Regressed writes
  preserve the entry; invalid or overflowing updates refuse atomically. Existing
  posterior storage, folds and serving paths remain unchanged.
- Packs can declare note property policies and discover the validated registry boot snapshot.
  Comm declares its nine existing kind-specific protected properties; writer enforcement is unchanged.
- `db_diagnostics` accepts `backend=<name>` to inspect only the opened database serving that
  backend, and `size_composition=false` to skip the `dbstat` walk, which reads every page of
  each file. Every file-backed report now carries `page_accounting` (`page_size_bytes`,
  `page_count`, `freelist_pages`), three pragma reads that are reported either way.
- `memory.recall` accepts `include_timings=true`, which returns this call's retrieval stage
  wall times (`embed_us`, `fts_us`, `ann_us`, `fresh_tail_us`, `hydrate_us`, `null` for a
  stage that did not run), `total_us`, `retrieval_rounds`, and per-round `candidate_limit`,
  `text_hits`, `vector_hits`, `hydrated` and `eligible` counts. An array response becomes
  `{results, timings}`; an object response gains `timings`. No environment variable or
  restart is needed; `KHIVE_RECALL_PROFILE` keeps working as before.

- `khive-retrieval` adds pure query-variant normalization and bounded RRF-60 admission
  merging. Original-query fallback preserves its bytes, generated variants are validated
  as a whole, and admitted IDs retain variant and retrieval-source attribution.
- `kkernel kg update <remote> [--ref <ref>]` resolves a Git ref to a full commit SHA
  and atomically updates that remote in an existing schema.yaml, reporting both old
  and new commits while preserving archive-content pins and other YAML values.
  Rust clients with exhaustive `KgCommand` matches must handle the new `Update` variant.
- Runtime packs may expose an optional `ContextContributor` capability with owned request
  and slice types and explicit score semantics. Registry discovery preserves pack order;
  context composition and execution remain separate.
- `EmbeddingCosineReranker` scores every supplied entity candidate in a named engine,
  preserves missing vectors with their incoming scores and an explicit flag, and implements
  the retrieval `Reranker` trait without a namespace-wide candidate cut.
- `khive-graph-diff` compares in-memory entity, edge and note NDJSON states with
  deterministic additions, removals and field changes, preserving missing versus null.
- Add explicit event sink declarations in `khive-types`. All existing event kinds
  retain `caller_event_store`; the closed metadata API requires each new kind to
  declare its sink. Event routing and storage behavior are unchanged.
- A standalone session raw-value codec writes level-3 zstd frames with BLOB
  prefix `0x01`, borrows legacy TEXT unchanged, and refuses unknown formats or
  malformed data. Callers select the decoded BLOB size bound; mirror storage
  paths and dictionaries remain unchanged.
- `VectorStore::scan_vectors` enumerates persisted vectors in UUID-ordered pages
  with exact metadata filters and an explicit row limit. SQLite reads stored bytes
  through reader admission; pages do not form a cross-request snapshot.
- The opt-in `charter` pack installs the seven recording tables from ADR-193 and
  provides revision-and-state checked transaction helpers. This schema foundation
  exposes no verbs, grants no action admission, and does not enforce merges.
- `khive_runtime::hooks::refuse_shared_create` provides a kind-owned creation refusal hook.
  Memory and channel-health hooks share its `InvalidInput` diagnostic, naming the specialized
  writer while retaining their update and proposal guards.
- `KhiveRuntime::resolve_prefix_in` resolves UUID prefixes in a named pack-owned table,
  reporting distinct-ID ambiguity and storage failures. A `PrefixScope` argument states which
  rows count: one namespace or all of them, and live rows only or soft-deleted ones too. Rows
  outside the scope neither match nor make a prefix ambiguous; any other selection policy stays
  with the caller.
- `khive-retrieval::fuse_two_stage` combines ordered engine arms before vector/text fusion,
  checks both strategies even on empty input, preserves the candidate union without a final
  limit, and accepts an async custom-executor adapter without a runtime dependency.
- Runtime `vector_search_in`, `knn_in` and `rerank_in` bind queries to an explicit engine,
  checking its dimensions and finite coordinates even for empty requests. Existing methods
  remain single-engine compatibility delegates to the configured first/default engine.
- `khive-fs` adds bounded directory-name listing and descriptor-relative tree enumeration with
  caller filters, explicit depth and entry caps, and optional within-root symlink following.
  Caps count hidden and non-UTF-8 entries and refuse overflow instead of truncating results.
- `kkernel kg import` accepts `--format bibtex` and `.bib` files, streaming bounded
  entries into document/paper records with authors properties, `@string` expansion
  and crossref edges. Malformed entries report skip counts and optional warnings;
  fatal source errors refuse the import before the target database opens.
- `khive-runtime::BackendHandle` provides five required core capabilities and optional per-binding
  vector, sparse, and text factories. SQLite construction performs no storage operation; first use
  retains existing readiness and repair, and unavailable retrieval tiers return named errors (#4706).
- `brain.auto_feedback(signal="unjudged")` records attributed serve telemetry
  without training or consuming a serve-ledger grade. Later caller-judged
  auto-feedback with the same scorer/serve pair commits its grade and posterior
  update exactly once; manual feedback behavior is unchanged.

### Changed

- `memory.recall` with `fusion_strategy="keyword_only"` no longer embeds the query or searches
  the vector index, and `fusion_strategy="vector_only"` no longer runs the full-text search.
  The saved work was most of the cost of a `keyword_only` recall that finds no text match.
  `keyword_only` no longer applies the vector `min_raw_relevance` floor; its hits carry
  `raw_score: null` and `score` is the composite; rank order is unchanged. Widening re-runs
  only the arm the strategy keeps and stops when that arm is exhausted. A `keyword_only`
  recall therefore never reports `degraded`, and a verbose `keyword_only` request on a
  multi-model store returns the ordinary shapes instead of the `vector_candidates_per_model`
  envelope, since no vector recall ran.

- A `keyword_only` recall whose query the embedder cannot embed now returns its text hits
  instead of failing with `recall: all embedding engines failed`, and a `vector_only` recall
  whose full-text table is unreadable now returns its vector hits instead of failing.

- `memory.recall` leaves an English-only embedding model's vector list out of fusion when the
  query is CJK-dominant (its CJK characters outnumber its other letters). Today that model is
  `all-minilm-l6-v2`, which maps most CJK characters to `[UNK]`, so its candidates for such
  queries carry no signal. The list gets weight zero under every strategy that fuses
  per-model vector lists, but only when another model contributed a candidate and the request
  does not name `embedding_model`. English and mixed queries fuse as before. The
  `enable_cjk_fts_bypass` documentation now states what the flag does: it skips term selection
  in the FTS gather path and is read only when FTS gather is enabled.
- `db_diagnostics` no longer fails as a whole when the request's read deadline arrives. The
  backends already inspected are returned, every remaining backend's `error` starts with
  `not inspected: request deadline`, and the new root field `complete` is `false`. Backend
  inspections stop slightly before the deadline (a tenth of the remaining time, at most two
  seconds) so the partial report reaches a client whose own deadline started first. A
  cancelled request still fails. When `backend` selects a database, the root fields describe
  that database rather than main.
- `khive_db::diagnostics::collect_with_runtime_audit_metrics_for_process_interruptibly` and
  `KhiveRuntime::db_diagnostics_for_opened_backend_with_audit_metrics` take a trailing
  `collect_size_composition: bool`; pass `true` for the previous behavior.
  `khive_db::diagnostics::DbDiagnostics` gains the public fields `page_accounting` and
  `page_accounting_error`; direct struct constructors must supply them.
- A stdio MCP host that forwards to the daemon is now strict by default whenever a daemon
  socket or PID file exists, or existed when the request arrived: a request that would
  have fallen back to local dispatch is refused with its reason code (`no_socket`,
  `config_mismatch` and the rest), as under `KHIVE_DAEMON_STRICT=1`. `KHIVE_DAEMON_STRICT=0`
  (or `false`, `no`, `off`) restores the fallback; with neither file on disk the quiet
  fallback is unchanged; `KHIVE_NO_DAEMON=1` remains the way to run without a daemon.
- A daemon that is alive but does not answer in time now returns `reason: "daemon_slow"`
  with `phase: "before_dispatch"` (nothing sent, `not_committed`, retry is safe) or
  `phase: "after_write"` (`unknown`; read state before any re-send) and the elapsed wait.
  A connection lost after the request was sent returns `daemon_connection_lost`, and an
  unreadable response `parse_failure`; all three post-write outcomes stay terminal.
  `bridge.diagnostics` reports `daemon_slow` counts per phase, outside `fallback_total`.
- Client daemon recovery acts on one rendezvous. With exactly one of `KHIVE_SOCKET` /
  `KHIVE_PID` set and a socket that does not answer, a request is refused with
  `reason: "rendezvous_split"` (`not_committed`) instead of reading the other half's PID
  file and signalling the daemon it names. A client also never signals a live daemon on a
  rendezvous a supervisor marker claims: it returns `supervised_daemon_unresponsive`
  (`not_committed`) naming the pid, the marker's job and the marker path.
- The daemon writes `<pid file>.exit` (`running` after it claims the PID file, `exited`
  with the reason and the pid that sent the shutdown signal), logs the previous record and
  the effective `RLIMIT_NOFILE` at WARN on every boot, and logs its shutdown at WARN.
- Each pooled SQLite connection now keeps an 8 MiB page cache instead of 64 MiB. The
  cache is per connection and on the heap, so a process with many connections paid the
  old size many times over. `KHIVE_SQLITE_CACHE_KIB` (512 to 1048576) and
  `KHIVE_SQLITE_MMAP_BYTES` (0 to 2^40, `0` disables memory-mapped reads; default 1 GiB)
  override the two settings; a value outside its range is logged and ignored.

- A stdio MCP host that forwards to the daemon no longer runs the legacy quarantine
  original repair at boot, whether it serves one store or several. The daemon runs
  it before it serves; the census read every message row on every backend, so each
  client start spent seconds pulling the stores through the page cache. The repair
  itself now stops after its first census when nothing is unowned, instead of
  reading the roster twice more to change nothing, and reports how many passes it
  made.
- Upgrade the shared `rmcp` dependency to 2.2, including its security and transport
  cancellation fixes. MCP, mounted tools and CLI consumers keep their existing
  feature configuration; MCP result tests use the renamed `ContentBlock` API.
- An oversized stdio line now receives Invalid Request (-32600, no id) instead of
  Parse Error (-32700): rmcp 2.2 ignores syntactically invalid JSON without
  replying. The line cap, drain and next-request recovery are unchanged.
- `khive-retrieval` weight updates and batch reads now accept `&dyn SqlAccess`
  instead of a raw SQLite connection. Pass the migrated backend's SQL access handle;
  updates and audit rows commit together through its writer route, and read-only
  handles refuse updates. Storage failures use `PersistError::Storage`.
  Core migrations now install `atom_weights` and `weight_events`; the standalone
  `init_weight_schema` bootstrap remains available for compatibility.
- Lattice dependencies move from 0.9.0 to 0.12.0 (`lattice-embed`, `lattice-inference`,
  `lattice-fann`). Moodboard descriptors now report inference version 0.12.0, so their
  fingerprints and model keys change and existing moodboard vectors need reindexing;
  preference bundles trained under 0.9.0 fail closed on load and need retraining. BLAKE3
  no longer runs with the `pure` backend: std members enable `blake3/std` directly.
- `khive-vamana` greedy search keeps its candidate list sorted and bounded: each neighbor is
  placed by binary search and the next candidate to expand is read from a cursor, instead of
  re-sorting the whole list and scanning it for the nearest unexpanded entry after every
  expansion. Candidates with equal SQ8 distance now order by node id rather than by exact f32
  distance; the exact f32 re-score of the final list still decides the returned top-k. An
  index built after this change can differ from one built before it where SQ8 distances tie.

- Public brain posterior folds use the shared filter and map combinators, preserving
  reducer state, standalone section-feedback handling and offered-event counts.
- Pool configuration, WAL ceiling policy and test-store guards move into an internal
  configuration module while preserving public paths, defaults and refusal behavior.
- Vamana v2 saves use one checkpoint-owned publication boundary: metadata is
  published and synced through the shared Unix helper before the four staged
  segments are promoted. Sidecar reads and cleanup share component-name conversion,
  preserving their refusal policy and native I/O errors (Refs #4498).
- Blob and WAL-pin publication use the shared descriptor-relative rename helper; blob-root
  verification uses the shared file identity type, preserving held handles and durability barriers.
- Vamana checkpoint staging, auxiliary sidecars and external-ID sidecars share Unix
  descriptor-relative atomic publication helpers. Checkpoints retain their separate metadata
  commit and segment promotion boundaries, stale-entry policies and I/O diagnostics.
- Git cursor inspection and annotation repair now share the bounded snapshot query, preserving
  their separate size limits, refusal messages and exact stored cursor bytes.
- Session mirror enable flags now use the shared boolean parser: `on` and surrounding
  whitespace on recognized true values enable the configured source. All four flags still
  default to disabled; the separately default-enabled backfill flag keeps its existing parser.
  Session maintenance uses shared typed SQL row accessors while preserving its error messages
  and rejection of negative counts.
- MCP configuration fingerprinting and verb catalog helpers now live together in the server
  config module, preserving public entry points, daemon identity values, and catalog output.
- Extract blob garbage-collection ownership and database lock handling into a
  private module while retaining the existing public API and lock behavior.
- `khive-db` moves checkpoint escalation and transaction-age state into a dedicated
  module, preserving the public `khive_db::checkpoint` paths and state-machine behavior.
- Extract runtime daemon wire frames and framing into a private module while preserving public paths,
  protocol constants, error normalization, and source-reader coverage.
- Move MCP server state, registration errors, and stdio settings into a dedicated
  core module, preserving public server entry points and existing function bodies.
- Split the `kkernel exec` run paths into a private module while preserving the public entry point,
  test module paths, and tracing targets.
- Entity create, claim, ordinary update, bulk, and direct code ingest now share manifest-aware
  candidate admission. For an admitted candidate, the entity, runtime stamp, required indexes,
  attachments and exemption audit commit together. Production manifests remain empty; non-empty
  fixtures are test-only. Notes and administrative/atomic updates retain their existing gates.

### Fixed

- The secret gate treats a label ending in `_token` as a credential label, so a UUID or opaque
  value assigned to `access_token`, `refresh_token`, `session_token` or any other `*_token` label
  is refused the way one assigned to `token` is. Pagination and sync cursors (`next_token`,
  `next_page_token`, `page_token`, `pagination_token`, `continuation_token`, `resume_token`,
  `sync_token` and a few others, matched as the whole label) stay allowed, as does the sealed
  `visibility_token` receipt in a quoted `memory.remember` response. The label directly before a
  repository revision URL or a LaTeX fragment follows the same rule.
- The daemon registers its signal-sender recorder before its shutdown signal streams, so the
  exit record names the sender of a SIGTERM or SIGINT instead of intermittently writing
  `sender_pid=unknown`. Registering the recorder for a signal that already has a handler is
  logged on `khive.boot`.
- On a database without `sqlite_stat1`, an ID-filtered event read (including the events-split
  store's check of each new audit batch against the legacy store) and the schedule
  creator-provenance lookup now seek the primary key and `idx_schedule_creator_provenance`.
  Before, both walked every event of the namespace in `created_at` order.
- Volume-lease waiters acquire in FIFO order without changing deadline or re-entry errors.
  File-backed kkernel test fixtures use private lock directories to avoid unrelated contention
  (#5087).
- `kkernel kg export` checks file sync and removes its own temporary after a write, sync or
  rename failure. On Unix it publishes through shared atomic publication: it refuses an existing
  temporary instead of replacing it, keeps the original filename bytes, creates the file with
  mode 0666 under the umask and syncs the parent directory after the rename. Existing shared
  staging policies keep mode 0644 and their behavior (#5068).
- Named-vector and memory session exact search select equal-distance boundary hits by
  subject UUID before limiting candidates, keeping their order stable across database reopen.

- `kkernel kg init` now creates the canonical `.khive/config.toml` with valid engine settings,
  preserves existing project configurations, and refuses the obsolete config path before writing
  scaffolding. Only the exact old generated ignore file is migrated to the canonical filename.

- Identifier splitting restores acronym boundaries in `DBUsers` and `APIUsage` while keeping
  terminal plural suffixes in `APIs`, `IDs`, `URLs`, and `APIsFoo`. `URLsafe` now splits as
  `ur`, `lsafe`; `XMLIsEmpty` follows the dictionary-free rule as `xmlis`, `empty` (#5103).
- `comm.read` preserves typed storage errors from its initial message lookup, including
  admission-timeout code, stage, and retry guidance. Bulk read and mark-read validation
  retain the same error source without marking messages after a failed lookup. (#4958)
- GTD status and priority refusals now mask credential-shaped values with the existing secret
  masker. Plain invalid values, accepted-value hints, and error kinds remain unchanged.
- `kg_name` and `IdentifierTokenizer` keep plural acronyms such as `APIs`, `IDs` and `URLs`
  whole (a terminal lowercase `s` after an acronym), while still splitting acronym-to-word
  boundaries such as `XMLParser`, `HTTPServer` and `XMLToJSON`.
- Knowledge atom list, count, cursor, coverage, and search now share import/delete's exact
  decoded domain-tag rule, preserving legacy malformed-tag handling. Near-marker atom tags
  remain visible, and an additive migration rebuilds the cursor index without rewriting rows.
- `git.push` accepts the optional boolean `force` validation field and refuses
  `true` with `force_denied` before transport. False and omission retain ordinary
  push behavior; null and non-boolean values return `invalid_params`.
- Pack schema registration now checks table ownership in every statement of a schema entry,
  refusing collisions before applying any DDL. Semicolons inside quoted names, strings, and
  comments remain part of those tokens.
- Pack-version regressions compare each linked pack with its factory's declared version and cover
  independent factory versions through the production JSON and human list formatter (#5086).
- Finding create/update and findings-file ingestion mask credential-shaped rejected enum values
  while retaining ordinary values, error kinds, and accepted-value lists (Refs #4585).

### Breaking (Rust crates)

- Memory's `RecallFtsGatherMode` now re-exports the canonical storage `TextGatherMode` instead of
  declaring a separate enum. Its public path, variant imports, `Copy` behavior, serialized config,
  and search options remain supported; code depending on the old nominal type identity must adapt.

- `khive-runtime` removes `RuntimeError::Sqlite`; concrete backend failures now use
  `RuntimeError::Storage`, retaining typed driver sources and the existing capacity,
  retry, and writer-settlement wire codes. Callers matching the removed variant
  should inspect `StorageError` instead. Typed policy reasons now report `Storage`,
  rendered backend messages use the storage envelope, and direct writer-checkout
  failures carry `sql`/`runtime` capability/operation context instead of nulls.
- `khive-runtime` accepts ordered peer `[[engines]]` entries with `name`, unit-default
  `weight`, and checked `dims`. `EngineConfig` replaces `model`, `default`, and
  `fusion_weight`; `RuntimeConfig` adds an authoritative optional peer list and
  `KhiveConfig` records an explicit empty list. Legacy TOML is converted with its
  former default first and canonical provider/index names preserved. Duplicate aliases,
  storage-key collisions, mixed schemas, invalid dimensions, and weights that are not
  applied by every retrieval path fail explicitly. Startup checks built-in provider
  dimensions; custom provider binding remains unavailable pending its startup integration.
  Direct struct literals and exhaustive `ConfigError` matches must be updated.
  Use `disable_embedding_models()` to clear peers and legacy projections together.
  Daemon reuse now requires the same ordered engine configuration.
- `khive_db::diagnostics::ReaderContentionDiagnostics` adds `reader_discards`; direct struct
  constructors and exhaustive destructurings must account for it. The `db_diagnostics` response
  now exposes this cumulative pool counter beside `reader_replacement_open_failures`, including
  successful reader recycling without changing pool behavior.
- `khive-retrieval`'s `fuse_search_results`, `hybrid::fuse_labelled_scored`, and
  `DualIndexRouter::merge_results` now return `Result<Vec<_>, khive_fusion::FuseError>`;
  callers must handle or propagate failures. Invalid weighted-RRF parameters, source counts and
  score overflow now propagate through memory recall instead of appearing as no results, including
  empty candidate sets. The checked helper keeps its `RetrievalError` result type, and existing
  unchecked `Weighted`/`Custom` fallback behavior is preserved.
- `khive_storage::NoteFilter` adds `max_created_at`, `max_expires_at`,
  `include_deleted`, `expiry_fallback` and `time_order`; exhaustive struct literals
  must add them or use `..Default::default()`. `FilterOp` adds
  `MissingNullOrSpaceEmptyText` and `TrueOrTextTrue`; exhaustive matches must handle
  both. Typed expiry fallback and native timestamp ordering preserve complete
  legacy cleanup predicates before LIMIT. The `created_before` / `expires_before` builders use
  inclusive Unix-microsecond bounds, and `include_deleted` affects reads only.
  Existing serialized filters retain live-only, unbounded defaults.
  `AttachmentStore::delete_attachment_if` adds a conditional ID/role/substrate/ref
  delete with an `Unsupported` default and an atomic SQLite implementation;
  comm cleanup adoption remains separate.

- `KhiveRuntime::list_embedding_models` now returns runtime-owned `EmbeddingModelRecord`
  values with all twelve persisted registry fields. Rust callers use `dim` instead of
  `dimensions` and the `EmbeddingModelStatus` enum instead of a status string. Status serde
  values remain lowercase; `kkernel engine list/status` retain their seven-field JSON shape.
  Invalid required fields, unknown statuses and invalid non-NULL optional dimensions/UUIDs
  are skipped with a warning. Existing optional timestamp decoding still maps non-integers
  to `None`; reads never fabricate missing identity or lifecycle history.
- Symmetric-edge runtime updates now use the guarded `GraphStore` capability. Direct-fallback
  backend failures carry `graph`/`update_edge` operation context, and blocking-task join failures
  return `RuntimeError::Storage` instead of `Internal`; reasons and rendered prefixes on those paths
  change. Concrete causes and writer-settlement distinctions are retained. Backends without the new
  transaction method explicitly return `Unsupported`.
- `khive_runtime::AtomicOpPlan` adds `FinalizeEntity` for runtime-owned entity admission.
  Downstream exhaustive matches must handle the new variant. Its plan has no public constructor;
  direct ingest uses `EntityCandidateContext` and `try_commit_manifest_entity_candidate`.
- `khive-fusion` adds `WeightedRrf { k, weights }`, positive-weight validation, and weighted-RRF
  fusion errors. This is source-breaking for consumers with exhaustive matches on
  `FusionStrategy`, `FusionStrategyError`, or `FuseError`; update those matches to handle the new
  rank-based strategy and its validation errors. Weighted RRF checks positive finite weights and
  source-count alignment, preserves empty source slots, and errors when a fused score exceeds the
  finite deterministic-score range.
- Breaking Rust API change: `khive_types::RerankExecutedPayload::model_id` is now
  optional, and the payload adds `reranker`, `query_id`, `tiers`, `ignored_weights`
  and `unidentified_candidates`. Direct constructors and exhaustive destructurings
  must account for those fields. Stored events without a discriminator still decode
  as native reranks; weighted memory reranking does not invent a model identity.
- `khive-storage` adds `StorageError::SqliteWrite` and `khive-db` adds
  `SqliteError::{Write, WriteSettlementUnknown}`, carrying native SQLite write evidence
  (`SqliteWriteFailure`: stage, primary and extended codes, settlement flag). Exhaustive matches
  must handle the new variants, `SqliteWriteFailure` literals must include every field, and
  `EventsResponse::Error` literals need the new optional `sqlite_write_failure` field. On the
  wire, a native `BEGIN` refusal now reports `code`/`stage` `sqlite_begin_busy` instead of
  `writer_task_begin_busy`, with retryability unchanged; without native evidence the code stays
  `writer_task_begin_busy`. A definite native write failure reports its stage as `code` and
  `stage`, with `sqlite_write_stage`, `sqlite_primary_code` and `sqlite_extended_code` beside it.

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
