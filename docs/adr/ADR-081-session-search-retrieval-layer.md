# ADR-081: Session Search and Retrieval Layer

**Status**: proposed
**Date**: 2026-06-30
**Authors**: Ocean, lambda:khive

## Context

### Session storage produces a corpus that has no content-search surface

ADR-080 establishes session storage as part of the OSS surface: the `khive-pack-session`
crate, the `session` note kind, and a background mirror that tails agent transcripts into
dedicated auxiliary tables. The mirror does not store transcripts as `kind=session` notes;
it writes per-session metadata to a `sessions` table and per-message rows to a
`session_messages` table (with a `session_mirror_cursor` table tracking byte offsets for
idempotent tailing). Two sources populate the same schema: Claude Code and Codex CLI
transcripts.

This corpus grows without bound across a user's working life. The generic note search,
`search(kind="session")`, reaches only `kind=session` _notes_ — it does not reach the
`session_messages` rows the mirror actually writes. There is today no way to ask the
corpus a content question.

### The content question is the commercial pillar

The session-continuity thesis is a content-recall capability: a user must be able to find
a past thread by what was said in it — "which session did I work through idea X" — across
all of their sessions and both CLIs, scoped by time, project, and agent. This is the
load-bearing user-facing value of session storage. Storage without retrieval is a
write-only log.

The query the layer must answer is therefore not "list my sessions" (ADR-080's
`session.list` already does that) but "find the session whose content matches this query,"
returning the thread and a locating snippet.

### khive already has the retrieval primitives this needs

khive ships a hybrid retrieval stack used for notes and entities: FTS5 trigram lexical
search (`khive-db`), dense ANN indexes (`khive-hnsw`, `khive-vamana`, sqlite-vec
compatibility), and reciprocal-rank fusion plus weighted, union, vector-only, and
keyword-only strategies (`khive-fusion`, `khive-retrieval`). The session search layer
should compose these primitives over the session tables rather than introduce a parallel
engine. Introducing a second search engine would violate the single-binary property and
add an operational surface.

### Resume is out of scope; this ADR is retrieval only

Replaying a stored session back into a live Claude Code or Codex process (resume) requires
deep, CLI-specific integration and is deferred. This ADR specifies the search and
retrieval surface only. It designs the contract; it does not, by itself, ship the
implementation.

## Decision

This ADR specifies a session search and retrieval surface over the mirror's storage
tables. It is design-only: the verbs, parameters, ranking contract, and phasing below are
the contract a subsequent implementation PR fulfills.

### 1. Retrieval granularity: message-level match, session rollup

The unit of indexing is the message (`session_messages` row). The unit of the default
answer is the session (the thread).

- `session.search` matches over message text and, by default (`rollup` mode), collapses
  hits to distinct sessions — each session carries its single best-matching snippet, the
  count of matching messages, and the session metadata. This directly answers "which
  thread."
- A `flat` mode returns raw message hits without rollup, for citation and excerpting (the
  caller wants the specific passages, not the containing session).

Rollup is the default because the primary use case is locating a thread; flat is the
opt-in for passage-level work.

### 2. Verb surface

Two new verbs, plus reuse of ADR-080's drill-down verbs.

#### `session.search` (Assertive)

Content search over the mirrored message corpus.

| Parameter    | Type                       | Required | Description                                                        |
| ------------ | -------------------------- | -------- | ------------------------------------------------------------------ |
| `query`      | string                     | yes      | Free-text query; lexical (M1) or hybrid (M2)                       |
| `mode`       | `"rollup"` \| `"flat"`     | no       | Default `"rollup"` (session rollup); `"flat"` returns message hits |
| `source`     | `"claude_code"`\|`"codex"` | no       | Filter by transcript source; omitted = all sources                 |
| `session_id` | UUID                       | no       | Scope the search to a single session (in-thread search)            |
| `agent_id`   | string                     | no       | Filter by `sessions.slug` / agent identifier                       |
| `cwd`        | string                     | no       | Filter by working directory recorded on the session                |
| `git_branch` | string                     | no       | Filter by git branch recorded on the session                       |
| `role`       | `"user"` \| `"assistant"`  | no       | Filter by message author                                           |
| `since`      | ISO datetime               | no       | Filter: message `created_at >= since`                              |
| `until`      | ISO datetime               | no       | Filter: message `created_at <= until`                              |
| `recency`    | float `0.0..=1.0`          | no       | Recency-boost weight; default `0.0` (pure relevance, no time bias) |
| `limit`      | integer                    | no       | Page size (default 20)                                             |
| `offset`     | integer                    | no       | Pagination offset                                                  |

All filters are AND-combined. Result entries carry, per match: `session_id`, `source`,
`snippet`, `score`, `role`, `created_at`, and the session's `cwd` / `git_branch` / `slug`.
In `rollup` mode each entry additionally carries `match_count`.

#### `session.thread` (Assertive)

Fetch the ordered message sequence of one session for context reconstruction.

| Parameter    | Type    | Required | Description                                  |
| ------------ | ------- | -------- | -------------------------------------------- |
| `session_id` | UUID    | yes      | The session to reconstruct                   |
| `limit`      | integer | no       | Max messages (default: full thread)          |
| `offset`     | integer | no       | Start offset within the ordered message list |

Returns messages ordered by `seq`, each with `role`, `text`, `msg_type`, `created_at`.
This is the read path a found session feeds into: search locates the thread, `session.thread`
materializes it. It is distinct from ADR-080's `session.get` (single-record fetch by note
UUID) and `session.list` (metadata listing).

### 3. Retrieval engine: tiered hybrid (FTS-first)

#### Tier 1 — lexical FTS (ships first)

A dedicated FTS5 virtual table over `session_messages.text`, kept in sync by triggers that
fire **only on the FTS-indexed columns** (per the established trigger-narrowing discipline:
broad triggers on non-indexed column updates caused WAL bloat and corruption elsewhere in
the codebase). Tier 1 is lexical-only: zero embedding cost, immediately useful, and
sufficient for keyword and phrase recall over the corpus. Tier 1 is the complete first
implementation.

#### Tier 2 — hybrid dense + lexical (deferred)

Per-message dense embeddings indexed in an ANN structure (sqlite-vec or `khive-vamana`,
matching the note/entity path), RRF-fused with the Tier-1 FTS ranking via `khive-fusion`.
Embedding population is **background and opt-in**, gated by configuration
(`KHIVE_SESSION_EMBED_ENABLED`, default off): embedding an unbounded message corpus is
costly, and the population must be batched, resumable, and idempotent. The `session.search`
contract is identical between tiers; Tier 2 changes ranking quality, not the verb surface.

This phasing mirrors ADR-080's M1/M2 split and the project's measure-before-optimizing
anti-pattern: ship lexical search, add the dense tier when retrieval quality over a large
corpus is the measured constraint.

### 4. Ranking contract

- With Tier 2 absent: FTS rank only.
- With Tier 2 present: RRF fusion of FTS and dense rankings (the established fusion default;
  weighted fusion is available as a tunable).
- Recency is a caller-supplied weight (`recency`, default `0.0` = no time bias), **not** a
  hardcoded boost. The use case spans "yesterday" to "three years ago"; baking in a recency
  prior would defeat long-horizon recall. This deliberately differs from `memory.recall`,
  whose decay-weighted ranking is correct for salience-calibrated memory but wrong for
  content-recall over an archival corpus.
- Ordering is deterministic: score descending, then `created_at` descending, then row id
  ascending as a stable tiebreak.

### 5. Access control and privacy

Session transcripts contain sensitive content. Three controls apply:

1. **Secrets are masked at mirror parse time** (the ingest path runs `mask_secrets` before
   any row is written). Search operates over already-masked text. This is defense in depth,
   not the access boundary.
2. **Namespace scoping.** `session.search` and `session.thread` default to
   `WHERE namespace='local'`; the only escape is an explicit `namespace=` parameter, exactly
   as every other multi-record verb behaves (ADR-007 Rev 6). The corpus is not cross-tenant
   (single namespace per ADR-080 M1).
3. **The Gate is the authorization seam** (ADR-018). Storage stores are ID-only; the search
   verbs are authorized at the Gate like every other verb, never by a post-fetch namespace
   check.

**Visibility recommendation.** The search verbs ship initially as `Visibility::Subhandler`
(operator / CLI surface, excluded from the agent MCP surface), matching the current mirror
verbs. Graduation to `Visibility::Verb` (agent-facing) is the eventual target, since the
user-facing value is agent-mediated context recall — but it is gated on an explicit
privacy and access-control review, because exposing whole-history transcript search to
every agent is a materially larger surface than per-call note search. The build PR makes
the graduation call; this ADR records the default-closed starting posture.

### 6. Source-agnostic unification

Because both the Claude Code and Codex mirrors write the same `session_messages` schema,
`session.search` is unified across both CLIs by construction, with `source` as an optional
narrowing filter. A single query spans a user's entire cross-CLI session history. This is a
differentiator, not an accident: the source-agnostic `ParsedEvent` storage layer is what
makes one search surface cover every transcript source the mirror supports.

### 7. Out of scope

- **Resume / replay** into a live CLI process (deferred; deep integration).
- **Summarization / digestion** of session content (ADR-080 places ingestion-derived
  structured output outside this repository's scope).
- **Cross-tenant / multi-namespace** search (single namespace per ADR-080 M1; multi-tenant
  is the channel/deploy concern tracked separately).
- **Curation / editing** of stored session content (sessions are an append-only mirror of
  external transcripts; the source of truth is the CLI's own files).

## Rationale

- **Why a dedicated surface over the mirror tables, not `search(kind="session")`.** The
  mirror writes messages to `session_messages`, not to the `notes` table; the generic note
  search cannot reach them. Storing every message as a note instead would bloat the `notes`
  table with millions of rows and pollute note/entity search — the dedicated-table design
  exists precisely to avoid that.
- **Why message-level indexing with session rollup.** The user's question is "which thread,"
  which requires matching at message granularity and rolling up to the session. Session-level
  indexing (e.g. concatenated transcript blobs) loses the locating snippet and the
  per-message filters (role, time within a session).
- **Why FTS-first.** Lexical search over the corpus costs nothing to populate and answers the
  majority of recall queries (keywords, names, phrases). Embedding an unbounded message
  corpus before measuring that lexical recall is insufficient would be premature
  optimization.
- **Why recency is a tunable, not a default boost.** Long-horizon recall ("three years ago")
  is a stated use case. A hardcoded recency prior optimizes for the recent-thread case at the
  expense of the archival case. Defaulting `recency=0.0` keeps ranking purely relevance-driven
  unless the caller asks otherwise.
- **Why default-closed visibility.** Whole-history transcript search is a larger privacy
  surface than per-call note retrieval. Shipping as a Subhandler and graduating after review
  is the conservative path; it costs nothing to start closed and open deliberately.
- **Why reuse `khive-fusion` / FTS5 / the ANN crates.** They are the same primitives that
  back note and entity search; reusing them keeps the single-binary property, avoids a second
  engine to operate, and inherits the existing tuning.

## Alternatives Considered

| Alternative                                           | Pros                               | Cons                                                                                  | Why rejected                                                                                         |
| ----------------------------------------------------- | ---------------------------------- | ------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Search only `kind=session` notes via the generic verb | No new verb; reuses note search    | Does not reach `session_messages`; coarse-grained; no per-message filters or snippets | The corpus the mirror writes lives in dedicated tables, not notes; the generic search misses it      |
| Store every message as a `kind=session` note          | Reuses note FTS/vector index       | Bloats the `notes` table with millions of rows; pollutes note/entity search           | Defeats the dedicated-table design that ADR-080's mirror established; SRP violation                  |
| Embed-everything from day one (no FTS-first tier)     | Best semantic recall immediately   | Embedding an unbounded corpus before measuring need; large storage + compute          | Premature optimization; FTS-first ships value at zero embedding cost; Tier 2 is the measured upgrade |
| Reuse `memory.recall` decay-weighted ranking          | Existing recall path               | Decay penalizes old sessions; conflates salience with content-recall                  | Session search is archival content-recall, not salience-weighted memory; recency must be opt-in      |
| External search engine (tantivy / meilisearch)        | Mature full-text features          | A second engine to deploy and operate; breaks the single-binary property              | khive's FTS5 + ANN + fusion stack is sufficient and keeps the single-binary design                   |
| Session-level (whole-transcript) indexing             | Simpler index; one row per session | Loses locating snippet, per-message role/time filters, and rollup match counts        | The "which thread, and where in it" question needs message granularity                               |

## Consequences

### Positive

- The session corpus gains a content-search surface: "which thread discussed X," scoped by
  time, project, agent, source, and role, with a locating snippet.
- Search is unified across Claude Code and Codex transcripts by construction, with `source`
  as an optional filter.
- Reuses the existing FTS5 / ANN / fusion primitives — no second engine, single binary
  preserved.
- Tier 1 (FTS) ships immediately at zero embedding cost; Tier 2 (hybrid) is a measured
  upgrade with no verb-surface change.
- The session-continuity commercial pillar gets the query surface that storage alone could
  not provide.

### Negative

- The Tier-1 FTS virtual table and its sync triggers add write overhead on mirror ingest.
  Mitigation: triggers narrowed to FTS-indexed columns only (the WAL-bloat discipline).
- Tier 2 adds embedding storage and background compute when enabled; gated off by default.

### Neutral

- No change to KG/note search, the GTD or memory packs, or the retrieval crate internals;
  the session search layer is a new consumer of existing primitives.
- Resume/replay remains deferred; this ADR does not affect it.
- Multi-tenant session search remains deferred to the deploy/channel layer.

## References

- [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md) — Session Pack OSS Storage Mechanism; the `sessions` / `session_messages` storage this layer searches over
- [ADR-013](ADR-013-note-kind-taxonomy.md) — Note Kind Taxonomy; the `session` note kind
- [ADR-007](ADR-007-namespace.md) — Namespace as attribution; multi-record default `WHERE namespace='local'`
- [ADR-018](ADR-018-authorization-gate.md) — Authorization Gate seam
- [ADR-021](ADR-021-memory-pack.md) — Memory Pack; contrast for decay-weighted recall vs. content recall
