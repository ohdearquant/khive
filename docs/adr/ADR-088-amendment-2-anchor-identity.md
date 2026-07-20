# ADR-088 Amendment 2: Canonical Repo-Anchor Identity for `git.digest`

**Status**: Proposed
**Date**: 2026-07-20
**Amends**: ADR-088 Amendment 1 (anchor-resolution clause of the `project`
parameter)
**Tracking**: issue #1173

## Context

Amendment 1 specified that when `project` is absent, the handler matches an
existing `project` entity by `properties.repo_url` **or by the name derived
from the URL/path basename**. In practice this produced duplicate anchors:
the same repository ingested once via its `https://` URL and once via a local
clone path has two distinct `repo_url` spellings, so neither matches the
other, and each spelling minted its own anchor with its own annotated corpus.
Conversely, the basename fallback over-matched: two genuinely distinct
repositories that happen to share a directory name collapsed onto one anchor.

## Decision

### Canonical identity

Every digest source resolves to one canonical **repo slug** stored in
`properties.repo_slug` on the anchor entity:

- A remote URL in any spelling git accepts — `https://`, `http://`, `git://`,
  `ssh://`, or scp-style shorthand — normalizes to `host/<path>`: scheme,
  userinfo credentials, a port in the authority, query and fragment
  components, a `.git` suffix, and trailing slashes are stripped; the host
  is lowercased (DNS is case-insensitive) and a leading `www.` label is
  folded (matching the existing github.com owner/repo derivation). This
  broad grammar governs **origin-remote normalization only** — the identity
  derived for a local path from its configured `origin`. The `git.digest`
  `source` argument itself remains restricted to `https://` URLs and local
  paths exactly as Amendment 1 specifies; no new transport is accepted.
  **All** path segments are preserved in the slug — a nested-group URL such
  as `host/group/subgroup/repo` keeps every segment, so two repositories
  under one subgroup never collapse. Path segments are preserved verbatim:
  case-folding them could merge genuinely distinct repositories on a
  case-sensitive host, so casing variants of the same path remain distinct
  slugs by design. Port stripping is likewise a deliberate trade: an
  alternate-port ssh remote converges with its https spelling, at the cost
  of aliasing genuinely distinct git servers on different ports of one
  host — an accepted residual. Inputs that do not yield a host plus at
  least two path segments, or that contain empty segments, do not
  normalize (they are not silently coerced).
- A local path derives the same slug from its configured `origin` remote.
- A local repository with no `origin` remote (or an origin that does not
  normalize) uses the fallback identity `local:<canonicalized-path>`.

`properties.repo_url` remains display metadata; it is never the matching key
for new anchors. The persisted `repo_url` is credential-redacted: userinfo,
query, and fragment components of the caller-supplied URL are stripped before
storage, so an access token embedded in a source URL is never written into
entity properties.

### Resolution order (replaces the Amendment 1 clause)

1. Match a live `project` entity on `properties.repo_slug`. If more than one
   live entity carries the slug (possible when two legacy anchors holding
   different URL spellings of the same repository were each backfilled on
   separate ingests), the handler deterministically selects the oldest by
   `created_at` and surfaces the condition as a report warning naming the
   duplicate anchor ids; it never picks arbitrarily or silently.
2. Otherwise match on legacy `properties.repo_url` — first by exact string
   equality, then by normalization: a legacy anchor without `repo_slug`
   whose stored `repo_url` normalizes to the same slug also matches (this
   reconciles an anchor created from one spelling with a later ingest under
   another, e.g. a local-path anchor with a subsequent remote-URL digest).
   Step-2 matches, exact or normalized, resolve multi-candidate cases by
   the same rule as step 1: oldest `created_at` (id tie-break) selected
   deterministically, with the remainder surfaced in the same report
   warning — never an arbitrary or silent pick. On a hit, backfill
   `properties.repo_slug` onto the matched entity, and redact its stored
   `properties.repo_url` (userinfo, query, fragment) in the same patch —
   the lazy-upgrade path also closes out any credential-bearing legacy URL
   it touches. Existing anchors therefore need no migration.
3. Otherwise create the anchor with both `repo_slug` and `repo_url` set.

Anchor creation carries no uniqueness constraint, so two concurrent digests
of a previously unseen repository can race and each create an anchor. This
is an accepted residual: the step-1 multi-match rule is the deterministic
recovery path — every subsequent ingest selects the oldest anchor and
surfaces the duplicates as a report warning for curation (`merge`).

The basename `name` fallback is **removed**. No resolution path matches an
anchor by name alone.

### Orphaned-corpus signal

When no live anchor matches but a soft-deleted anchor with the same identity
still has at least one live `annotates`-linked git note, the handler does not
silently mint a fresh anchor beside the orphaned corpus. It proceeds with
creation but reports the condition in the `IngestReport` via three added
fields:

- `orphaned_corpus_detected: bool` (`false` when no orphan exists)
- `orphaned_project_id: string | null` — UUID of the soft-deleted anchor
  holding the live corpus; `null` when no orphan exists
- `orphaned_note_count: u64` — the count of its live annotating notes; `0`
  when no orphan exists

These fields disclose the id of a soft-deleted record to the caller. That is
consistent with the substrate's authorization model: namespace is
attribution, not isolation (ADR-007 Rev 6), read authorization is the Gate's
concern (ADR-018), and soft-deleted state is a view-layer distinction — the
report surfaces it precisely so the caller can act on it deliberately.

A soft-deleted anchor with zero live annotating notes is not an orphaned
corpus and raises no signal. A hard-deleted anchor's identity is
unrecoverable (the entity row, including `repo_slug`, is removed); this is a
documented limitation, consistent with hard-delete cascade semantics.

## Consequences

- All spellings of one repository converge on one anchor and one corpus;
  same-basename distinct repositories no longer collapse.
- Legacy anchors upgrade lazily on first contact, with no migration step.
- Deleting an anchor while its corpus remains live is surfaced to the caller
  instead of silently duplicated around.
