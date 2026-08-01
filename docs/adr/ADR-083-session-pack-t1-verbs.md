# ADR-083: Session Pack T1 Verb Surface

**Status**: accepted
**Date**: 2026-07-02
**Authors**: khive maintainers
**GitHub**: #342
**Supersedes**: [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md) §3
(verb visibility, verb count, names, export behaviour, and parameter
vocabulary) for the T1 continuity epic. ADR-080 §1, §2, §4, §5, and §6 remain
in force; in particular, the session mirror is unaffected and continues to run.
**Note**: ADR-081 is assigned to the recall-retune driver (PR #400, branch `adr-081-retune-driver`).
ADR-082 is the retrieval quality measurement loop. This document takes the next free number, 083,
for numbering reasons only; it has no topical relationship to either.

## Context

Epic `ohdearquant/khive#342` asks for the first working slice of the khive T1 session pack: an
agent-facing surface to store, list, resume, and export session records through the local backend,
so an agent can pick up a prior session instead of starting cold. The 2026-07-02 rescope narrows
this to storage and retrieval only. The digester and summarization pipeline, storage tiering,
and other account-level concerns stay out of scope (ADR-080 Context).

ADR-080 already decided that sessions belong in the OSS pack surface, modeled as `kind="session"`
notes via `Pack::NOTE_KINDS` (ADR-080 §1 and §2), and that decision is shipped on `main`. ADR-080
§3, accepted and amended 2026-07-02, also records the verb surface that shipped alongside it: three
handlers (`session.store`, `session.list`, `session.get`), all `Visibility::Subhandler`, with
parameters `agent_id`, `metadata`, and `since`, and no dispatchable `session.export` handler.

The T1 implementation exposes a different verb surface: four handlers,
all `Visibility::Verb`, with `session.get` renamed to `session.resume`,
`session.export` promoted to a dispatchable handler, and the parameter
vocabulary changed to `provider` and `provider_session_id`. This accepted ADR
records that deliberate, in-scope redesign and supersedes ADR-080 §3 rather
than treating the change as routine drift.

An earlier version of this document was drafted directly on the `session-pack-t1` branch, under a
different number, against a copy of ADR-080 that predated the 2026-07-02 amendment. That draft
stated in four places that the session mirror was being removed. It was not. This document
supersedes that draft: it corrects the mirror claims, states the ADR-080 §3 reversal explicitly, and
fixes a code citation (§4).

## Decision

### Amendment 1 (2026-08-01): complete the list filter contract

Issue #1493 establishes the concrete query need anticipated below: callers
must be able to page a sparse subset without first loading an unbounded result
set. `session.list` therefore adds two optional, server-side filters:
`agent_id`, an exact match on legacy `properties.agent_id`, and `since`, an
inclusive RFC 3339 lower bound on `notes.created_at`. Both feed the existing
`query_notes_filtered` storage request before `offset` and `limit` are applied.
This amendment does not restore `agent_id` or open-ended `metadata` to
`session.store`; it only makes existing property-bearing session notes
queryable. No response shape or other verb changes.

### 1. ADR-080 §3's shipped-surface record is superseded for T1

This accepted ADR supersedes ADR-080 §3's intervening shipped-surface record
(three `Visibility::Subhandler` handlers, `agent_id`/`metadata`/`since`
parameters, and no dispatchable export) with the surface in §2 below. ADR-080
§1, §2, §4, §5, and §6 remain authoritative and unchanged.

No new schema is required for T1's own storage. The four verbs read and write the existing `notes`
table through `runtime.core().create_note` and `runtime.core().notes(token)?.query_notes_filtered(...)`
(§4). No new migration is added, and V1 is not edited. Separately, the pack's `SCHEMA_PLAN` is
declared as `Some(...)` today, but that schema belongs to the session mirror (three auxiliary tables,
ADR-080 §6), not to T1. T1 introduces no auxiliary tables of its own.

Alternative considered: add a new versioned migration and a normalized session table for T1.
Rejected. The existing note substrate already supports the required fields and keeps sessions inside
search, recall, and graph-adjacent workflows. `crates/khive-db/sql/` currently ends at
`005-unique-comm-external-id.sql`, so a future performance-driven index would be a new
`006-*.sql` file with a new `VersionedMigration { version: 6, ... }`, never an edit to V1.

Alternative considered: reuse the session mirror's tables for T1 storage. Rejected. T1 records are
caller-authored; the mirror's tables are populated by passive ingestion. Conflating the two schemas
would blur that distinction. The mirror itself is unaffected by this decision and continues to run
via `warm()` (§5).

### 2. Four public verbs

All four handlers register with `visibility: Visibility::Verb` in `SESSION_HANDLERS`
(`crates/khive-pack-session/src/vocab.rs`). Speech-act categories follow ADR-025: `session.store` is
a `Directive`; `session.list`, `session.resume`, and `session.export` are `Assertive`.

#### `session.store`

| Parameter             | Type     | Required | Description                                                      |
| --------------------- | -------- | -------- | ---------------------------------------------------------------- |
| `content`             | string   | yes      | Verbatim transcript or summary content.                          |
| `title`               | string   | no       | Human-readable session title, stored as `notes.name`.            |
| `provider`            | string   | no       | Provider label, for example `codex`, `claude_code`, or `openai`. |
| `provider_session_id` | string   | no       | Provider-native continuity anchor.                               |
| `tags`                | string[] | no       | Caller labels stored in `properties.tags`.                       |

Errors: empty `content` is rejected (`session.store: content must not be empty`). A present-but-empty
`title`, `provider`, or `provider_session_id` is rejected (`session.store: {field} must be a
non-empty string when provided`). Any empty-string entry in `tags` is rejected (`session.store: tags
entries must be non-empty strings`).

Return shape:

```json
{
  "ok": true,
  "session": {
    "id": "...",
    "kind": "session",
    "title": null,
    "provider": null,
    "provider_session_id": null,
    "tags": [],
    "content": "...",
    "properties": {},
    "created_at": "...",
    "updated_at": "...",
    "namespace": "..."
  }
}
```

#### `session.list`

| Parameter  | Type    | Required | Description                                           |
| ---------- | ------- | -------- | ----------------------------------------------------- |
| `limit`    | integer | no       | Page size, 1 through 200, default 20.                 |
| `offset`   | integer | no       | Pagination offset, default 0.                         |
| `provider` | string  | no       | Exact filter on `properties.provider`.                |
| `agent_id` | string  | no       | Exact filter on legacy `properties.agent_id`.         |
| `since`    | string  | no       | Inclusive RFC 3339 lower bound on `notes.created_at`. |

List summaries omit `content`; `SessionSummary` carries no `content` field.

Errors: a present-but-empty `provider` or `agent_id` is rejected. An invalid
`since` is rejected with the required RFC 3339 format. A `limit` outside
`1..=200` is rejected (`session.list: limit must be in 1..=200; valid values: integers 1 through
200; got {limit}`).

Return shape:

```json
{ "ok": true, "sessions": [], "count": 0, "total": 0, "limit": 20, "offset": 0 }
```

#### `session.resume`

| Parameter | Type   | Required | Description                       |
| --------- | ------ | -------- | --------------------------------- |
| `id`      | string | yes      | Full UUID or 8+ hex short prefix. |

Fetches one session's full content. Unlike `session.list`, the return includes `content`.

Errors: an `id` that is neither a full UUID nor an 8+ character hex string is rejected
(`session.resume: id must be a full UUID or 8+ hex prefix; valid values: full UUID or 8+ hex prefix;
got {id}`). A hex prefix matching no record is rejected (`session.resume: id prefix {id} matched no
records; valid values: full UUID or 8+ hex prefix`). An id resolving to a non-`session` note is
rejected (`session.resume: expected note kind "session"; valid note kind: session; got {kind}`). An
id resolving to a non-note record is rejected (`session.resume: id must resolve to a session note;
valid substrate: note kind session`). An id that resolves to nothing returns not-found (`session not
found: {id}`).

Return shape:

```json
{
  "ok": true,
  "session": {
    "id": "...",
    "kind": "session",
    "content": "...",
    "properties": {},
    "created_at": "...",
    "updated_at": "...",
    "namespace": "..."
  }
}
```

#### `session.export`

| Parameter | Type   | Required | Description                           |
| --------- | ------ | -------- | ------------------------------------- |
| `id`      | string | yes      | Full UUID or 8+ hex short prefix.     |
| `format`  | string | no       | `json` or `markdown`, default `json`. |

Uses the same id resolution and error contract as `session.resume`. An invalid `format` is rejected
before resolution (`session.export: format must be one of ["json", "markdown"]; got {format}`).

Return shapes:

```json
{ "ok": true, "format": "json", "session": { "id": "...", "kind": "session", "content": "..." } }
```

```json
{ "ok": true, "format": "markdown", "content": "# ...\n\n## Content\n\n..." }
```

### 3. `provider_session_id` as the continuity anchor

`provider_session_id` is the provider-native external session, conversation, or thread identifier.
It is not the khive note UUID, not a billing account, and not a message or event ID. The strongest
grouping key is `(provider, provider_session_id)` when both are present. T1 does not enforce
uniqueness on this pair: a caller may legitimately store more than one record against the same
provider session, such as a raw transcript and a separate summary.

Naming note: the session mirror's `sessions` table (ADR-080 §6) already has a column named
`provider_session_id`, and a `source` column playing a role similar to this section's `provider`.
The two vocabularies are adjacent but not identical (`source` versus `provider`), and they serve
different subsystems. The mirror's columns hold passively ingested transcript metadata; T1's
`properties.provider` and `properties.provider_session_id` hold caller-authored record metadata.
They are related but not interchangeable, and this ADR does not unify them.

### 4. Backend seam

| Verb             | Call                                                                                                                   |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `session.store`  | `runtime.core().create_note(token, "session", title, &content, None, Some(properties), vec![])`                        |
| `session.list`   | `runtime.core().notes(token)?.query_notes_filtered(namespace, &filter, PageRequest { offset, limit })`                 |
| `session.resume` | `runtime.core().resolve_prefix(token, raw)` (hex-prefix case only), then `runtime.core().resolve_primary(token, uuid)` |
| `session.export` | Same resolution as `session.resume`, then serializes the resolved note to the requested format                         |

All four handlers route through `runtime.core()`, the ADR-073 accessor.

History: as originally shipped, `session.resume` and `session.export` called `resolve_prefix` and
`resolve_primary` directly on `runtime` — harmless in single-backend M1 (where `core()` is a
`self.clone()` no-op), and flagged in an earlier revision of this section as a latent inconsistency
for any future per-pack backend assignment. That divergence became observable the first time the
pack ran with a `[packs.session]` backend assignment: `store`/`list` wrote and read session notes in
the main shared graph while `resume`/`export` resolved against the pack backend and reported every
stored session as not found. Both handlers now resolve through `core()`, with a two-backend regression test in
`crates/khive-pack-session/src/handlers/resume.rs`.

No handler holds a direct reference to `Arc<StorageBackend>` or any `khive-db` type, preserving the
ADR-071 `BackendHandle` seam per ADR-080 §5.

### 5. The session mirror is unaffected

ADR-080 §6 documents the session mirror: a read-only background poll loop, spawned from
`PackRuntime::warm()`, that ingests Claude Code, Codex CLI, ChatGPT-export, and claude.ai-export
transcripts into three auxiliary tables (`sessions`, `session_messages`,
`session_mirror_cursor`) declared through the pack's `SCHEMA_PLAN`
(`SESSION_SCHEMA_PLAN_STMTS`, the ADR-028 mechanism). It shipped as the pack's M2 milestone
(issue #350, PR #368), gained the Codex CLI source in PR #375, and is disabled by default.

The T1 verb surface and the session mirror are additive, not a replacement. This is a deliberate
design ruling. T1's four verbs read and write `kind=session` notes in the shared `notes` table.
The mirror reads transcript files on disk and writes its own three auxiliary tables. Neither
subsystem's schema, code, or tests depend on the other, and both run in the same crate and the same
pack instance at the same time.

This ADR does not remove, disable, deprecate, or defer any part of the mirror,
its transcript parsers, or its session-message tables. All of that is shipped
today, per ADR-080 §6, independent of this ADR's verb-visibility change.
`src/mirror/` and `SESSION_SCHEMA_PLAN_STMTS` are unchanged by this ADR. T1
adds verb handlers alongside them, not in place of them.

### 6. Deferred

Out of scope for this ADR, consistent with ADR-080's Context: account-level concerns —
including digestion and summarization pipelines and storage tiering — are out of scope for
this pack.

T1-specific deferrals:

- Uniqueness or upsert semantics for `provider_session_id`. Duplicate anchors are possible by design
  (§3).
- `session.import` as a caller-driven verb. Ingestion remains non-caller-driven. It ships as the
  mirror (ADR-080 §6), not as a T1 verb.

Not deferred, and not something this ADR changes: transcript parsing, the mirror's ingestion
service, and the `session_messages` table. These are shipped today (§5).

## Rationale

- **Why revise ADR-080 §3 now.** T1's stated purpose is agent session continuity: an agent resuming
  or exporting its own prior session. That requires the verbs to be callable from the agent-facing
  MCP `request` surface, not only through operator tooling such as `kkernel exec`.
  `Visibility::Subhandler` was the right choice while the session-continuity query UX was undecided
  (ADR-080 §3 amendment). This ADR treats that question as settled for T1's scope by fixing the
  four-verb, fixed-parameter contract in §2.
- **Why rename `session.get` to `session.resume`.** `resume` names the continuity use case directly:
  fetching a session in order to continue it. `get` is a generic accessor name shared with unrelated
  substrate operations.
- **Why promote `session.export` to a dispatchable verb.** Serialization was previously an in-process
  helper with no `HandlerDef`. Making it dispatchable lets an agent request a specific export format
  directly, matching the other three verbs' agent-facing status.
- **Why `provider` and `provider_session_id` over `agent_id`, `metadata`, and `since`.** `agent_id`
  conflates the calling agent's identity with the external provider's session identity.
  `provider` and `provider_session_id` name the actual continuity anchor T1 needs: which external
  system, and which session in that system. `metadata` (an open-ended object) and `since` (a
  list-time filter) are dropped from the T1 parameter surface; filtering by time can be added later
  without a breaking change if a real query need appears. Amendment 1 records that later query need
  and restores `agent_id` and `since` only as additive `session.list` filters; the T1 store contract
  remains unchanged.
- **Why not fold this into ADR-080.** ADR-080 already carries three same-day amendments (Context,
  §3, §6). A verb-visibility, verb-count, and parameter-vocabulary change is a public-contract
  decision, not a storage-mechanism refinement, which is what ADR-080 is about. A separate ADR that
  explicitly revises one section is easier to review and sign off than a fourth layered amendment.

## Alternatives Considered

Alternatives to the underlying storage design (note-kind versus observation, standalone store, or
memory-pack modeling) are already recorded in ADR-080's Alternatives Considered table and are not
repeated here. Alternatives specific to this ADR's verb-surface decision:

| Alternative                                                                                                 | Rejected because                                                                                                                                                                                                                                          |
| ----------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Keep `Visibility::Subhandler` and design the session-continuity query UX before exposing any verb to agents | T1's epic scope is agent-facing continuity; waiting blocks the working slice epic #342 asks for. The UX question is about how an agent discovers and queries sessions, not whether `store`, `list`, `resume`, and `export` themselves should be callable. |
| Keep `agent_id`, `metadata`, and `since`, and add `provider` and `provider_session_id` alongside them       | Carrying both vocabularies forward increases the parameter surface without a clear present need for `metadata`'s open-endedness or `since`'s filtering. T1 scopes the surface to what it actually uses and can extend later if a real need appears.       |
| A new versioned migration and normalized session table for T1                                               | See §1. The existing note substrate is sufficient; a future `006-*.sql` file remains available if a measured bottleneck justifies it.                                                                                                                     |
| Reuse the session mirror's tables for T1 storage                                                            | See §1. T1 records are caller-authored; the mirror's rows are passively ingested. Conflating the schemas would blur that distinction.                                                                                                                     |

## Consequences

### Positive

- T1's four verbs are small, testable, and consistent with the existing pack architecture
  (`Pack::NOTE_KINDS`, `HandlerDef`, `PackRuntime`).
- Sessions remain first-class notes and participate in existing search, recall, namespace, and
  graph-adjacent flows.
- No new schema or migration is needed for T1's own storage (§1).
- The agent-facing surface is stable and explicit: four verbs, fixed parameters, a documented error
  contract.
- `provider_session_id` gives callers a continuity anchor without forcing every caller-authored
  record into a normalized session-message schema (§3).

### Negative

- Filtering by `provider` uses JSON property filtering (`$.provider`) rather than a dedicated SQL
  column; this does not scale as well as an indexed column at large corpus sizes.
- T1 cannot efficiently query by `provider_session_id` at very large scale without a later index or
  table decision.
- Duplicate provider-session anchors are possible by design (§3); no uniqueness is enforced.

### Neutral

- T1 and the session mirror (ADR-080 §6) are independent subsystems that read and write different
  tables and coexist in the same crate and pack instance by design (§5). This
  ADR does not change, disable, or deprecate the mirror.
- No change to `khive-vamana`, `khive-db`, `khive-storage`, or `khive-runtime`. The session pack
  remains a pure consumer of the existing runtime API.
- This ADR supersedes only ADR-080 §3. ADR-080 §1, §2, §4, §5, and §6 are unaffected.

## Validation

The implementation's integration suite (`crates/khive-pack-session/tests/integration.rs`) covers, by scenario:
store-to-resume content equality, list visibility (summaries omit `content`), export as JSON, export
as markdown, UUID short-prefix resolution, provider filtering, invalid-`format` errors listing valid
values, invalid-`limit` range errors, and rejection of a non-session UUID. Amendment 1 adds
server-side `agent_id` and `since` filtering, an exact offset window, and explicit resume/export
regressions for soft-deleted sessions.

## References

- [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md): Session Pack, OSS Storage Mechanism.
  This ADR supersedes ADR-080 §3 only. ADR-080 §1, §2, §4, §5, and §6 remain authoritative and
  unchanged.
- [ADR-013](ADR-013-note-kind-taxonomy.md): Note Kind Taxonomy. `session` is a pack-registered kind
  under the mechanism this ADR describes.
- [ADR-017](ADR-017-pack-standard.md): Pack Standard. `Pack` and `PackRuntime` traits, `HANDLERS`.
- [ADR-025](ADR-025-verb-speech-acts.md): Verb Surface as Speech-Act Taxonomy. `Directive` and
  `Assertive` classification used in §2.
- [ADR-027](ADR-027-dynamic-pack-loading.md): Dynamic Pack Loading via Self-Registration.
- [ADR-028](ADR-028-pack-scoped-backends.md): Pack-Scoped Backends and Per-Pack Schema Declaration,
  the mechanism behind the mirror's `SCHEMA_PLAN` (§5).
- [ADR-071](ADR-071-backend-pluggable-runtime.md): Backend-Pluggable Runtime. The `BackendHandle`
  seam discussed in §4.
- [ADR-073](ADR-073-pack-core-backend-accessor.md): Pack Core-Backend Accessor. The `core()` call
  discussed in §4.

ADR-081 and ADR-082 are unrelated to this document's content; see the header note for why this ADR
is numbered 083.
