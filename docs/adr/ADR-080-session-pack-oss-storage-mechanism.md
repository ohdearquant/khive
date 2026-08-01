# ADR-080: Session Pack — OSS Storage Mechanism

**Status**: accepted
**Date**: 2026-06-28
**Amended**: 2026-07-02 — shipped-surface record (§3), session mirror (§6), scope revision (Context); 2026-08-01 — claude.ai export source (§6)
**Superseded by**: [ADR-083](ADR-083-session-pack-t1-verbs.md) for §3 only; ADR-083 is
the current authority for the public session verb surface, while the rest of this record
remains in force.
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
handlers call only the public `KhiveRuntime` APIs specified by ADR-083 §4 and therefore
require no modification when ADR-071 Phase 4 lands. The `BackendHandle` seam is
preserved by this ADR.

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

### 3. Verb surface _(historical; superseded by ADR-083)_

[ADR-083](ADR-083-session-pack-t1-verbs.md) supersedes this section in full
and is the sole accepted authority for the current caller-facing surface. The
live declaration has:

- four `Visibility::Verb` handlers: `session.store`, `session.list`,
  `session.resume`, and `session.export`;
- `session.store(content, title?, provider?, provider_session_id?, tags?)`;
- `session.list(limit?, offset?, provider?, agent_id?, since?)` (ADR-083
  Amendment 1 adds the two server-side filters without changing `session.store`);
- `session.resume(id)` by full UUID or an 8+ hex prefix; and
- `session.export(id, format?)`, where `format` is `json` or `markdown`.

The original ADR-080 decision and its 2026-07-02 amendment are retained only
as history: the original draft described four verbs with an earlier parameter
vocabulary, while the amendment recorded the intervening shipped surface of
three `Visibility::Subhandler` handlers (`session.store`, `session.list`, and
`session.get`) with no dispatchable export. Neither historical surface is the
current contract.

ADR-083 does not change this ADR's storage-mechanism decisions (§1, §2, §4,
§5), and it leaves the background mirror (§6) intact. There is still no
caller-driven `session.import` verb: transcript ingestion ships through the
read-only background mirror, while summarization and derived aggregation
remain out of scope.

### 4. Storage phasing: M1 (substrate-native) and M2 (optional auxiliary index)

These phases describe storage only. The current public verb surface is owned
by ADR-083 and remains unchanged if a future auxiliary index is added; the
difference between M1 and M2 is where auxiliary index data lives.

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
same ADR-028 mechanism GTD uses for `gtd_lifecycle_audit`. The table indexes
`provider`, `provider_session_id`, and note creation time as SQL columns, enabling
indexed forms of ADR-083's current list and continuity lookups without a full
`notes` table scan.

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

Session verb handlers use only the public `KhiveRuntime` APIs specified by
ADR-083 §4 and, for a future M2 index, `runtime.core()` (ADR-073) and
`runtime.sql()`. They do not hold a direct reference to `Arc<StorageBackend>`
or any `khive-db` type. When ADR-071 Phase 4 replaces `Arc<StorageBackend>`
with `BackendHandle`, the session pack requires no modification. This is an
explicit constraint on the implementation.

### 6. The session mirror (Amendment, 2026-07-02)

The pack's shipped ingestion surface is a **read-only background mirror**: a poll loop,
spawned from `PackRuntime::warm()` when enabled, that tails known transcript locations on
the local filesystem and lands their content in the pack's auxiliary tables. It shipped as
the M2 milestone of the session pack build (issue #350, PR #368) with the Claude Code
source, gained the Codex CLI source in PR #375, and gains the ChatGPT export source with
the 2026-07-02 amendment. The 2026-08-01 amendment adds claude.ai data exports as a
separate fourth source. The mirror is disabled by default and never writes to, moves, or
deletes the files it reads.

#### Mirror sources — closed set

`MirrorSource` is a closed enum. Adding a source requires amending this section.

| Source           | `source` value     | Input shape                                        | Ingestion mode |
| ---------------- | ------------------ | -------------------------------------------------- | -------------- |
| Claude Code      | `claude_code`      | `<projects dir>/**/<session-uuid>.jsonl`           | line-tail      |
| Codex CLI        | `codex`            | `<sessions dir>/**/rollout-*-<session-uuid>.jsonl` | line-tail      |
| ChatGPT export   | `chatgpt_export`   | `<exports dir>/**/conversations.json` (JSON array) | whole-file     |
| claude.ai export | `claude_ai_export` | `<exports dir>/**/conversations.json` (JSON array) | whole-file     |

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

ChatGPT and claude.ai both name this file `conversations.json`, while the cursor key is the
file path. Each export parser therefore rejects a document containing the other provider's
recognizable conversation shape, and a mixed-provider array is unsupported. This leaves the
cursor untouched instead of allowing a misconfigured overlapping root to let the wrong source
consume that path first.

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

#### claude.ai export mapping (Amendment, 2026-08-01)

The claude.ai source ingests the `conversations.json` from Claude's data export through a
parser distinct from both Claude Code and ChatGPT. Its top-level array contains conversation
objects with `uuid`, `name`, and `chat_messages` rather than ChatGPT's `id` / `mapping` tree.

- One conversation object becomes one `sessions` row. `id` and
  `provider_session_id` are the conversation `uuid`; `slug` carries `name` (falling back to
  `summary`); `cwd` and `git_branch` are null.
- Each non-empty message with a `uuid` becomes a `session_messages` row keyed on that UUID.
  `human` is normalized to role `user`; `assistant` remains `assistant`, with legacy `role`
  accepted when `sender` is absent. Events are emitted by numeric `index`, with source-array
  position as the deterministic fallback and tie-breaker.
- `parent_uuid` comes from `parent_message_uuid`. The provider's all-zero root sentinel and
  parents that did not produce a stored event become null, so the stored graph has no dangling
  export-only parent reference.
- When `current_leaf_message_uuid` is present, the parent chain to that leaf is the current
  path and all other retained messages have `is_sidechain = true`. Older flat exports without
  an active-leaf field keep every message on the main path. Alternate branches are preserved;
  selecting one remains a view concern.
- Timestamps fall back per message: `created_at`, then `updated_at`, then the conversation's
  `created_at` / `updated_at`, else zero — parsed as RFC 3339 and converted to microseconds.
- Visible text comes from supported structured blocks (`text`, `voice_note`, `tool_use`,
  `tool_result`); thinking and unknown internal blocks are not display text. A distinct
  top-level `text` value is appended after the blocks because agent turns can carry both tool
  activity and a separate final answer; an exact duplicate of a text block is emitted once.
  Text and serialized raw messages use the same unconditional secret masking as every source.
- Discovery recursively scans the independently configured claude.ai export root for files
  named exactly `conversations.json`; the separate root is necessary because ChatGPT uses the
  same filename for an incompatible JSON shape.

#### Configuration

Most configuration is environment-driven, read once into `MirrorConfig` when `warm()`
starts the service:

| Variable                         | Default                  |
| -------------------------------- | ------------------------ |
| `KHIVE_MIRROR_ENABLED`           | `false`                  |
| `KHIVE_MIRROR_PROJECTS_DIR`      | `$HOME/.claude/projects` |
| `KHIVE_MIRROR_CODEX_ENABLED`     | `false`                  |
| `KHIVE_MIRROR_CODEX_DIR`         | `$HOME/.codex/sessions`  |
| `KHIVE_MIRROR_CHATGPT_ENABLED`   | `false`                  |
| `KHIVE_MIRROR_CHATGPT_DIR`       | `$HOME/.chatgpt/exports` |
| `KHIVE_MIRROR_CLAUDE_AI_ENABLED` | `false`                  |
| `KHIVE_MIRROR_CLAUDE_AI_DIR`     | `$HOME/.claude/exports`  |
| `KHIVE_MIRROR_POLL_SECS`         | `2`                      |
| `KHIVE_MIRROR_BACKFILL`          | `true`                   |

`KHIVE_MIRROR_CHATGPT_MAX_BYTES` (default `268435456`, 256 MiB) is read separately, per
pass, by `mirror_chatgpt_export_file` itself rather than through `MirrorConfig`. It is a
hard ceiling on a `conversations.json` export's _whole_ file length (whole-file ingestion
has no incremental delta to bound the way the line-tail sources do): an export over the
ceiling is skipped for that pass — `tracing::warn!`-logged, never `read_to_string`'d — and
its cursor is left untouched, so it is retried (and re-warned) on every later tick rather
than silently dropped forever. A zero or non-numeric value falls back to the default.

`KHIVE_MIRROR_CLAUDE_AI_MAX_BYTES` applies the same 256 MiB default and fallback behavior
independently to `mirror_claude_ai_export_file`.

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

- Agent sessions are storable, retrievable by full UUID or short prefix,
  listable by `provider`, and exportable as JSON or Markdown — all through the
  ADR-083 `session.*` verb surface — without requiring any deployment outside
  this repository.
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
- [ADR-083](ADR-083-session-pack-t1-verbs.md) — current public session verb surface; supersedes §3
