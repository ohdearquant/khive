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
  `ssh://`, or scp-style shorthand — normalizes to `host/owner/repo`:
  scheme, userinfo credentials, a port in the authority, a `.git` suffix, and
  trailing slashes are stripped; the host is lowercased (DNS is
  case-insensitive); owner and repo segments are preserved verbatim. Inputs
  that do not yield a host plus at least owner and repo segments, or that
  contain empty segments, do not normalize (they are not silently coerced).
- A local path derives the same slug from its configured `origin` remote.
- A local repository with no `origin` remote (or an origin that does not
  normalize) uses the fallback identity `local:<canonicalized-path>`.

`properties.repo_url` remains display metadata; it is never the matching key
for new anchors.

### Resolution order (replaces the Amendment 1 clause)

1. Match a live `project` entity on `properties.repo_slug`.
2. Otherwise match on legacy exact `properties.repo_url`; on a hit, backfill
   `properties.repo_slug` onto the matched entity. Existing anchors therefore
   need no migration.
3. Otherwise create the anchor with both `repo_slug` and `repo_url` set.

The basename `name` fallback is **removed**. No resolution path matches an
anchor by name alone.

### Orphaned-corpus signal

When no live anchor matches but a soft-deleted anchor with the same identity
still has at least one live `annotates`-linked git note, the handler does not
silently mint a fresh anchor beside the orphaned corpus. It proceeds with
creation but reports the condition in the `IngestReport` via three added
fields:

- `orphaned_corpus_detected: bool`
- `orphaned_project_id` — the soft-deleted anchor holding the live corpus
- `orphaned_note_count` — the count of its live annotating notes

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
