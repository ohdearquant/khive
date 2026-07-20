# ADR-088 Amendment 1: `git.digest` — Agent-Facing Digest Verb with Remote-URL Support

**Status**: Accepted\
**Date**: 2026-07-09\
**Authors**: khive maintainers\
**Amends**: [ADR-088](ADR-088-git-lifecycle-pack.md) (Git-Lifecycle Pack)\
**Related**: ADR-016 (Request DSL), ADR-017 (Pack Standard), ADR-023 (Pack Verb Surface)

## Context

ADR-088 §5 deliberately shipped the git pack verb-less: population happens through the
admin CLI ingester (`kkernel git-ingest`), and the ADR states "No new agent-facing verbs."
Operational experience (2026-07-09) showed the consequence: the ingester existed for weeks
and was never run, because agents — the system's primary operators — had no surface for it.
The directive is to make digestion agent-facing, and to accept a remote URL directly so an
agent can point the verb at any repository and ingest its history without a pre-existing
local clone.

This amendment supersedes ADR-088's "no new agent-facing verbs" clause for exactly one
verb. Note kinds, edge usage, cursor semantics, secret masking, and the `gh` access path
are unchanged.

## Decision

Add one verb to the git pack:

```
git.digest(source, project?, max_items?, include?)
```

- `source` (required, string) — either an absolute local path to a git repository, or an
  `https://` git URL (e.g. `https://github.com/org/repo`). Any https host is accepted;
  hosts other than github.com degrade to commits-only ingestion with an explicit warning
  (issue/PR ingestion requires `gh`, mirroring ADR-088's gh-unavailable degradation
  semantics). SSH URLs are rejected in v1 (no interactive auth in the daemon).
- `project` (optional, string) — UUID or 8+ hex prefix of the repo-anchor `project`
  entity. When absent: the handler searches for a `project` entity whose
  `properties.repo_url` (or name derived from the URL/path basename) matches; if none is
  found it CREATES the anchor entity (`kind=project`, `name=<repo basename>`,
  `properties.repo_url=<canonical url>`), returning its id in the report. Auto-creation is
  reported, never silent.
  > **Amended by [Amendment 2](ADR-088-amendment-2-anchor-identity.md)** (issue #1173):
  > anchor resolution is now slug-first on `properties.repo_slug` with exact legacy
  > `repo_url` fallback and backfill; the basename fallback is removed, and an
  > orphaned-corpus signal is added to the report.
- `max_items` (optional, int, default 500, clamp 1..2000) — bounded work per call, counted
  across commits + issues + PRs. The existing per-repo cursors (ADR-088 §5) make the verb
  resumable: each call ingests up to `max_items` and returns `done: false` with cursor
  state when more remains. Agents loop until `done: true`. This keeps the verb inside MCP
  call-latency envelopes instead of blocking minutes on a large repo.
- `include` (optional, array of `commits|issues|pull_requests`, default all three).

Return shape: the existing `IngestReport` (counts, skips, warnings, `gh_available`)
extended with `done: bool`, `project_id`, `project_created: bool`, and
`commit_embeddings_truncated: u64` (count of commits whose vector-embedding input was
capped this pass; see "Commit embedding truncation" below).

### Remote-URL mode

1. Clone to a daemon-owned scratch directory (`~/.khive/scratch/git-digest/<hash>/`),
   `git clone --filter=blob:none` (history + trees without file blobs — commit walking
   needs messages and file lists, not contents; `git log --name-only` works against a
   partial clone with lazy fetch disabled for our read pattern).
2. Derive the `owner/repo` slug from the URL for `gh`-based issue/PR ingestion (unchanged
   ADR-088 Open Question 3 resolution: shell `gh`; skip with warning when unavailable).
3. The clone is cached keyed by canonical URL: subsequent digest calls `git fetch` instead
   of re-cloning. An LRU cap (default 5 repos / 2GB, config `[git] digest_cache_*`) evicts
   oldest; eviction is safe because cursors live in the database, not the clone.
   Additionally, a per-clone size cap (operator-configurable, default 1GB) bounds any
   single clone: if a clone or fetch would exceed it, the operation aborts with a clear
   error before writing further — `max_items` bounds ingestion work, but only this cap
   bounds disk consumption by a single large-history repository before LRU eviction can
   apply.
4. Cleanup on eviction uses directory removal of the scratch path only (never touches
   user-owned paths).

### Security posture

- `git clone` of an untrusted remote does not execute repository-supplied code (no hooks
  run on clone/fetch). The handler additionally sets `GIT_TERMINAL_PROMPT=0` and
  `core.hooksPath=/dev/null` on the scratch clone as defense in depth.
- Local-path mode requires an absolute path; relative paths are rejected. The path must
  contain a `.git` directory; arbitrary directory walking is not performed.
- Secret masking is unchanged: ingested text goes through the same `create`-verb gate
  (ADR-088 acceptance note 5). Blocked writes surface as report warnings, fail-closed.
- Namespace/attribution: writes stamp the caller's token namespace exactly as the CLI
  ingester does today; no new authorization surface. The Gate (ADR-018) remains the
  authorization seam for callers who should not write.

### Surface-contract touch points

- ADR-023 (pack verb surface): git pack's verb table gains one row; `verbs()` output and
  the khive-mcp tool description regenerate (CLAUDE.md guidance: re-run
  `request(ops="verbs()")` before editing the count line).
- ADR-015 product-verb table is NOT amended: `git.digest` is a pack-prefixed verb
  (`pack.verb` convention, ADR-023), not one of the 15 flat product verbs.
- `kkernel git-ingest` remains as the admin path (shared implementation; the verb handler
  and CLI both call `khive_pack_git::ingest::run_ingest` with the same options struct,
  extended with the bounded `max_items` + remote-source support).

## Ingest enrichment (consumer-evidence riders, 2026-07-09)

First-consumer evidence (an agent running 14 live GQL operations against a freshly
ingested multi-repository corpus) showed the corpus is a flat property store: the only
edges the v0 ingester creates are note→project `annotates` and merge-commit→PR. The three
cross-references consumers actually want — PR-to-issue trails, commits-touching-an-issue,
fix chains — are impossible by traversal because `Closes #N` / `#M` references exist only
as unextracted body text, and issues are isolated leaves. Two riders, both ingest-side and
in scope for this amendment:

1. **Reference-edge extraction.** At ingest, parse commit messages and PR/issue bodies for
   GitHub reference grammar (`Closes/Fixes/Resolves #N`, bare `#N` mentions) and
   materialize edges: closing references as `annotates` from the closing commit/PR note to
   the issue note (with `properties.ref_kind = "closes" | "mentions"` on the edge's
   annotating metadata), and commit `parents[]` as `precedes` between commit notes
   (parent precedes child; both endpoints are same-substrate notes, legal per supersedes/
   precedes note rules — verify against `EDGE_RULES` at impl time and fall back to
   `annotates` + `ref_kind="parent"` if `precedes` n→n is not in the base contract).
   Cross-repo `#N` collisions resolve within the same `project_id` only; unresolved
   references (issue not ingested) are skipped and counted in the report. Extraction is
   fail-open: a malformed or unresolvable reference never fails an ingest batch — it is
   skipped with a warning in the report. Edge extraction runs at ingest only: this
   amendment adds no retroactive backfill verb. Re-digesting an already-cursored
   repository picks up edges for new items; a one-shot backfill over already-ingested
   notes is an admin pass (`kkernel`), out of scope here.
2. **Readable names.** Provenance notes currently carry `name=null`, so neighbors/GQL
   render placeholders and force a `get()` per hop. Set `name` at ingest: issues/PRs
   `"#<number> <title>"` (truncated), commits `"<short-sha> <subject>"` (truncated).

Data-fidelity checks from the same evidence run were verified clean: `closed_at` values
that cluster at one instant reflect real GitHub bulk-close events (confirmed against
`gh issue view`), and `author` is the genuine GitHub login (commit author names come from
git identity, a different identity system — both correct).

## Commit embedding truncation (issue #764, 2026-07-10)

Commit note content (subject plus body, after secret masking) has no upper bound, but
vector embedders do. When a commit's content exceeds 32,768 bytes, the ingester computes a
UTF-8-boundary-safe head prefix at that cap and passes it as the `create` verb's
`embedding_content` parameter. Only the vector-embedding input is capped this way: the
full, untruncated commit content is always stored and FTS-indexed unchanged. Each commit
whose embedding input was truncated increments `commit_embeddings_truncated` in the
returned `IngestReport`; the field is `0` for a pass with no over-cap commits.

This reuses the pack-kg `create` verb's existing `embedding_content` parameter (a
non-empty proper prefix of `content`, subject to the same secret-gate check as any other
stored text) rather than adding pack-git-local truncation logic.

## Consequences

- One-call adoption for agents: "digest this repo" becomes a verb loop instead of an admin
  task nobody runs. Periodic re-digest can be scheduled via `schedule.schedule(action=
  "git.digest(source=...)")` — composing two existing packs with zero new machinery.
- The bounded-call contract adds cursor-state plumbing to the report but removes the
  worst failure mode (an MCP call blocked for minutes on a 10k-commit clone).
- Scratch-clone cache is new daemon-owned disk state; sized and evictable, documented in
  the operator guide.

## Alternatives rejected

- **Fire-and-forget background job + status verb**: heavier (job table, lifecycle,
  another verb); the cursor design already gives resumability with strictly simpler
  semantics. Revisit only if per-call latency at max_items=500 proves unacceptable.
- **Full clone in URL mode**: blob download dominates clone time and disk for zero
  ingestion value; `--filter=blob:none` keeps everything the ingester reads.
- **Direct GitHub REST instead of `gh`**: re-opens ADR-088 Open Question 3 for no gain;
  `gh` handles auth and pagination and is already the accepted path.

## Spec-gate rulings (2026-07-09)

1. No host allowlist. Any https git host is accepted; non-github hosts degrade to
   commits-only with an explicit warning (mirrors the gh-unavailable degradation
   semantics). SSH remains hard-rejected in v1.
2. `max_items` default 500 confirmed (measured ~10s per call on repositories of 448-991
   items).
3. A per-clone size cap on the scratch cache is required in addition to the LRU cap, so a
   single large-history repository cannot exhaust daemon disk before eviction applies.
