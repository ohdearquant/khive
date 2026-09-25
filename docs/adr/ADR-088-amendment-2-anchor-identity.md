# ADR-088 Amendment 2: Canonical Repo-Anchor Identity for `git.digest`

**Status**: Accepted (2026-09-25)
**Date**: 2026-07-20
**Amends**: [ADR-088 Amendment 1](ADR-088-amendment-1-git-digest.md)
(anchor-resolution clause of the `project`
parameter)
**Tracking**: issues #1173, #1708; #3175 (the slug derivation did not yet
confine userinfo to the URL authority, so an `@` in a path segment replaced
the host, contrary to the all-segments rule below)

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
  broad grammar governs **normalization only**: the identity derived for a
  local path from its configured `origin`, and the stored
  `properties.repo_url` values compared during step-2 reconciliation below,
  which may hold any spelling an earlier anchor recorded. The `git.digest`
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
  least two path segments, or that contain empty segments, do not normalize
  to a slug. An HTTPS value accepted by the top-level `source` parser still
  needs a stable identity in that case, so a shared fallback canonicalizer
  strips credentials, query/fragment material, trailing slashes, and a
  trailing `.git` suffix from its identity while retaining the original URL
  for clone/fetch. Stored-URL reconciliation calls that same canonicalizer
  only for values accepted by the HTTPS source grammar;
  arbitrary malformed strings are not silently coerced into identities.
- A local path derives the same slug from its configured `origin` remote.
- A local repository with no `origin` remote (or an origin that does not
  normalize) uses the fallback identity `local:<canonicalized-path>`.

`properties.repo_url` remains display metadata; it is never the matching key
for new anchors. The persisted `repo_url` is credential-redacted: userinfo
from either a scheme URL or SCP-style shorthand, plus query and fragment
components, is stripped before storage, so an access token embedded in a
source URL is never written into entity properties.

### Resolution order (replaces the Amendment 1 clause)

1. Match a live `project` entity on `properties.repo_slug`. If more than one
   live entity carries the slug (possible when two legacy anchors holding
   different URL spellings of the same repository were each backfilled on
   separate ingests), the handler deterministically selects the oldest by
   `created_at` and surfaces the condition as a report warning naming the
   duplicate anchor ids; it never picks arbitrarily or silently.
2. Otherwise match on stored `properties.repo_url` evidence — first by exact
   string equality among pre-slug anchors, then by normalization among every
   live anchor whose `repo_slug` is absent **or differs from the canonical
   source slug**. A stored URL that normalizes to the canonical slug is
   authoritative reconciliation evidence even when the row already carries
   a hand-written, stale, or otherwise non-canonical slug (#1708). This
   reconciles an anchor created from one spelling with a later ingest under
   another, including a local-path anchor with a subsequent remote-URL
   digest. Exact and normalized tiers each select the oldest candidate by
   `created_at` (id tie-break); exact-string resolution precedes normalized
   resolution. The selected anchor is backfilled with the canonical
   `properties.repo_slug`, and its stored `properties.repo_url` is redacted
   (scheme or SCP-style userinfo, query, fragment) in the same patch. The
   lazy-upgrade path also closes out any credential-bearing legacy URL it
   touches. A remote-less local path's `local:<canonical-path>` fallback is a
   canonical identity and participates in this normalized reconciliation.

   A canonical step-1 winner always keeps precedence, even when a normalized
   URL-equivalent anchor with a conflicting slug is older. Such anchors are
   not rewritten behind the winner; they are surfaced as duplicate or
   conflicting anchor ids in the same report warning. The same warning also
   names unselected candidates from an exact or normalized step-2 match.
   Candidate enumeration and warning order remain deterministic, and an id
   is emitted at most once. Existing anchors therefore need no migration.
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
- Legacy and present-but-noncanonical anchors upgrade lazily on first contact,
  with no migration step; conflicts beside an existing canonical winner are
  visible for deliberate curation.
- Deleting an anchor while its corpus remains live is surfaced to the caller
  instead of silently duplicated around.

## Proposed rider: three anchor-resolution behaviours (2026-09-25)

Status: Proposed. Needs maintainer sign-off before it binds. Refs #3176. It adds to the resolution
rules above and replaces none of them.

### Context

Three behaviours of `git.digest` anchor resolution are not covered by the text above, and no test pins
the choice the code makes (read in `crates/khive-pack-git/src/handlers.rs` and
`crates/khive-pack-git/src/ingest.rs`):

1. With no `project` argument, every candidate lookup filters on the caller's namespace
   (`projects_by_slug_select.sql`, `projects_by_legacy_repo_url_select.sql`,
   `projects_without_canonical_slug_select.sql`, each `namespace=?1`). With an explicit `project`,
   `resolve_project_id` takes a full UUID as given and resolves an id prefix through
   `resolve_prefix_unfiltered`, with no namespace filter.
2. Resolution runs before the ingest, and step 2 can write the canonical `repo_slug` onto the selected
   anchor. The duplicate-anchor warning, the orphaned-corpus fields and `project_created` are added to
   the report only after the ingest returns successfully, so a failed ingest returns its error without
   them.
3. `find_orphaned_anchor` sorts soft-deleted candidates by `deleted_at` alone, newest first, with a
   stable sort over rows from two queries that have no `ORDER BY` (`orphaned_projects_select.sql`,
   `soft_deleted_projects_without_canonical_slug_select.sql`). Two tombstones with the same
   `deleted_at` keep the order the queries returned, so the anchor the signal names can change
   between runs.

### Decision

1. **An explicit `project` is an id, not a lookup.** When the caller supplies `project`, the anchor is
   the entity that argument names, by full UUID or unique id prefix, in any namespace; the caller's
   namespace scopes only the derived resolution of steps 1 to 3. This records the current behaviour. It
   is consistent with the by-id access the pack already follows and with namespace as attribution
   rather than isolation (ADR-007), and the Gate still authorizes the call (ADR-018). Whether a full
   UUID that names no live `project` entity should be refused is outside this rider; the code does not
   check it today.
2. **Resolution facts reach the caller on failure too.** Step 1's rule that the handler "never picks
   arbitrarily or silently" holds on the failure path. When the ingest fails after resolution, the
   error names the selected anchor, any duplicate or conflicting anchor ids in the words of the
   success-path warning, whether step 2 backfilled `repo_slug`, whether step 3 created the anchor, and
   the orphaned-corpus signal when it fired. This changes the code.
3. **Tombstone order is total.** Soft-deleted candidates are ordered by `deleted_at` descending, then by
   `id` ascending, matching the `id` tie-break step 2 already uses for `created_at`. The signal names
   the first candidate in that order that still has a live annotating note. This changes the code.

### Alternatives considered

- _Scope an explicit `project` to the caller's namespace._ Rejected: an explicit id is a deliberate
  choice by the caller, and refusing it would make a cross-namespace anchor reachable only by moving
  records, which the namespace model does not require.
- _Defer the step-2 backfill until the ingest succeeds, so a failed digest writes nothing during
  resolution._ Rejected: the ingest itself can fail after writing notes, so a failed digest is not
  write-free either way, and the backfill is an identity correction step 2 applies on contact.
- _Leave the tie order to the database._ Rejected: the signal would name a different anchor between
  runs of the same input, which the deterministic rules above exist to prevent.

### Acceptance

1. A caller in namespace A passing `project` equal to the id of an anchor in namespace B digests into
   that anchor; the same digest without `project` does not match it.
2. A digest whose resolution finds a duplicate anchor and whose ingest is made to fail returns an error
   that names the selected and the duplicate anchor ids.
3. Two soft-deleted anchors with the same identity and the same `deleted_at`, each with a live
   annotating note, yield a signal naming the lower `id`, whichever order the rows were inserted in.
