# ADR-088: Git-Lifecycle Pack — Commit and Issue Note Kinds

**Status**: Proposed\
**Date**: 2026-07-03\
**Authors**: khive maintainers
**Depends on**: ADR-001 (Entity Kind Taxonomy — `project` as the repo anchor), ADR-002
(Edge Ontology — `annotates`), ADR-013 (Note Kind Taxonomy — pack-declared note kinds),
ADR-017 (Pack Standard — `NOTE_KINDS`, `NoteKindSpec`, `KindHook`), ADR-085 (Code Pack —
`finding` note kind and its Alternative A7, directly reconciled below)\
**Related**: ADR-010 (KG Versioning — "khive does not build a GitHub replacement"),
ADR-087 (Workspace Mirror — sibling background-ingestion pattern this pack's ingester
reuses)

## Context

A design review correction superseded an earlier
properties-only framing this workstream had proposed): commits and issues should be
first-class, pack-registered **note kinds** — not entity subtypes, not bare properties on
another record — carrying canonical edge relations to code-pack `finding` notes and to
`project` entities. Population is via a background git-ingester modeled on the mirror
pattern already established (ADR-080 §6, ADR-087), not through new agent-facing verbs.

This directly engages ADR-085's own Alternative A7, decided one day earlier in this same
repository:

> **A7: Commit/PR provenance entities in v1.** Deferred. Entity-per-commit is graph bloat
> at exactly the granularity the D6.1 fence exists to prevent; findings carry `refs`
> (pr/commit/issue) as properties, which serves the audit lane's resolution-tracking need.
> If a real consumer needs commit-graph traversal, that is a v2 amendment with `artifact`
> subtypes and `introduced_by`/`derived_from` rules.

A7 correctly anticipated that commit/PR provenance would eventually need first-class
representation once a real consumer needed commit-graph traversal — and correctly rejected
entity-per-commit as bloat at the D6.1 granularity fence. It also **guessed a specific
shape** for that eventual v2 amendment: `artifact` entity subtypes linked via
`introduced_by`/`derived_from`. The adopted resolution takes a different shape: note
kinds, linked via `annotates`. This ADR is that v2 amendment, and it explains, not just
asserts, why the note-kind shape is the better fit than the artifact-entity shape A7
guessed at.

## Decision

New pack crate `khive-pack-git`, `REQUIRES = ["kg"]`, following `khive-pack-formal`'s and
`khive-pack-code`'s established shape (`crates/khive-pack-formal/src/pack.rs`,
`crates/khive-pack-code`).

1. **`NOTE_KINDS = ["commit", "issue"]`.** Both reuse the Note substrate's existing
   `content: String` field for body text — `commit.content` = the commit message,
   `issue.content` = the issue body — no new content table, unlike ADR-086's `document`
   entities, which needed a usage convention over `description` precisely because `Entity`
   has no content field. Notes already carry one; this is a structural argument for the
   note-kind choice, not just a naming one.

2. **`commit` properties** (validated by a `prepare_create` `KindHook`, same mechanism as
   `finding`'s): `sha` (required, full 40-char, the natural identity key), `short_sha`,
   `author`, `author_email` (plain — email addresses are PII, not secrets, per the root
   CLAUDE.md's "Scope = secrets only, not general PII" rule; no masking required),
   `committed_at`, `parents` (array of parent SHA strings — a plain property, not a graph
   edge; see Alternatives A2 for why commit-to-commit lineage is deliberately not an edge
   in v1). No lifecycle — commits are immutable once created, so `commit` carries no
   `kind_status`.

3. **`issue` properties**: `number`, `title`, `author`, `created_at`, `closed_at`
   (optional), `labels` (array), `state_reason` (optional: `completed` / `not_planned`).
   `NoteKindSpec` lifecycle (declared now, enforced when the generic Phase-2 lifecycle
   layer lands — same posture as `finding`'s): `kind_status`, initial `open`, terminal
   `closed`. `pull_request` is explicitly NOT in v1 — see Open Questions.

4. **Edges: `annotates` only, no new relation.** `commit` and `issue` notes `annotates`
   (note → any-substrate-UUID, already universally legal per ADR-002 and already the
   mechanism `finding` uses for its own `project`/decision links) the `project` entity that
   is the repo anchor, and, where applicable, a code-pack `finding` note the commit fixes
   or the issue tracks. This directly answers the delegated question of whether a new edge
   relation is warranted: **it is not.** `annotates` already covers note→entity and
   note→note in exactly the shape needed.

5. **Population: background git-ingester, same operational pattern as ADR-087.**
   Cursor-based (per-repo, per-kind: last-ingested commit SHA per branch; last-synced
   issue `updated_at` per repo), reading local `.git` history and, for issues, available
   GitHub API access (`gh`, an installed GitHub App, or direct REST) — not a live poll of the GitHub API on a tight
   interval, and not a new API client. Unconditional `secret_gate::mask_secrets` on commit
   messages and issue bodies before write (external, less-trusted content, same posture as
   ADR-087's file mirroring). No new agent-facing verbs — `create`/`update`/`get`/`search`
   already suffice for anything an agent needs to do with a `commit`/`issue` note once it
   exists.

6. **Explicit non-goals**, matching the design framing and ADR-010's established
   principle:
   - **Not a GitHub API mirror or replacement.** ADR-010: "khive does not build a GitHub
     replacement... add semantic enrichment instead." This pack structures git/GitHub
     lifecycle events khive tracks; it does not attempt to reproduce
     GitHub's UI, review threads, or CI state.
   - **Not first-class git entities.** Commits/issues are notes, per Decision §1 — deliberately
     lighter-weight than entities, matching their high-cardinality, event-like nature.
   - **No commit-to-commit lineage edges in v1** — see Alternatives A2.
   - **No write-back.** khive never pushes commits, comments, or state changes to GitHub;
     one-directional, git/GitHub → graph only.

## Rationale

### Why note kinds, not artifact-entity subtypes (the A7 divergence, explained)

A7 assumed the eventual v2 amendment would take the shape of every other "first-class
provenance record" this codebase has modeled so far: an entity subtype with typed edges
(`artifact` + `introduced_by`/`derived_from`), because that is the pattern ADR-069 and
ADR-085 both used for code declarations and papers. But entities are for curated, named,
referenceable concepts — the D6.1 granularity fence A7 itself invoked exists precisely to
keep the shared production graph from being overrun by high-cardinality, non-curated
records. Commits arrive in the thousands per active repo; issues, in the hundreds. That
cardinality and event-like character is exactly the profile khive already reserves for
**notes**, not entities — `task`, `memory`, and `finding` are all note kinds for the same
reason: numerous, time-bound, evidence-bearing records that reference curated entities
rather than being curated entities themselves. Modeling `commit`/`issue` as note kinds is
more consistent with khive's own existing note-vs-entity split than A7's tentative
artifact-subtype guess would have been — it satisfies the granularity fence more
conservatively, not less.

### Why `annotates` and not a new relation

The 17-relation edge ontology (ADR-002, extended by ADR-055) is closed by design; adding a
relation requires demonstrating that no existing relation fits. `annotates` was designed
for exactly this shape — a note commenting on, or providing evidence about, an entity or
another note — and `finding` already uses it for the identical purpose (annotating
`project` and other notes). A commit fixing a finding, or an issue tracking one, is not
semantically different from a finding annotating the project it concerns; it is the same
relation with a different note kind on the source side. No new relation clears the bar.

### Reconciling with `finding.refs.*` — the two mechanisms coexist

ADR-085 D4's `finding` properties contract already includes `refs` (`github_issue`, `pr`,
`commit` as plain string references) explicitly for the audit lane's lightweight
resolution-tracking need — "this finding was fixed by commit abc123" without requiring the
commit to be a graph citizen. This ADR does not remove or change that. `finding.refs.commit`
remains valid for the case where a single finding needs to point at a SHA and nothing more.
The new `commit`/`issue` note kinds are for the different, additional case now needed:
when the commit or issue itself needs identity, and needs to
be a target other records can `annotates` or be `annotates`-ed by. Both can point at the
same SHA without conflict — a `finding.refs.commit` string and a `commit` note's `sha`
property are simply two independent references to the same real-world commit; nothing
forces them to be reconciled, though a future enrichment pass could optionally resolve
`finding.refs.commit` into an `annotates` edge to the matching `commit` note once one
exists. That resolution step is optional polish, not required by this ADR, and needs no
ADR-085 amendment.

## Alternatives Considered

**A1: Model commits/issues as `artifact` entity subtypes with `introduced_by`/
`derived_from` (A7's literal suggestion).** Rejected — see Rationale. Wrong cardinality
profile for the entity substrate; the note substrate already exists for exactly this shape
of record and gets body-text storage for free.

**A2: Commit-to-commit lineage as a note→note edge (extending `precedes` or a new
relation) instead of a plain `parents` property.** Rejected for v1. Only two target
relations were named in the governing directive — commit/issue → `finding`, and → `project`
— not commit-to-commit history traversal. Adding a third edge use beyond what was actually
asked is scope creep; `parents` as a plain SHA-array property serves any consumer that
needs to walk lineage today (resolve via a follow-up `get`/`search` by SHA). If a real
consumer needs graph-native commit-history traversal, that is its own future amendment —
the same "defer until needed" discipline A7 itself used.

**A3: `finding.refs.*` alone, no new note kinds — reject the updated design and keep
ADR-085's v1 scope as final.** Rejected. This is precisely the resolution the design
correction superseded; `refs` properties do not give a commit/issue independent identity,
annotatability, or traversal, which is the explicit ask.

**A4: Include `pull_request` as a third note kind in v1** (a PR is structurally
issue-like — `merged_at`, `base`, `head` — plus everything `issue` has). Deferred, not
rejected — see Open Questions.

## Consequences

- Commit/issue history relevant to maintainer work (fixes, tracked audit findings)
  becomes queryable and linkable, closing the exact gap A7 flagged as a future amendment
  trigger.
- The audit pipeline's existing `finding.refs.*` properties are unaffected — no migration,
  no ADR-085 amendment.
- `khive-pack-git` is a genuinely new, small pack crate (two note kinds, one `KindHook`,
  zero new edge rules beyond what `annotates` already licenses) — low ongoing maintenance
  surface, matching `khive-pack-formal`'s demonstrated cost profile.
- The ingester reuses ADR-087's operational pattern rather than inventing a third mirror
  design in as many ADRs.

## Open Questions

1. **`pull_request` note kind** — same shape as `issue` plus `merged_at`/`base`/`head`.
   Not requested explicitly in the governing directive; recommend deferring to a
   follow-up, severable addition once `commit`/`issue` are shipped and a real PR-tracking
   consumer exists, mirroring ADR-085's own "severable" discipline for `finding`.
2. **Per-repo cursor granularity** — commit ingestion cursors naturally key off branch tip
   SHAs; issue ingestion cursors key off `updated_at`. Exact schema for the shared cursor
   table is an implementation detail, not fixed here.
3. **GitHub API access path for issues** — whether the ingester uses `gh` CLI, an
   installed GitHub App, or direct REST calls is an implementation choice deferred
   to whoever builds the ingester; this ADR only requires that it not become a live,
   tight-interval GitHub API poller (rate-limit and "GitHub replacement" concerns both
   argue for batch/cursor-based sync, matching ADR-087's own poll cadence discipline).

## Implementation

- New crate `crates/khive-pack-git`, `NAME = "git"`, `REQUIRES = ["kg"]`.
- `NOTE_KINDS = ["commit", "issue"]`, `NoteKindSpec` lifecycle for `issue` only.
- One `prepare_create` `KindHook` validating `commit.sha` shape and `issue.state_reason`
  against a governed value set (fail-closed, no silent coercion — matching `finding`'s
  hook precedent).
- Ingester submodule reusing ADR-087's cursor/secret-masking pattern; a shared
  `git_mirror_cursor(project_id, kind, cursor_value, updated_at)` table.
- No new edge relations; no new top-level verbs.

## References

- ADR-085 §D4, §Alternatives (A7) — the deferred commit/PR provenance question this ADR
  resolves, and the shape divergence from A7's own guess
- ADR-002 — `annotates` (note → any substrate UUID, source always a note)
- ADR-010 — "khive does not build a GitHub replacement"
- ADR-087 — background ingestion operational pattern reused here
- `crates/khive-types/src/note.rs` — `Note.content: String` (free body-text storage this
  ADR relies on)

## Amendment (v0 implementation)

The v0 implementation of `khive-pack-git` resolves the three Open Questions above and
records two shape decisions made during the build that were not fixed in the original
Decision text.

1. **`pull_request` shipped in v0, not deferred.** Open Question 1 recommended deferral,
   but the acceptance criterion for this pack — a provenance query that walks from a
   `project` entity to the commits and pull requests that touch it — cannot be
   demonstrated without a PR-linked commit in the graph. `pull_request` is therefore a
   third `NOTE_KINDS` entry alongside `commit` and `issue`, sharing `issue`'s properties
   shape (`number`, `title`, `author`, `created_at`, `closed_at`, `merged_at`, `base_ref`,
   `head_ref`) plus a `KindHook` that validates `number` is present but does not enforce a
   governed `state_reason` enum for PRs — GitHub's PR schema does not document one the way
   it does for issues (`completed` / `not_planned`), so PR `state_reason`, when present, is
   only checked for non-emptiness rather than validated against a closed set.

2. **Commit-to-document `annotates` enrichment.** The ingester additionally links a
   `commit` note to a `document`-kind entity (ADR-086) when the commit's touched-file paths
   match that document's `properties.source_uri` or filename. This is a best-effort
   enrichment, not a hard requirement: no match means no edge and no document is
   auto-created. This narrows, rather than widens, the pack's footprint — it reuses an
   edge relation and entity kind already licensed elsewhere in the schema.

3. **Cursor table shape (Open Question 2).** `git_mirror_cursor(project_id, kind,
   cursor_value, updated_at)`, primary key `(project_id, kind)`. `kind` is a generic
   discriminator (`"commits"`, `"issues"`, `"prs"`), not separate per-kind tables or
   columns, so a future ingestion pack for another lifecycle domain (for example, a
   code-review pack) can reuse this table for its own cursor rows without a schema
   migration.

4. **GitHub API access path (Open Question 3).** The ingester shells the `gh` CLI (already
   the fleet-standard GitHub client) rather than an installed GitHub App or direct REST
   calls. When `gh` is unavailable, or fails against a given repository (no linked GitHub
   remote, no auth), issue and pull-request ingestion are skipped with a warning in the
   ingest report; commit ingestion — which depends only on local `.git` history — proceeds
   regardless. This is a one-shot batch pass per invocation, not a poller.

5. **Secret masking is enforced at the generic `create` verb, not re-implemented in the
   ingester.** The KG pack's `create_note_inner` already hard-rejects content containing
   unmasked secret patterns. The ingester calls the same masking helper the gate uses
   before submitting commit-message and issue/PR-body content, so ingested provenance text
   is redacted rather than rejected outright.
