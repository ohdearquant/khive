# ADR-047: Knowledge Pack

**Status**: accepted (amended 2026-06-07, 2026-06-10, 2026-06-10b, 2026-08-01, 2026-08-06, 2026-08-29, 2026-08-30b, 2026-08-30c, 2026-09-14, 2026-09-15)
**Date**: 2026-05-25
**Authors**: khive maintainers
**Amended by**: proposed [ADR-160](ADR-160-shared-pack-infrastructure.md), which adds a bounded,
operator-opt-in intent-rephrase retrieval path while preserving original-only behavior by default
on acceptance.

## Amendment (2026-09-23): refusal events for existing atoms

**Status: Proposed for final specification and implementation gates (#2995).**

A refused public `knowledge.upsert_atoms` batch leaves every atom row and its
indexes unchanged, including valid siblings. It separately appends a `refusal`
event for each submitted item that resolves an existing live ordinary atom.
Slug lookup uses the caller namespace; properties-only UUID lookup retains its
existing namespace-independent identity semantics. Deprecated rows remain eligible
because direct reads still return them. Missing, deleted, domain and mirror targets
have no trace; invalid or unresolvable input identities are not echoed or invented.
Whole-request deserialization failures have no item-level trace. Import retains its
existing refusal semantics and emits none of these events.

Admission validates the entire submitted batch before any atom writer is acquired.
On refusal, it resolves eligible targets read-only, releases the reader, then attempts
the separate event appends. No valid prefix or sibling is committed. The original
first refusal remains the operation result even when a trace append fails. Target
lookup/storage failures never turn the refused mutation into a successful write.

Events use the Event substrate, `kind=refusal`, `verb=knowledge.upsert_atoms`, and
`target_id` equal to the serving atom UUID. They carry the caller token's namespace
and actor, including when a properties-only UUID names another namespace. An atom
is not represented as a KG entity or event observation. Authorized readers use
`list(kind="event", event_kind="refusal", target_id=<complete atom UUID>)`; target
identity does not widen event namespace visibility. Existing edge `target_id`
resolution remains unchanged. Exact event-subject filtering applies to query and
count and is preserved by the versioned split-events transport.

Payload schema version 1 contains `subject_kind="knowledge_atom"`, `item_index`,
`digest_input="masked_submitted_atom_v1"`, and `rejected_digest="blake3:<64 lowercase
hex digits>"`. A directly refusing item records `reason="secret_detected"` with safe
detector, trigger and positional location, or `reason="validation_refused"`. A valid
item rejected with its batch records `reason="batch_refused"` and
`first_refusing_item_index`. It contains no raw submitted fields, secret excerpt,
masked preview, arbitrary validation message or backend error.

The digest covers the normalized would-persist record fields: slug, name, content,
tags, properties, source_uri, source_type and finalized. Omitted patch fields retain
their existing values; properties-only submissions retain the other stored fields.
Identity, timestamps and derived lifecycle status are excluded. Before hashing,
canonical secret detection replaces every detected span with a fixed token naming
only its detector, retaining neither characters nor span length. Any suffix not
proven clean within the bounded scan budget becomes a fixed unscanned token. The
canonicalization is versioned by `digest_input`; unscanned fallback still produces
a digest and a refusal event, never a raw-content hash. The work budget is
2,097,152 bytes divided equally (integer floor) among every object key and string
leaf in the candidate; fixed root labels count as units but their shares remain
unused. Each actual string charges its full byte length before tokenization and
each remaining detector sweep, including any revisited token prefix. Quotas are
independent of traversal order; structural traversal and serialization are separate
work. Initial exhaustion replaces the whole string; later exhaustion replaces the
remaining unproven suffix.

The known atom root is an object sorted by key. Objects under properties are encoded
as arrays of `[masked_key, masked_value]` pairs, recursively canonicalized and sorted
by each pair's canonical JSON bytes. Duplicate masked pairs are retained; no property
is overwritten by a masked-key collision and ordering never depends on the original
secret keys. Ordinary arrays retain their element order. JSON is encoded as compact
UTF-8, then hashed with full BLAKE3-256. The digest is a masked-attempt correlation
identifier, not an authentication or confidentiality primitive.

When eligible traces exist, the original typed error projection adds
`refusal_recorded` and `refusal_events`. Each trace identifies `item_index`, `subject`
and either the confirmed `event_id` or a closed `error_class`:
`event_store_unavailable` or `event_append_failed`. The aggregate flag is true only
when all eligible attempted appends are confirmed. An append failure also emits a
warning naming the operation, item, UUID and class, without rejected content or raw
storage diagnostics. False means no confirmed trace for the failed item, not proof
that an ambiguous append wrote nothing. No automatic append retry is implied. With
no eligible targets the original refusal is returned without a fabricated receipt.

Acceptance requires unchanged complete old rows and sibling state; exact subject and
namespace-filtered readback; stable digests when only detected span bytes change;
different digests when clean content changes; retained masked-key collisions and
bounded unscanned fallback; and append-failure projection that preserves the original
refusal. The new public event enum/filter fields and split-events protocol version
are additive source compatibility changes and must pass their consumers' full suites.

## Amendment (2026-09-14): existing atom properties-only updates

**Status: Accepted (2026-09-14).** Acceptance of the text is not implementation
acceptance; the dependent implementation lands on its own gates.

`knowledge.upsert_atoms` gains a second atom row form:
`{id: <complete UUID>, properties: <JSON value or null>}`. Both keys are required.
Every other row key, including `content`, `slug`, and `name`, is rejected rather
than assigned precedence. Malformed IDs, slugs, and short prefixes are invalid
input; complete UUID spellings accepted by the UUID parser are canonicalized.

The target must be an existing live ordinary atom. An absent or soft-deleted UUID
returns `NotFound`; this form never inserts a row. A domain or its mirror atom is
invalid input and must be changed through the domain's own verbs. UUID lookup is
namespace-agnostic under ADR-007, with authorization at registry dispatch. The
record retains its stored namespace.

The supplied properties replace the complete stored value, following the existing
upsert implementation: keys are not merged, an empty object replaces the value
with `{}`, and explicit null stores SQL NULL. Existing acceptance of arbitrary
JSON values is retained. Caller-supplied properties undergo the same secret scan
and reserved-property rejection as the ordinary row form. Only `properties` and
`updated_at` change. All other fields, including stored content bytes, remain
unchanged; stored short or empty content is neither trimmed nor revalidated.

The original `(namespace, slug)` row form remains unchanged and requires content
meeting the 20-word minimum whenever creating or replacing atom content, even
when supplied content matches the stored value. Its source/finalization tri-state
rules remain in force. Batches may mix the two forms, preserve input write order,
and validate all inputs and resolve targets before writing any row. A refused
row must not commit a valid prefix. The existing `{created, updated, total}`
response remains, with successful id rows counted as updated.

## Amendment (2026-08-30c): indexed exact-name recovery for short queries

A query such as `AI` has no scoreable term, and the trigram FTS tokenizer cannot match
a phrase below three characters. When a completed lexical pass finds no match and the raw
query contains no scoreable non-stopword term, `knowledge.search` probes the unique
`(namespace, slug)` index using the same slug normalization as the pack's import path.
An eligible hit reports `candidate_provenance.lexical: "exact_name"` with lexical score
provenance. This is an exact normalized-slug lookup, not a general name index or substring
search: an atom with a caller-chosen slug outside the import convention remains outside
this guarantee. A role prefix affects scoring but cannot suppress the raw-query probe.

The probe runs after FTS retrieval, using the same reader and remaining lexical-stage
deadline. It retains namespace, live-row, status, and atom/domain eligibility; an existing
ineligible slug reports `filtered`. An expired probe reports `timed_out` and the existing
lexical degradation flags, with an internal `exact_name_probe` timing phase. It does not reset
the stage budget, rerun a timed-out pass, or change public timeout-detail redaction. Exact-name
hits then follow the existing fusion, score normalization, status multiplier, and final
`min_score` gates.

The probe sits downstream of the term admission bound described in the 2026-08-30b
amendment. It consumes no admission of its own, and a pass that was refused every
admission opens no reader, so it never reaches the probe.

## Amendment (2026-08-30b): request-wide bound on distinct FTS terms

The lexical candidate stage admits at most 32 distinct expanded terms across one
`knowledge.search` or `knowledge.suggest` request. Terms are deduplicated and expanded,
then admitted in deterministic spelling order before any database read, including the
rarest-first frequency probes. The full query and both optional decomposed passes share
one allowance; repeating a term in a later pass consumes another admission because that
pass repeats the retrieval work. A pass with no allowance left opens no reader.

Within a pass, rarity ordering, phase-A rowid probes and widening, eligibility fallback,
and namespace-only existence recovery all use the same admitted terms or their subsets.
The bound limits combined distinct-term work, not the number of SQL statements: one term
can require multiple bounded probes. Existing per-term row caps, widening ceilings, and
lexical deadlines remain in force. Queries with no scoreable terms retain the raw-phrase
fallback, which consumes one admission.

`candidate_provenance.terms_truncated` is true when any lexical pass drops terms because
the shared allowance is exhausted. It is independent of timeout reporting. `no_match`
and `filtered` describe only admitted terms in the caller's namespace: a local match
reachable only through an untested term must not turn a truncation-caused miss into
`filtered`. ANN retrieval and scoring remain unchanged.

## Amendment (2026-08-29): tri-state atom upsert patches

On an existing atom, `knowledge.upsert_atoms` treats `source_uri`, `source_type`, and `finalized`
as patch fields with three wire states. An omitted key preserves the stored value. JSON `null`
clears a source field, while a non-blank string replaces it. A boolean sets `finalized`
explicitly. Because the `finalized` column is non-nullable, `finalized: null` resets it to the
schema default (`false`), the same persisted flag as explicit `false`.

The atom lifecycle `status` remains independent from the legacy finalization flag. Setting
`finalized: true` promotes `draft` to `reviewed`; `false` or `null` does not demote `reviewed`,
`deprecated`, or any future non-draft lifecycle state. On insert, omitted or null source fields
store SQL NULL, and omitted or null `finalized` creates a non-finalized `draft` atom. Blank source
strings retain their prior compatibility behavior: they store NULL on insert and leave an
existing source unchanged on update.

## Amendment (2026-08-06): identifier parity for `knowledge.get`

`knowledge.get(id=...)` follows the shared identifier ladder: a complete UUID is parsed
first; for non-UUID input, an exact registered slug in the caller's namespace wins before
an undashed hexadecimal string of at least eight characters is interpreted as a unique
short UUID prefix. UUID and prefix forms are namespace-agnostic by-ID reads under ADR-007
Rule 2, while slug lookup remains scoped to the caller's namespace. Prefix resolution
deduplicates a domain and its same-UUID FTS mirror atom before deciding ambiguity, while
distinct matching UUIDs fail closed. Atom section loading follows the resolved atom's
stored namespace so `include_sections=true` preserves the same by-ID contract.

## Amendment (2026-08-06): fail-closed search filter values

`knowledge.search` accepts only `atom` or `domain` for `kind`/`type`. Its atom-status
vocabulary is closed to `draft`, `reviewed`, and `deprecated`, so a non-blank
`exclude_status` outside that set is invalid. Both cases return `InvalidInput`; an unknown
kind never falls through to atom search, and a misspelled exclusion never replaces the safe
default status filter.

## Amendment (2026-08-01): exact namespace support for `knowledge.compose`

Issue #1505 adds an optional `namespace` parameter to `knowledge.compose`. An absent parameter
preserves the existing caller-token scope. An explicit value is parsed with `Namespace::parse`
and scopes the operation to exactly one namespace under ADR-007's precise escape: automatic
domain suggestion, domain and atom resolution, section loading, KG blending, and brain-profile
type-weight reads all use the same derived token. Invalid values fail closed. The handler repeats
the namespace parse for direct-call defense in depth, requires it to match the already-authorized
token, and then narrows any broader visible set to that exact namespace; it never elevates a token.
Nested brain dispatches preserve the token's request actor and scope through their own Gate checks.
The pack-local Tier-3 section-posterior fallback is keyed by namespace so live feedback cannot
change an untouched measurement arm. Registry dispatch remains the authorization seam.

## Amendment (2026-06-10b): exclude_status precedence fix; auto-compose member filter; atom status taxonomy clarification

Follow-up to the 2026-06-10 amendment (PR #90):

- **`exclude_status` precedence corrected**: the parameter now works as documented.
  Precedence: explicit `status=` → no exclusion; else explicit `exclude_status=` → use it;
  else `include_drafts=true` → exclude deprecated only; else default → exclude draft+deprecated.
  The previous implementation silently ignored `exclude_status` when no `status=` was present.
- **Auto-compose member atoms filtered**: when `knowledge.compose` runs in auto mode (no explicit
  `domain_ids` or `atom_ids`), domain member atoms are now filtered by the same
  `["draft", "deprecated"]` exclusion before building the briefing. Explicit `atom_ids` are not
  filtered — the caller opts into whatever those IDs hold.
- **Atom status taxonomy**: the closed atom-level status set is `draft | reviewed | deprecated`.
  `verified` is a **section-level** status only (used by the dispute/resolution flow in
  `knowledge.edit`). No public write path (`upsert_atoms`, migrations) sets atom `status` to
  `verified`. The `verified` arm in `status_multiplier` has been removed; unknown atom statuses
  fall through to the `reviewed`-equivalent 1.0 multiplier.

## Amendment (2026-06-10): knowledge.search draft exclusion default; status taxonomy; suggest/compose alignment

The search-quality work (issue #78, PR #90) adds the following contract changes. Where the body
still reads otherwise, this amendment governs:

- `knowledge.search` and `knowledge.suggest` now exclude `draft` and `deprecated` atoms by
  default. Pass `include_drafts=true` to include drafts (`deprecated` always excluded by default).
  Explicit `status=` overrides all defaults.
- The new public parameters `include_drafts`, `status`, and `exclude_status` are documented in
  the `knowledge.search` verb contract below.
- `knowledge.suggest` and auto-`knowledge.compose` share the same default exclusion. There is
  no `include_drafts` on `suggest`.
- The default exclusion applies to **all result sources** including ANN-sourced candidates
  (filtered post-hydration), not only the SQL/FTS path.

## Amendment (2026-06-07): content-only atoms; normalized response envelope

The schema-consolidation work supersedes two parts of the original contract below.
Where the body still reads otherwise, this amendment governs:

- **Atoms have no separate `description` column — the `content` column carries it.**
  `content` holds the atom's _description_ (the `description` field from the atom
  markdown front matter — a short summary, ≥ 20 words). The atom's full **body** is
  its typed **sections** (`knowledge_sections`), not the `content` column.
  `knowledge.upsert_atoms` accepts `content` only — there is no `description` input
  alias. The `knowledge_atoms` table and `fts_knowledge` index carry no `description`
  column, and atom scoring ranks across name, tags, and content.
  (`knowledge_domains` keep their own `description` — this change is atoms-only.) See
  [ADR-048](ADR-048-knowledge-section-profiles.md) §"Atom and section content constraints".
- **`search`, `topic`, and `list` return `{results, total, ...}`**, not
  `{items, total}` — part of the response-envelope normalization. Inline
  `{items: ...}` references below are stale. The key beside `results` is what the
  [2026-09-15 amendment](#amendment-2026-09-15-topic-query-candidate-window-counts)
  narrows for one branch: a `topic` request carrying a non-null `query` reports
  `candidate_window_count` and no `total`.

## Context

khive's `kg` pack ([ADR-017](ADR-017-pack-standard.md)) exposes a complete CRUD surface
over the eight entity kinds and fifteen edge relations. Registering a research concept
requires at minimum three steps: `create(kind="concept", ...)`, optionally
`link(relation="introduced_by", ...)`, and `search(kind="concept", ...)` for retrieval.
These three steps recur in every research-agent workflow.

> **Amended**: ADR-048 added a 9th entity kind (`resource`); ADR-055 added 2 epistemic
> relations (`supports`, `refutes`); the current totals are 9 entity kinds and 17 edge
> relations.

Agents that work exclusively with research concepts encounter two friction points:

1. **Domain promotion is manual.** `create` accepts a `tags` list; callers must
   remember to add the domain string both to `properties.domain` (for structured
   access) and to `tags` (for FTS discoverability). Omitting either silently degrades
   retrieval quality.
2. **Parameter shape for citations is inverted relative to how researchers think.** The
   underlying `link` verb names its parameters `source_id` (the graph-source entity) and
   `target_id` (the graph-target entity). For `introduced_by` edges, the graph-source is
   the concept and the graph-target is the paper — but researchers naturally say "cite
   _this concept_ to _this paper_", which maps to `concept_id` / `source_id` in
   domain vocabulary.

Other packs ([ADR-019](ADR-019-gtd-pack.md) for tasks, [ADR-021](ADR-021-memory-pack.md)
for memory) demonstrate the pattern: wrap kg primitives with an opinionated verb surface
that encodes domain conventions, leaving the underlying substrate unchanged.

## Decision

### 1. Two tiers: corpus verbs and concept verbs

The knowledge pack has two tiers of functionality:

**Corpus tier** (9 verbs) — a standalone knowledge-atom store with its own tables,
FTS5 index, TF-IDF search, and budget-constrained selection. Atoms are slug-keyed
content units; domains are named groupings of atoms. This tier ports the retrieval
capabilities of the lore service into the pack system.

**Concept tier** (3 verbs) — sugar over the kg pack's entity/edge substrate for
research-concept workflows. These verbs use existing entity kinds and edge relations;
they do not introduce new ones.

| Verb                       | Tier    | Category   | Description                                                                  |
| -------------------------- | ------- | ---------- | ---------------------------------------------------------------------------- |
| `knowledge.upsert_atoms`   | Corpus  | Commissive | Bulk insert/update slug-keyed knowledge atoms                                |
| `knowledge.upsert_domains` | Corpus  | Commissive | Bulk insert/update domain groupings of atoms                                 |
| `knowledge.get`            | Corpus  | Assertive  | Fetch one atom or domain by ID, exact slug, or short prefix                  |
| `knowledge.list`           | Corpus  | Assertive  | Paginated listing of atoms or domains                                        |
| `knowledge.delete_atoms`   | Corpus  | Commissive | Soft-delete atoms by slug                                                    |
| `knowledge.stats`          | Corpus  | Assertive  | Corpus statistics (counts, coverage)                                         |
| `knowledge.index`          | Corpus  | Commissive | Backfill embeddings + FTS for atoms                                          |
| `knowledge.fold`           | Corpus  | Assertive  | Budget-constrained knapsack selection (token budgeting)                      |
| `knowledge.search`         | Corpus  | Assertive  | TF-IDF + embedding rerank (default when embedder configured) over the corpus |
| `knowledge.learn`          | Concept | Commissive | Register a concept entity with domain promotion                              |
| `knowledge.cite`           | Concept | Commissive | Link a concept to its source (document, person, or org) via `introduced_by`  |
| `knowledge.topic`          | Concept | Assertive  | List/search concepts, optionally filtered by domain                          |

### 1a. Corpus tier schema (V19 migration)

The corpus tier introduces two tables via versioned migration V19
(`knowledge_atoms_and_domains`):

- `knowledge_atoms` — slug-keyed content units with name, content (the atom's
  description/summary from front matter; no separate `description` column — the
  full body lives in the typed `knowledge_sections`), tags (JSON array),
  properties (JSON object), and finalized flag.
- `knowledge_domains` — named groupings with slug, name, description, tags, and
  members (JSON array of atom slugs).

An FTS5 external-content virtual table (`fts_knowledge`) indexes slug, name,
and content from `knowledge_atoms` via triggers that sync on
insert/update/delete. The trigram tokenizer enables substring matching.

Soft-deleted atoms (non-null `deleted_at`) are excluded from the FTS index. V26
names a live-row view (`knowledge_atoms_fts_content`) as the external content
object, so FTS5's index and its content object cover the same rows. Transition-
symmetric triggers remove a document only when the old row was live and insert
one only when the new row is live; soft-delete → hard-delete is therefore a
no-op at the FTS layer, while resurrection inserts exactly once.

### 1b. Concept tier: three verbs, no new kinds

The concept tier registers three verbs over the existing `concept` entity kind. It
does **not** introduce new note kinds, entity kinds, or edge relations:

| Verb    | Underlying operation                           | Value-add                                                       |
| ------- | ---------------------------------------------- | --------------------------------------------------------------- |
| `learn` | `create(kind="concept")`                       | Auto-promotes `domain` to both `properties.domain` and `tags`   |
| `cite`  | `link(relation="introduced_by")`               | Domain-oriented parameter names; weight clamped to `[0.0, 1.0]` |
| `topic` | `search(kind="concept")` + optional tag filter | Domain-filter parameter; consistent `limit` cap of 100          |

### 2. Corpus tier verbs

#### `knowledge.upsert_atoms` — bulk atom insert/update

```
upsert_atoms(atoms: [{slug, name, content, tags?, properties?, source_uri?, source_type?, finalized?}, ...], chunk_size?) → {upserted: N}
```

Inserts or updates atoms by `(namespace, slug)` key. On conflict, updates name,
content, tags, properties, source attribution, finalized, and `updated_at` according to the
tri-state amendment above. Empty `atoms` array is rejected. Tags are stored as a JSON array
string; properties as a JSON object string.

#### `knowledge.upsert_domains` — bulk domain insert/update

```
upsert_domains(domains: [{slug, name, description?, tags?, members?}, ...]) → {upserted: N}
```

Inserts or updates domains by `(namespace, slug)` key. Members is a JSON array of
atom slugs.

#### `knowledge.get` — fetch by ID, exact slug, or short prefix

```
get(id: <uuid|slug|short-prefix>) → {type: "atom"|"domain", ...fields}
```

Resolves by complete UUID first. For non-UUID input, it tries an exact slug against both
`knowledge_atoms` and `knowledge_domains` in the caller namespace, then interprets a
remaining 8+ hex string as a unique UUID prefix. UUID and prefix reads are
namespace-agnostic. Returns 404 if not found.

#### `knowledge.list` — paginated listing

```
list(
  type?: "atom"|"domain",
  limit?: 20,
  offset?: 0,
  after?: <full-uuid|"">,
  fields?: [<field>, ...]
) → {results: [...], limit, order, total?, offset?, next_after?}
```

Default type is `atom`. Limit is capped at 500. Legacy offset pages have a
declared total order of `created_at DESC, id DESC`.

The [2026-09-14 limit-report amendment](#amendment-2026-09-14-knowledge-list-and-topic-limit-reports)
extends this response shape and specifies the existing lower bound on acceptance;
this section's pagination and projection rules otherwise remain in force.

Completeness-sensitive consumers use keyset mode: pass `after=""` on the first
request, then round-trip each non-null `next_after` full UUID. Cursor pages seek
by `created_at ASC, id ASC`; `after` and `offset` are mutually exclusive. This is
a live traversal rather than an MVCC snapshot. Inserts whose key is behind an
already-issued boundary belong to a fresh walk, while inserts ahead of the
boundary may extend the current walk. Existing rows are not shifted, skipped,
or duplicated by those inserts. A cursor remains usable if its row is
soft-deleted, but a missing, wrong-type, or out-of-namespace cursor fails.
Callers must retain the same type and status filters for the whole walk.
The walk is complete when `next_after` is null. Cursor pages carry no `total`:
counting the namespace is a full scan per page and cannot signal completion.
Offset pages keep `total`.

`fields` is a strict, non-empty response projection. Atom fields are `id`,
`namespace`, `slug`, `name`, `content`, `tags`, `properties`, `status`,
`source_uri`, `source_type`, `finalized`, `kind`, `created_at`, and `updated_at`.
Domain fields are `id`, `namespace`, `slug`, `name`, `description`, `tags`,
`members`, `kind`, `created_at`, and `updated_at`. Projection is applied at the
SQL boundary: `fields=["id","slug"]` selects no atom content, apart from hidden
`id`/`created_at` pagination keys that are not rendered unless requested.

#### `knowledge.delete_atoms` — soft delete

```
delete_atoms(ids: [<slug|uuid>, ...], cascade?: true) → {deleted: N}
```

Sets `deleted_at` timestamp. FTS trigger automatically removes from search index.

#### `knowledge.stats` — corpus statistics

```
stats() → {atoms: N, domains: N, ...}
```

#### `knowledge.index` — backfill embeddings

```
index(ids?: [<slug|uuid>], batch_size?: 500, insert_only?: false,
      rebuild_ann?: false) → {indexed: N}
```

Backfills default-model embedding vectors. The knowledge retrieval
paths read only the default model, so secondary registered models are not embedded
until a model-aware or fused knowledge read path exists. When `ids` is omitted,
indexes the entire corpus in batches. `insert_only` skips the delete-then-reinsert
cycle for fresh corpus backfill.

This verb does not accept `rebuild_fts`: rebuilding `fts_knowledge` and
`fts_sections` is a whole-database operation independent of the caller's
namespace, and the ordinary verb has no per-caller cost admission to bound it.
That rebuild — which runs each external-content table's rank-1 FTS5 integrity
check before reporting success — is reachable only through the `kkernel
reindex` operator CLI (`--rebuild-fts`), whose report names both index names,
elapsed time, and the integrity-check outcome.

#### `knowledge.fold` — budget-constrained selection

```
fold(candidates: [{id, score, size, content?, category?}, ...], budget: N, min_score?: 0.0, category_weights?: {}) → {selected: [...], total_size: N}
```

Greedy knapsack: sorts candidates by score-density (score/size), applies category
weight multipliers, filters by `min_score`, then packs greedily until budget is
exhausted. Pure computation — no database access.

#### `knowledge.search` — TF-IDF ranked search

```
search(query, type?, status?, exclude_status?, include_drafts?: false, role?, limit?: 10, min_score?: 0.0, weights?: {}, decompose?: false, decompose_threshold?: 4, intersection_bonus?: 0.25, rerank?: true, rerank_alpha?: 0.7) → {results: [...], total: N, candidate_provenance: {...}}
```

FTS5 recall → in-memory TF-IDF scoring across name, tags, and content
fields with configurable weights. Features:

- **Query decomposition**: splits long queries into sub-queries, scores each
  independently, and bonuses items that appear across multiple sub-queries. Opt in with `decompose=true`.
- **Embedding rerank** (default when embedder configured): blends TF-IDF scores with cosine
  similarity against the query embedding. `rerank_alpha` controls the blend (0.7 = TF-IDF dominant).
  Disable with `rerank=false`. No-op if no embedder is configured.
- **Role weighting**: prepends the agent role to the query for contextual scoring.

**Status filtering (amended 2026-06-10)**:

The atom status taxonomy is a closed set: `draft` | `reviewed` | `deprecated`. Atoms with no
status are treated as `reviewed` for scoring purposes. `verified` is a **section-level** status
only (set by the dispute-resolution flow); atom-level `verified` is not a valid public value.

By default, `knowledge.search` and `knowledge.suggest` exclude both `draft` and `deprecated` atoms
from results. This is the quality default — callers that index atoms before they are finalized
should not have those drafts polluting agent orientation or search results.

Parameter precedence (highest to lowest):

1. `status=<value>` — explicit filter: only atoms with this exact status are returned.
   `include_drafts` has no effect when `status` is set.
2. `exclude_status=<value>` — exclude this status; only effective when `status` is not set.
3. `include_drafts=true` — include `draft` atoms; `deprecated` remain excluded regardless.
4. Default (no status params) — excludes both `draft` and `deprecated`.

The exclusion policy applies to **all result sources**: FTS/SQL candidates (via SQL predicate)
and ANN-sourced candidates (filtered post-hydration before rerank). Every result regardless of
retrieval path goes through the same status gate.

`knowledge.suggest` and auto-`knowledge.compose` (which calls `suggest` internally) share the
same default exclusion. There is no `include_drafts` override on `suggest` — domain atoms in
draft state should not drive agent composition.

**Search score interpretation**: scores are request-relative ranking values, not calibrated
relevance probabilities or absolute presence signals. Use result rank together with
`candidate_provenance` and each result's `score_provenance`; no fixed numeric band establishes
relevance across queries.

Every `knowledge.search` result carries `score_provenance` with these fields:

- `sources`: a stable-order subset of `["lexical", "ann"]`. A hit present in both candidate
  sources retains both labels after RRF fusion.
- `embedding_rerank`: whether a successful embedding rerank transformed this hit's score.
- `normalization`: `"s_over_s_plus_1"`. Search applies the monotonic `s / (s + 1)` squash to
  the score before the status multiplier and final `min_score` filter.
- `calibrated`: `false`.

The response's `candidate_provenance.lexical` records the lexical candidate-stage outcome:
`matched` for eligible FTS candidates, `exact_name` for indexed short-query recovery,
`no_match` for no lexical match in the caller's namespace,
`filtered` for matches removed by eligibility, `partial_timeout` when a timed-out fetch retains
eligible candidates or decomposed passes mix completion and timeout, and `timed_out` when a
fetch times out with no retained candidates (or every decomposed pass does so).
Completed empty terms alone do not make a fetch partial. These states supplement the
lexical timeout diagnostics.

`candidate_provenance.terms_truncated` reports whether any pass exceeded the shared
32-term allowance described above; the lexical state applies only to admitted terms.

`candidate_provenance.fallback` is `ann` only when the returned set has ANN evidence and no
returned hit has lexical evidence; otherwise it is `none`, including for an empty result.
A genuine lexical miss returns no lexical candidates instead of ranking unrelated recent
corpus rows. A healthy ANN leg can still supply explicitly labeled ANN-only results. Bounded
eligibility recovery for actual FTS matches remains part of lexical candidate retrieval.

#### `knowledge.compose` — namespace-consistent briefing composition

```
compose(query, namespace?, domain_ids?, atom_ids?, blend_kg?, auto_limit?, max_tokens?, explain?) → {status, data}
```

When `namespace` is present, every data-dependent leg is scoped to that exact namespace,
including the bound brain-profile read that supplies section-type weights. Identical atom or
domain slugs in another namespace cannot enter the briefing. When absent, existing behavior is
unchanged.

### 3. `learn` — concept registration with domain promotion

```
learn(name, description?, domain?, tags?) → {id, full_id, kind, name, domain, tags, ...}
```

- `name` is required and must be non-empty after trimming.
- `domain`, if provided, is stored in `properties.domain` **and** appended to `tags`
  unless already present. This ensures the domain is reachable via both structured queries
  and FTS.
- `tags` accepts an explicit list; the domain tag is merged in, not replaced.
- `learn` is **not idempotent**. Calling `learn(name="LoRA")` twice creates two entities.
  Callers that need idempotent registration should use `topic(query="LoRA", limit=1)` first
  and fall back to `learn` only when no result is found. This is documented in the SKILL.md
  anti-patterns section; the verb intentionally does not add the round-trip overhead by
  default.

### 4. `cite` — provenance citation

```
cite(concept_id, source_id, weight?) → {id, full_id, relation, concept_id, source_id, weight}
```

- `concept_id` is the concept being introduced (graph-source in `introduced_by` terms).
- `source_id` is the document (e.g. a paper), person, or org that introduced it (graph-target).
- Both accept full UUID or 8-char hex prefix (via `resolve_prefix`).
- `weight` defaults to `1.0` (definitional). Values outside `[0.0, 1.0]` are **silently
  clamped**. This is consistent with how other handlers treat weight: the substrate does
  not admit out-of-range weights; clamping is preferable to an error for an optional
  quality annotation. The effective weight is reflected in the response.
- The underlying edge relation is `EdgeRelation::IntroducedBy` (ADR-002). The pack does
  not bypass the closed edge ontology.

### 5. `topic` — concept browsing

```
topic(domain?, query?, limit?) → {results: [...], total: N}
```

- Without `query`: lists all concepts in the namespace up to `limit`.
- With `query`: runs hybrid FTS+vector search scoped to `kind="concept"`, then optionally
  post-filters by `domain` tag.
- `limit` defaults to 20 and is capped at 100. The cap is applied silently; the response
  reflects the capped limit via `items` and `total`.
- The domain filter is case-insensitive tag match (`eq_ignore_ascii_case`).

The [2026-09-14 limit-report amendment](#amendment-2026-09-14-knowledge-list-and-topic-limit-reports)
replaces this section's silent-cap, `items`, and cap-through-`total` description
on acceptance, with separate definitions for the two existing `total` values. The
[2026-09-15 candidate-window amendment](#amendment-2026-09-15-topic-query-candidate-window-counts)
then renames the queried one: the signature above holds for the unqueried branch,
and a request carrying a non-null `query` returns `candidate_window_count` in place
of `total`.

### 6. Pack dependency declaration

The pack declares `REQUIRES: &["kg"]`. The runtime enforces this at boot: loading
`knowledge` without `kg` fails with a dependency error. The concept tier delegates
entity CRUD to the `kg` pack; the corpus tier operates on its own tables
(`knowledge_atoms`, `knowledge_domains`) via direct SQL through the runtime's
`SqlAccess` trait.

### 7. Binary wiring

`crates/khive-mcp/Cargo.toml` declares `khive-pack-knowledge` as a direct dependency.
`crates/khive-mcp/src/pack.rs` re-exports `KnowledgePack` under a `#[doc(hidden)]` alias
to force-link the crate so `inventory::submit!` constructors run. This is the standard
pattern for all first-party packs in this binary.

`scripts/publish.sh` includes `khive-pack-knowledge` after `khive-pack-schedule` and
before `khive-pack-template`, reflecting the dependency ordering.

## Consequences

### Accepted trade-offs

- `learn` creates duplicates on repeated calls. The idempotency round-trip is the caller's
  responsibility. This is consistent with how `create` works; the pack is sugar, not a
  new semantic contract.
- `cite` silently clamps weight. An invalid weight is a caller error on an optional
  annotation; clamping over rejecting avoids breaking batch ingestion pipelines.
- `topic` has a hard cap of 100. Callers who need more than 100 concepts should page via
  `list(kind="concept", ...)` from the kg pack directly.

### What this ADR does NOT cover

- Idempotent variant (`learn_or_get`) — deferred; no current demand from agent workflows.
- `weight_requested` surfacing in `cite` response — deferred; low-priority annotation.
- Pagination for `topic` — callers who need full pagination should use the kg pack's
  `list(kind="concept")` which has explicit `offset` support.
- ADR amendment for ADR-002 or ADR-001 — not needed; the knowledge pack uses existing
  kinds and relations only.

## Alternatives considered

### Extend kg handlers with domain-aware variants

Rejected. Adding `domain` auto-promotion to `create` would impose the research-agent
convention on callers who use `create` for non-research purposes. The pack model exists
precisely to keep the kg substrate neutral and compose opinionated layers above it.

### Single `knowledge` verb dispatched by sub-command

Rejected. A single entry-point with a `kind` discriminant (`knowledge(action="learn",
...)`) violates the verb-flat interface (ADR-015). The three verbs are distinct
speech acts (two Commissive, one Assertive per ADR-025); flattening them degrades
discoverability.

### Introduce a `concept` note kind alongside the entity kind

Rejected. Research concepts are entities (named, structured, graph-connected). Notes are
for context and observations _about_ entities, not for the entities themselves. The
existing `concept` entity kind in ADR-001 is the correct substrate; no new kind is needed.

## Amendment (2026-09-14): knowledge list and topic limit reports

**Status: Accepted (2026-09-14).**
**Related issue:** #2679.

This amendment adds numeric normalization reports to `knowledge.list` and
`knowledge.topic` and proposes the associated structural empty-result behavior
in [ADR-045 Amendment 5 (2026-09-14)](ADR-045-verb-response-presentation.md#amendment-5-2026-09-14-structural-knowledge-limit-envelopes).
Both decisions were approved on 2026-09-14; the accepted parent ADRs retain
their status. Acceptance of the text is not implementation acceptance: the
dependent implementation lands on its own gates.

On acceptance, this amendment extends §2's `knowledge.list` response signature
and limit description. It replaces only §5's statement that the topic cap is
silent and reflected through `items` and `total`, and clarifies that section's
two totals. All other input, retrieval, pagination and storage decisions remain
in force. It does not specify other verbs' numeric reports.

### Defaulted request and effective setting

Every successful payload carries three non-null scalar siblings:
`requested_limit`, `effective_limit`, and `limit_clamped`. The first records the
accepted typed argument after its existing default; the second records the
existing handler output ceiling; the boolean is their inequality.

| Verb              | Existing accepted type | Omitted or null request | Effective limit                 | Explicit zero   |
| ----------------- | ---------------------- | ----------------------- | ------------------------------- | --------------- |
| `knowledge.list`  | `Option<usize>`        | 20                      | `requested_limit.clamp(1, 500)` | `0 / 1 / true`  |
| `knowledge.topic` | `Option<u32>`          | 20                      | `requested_limit.min(100)`      | `0 / 0 / false` |

Exact bounds and unchanged requests report `limit_clamped: false`. A list
request 501 reports `501 / 500 / true`; a topic request 101 reports
`101 / 100 / true`. These values describe the setting, even when no rows match;
they are not result counts, total counts, internal candidate-work budgets or
promises that the page is full.

Input types, defaults, decoding, validation order and error behavior do not
change. Negative integers, incompatible JSON types and values outside each
existing unsigned domain still fail. In particular, a 64-bit `usize` list input
above `u32::MAX` remains valid and clamps before SQL conversion; topic continues
to reject values above `u32::MAX`. No common `u32` conversion may narrow list's
accepted domain. The report keys are output-only: they are neither input
parameters nor allowed record projection fields. Failed requests do not acquire
a fabricated success report.

### Six successful response paths

Each path below adds the same three siblings to its existing object; no wrapper
is introduced. The legacy list `limit` remains present and equals
`effective_limit` on every list success, including empty or exhausted pages.

| Path                  | Existing payload fields retained               | Meaning preserved                                                                                           |
| --------------------- | ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| Atom list, offset     | `results`, `total`, `limit`, `offset`, `order` | Exact-namespace matching live-atom count; descending created_at/id order and existing status/mirror filters |
| Domain list, offset   | `results`, `total`, `limit`, `offset`, `order` | Exact-namespace live-domain count; descending created_at/id order                                           |
| Atom list, cursor     | `results`, `limit`, `order`, `next_after`      | Ascending created_at/id walk, existing atom filters, no `total` or `offset`                                 |
| Domain list, cursor   | `results`, `limit`, `order`, `next_after`      | Ascending created_at/id walk, no `total` or `offset`                                                        |
| Topic with `query`    | `results`, `total`                             | Filtered candidate-window count before output truncation                                                    |
| Topic without `query` | `results`, `total`                             | Full matching caller-visible concept count before output limit                                              |

List retains the `type`/`kind` alias, omitted/null `after` offset mode, full-UUID
cursor rules, mutually exclusive after/offset parameters, strict per-kind row
projection, unchanged SQL predicates and ordering, effective-plus-one keyset
fetch and existing continuation construction. Only rows are projected; report
siblings remain present. No additional count, refill or retrieval query is
introduced to produce the report.

Topic continues to resolve shared graph reads through `core()`, including
query, hydration, count and listing. A present empty or whitespace query still
takes the search branch; omitted/null query takes listing. Domain normalization
and post-filtering remain unchanged. Query requests retain the existing
`effective_limit * 4` candidate bound, hydration, domain filtering and final
take. Their `total` is the resulting candidate-window length before that take,
not the whole matching corpus; excluded candidates are not replenished to fill
the output. Listing retains its full visible matching count. Neither total is
replaced with `effective_limit` or the number of returned rows.
A corpus-true total for the query branch is tracked separately in #2732.

Topic zero continues through the existing selected branch, including its reads
and possible errors. A successful query zero has empty results and total 0; a
successful listing zero has empty results while total may be positive. No new
zero shortcut or namespace unification is authorized.

### Presentation and compatibility

[ADR-045 Amendment 5 (2026-09-14)](ADR-045-verb-response-presentation.md#amendment-5-2026-09-14-structural-knowledge-limit-envelopes)
proposes retaining envelope `results: []` for Agent offset-list and topic
successes once all three report fields are present. This is an explicit
observable change, separate from the numeric metadata. Cursor empty results
and null continuation already survive presentation and retain that behavior.
Row transformations, other empty fields and existing format rules are unchanged.

After removing only the three new siblings, canonical legacy fields and values
must equal the prior payload for the same effective setting. For Agent empty
offset-list/topic payloads, the sole additional allowed difference is the
newly retained `results: []`. Additive reports need not preserve rendered bytes.

### Falsifiable acceptance and mutation arms

- **K-LIMIT:** All four list and both topic routes prove omitted/null/default,
  lower/exact/upper bounds, zero and typed overflow behavior, with an
  effective-value twin. A wide 64-bit list request must remain admitted while
  the same out-of-u32 value fails topic. Isolated mutations to a default, cap,
  zero rule, inequality flag or list-width conversion must fail these witnesses.
- **K-ENVELOPE:** Empty, populated and exhausted successes retain every legacy
  field and report the original request independently of result length. Strict
  row projection retains the siblings but rejects them as projected fields or
  inputs. Omitting a report at any return, reporting result length, replacing
  legacy `limit`, or broadening a projection/input allowlist must be detected.
- **K-PAGE:** Atom/domain offset and cursor controls establish exact IDs, order,
  counts, continuation, live/status/namespace filtering and all existing cursor
  errors before checking reports. Removing cursor lookahead, reversing the ID
  tiebreak or relaxing an existing filter must fail the corresponding controls;
  source review must also reject added reads.
- **K-TOTAL:** A controlled query corpus larger than the candidate bound must
  distinguish the candidate-window total from both output size and corpus size;
  listing must retain its full matching total when output is limited. Zero and
  empty/filter-empty arms need
  populated positive controls. Substituting output length, corpus count or
  effective limit for a branch total, or changing the 4× candidate request, must
  fail the corresponding exact witness.
- **K-ROUTE:** Query absent/null versus empty string, normalized domain,
  secondary-pack/core routing and a matching domain candidate outside the
  initial four-hit window establish existing behavior. Refill, premature domain
  filtering, non-core reads or changed branch selection must be detected.
- **K-PRESENT:** Satisfy ADR-045 Amendment 5's complete presentation/format
  matrix and real MCP empty-result witness. Removing or renaming a report on an
  empty offset-list/topic success must make that witness fail without modifying
  the renderer.

Freeze the named test and individual mutant mapping before execution. Establish
all legacy fixture, count, ordering and error controls on baseline production
before the first expected missing-report or newly structural-array failure;
observe and retain that baseline before production edits. Exact-count/rank
fixtures that cannot establish their positive control require correction and
refreezing, not weakened inequalities. Fixed runs must pass the same controls.
A mutant kill requires the intended semantic assertion with a nonzero selected
test count; compilation, setup failure and unrelated errors do not qualify.
No test or mutation result is claimed by this contract.

## Amendment (2026-09-15): topic query candidate-window counts

**Status: Accepted (2026-09-15).** Related issue: #2732. This follow-on extends
the accepted 2026-09-14 knowledge list/topic limit-report contract. Its
implementation proceeds independently of the limit-report implementation; this
amendment does not claim that those reports are already implemented.

The accepted limit-report amendment deliberately retains `total` for two different quantities.
This amendment supersedes only the queried topic count name: a successful
`knowledge.topic` with a non-null `query` returns `candidate_window_count` instead
of `total`. The count is the hydrated, domain-filtered candidate-window length
before the final output take. It is bounded by the existing
`effective_limit * 4` search request and is not a corpus-wide match count or an
indication that a page walk can enumerate all matches. An empty or whitespace
query remains on the query branch. Successful empty and zero-limit query results
report `candidate_window_count: 0` and omit `total`.

Without `query`, or with `query: null`, listing retains its full matching
caller-visible concept `total` and omits `candidate_window_count`. A zero output
limit may still report a positive listing total. The names are mutually exclusive;
retaining the query `total` as an alias would retain the original ambiguity.

Result shape and order, scores/snippets, branch selection, core-backend routing,
namespace visibility, hydration, domain normalization/post-filtering, candidate
bound and final take are unchanged. Wherever the three accepted limit-report
fields are implemented, preserve them and their contract semantics; this
amendment does not add those reports.
No additional count, search, refill or storage operation is introduced. This is
an explicit response-key migration: callers using query `total` must move to
`candidate_window_count`; the new field remains output-only under the existing
strict parameter decoder. Unqueried consumers keep using `total`.

Acceptance requires a real indexed corpus with matching count greater than the
candidate window, itself greater than returned rows; exact queried count and
key-absence assertions must distinguish all three. A domain-filtered control
must measure only the surviving initial window and show no refill from matching
concepts outside it. Unqueried/null-query, zero and empty controls preserve the
full-count semantics. Wherever limit reports and their queried-count witnesses
are implemented, retain the reports and migrate only the queried-count key;
the witnesses' numeric expectations and other controls remain unchanged.

Native result: the arms added in #2818 witness all four requirements above. A
17-concept corpus, a 12-candidate window and 3 returned rows are asserted with
strict inequalities between all three, not only at the endpoints. The queried
branch asserts `candidate_window_count == 12` and the absence of `total`; the
listing branch asserts `total == 17` and the absence of `candidate_window_count`.
The domain-filtered control tags four of a thirteen-deep ranking at positions 0,
2, 5 and 8, requests a four-candidate window, and asserts
`candidate_window_count == 2` against a listing total of 4, so two matches inside
the window and two outside it show that the filter does not refill.
