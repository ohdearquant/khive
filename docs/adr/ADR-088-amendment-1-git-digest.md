# ADR-088 Amendment 1: `git.digest` — Agent-Facing Digest Verb with Remote-URL Support

**Status**: Accepted\
**Date**: 2026-07-09\
**Authors**: khive maintainers\
**Amends**: [ADR-088](ADR-088-git-lifecycle-pack.md) (Git-Lifecycle Pack)\
**Amended by**: proposed [ADR-088 Amendment 2](ADR-088-amendment-2-anchor-identity.md),
which narrows canonical repo-anchor identity\
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
remain unchanged except where the accepted operational rider below narrows GitHub
capability detection and successful-response durability.

## Decision

Add one verb to the git pack:

```
git.digest(source, project?, max_items?, include?)
```

- `source` (required, string) — either an absolute local path to a git repository, or an
  `https://` git URL (e.g. `https://github.com/org/repo`). Any https host is accepted;
  issue/PR ingestion proceeds only when the post-clone source-bound `gh` probe
  resolves a matching github.com repository. Otherwise, each requested remote source is
  structured as `skipped` and a safe warning is returned while commits still ingest.
  SSH URLs are rejected in v1 (no interactive auth in the daemon).
- `project` (optional, string) — UUID or 8+ hex prefix of the repo-anchor `project`
  entity. When absent: the handler searches for a `project` entity whose
  `properties.repo_url` (or name derived from the URL/path basename) matches; if none is
  found it CREATES the anchor entity (`kind=project`, `name=<repo basename>`,
  `properties.repo_url=<canonical url>`), returning its id in the report. Auto-creation is
  reported, never silent.
  > **Amended by [Amendment 2](ADR-088-amendment-2-anchor-identity.md)** (issue #1173):
  > anchor resolution is now slug-first on `properties.repo_slug` with a legacy
  > `repo_url` fallback (exact, then normalized) and backfill; the basename
  > fallback is removed, and an orphaned-corpus signal is added to the report.
- `max_items` (optional, int, default 500, clamp 1..2000) — bounded work per call, counted
  across commits + issues + PRs. The existing per-repo cursors (ADR-088 §5) make the verb
  resumable: each call ingests up to `max_items` and returns `done: false` with cursor
  state when more remains. Agents loop until `done: true`. The bound limits write volume,
  not wall-clock duration; the durable-receipt rider below covers transport response loss.
- `include` (optional, array of `commits|issues|pull_requests`, default all three).

Return shape: the existing `IngestReport` (counts, skips, warnings, `gh_available`)
extended with `done: bool`, `project_id`, `project_created: bool`, and
`commit_embeddings_truncated: u64` (count of commits whose vector-embedding input was
capped this pass; see "Commit embedding truncation" below). It also includes
`writes_refused: u64` and `write_refusals: [...]`: a per-call secret-gate refusal count
and one safe structured diagnostic per refused record write. Each diagnostic names the
attempted verb, record kind, trusted natural key, detector, and masked excerpt; it never
copies the rejected content. Per-record refusal remains non-fatal so the pass can surface
all affected records and ingest clean siblings, but a caller requiring a clean run must
assert `writes_refused == 0` as well as loop until `done == true`. Successful runtime verb
dispatch also adds the durable `receipt_id` defined below.

### Accepted operational rider: truthful GitHub capability and durable receipts (2026-08-07)

Three production gaps (#1510, #1617, #1647) narrow the contract as follows:

The response-loss bound is client-side, not a `git.digest` or daemon deadline. The
khive stdio/daemon [`try_forward_inner`](../../crates/khive-mcp/src/daemon.rs) path writes
the request and then awaits the response frame without a per-dispatch timeout; its short
timeouts are limited to boot/recovery probes. The observed bound is the MCP client's
300,000 ms default, also recorded in the repository's
[performance metadata](../../scripts/perf/flagship_workloads.toml). A large pass can
therefore keep running and commit after that client stops waiting. This amendment does not
couple `max_items` to one transport's timeout; it makes the completed report recoverable.

1. **Repository usability, not binary presence.** When issues or pull requests are
   requested, the ingest core derives the expected GitHub `owner/repo` from the canonical
   remote source or the local checkout's configured `origin`, then runs
   `gh repo view <owner/repo> --json nameWithOwner,url`. Argument-less `gh` repository
   selection is forbidden: an alternate remote selected by `gh repo set-default` is not
   the digest source. `gh_available: true` means that explicitly targeted operation
   succeeded and returned the same `owner/repo` with a matching github.com URL; the value
   is passed explicitly to every subsequent `gh pr list --repo <owner/repo>` and
   `gh issue list --repo <owner/repo>` call.
   Installed-but-unauthenticated `gh`, local-only checkouts, and non-GitHub remotes report
   `gh_available: false`. Each requested remote source is `skipped` with a stable reason,
   so it cannot disappear as though it were unrequested and cannot contribute a false
   `history_exhausted: true`. `gh_available` remains `null` when neither remote source was
   requested. Probe/list stderr and origin URLs are not copied into reports.
2. **Every returned success has a durable receipt.** After the handler produces its
   complete report, the runtime allocates a schema-v2 `audit` event, adds the event UUID
   to the report as `receipt_id`, stores that same complete report under
   `event.payload.result`, targets the event at `project_id`, and appends it before
   returning. The normal audit envelope remains flattened at the payload root and
   `payload.resource.request_id` retains a transport-supplied correlation id when one was
   provided. This reuses the accepted event store and `list`/`get` query surface; it adds
   no verb, table, or event kind.
3. **Receipt persistence is strict only for successful digest calls.** Ordinary dispatch
   audits remain best-effort. A completed `git.digest` whose receipt cannot be built or
   durably appended returns the stable error code
   `git_digest_receipt_persist_failed` and warns that writes may already have committed;
   it never returns an unqualified success. The error omits backend paths, source URLs,
   and command stderr. If malformed handler output prevents receipt construction while a
   gate audit and store are available, that audit is preserved and appended once as a
   generic Error row; a failed strict append is not retried against the same store.
   Handler failures retain the ordinary error-audit behavior. A hard
   process crash after ingest writes but before the receipt append can still leave writes
   without a completed receipt; absence of a receipt is therefore not proof that nothing
   committed, and recovery must inspect ingest state before retrying.

If an MCP response is lost, recover through a frozen, fully paginated event window. Record
`request_started_at_us` before dispatch and use
`since=request_started_at_us.saturating_sub(1)` because event-list `since` maps to a strict
`created_at > since` predicate. At the start of one recovery attempt, freeze
`until=recovery_query_time_us + 1` (event-list `until` is exclusive). Keep both values and
all filters unchanged while requesting offsets `0, 1000, 2000, ...` with:

```text
request(
  presentation="verbose",
  ops="list(namespace=\"<original namespace>\", kind=\"event\", event_kind=\"audit\", verb=\"git.digest\", since=<since>, until=<frozen until>, limit=1000, offset=<offset>)"
)
```

Advance `offset` by the returned row count and stop only when a page has fewer than 1000
rows. Freezing `until` prevents a receipt that lands during paging from shifting the
newest-first offsets. `presentation="verbose"` is required because `list` itself uses the
standard presentation policy; recovery needs full event IDs, exact timestamps, and the
unmodified stored payload. The explicit namespace constrains this multi-record discovery
query to the digest's attribution namespace. It is not by-ID storage isolation: under
`AllowAllGate`, ADR-007 makes `get(id=<event id>)` namespace-agnostic. A deployment may
still repeat the namespace on `get` to supply its Gate/routing context, but it does not
turn the ID lookup into a namespace filter. Omitting namespace from `list` instead uses
the caller's normal visible-namespace scope and can broaden or narrow discovery according
to runtime configuration.

Across every page, select rows whose `payload.result.project_id` matches the repository
and, when present, whose `payload.resource.request_id` matches the lost request. The exact
response is `payload.result`, its full `receipt_id` equals the event ID, and default
`git.digest` MCP output is `AlwaysVerbose` so the originally returned result has that same
shape. Without a request ID, use project plus the narrow frozen timestamp window;
concurrent digests of the same project can otherwise be ambiguous. A transport
`request_id` is a **request-group** selector, not an operation ID: every operation in one
batch or chain carries the same value. Recovery must therefore inspect every matching
receipt row. When one request contained multiple `git.digest` operations for the same
project, each row remains exact and uniquely keyed by `receipt_id`, but neither request ID
nor event order maps it back to an input position; send one digest per request when that
one-to-one mapping is required. A client timeout may precede completion of the still-running
daemon pass. If no match exists, begin a later recovery attempt at offset zero with a new
frozen `until`; temporary absence is not evidence that the pass failed or committed nothing.

### Remote-URL mode

1. Clone to a daemon-owned scratch directory (`~/.khive/scratch/git-digest/<hash>/`),
   `git clone --filter=blob:none` (history + trees without file blobs — commit walking
   needs messages and file lists, not contents; `git log --name-only` works against a
   partial clone with lazy fetch disabled for our read pattern).
2. Derive `owner/repo` from the canonical source, target that value explicitly in the
   source-bound `gh repo view` probe, and pass it explicitly to issue/PR listing; skip
   requested remote sources with a stable warning when that operation is unavailable.
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

### Accepted cache crash-residue rider (2026-08-09)

The staging-then-rename design can clean every ordinary error return, but no
in-process guard runs after `SIGKILL`, OOM termination, or host loss. All
staging state therefore lives in a private namespace directory
(`<scratch root>/.khive-git-staging/`) that nothing but this cache creates
entries under, and a throttled sweep of that namespace reclaims residue on
cache-root open.

A sweep candidate must be a real directory (never a symlink, file, or nested
path) that is a direct child of the private namespace and whose name is
either exactly a canonical lowercase hyphenated UUID (an in-flight clone
wrapper) or `trash-<canonical UUID>` (an interrupted deletion moved out of
the shared root). The deletion criterion is liveness, not age: every live
clone holds an advisory lock on a file inside its wrapper, so a candidate
whose lock is still held survives regardless of how old it is, while a
killed process's lock is released by the kernel the instant it dies and the
wrapper is reclaimed on the next sweep regardless of how fresh it looks. A
candidate with no lock file — the narrow crash-before-lock-file window, and
every `trash-` entry — falls back to a conservative mtime fence (strictly
older than 24 hours) before it may be reclaimed. Prefix lookalikes,
non-canonical names, future-dated entries, and fresh lock-less entries are
retained. This recovery is distinct from LRU eviction because interrupted
staging residue has no addressable cache key or ownership marker and
otherwise lies outside both configured cache caps forever.

### Security posture

- `git clone` of an untrusted remote does not execute repository-supplied code (no hooks
  run on clone/fetch). The handler additionally sets `GIT_TERMINAL_PROMPT=0` and
  `core.hooksPath=/dev/null` on the scratch clone as defense in depth.
- Local-path mode requires an absolute path; relative paths are rejected. The path must
  contain a `.git` directory; arbitrary directory walking is not performed.
- Secret masking is unchanged: ingested text goes through the same `create`-verb gate
  (ADR-088 acceptance note 5). Blocked writes remain fail-closed and surface both as
  warnings and through the result's `writes_refused` / `write_refusals` contract. This
  accounting is call-local rather than a process-global daemon counter, so overlapping
  digest calls cannot make one caller's zero-refusal assertion ambiguous.
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
3. **Changed-path and code-map enrichment.** Persist each commit's sorted, deduplicated,
   repository-relative touched paths in `properties.changed_paths`. Read paths from Git's
   NUL-delimited raw output, applying ADR-085's lossy UTF-8 filesystem-path normalization;
   for merges, use the diff against the first parent as the single canonical path set.
   When ADR-085 modules with an exact `(source_revision=repository snapshot HEAD,
   source_path=changed path)` match already exist in the same database and namespace,
   include the uniquely matching module in the commit note's `annotates` targets. Multiple
   live matches are ambiguous and therefore annotate none. The existing document
   enrichment remains unchanged. A missing or ambiguous module match is best-effort and
   never creates a code entity; path properties remain sufficient for later graph-side
   joins. Touched paths pass through the same secret-masking boundary as other
   repository-authored commit fields before they enter properties or path resolution.
   This rider relies on ADR-085 Amendment 5 and adds neither a storage column nor an edge
   relation.

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
- The bounded-call contract limits per-pass write volume and adds cursor-state plumbing,
  but does not promise a wall-clock bound. The durable receipt makes a completed report
  recoverable when a caller's transport wait expires.
- Scratch-clone cache is new daemon-owned disk state; sized and evictable, documented in
  the operator guide.

## Alternatives rejected

- **Fire-and-forget background job + status verb**: heavier (job table, lifecycle,
  another verb); cursor resumability plus the existing audit-event query surface supplies
  a durable completion receipt without that machinery. Revisit if callers need live
  progress before a pass completes rather than recovery after completion.
- **Full clone in URL mode**: blob download dominates clone time and disk for zero
  ingestion value; `--filter=blob:none` keeps everything the ingester reads.
- **Direct GitHub REST instead of `gh`**: re-opens ADR-088 Open Question 3 for no gain;
  `gh` handles auth and pagination and is already the accepted path.

## Spec-gate rulings (2026-07-09)

1. No source-host allowlist. Any https git host is accepted; after clone, issue/PR work
   requires the source-bound GitHub probe. An unusable probe degrades to commits-only
   with structured `skipped` source states plus a safe warning. SSH remains hard-rejected
   in v1.
2. `max_items` default 500 confirmed (measured ~10s per call on repositories of 448-991
   items).
3. A per-clone size cap on the scratch cache is required in addition to the LRU cap, so a
   single large-history repository cannot exhaust daemon disk before eviction applies.
