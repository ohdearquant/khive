# ADR-080: Session Pack — OSS Storage Mechanism

**Status**: accepted
**Date**: 2026-06-28
**Amended**: 2026-07-02 — shipped-surface record (§3), session mirror (§6), scope revision (Context)
**Authors**: khive maintainers

## Context

### Session storage was previously deferred from the OSS surface

An earlier internal stance held that session storage was a deployment concern and therefore
out of the OSS repository's scope. That boundary was a scoping choice, not a technical one.
This ADR supersedes it: the session-storage _mechanism_ — the `khive-pack-session` crate,
the `session.*` verb surface, and the note-kind registration — is now part of the OSS surface.
The scope boundary moves, not the underlying design.

**What remains outside the OSS scope.** The session _ingestion and digestion pipeline_ —
JSONL parsers, summarization, transcript processing, and any aggregation logic that derives
structured output from raw session content — is not in scope for this repository or this
ADR. The OSS pack ships storage and retrieval verbs only.

> **Amended 2026-07-02**: partially superseded. Transcript _format parsers_ and idempotent
> _ingestion_ are now in scope, shipped as the pack's read-only background mirror (§6).
> Summarization, digestion, and any aggregation that derives structured output from raw
> session content remain out of scope, unchanged.

### The pack system already supports the required extension points

Three ADRs establish the building blocks for any new pack:

- **ADR-017** defines the `Pack` trait (`NAME`, `NOTE_KINDS`, `ENTITY_KINDS`, `HANDLERS`,
  `EDGE_RULES`, `REQUIRES`) and the `PackRuntime` async dispatch trait. Note kinds registered
  in `NOTE_KINDS` are additive to the base five (ADR-013); they are full peers of `task`
  (GTD, ADR-019) and `memory` (memory pack, ADR-021) — same storage substrate, same edge
  ontology, same supersession rules.
- **ADR-027** establishes self-registration via `inventory::submit!`. A pack crate submits a
  factory at link time; `PackRegistry::register_packs()` collects all submissions at startup,
  validates `REQUIRES` ordering, and constructs pack instances. No edits to `serve.rs` or
  any dispatch crate are needed when a new pack is added.
- **ADR-028** specifies pack-scoped backends and per-pack schema declaration via `PackSchemaPlan`.
  The GTD pack uses this to declare its `gtd_lifecycle_audit` auxiliary table — the session
  pack's M2 upgrade path follows the same mechanism.

### The ADR-073 `core()` accessor enables a hybrid write pattern

ADR-073 adds `core()` to `KhiveRuntime`: it returns a runtime backed by the main (shared)
backend, falling back to `self.clone()` when `core_backend` is `None` (the single-backend
case). This accessor is the contract that lets a pack assigned to a dedicated backend write
linkable notes to the main store while writing bulk rows to its own auxiliary tables. For
M1, where only the single main backend is in use, `core()` is a no-op clone and session
notes land in the shared store alongside KG, GTD, and memory notes.

### The ADR-071 `BackendHandle` seam is in place but deferred

ADR-071 replaces `Arc<StorageBackend>` with a `BackendHandle` struct carrying individual
trait objects for each storage capability. Phase 4 of ADR-071 is not yet implemented; the
current runtime still holds a concrete `Arc<StorageBackend>`. The session pack's verb
handlers call `runtime.create_note` / `runtime.get_note` / `runtime.list_notes` — the
public `KhiveRuntime` API — and therefore require no modification when ADR-071 Phase 4
lands. The `BackendHandle` seam is preserved by this ADR.

### A `session` note kind fills a gap in the note taxonomy

The five base note kinds (ADR-013) cover research-KG cognition: `observation`, `insight`,
`question`, `decision`, `reference`. Agent sessions — transcripts, context snapshots,
accumulated state — do not fit neatly into any of these. Storing them as `observation`
notes misuses the kind and loses the ability to discriminate them in queries
(`search(kind="session")`). The session kind is a domain-appropriate extension, following
the same rationale as `task` (GTD) and `memory` (memory pack): a new domain, a new kind.

## Decision

### 1. New crate `khive-pack-session`, scaffolded from `khive-pack-template`

A new crate `crates/khive-pack-session/` is added to the workspace. It follows the
scaffold established by `khive-pack-template` and the implementation pattern of
`khive-pack-gtd`:

- `src/pack.rs` — `SessionPack` implementing `Pack` and `PackRuntime`; `SessionPackFactory`
  with `inventory::submit! { khive_runtime::PackRegistration(&SessionPackFactory) }`.
- `src/vocab.rs` — `SESSION_HANDLERS: [HandlerDef; 4]` and, for M2, the optional
  `SESSION_SCHEMA_PLAN_STMTS`.
- `src/handlers/` — one file per verb (`store.rs`, `list.rs`, `resume.rs`, `export.rs`).

`crates/khive-mcp/Cargo.toml` gains a `khive-pack-session` dependency; the `inventory`
self-registration wires it into the binary without any code change in `serve.rs`.

### 2. `session` note kind registered via `Pack::NOTE_KINDS`

```rust
impl Pack for SessionPack {
    const NAME:       &'static str = "session";
    const NOTE_KINDS: &'static [&'static str] = &["session"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS:   &'static [HandlerDef] = &SESSION_HANDLERS;
    const REQUIRES:   &'static [&'static str] = &["kg"];
    // SCHEMA_PLAN: None for M1; Some(PackSchemaPlan { ... }) for M2
}
```

Registering `"session"` in `NOTE_KINDS` is the ADR-013 pack-owned extension mechanism —
the same path GTD takes for `"task"`. No schema migration is required for M1: the existing
`notes` table accepts arbitrary `kind` values and `content TEXT` is unbounded in SQLite.
The runtime validates the kind against all registered `NOTE_KINDS` at write time and
returns `RuntimeError::UnknownNoteKind` if the pack is not loaded.

### 3. Verb surface: four verbs, all prefixed `session.*`

All four verbs have `visibility: Visibility::Verb`. Speech-act categories follow ADR-025.

> **Amended 2026-07-02** — shipped-surface record. The implementation that landed diverges
> from this section in three ways, all deliberate:
>
> 1. **Three handlers, not four.** `session.resume` was renamed `session.get` before ship
>    (consistency with the substrate-wide `get`). `session.export` was never registered as
>    a verb: serialization is an internal helper (`handle_export`) called in-process, with
>    no `HandlerDef` entry.
> 2. **`Visibility::Subhandler`, not `Verb`.** All three handlers ship operator-only:
>    dispatchable through the runtime and `kkernel exec`, withheld from the agent-facing
>    MCP `request` surface until the session-continuity query UX is designed.
> 3. **The pack's active feature is the background mirror** (§6), which runs from the
>    `warm()` hook independent of verb visibility.

#### `session.store` (Directive)

Store a session blob: transcript, context snapshot, accumulated agent state, or arbitrary
text content.

| Parameter  | Type     | Required | Description                                            |
| ---------- | -------- | -------- | ------------------------------------------------------ |
| `content`  | string   | yes      | Arbitrary text content                                 |
| `agent_id` | string   | no       | Stored in `properties.agent_id`; used as a list filter |
| `tags`     | string[] | no       | Standard note tags                                     |
| `metadata` | object   | no       | Arbitrary JSON merged into `properties`                |

Implementation: `runtime.create_note(token, "session", None, content, None, props, vec![])`.
Returns the standard Note envelope (`id`, `kind`, `created_at`, and properties).

#### `session.list` (Assertive)

List stored sessions, newest first.

| Parameter  | Type         | Description                     |
| ---------- | ------------ | ------------------------------- |
| `agent_id` | string       | Filter by `properties.agent_id` |
| `limit`    | integer      | Page size (default 20)          |
| `offset`   | integer      | Pagination offset               |
| `since`    | ISO datetime | Filter: `created_at >= since`   |

Implementation: `runtime.list_notes(token, "session", filters, limit, offset)`.

#### `session.resume` (Assertive)

Fetch a single session record by UUID for replay or context injection.

| Parameter | Type | Description |
| --------- | ---- | ----------- |
| `id`      | UUID | Required    |

Implementation: `runtime.get_note(id)`. Returns the full Note record (`id`, `kind`,
`content`, `properties`, `tags`, `created_at`).

#### `session.export` (Assertive)

Serialize a session record for downstream use.

| Parameter | Type                 | Description      |
| --------- | -------------------- | ---------------- |
| `id`      | UUID                 | Required         |
| `format`  | `"json"` \| `"text"` | Default `"json"` |

Implementation: fetch via `runtime.get_note(id)`, then serialize to the requested format.
`"json"` returns the full Note envelope as a JSON object; `"text"` returns `content` only.

`session.import` is **not in scope** for this pack. Ingestion and processing of external
session content belongs to layers outside this repository.

> **Amended 2026-07-02**: still no `session.import` verb — ingestion is not caller-driven.
> It ships instead as the read-only background mirror (§6), which tails known transcript
> locations on a poll loop. The digestion half of the original exclusion (summarization,
> derived aggregation) remains out of scope.

### 4. Storage phasing: M1 (substrate-native) and M2 (optional auxiliary index)

The two phases share the same verb surface. The difference is where auxiliary index data
lives; the caller sees no API change between M1 and M2.

#### M1 — substrate-native note storage (shipped)

Session records are stored as `kind=session` notes in the main backend via
`runtime.create_note`. The ADR-073 `core()` call is a no-op clone in the single-backend
case (the only currently supported configuration): `core()` returns `self.clone()` when
`core_backend` is `None`, so session notes land in the shared `notes` table alongside KG,
GTD, and memory notes, queryable by `search(kind="session")`.

M1 requires no schema migration and no auxiliary tables. It is the complete shipped
implementation for the first PR.

#### M2 — optional dedicated `session_metadata` index (deferred)

> **Amended 2026-07-02**: the pack now ships a schema plan, but it carries the mirror's
> auxiliary tables (`sessions`, `session_messages`, `session_mirror_cursor` — §6), not the
> `session_metadata` index sketched below. That index remains deferred and unshipped; the
> paragraphs below stay as the upgrade sketch for a measured list-query bottleneck.

When list-query performance over large session corpora becomes the constraint, the pack
may introduce a dedicated `session_metadata` auxiliary table via `PackSchemaPlan` — the
same ADR-028 mechanism GTD uses for `gtd_lifecycle_audit`. The table indexes `agent_id`,
`started_at`, `ended_at`, and `session_id` as SQL columns, enabling fast range queries
without a full `notes` table scan.

The M2 schema plan would be declared as:

```rust
const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
    pack: "session",
    statements: &SESSION_SCHEMA_PLAN_STMTS,
});
```

The cross-backend write pattern for M2 uses ADR-073: the `session_metadata` row goes to
the pack's assigned backend via `runtime.sql().execute(...)`, while the note (which must
be reachable by `memory.recall`, `search`, and cross-pack `annotates` edges) is written
to the main backend via `runtime.core().create_note(...)`. ADR-073 §5 constraint applies:
no graph edges may span SQLite files; cross-backend linking between the metadata row and
the note is illegal.

M1 is the degenerate single-backend case of this same pattern: `core()` returns
`self.clone()`, so both paths write to the same backend. The M2 upgrade adds the auxiliary
table and, optionally, a dedicated session backend; the verb handlers require no change
beyond routing the metadata write.

### 5. The ADR-071 `BackendHandle` seam is preserved

Session verb handlers call only the public `KhiveRuntime` API methods (`create_note`,
`get_note`, `list_notes`) and, for M2, `runtime.core()` (ADR-073) and `runtime.sql()`.
They do not hold a direct reference to `Arc<StorageBackend>` or any `khive-db` type. When
ADR-071 Phase 4 replaces `Arc<StorageBackend>` with `BackendHandle`, the session pack
requires no modification. This is an explicit constraint on the implementation.

### 6. The session mirror (Amendment, 2026-07-02)

The pack's shipped ingestion surface is a **read-only background mirror**: a poll loop,
spawned from `PackRuntime::warm()` when enabled, that tails known transcript locations on
the local filesystem and lands their content in the pack's auxiliary tables. It shipped as
the M2 milestone of the session pack build (issue #350, PR #368) with the Claude Code
source, gained the Codex CLI source in PR #375, and gains the ChatGPT export source with
this amendment. The mirror is disabled by default and never writes to, moves, or deletes
the files it reads.

#### Mirror sources — closed set

`MirrorSource` is a closed enum. Adding a source requires amending this section.

| Source         | `source` value   | Input shape                                        | Ingestion mode |
| -------------- | ---------------- | -------------------------------------------------- | -------------- |
| Claude Code    | `claude_code`    | `<projects dir>/**/<session-uuid>.jsonl`           | line-tail      |
| Codex CLI      | `codex`          | `<sessions dir>/**/rollout-*-<session-uuid>.jsonl` | line-tail      |
| ChatGPT export | `chatgpt_export` | `<exports dir>/**/conversations.json` (JSON array) | whole-file     |

**Line-tail** (append-only JSONL): each pass reads from the file's stored byte offset,
parses complete lines, and advances the cursor to the last complete line boundary. A file
whose length has not grown past the cursor is skipped without being opened.

A single line is never buffered past a hard per-line byte cap (`MirrorLimits::max_line_bytes`,
PACKSESSION-AUD-003): a complete line (terminated by `\n`) over the cap is skipped —
`tracing::warn!`-logged with the file and byte offset, never parsed — and the cursor advances
past it so ingestion cannot wedge on one oversized line. A line that crosses the cap with **no**
terminating `\n` yet (a still-growing file's in-progress final line, or a truncated/corrupt tail)
is a distinct, bounded case: the read for that line stops as soon as one bounded read window
crosses the cap without finding `\n`, instead of scanning onward to EOF searching for one that may
never come. The cursor is left at that line's start — the same as an ordinary incomplete trailing
line — so the next pass, or the next daemon start, repeats the same bounded read rather than an
unbounded tail scan; once the line eventually terminates (or growth stops and the file reaches
true EOF mid-line), it resolves through the ordinary skip-and-advance path above.

**Whole-file** (export archives): the file is parsed as a single JSON document. On success
the cursor is set to the file's byte length, so an unchanged export is skipped by the same
length fast-path on every later pass. On parse failure — including a partially downloaded
export — the cursor does not advance and the file is retried on the next pass.

#### Auxiliary schema

The shipped schema plan (ADR-028 mechanism, applied at boot) declares three tables and
three indexes: `sessions` (one row per session or conversation: provider id, source, cwd,
git branch, slug, message count, first/last seen), `session_messages` (one row per
transcript event: uuid key, session id, per-session `seq`, parent uuid, sidechain flag,
role, type, masked text, masked raw, timestamp), and `session_mirror_cursor` (one row per
watched file: byte offset, session id, updated-at).

#### Invariants (normative for every source)

1. **Sessions are create-only.** The session row inserts with `ON CONFLICT(id) DO NOTHING`;
   later passes only touch metadata, monotonically (`last_seen_at = MAX(...)`) with
   `COALESCE` backfill of nullable fields, and only when the pass actually inserted rows.
2. **Message inserts are idempotent.** `INSERT OR IGNORE` keyed on the event UUID;
   re-mirroring any file, in whole or in part, changes nothing already stored. `seq` is
   assigned per session at insert time.
3. **Secret masking is unconditional.** Every `text` and `raw` value passes through
   `khive_runtime::secret_gate` masking before storage, for every source.
4. **Errors never advance the cursor.** A per-file parse or IO error leaves that file's
   cursor untouched (the file is retried next pass) and does not block other files.
5. **One transaction per file pass.** Message rows, the metadata touch, the message-count
   refresh, and the cursor upsert commit atomically.

#### ChatGPT export mapping

The ChatGPT source ingests the `conversations.json` from a ChatGPT data export: a JSON
array of conversation objects, each carrying a `mapping` of message nodes forming a tree
(regenerations and edits create sibling branches; `current_node` marks the active leaf).

- One conversation object becomes one `sessions` row. `id` and `provider_session_id` are
  the conversation UUID; `slug` carries the conversation title; `cwd` and `git_branch` are
  null (no workspace context exists in the export).
- The `mapping` tree is traversed depth-first preorder from the root, following each
  node's `children` order. Every node with a non-null message and non-empty extracted text
  becomes a `session_messages` row keyed on the ChatGPT message UUID, with `parent_uuid`
  taken from the tree.
- `is_sidechain` is set for nodes not on the root-to-`current_node` path. Branches are
  ingested, not dropped: the data layer preserves history; selecting the active thread is
  a view concern.
- Timestamps fall back per message: `message.create_time`, else the conversation's
  `create_time`, else zero — converted to microseconds.
- Content extraction covers the `text` (parts array), `code`, and `execution_output`
  content types. System scaffolding nodes with empty parts are skipped.
- Discovery is a recursive scan of the configured directory for files named exactly
  `conversations.json`, so both a bare file and unpacked export archives work.

#### Configuration

Most configuration is environment-driven, read once into `MirrorConfig` when `warm()`
starts the service:

| Variable                       | Default                  |
| ------------------------------ | ------------------------ |
| `KHIVE_MIRROR_ENABLED`         | `false`                  |
| `KHIVE_MIRROR_PROJECTS_DIR`    | `$HOME/.claude/projects` |
| `KHIVE_MIRROR_CODEX_ENABLED`   | `false`                  |
| `KHIVE_MIRROR_CODEX_DIR`       | `$HOME/.codex/sessions`  |
| `KHIVE_MIRROR_CHATGPT_ENABLED` | `false`                  |
| `KHIVE_MIRROR_CHATGPT_DIR`     | `$HOME/.chatgpt/exports` |
| `KHIVE_MIRROR_POLL_SECS`       | `2`                      |
| `KHIVE_MIRROR_BACKFILL`        | `true`                   |

`KHIVE_MIRROR_CHATGPT_MAX_BYTES` (default `268435456`, 256 MiB) is read separately, per
pass, by `mirror_chatgpt_export_file` itself rather than through `MirrorConfig`. It is a
hard ceiling on a `conversations.json` export's _whole_ file length (whole-file ingestion
has no incremental delta to bound the way the line-tail sources do): an export over the
ceiling is skipped for that pass — `tracing::warn!`-logged, never `read_to_string`'d — and
its cursor is left untouched, so it is retried (and re-warned) on every later tick rather
than silently dropped forever. A zero or non-numeric value falls back to the default.

#### What remains out of scope

Summarization, transcript digestion, and any aggregation that derives structured output
from mirrored rows. The mirror lands faithful, masked, idempotent copies; everything that
interprets them lives outside this repository.

## Rationale

- **Why `kind=session` over `kind=observation`.** Using the existing `observation` kind
  would prevent discriminating session records in queries and searches. A dedicated kind
  costs one entry in `NOTE_KINDS` and zero schema changes; the benefit is precise filtering
  (`search(kind="session")`), a clear lifecycle contract, and accurate kind-level validation
  at write time.
- **Why M1 before M2.** The `notes` table with `kind='session'` is sufficient for the
  initial walking-skeleton implementation: `list_notes` with a kind filter handles
  moderate volumes, FTS and vector search cover the retrieval cases, and no auxiliary table
  is needed. M2 is an upgrade path for when a measured list-query bottleneck justifies the
  added complexity. Shipping M2 before the bottleneck exists violates the project's
  anti-pattern of premature optimization.
- **Why `inventory::submit!` over a match arm in `serve.rs`.** ADR-027 established
  self-registration precisely to avoid editing dispatch crates for each new pack. Adding
  a match arm in `serve.rs` would be a regression to the pre-ADR-027 pattern.
- **Why no `session.import`.** The ingestion pipeline that transforms external session
  content into storable records involves parsing, summarization, and content-specific
  logic. These belong outside this repository. The storage and retrieval verbs are
  sufficient for the OSS mechanism; digestion is a separate concern.
- **Why preserve the ADR-071 seam.** ADR-071 is an accepted ADR targeting a material
  change to the runtime's storage handle. Coupling the session pack to the concrete
  `Arc<StorageBackend>` type would require revisiting it when ADR-071 Phase 4 lands.
  Using only the public `KhiveRuntime` API costs nothing and preserves forward compatibility.

## Alternatives Considered

| Alternative                                                   | Pros                                | Cons                                                                                                 | Why rejected                                                                                                                           |
| ------------------------------------------------------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Store sessions as `kind=observation` in the existing pack     | Zero new code; no new kind          | No kind-level discrimination; `search(kind="observation")` pollutes unrelated results                | A dedicated kind costs one `NOTE_KINDS` entry and zero schema changes; the discrimination benefit is real                              |
| Start with M2 (auxiliary table) immediately                   | Faster list queries from day one    | Introduces `PackSchemaPlan` complexity before the bottleneck is measured                             | Anti-pattern: premature optimization; M1 is simpler and correct; M2 is an upgrade path                                                 |
| Standalone binary / separate KV store for session data        | No coupling to the KG substrate     | Session records unreachable by `memory.recall`, `search`, and `annotates`; a second store to operate | The KG substrate is the purpose of this repository; keeping sessions in it enables cross-pack recall                                   |
| Embed session storage in the `memory` pack as a `memory_type` | Reuses existing pack infrastructure | Conflates two distinct concepts: agent sessions and decay-weighted memories; complicates both        | SRP: session storage and recall-calibrated memory are distinct concerns with distinct lifecycles                                       |
| Keep session storage outside the OSS repository entirely      | No scope expansion                  | Duplicates pack boilerplate wherever sessions are needed; fragments the verb surface                 | The mechanism is generic enough to ship once, in the OSS pack layer; the prior stance was a scoping choice, not a technical constraint |

## Consequences

### Positive

- Agent sessions are storable, retrievable by UUID, listable by `agent_id` and time range,
  and exportable — all through the established `session.*` verb surface — without requiring
  any deployment outside this repository.
- Session records participate in the shared graph: `memory.recall`, full-text and vector
  search, and `annotates` edges all work because session notes land in the main backend.
- The pack adds no schema migration for M1: the existing `notes` table and `NOTE_KINDS`
  registration mechanism are sufficient.
- The M2 upgrade path (auxiliary `session_metadata` index via `PackSchemaPlan`) is
  available without any verb API change when list-query scale warrants it.
- The `inventory::submit!` self-registration keeps `serve.rs` unmodified; adding or
  removing the session pack requires only a `KHIVE_PACKS` config change or the dependency
  entry in `khive-mcp/Cargo.toml`.

### Negative

- A new crate (`khive-pack-session`) adds to the workspace build graph and to the binary
  size when the pack is included. Mitigation: the crate is unconditionally small (four
  verb handlers over existing runtime methods); it can be excluded from a minimal build
  via `KHIVE_PACKS` at runtime.
- The `session` note kind is a permanent addition to the kind registry for any deployment
  that loads this pack. Note-kind registrations are validated at boot, so the addition is
  visible and explicit — not silent — but it cannot be unregistered without removing the
  pack from the binary.

### Neutral

- No change to the `khive-vamana`, `khive-db`, `khive-storage`, or `khive-runtime` crates.
  The session pack is a pure consumer of the existing runtime API.
- No schema migration is introduced by this ADR. If M2 is adopted, it will carry a
  migration via the standard `PackSchemaPlan` mechanism (ADR-028); that migration is out
  of scope here.
- ADR-013's note kind taxonomy gains one pack-registered kind (`session`) in the same
  manner as `task` (ADR-019) and `memory` (ADR-021). No amendment to ADR-013 is required;
  the pack extension mechanism ADR-013 §"Pack-registered note kinds" anticipates this.

## References

- [ADR-013](ADR-013-note-kind-taxonomy.md) — Note Kind Taxonomy; §"Pack-registered note kinds" establishes the extension mechanism this ADR uses
- [ADR-017](ADR-017-pack-standard.md) — Pack Standard; `Pack` and `PackRuntime` traits; `NOTE_KINDS` const
- [ADR-019](ADR-019-gtd-pack.md) — GTD Pack; reference for `kind=task` and `PackSchemaPlan` usage
- [ADR-021](ADR-021-memory-pack.md) — Memory Pack; reference for `kind=memory` as a pack-registered kind
- [ADR-023](ADR-023-declarative-pack-format.md) — Pack Verb Surface, Visibility, and Composition; verb registration contract
- [ADR-025](ADR-025-verb-speech-acts.md) — Verb Surface as Speech-Act Taxonomy; Directive / Assertive classification
- [ADR-027](ADR-027-dynamic-pack-loading.md) — Dynamic Pack Loading via Self-Registration; `inventory::submit!` pattern
- [ADR-028](ADR-028-pack-scoped-backends.md) — Pack-Scoped Backends and Per-Pack Schema Declaration; `PackSchemaPlan` for M2
- [ADR-071](ADR-071-backend-pluggable-runtime.md) — Backend-Pluggable Runtime; `BackendHandle` seam preserved by §5
- [ADR-073](ADR-073-pack-core-backend-accessor.md) — Pack Core-Backend Accessor; `core()` accessor used by M2 cross-backend write pattern
