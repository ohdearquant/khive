# ADR-039: Bulk Import Adapters

**Status**: accepted (file-level `khive kg import/export` implemented; D1 MCP import verb rescinded; bulk import is CLI-level only)\
**Date**: 2026-05-19\
**Authors**: khive maintainers

## Context

khive's value proposition is "structured persistence for research." The KG pack defines 6 entity
kinds (ADR-001), 13 edge relations (ADR-002), and full CRUD + curation operations (ADR-014). Yet
every instance starts with an empty graph. The only path to seed it is one-at-a-time `create(kind=
document, ...)` calls — impractical for a researcher arriving with a Zotero export, a literature
survey's arxiv ID list, or a Notion dump.

GitHub issue #3 documents this gap. The expected sources are:

- **arxiv ID lists** (e.g., the 40 papers a literature survey covers)
- **BibTeX files** (Zotero exports, Mendeley exports)
- **DOI lists** (Crossref-resolvable references)
- **JSON-LD or KgArchive dumps** (cross-instance portability — symmetric with `export_kg`,
  ADR-015)

Bulk import also has a security dimension filed as GitHub issue #28: the existing `import_kg`
implementation in `crates/khive-runtime/src/portability.rs` writes edges via `graph.upsert_edge`
with **no endpoint validation**. An untrusted archive can therefore insert dangling edges, silently
violating the referential-integrity invariant established for `link` in `handlers.rs` (lines
760-769). Any bulk import path defined by this ADR must close this hole.

Two constraints bound the design space:

1. **Closed edge ontology (ADR-002, ADR-021)**: only the 13 canonical `EdgeRelation` variants are
   legal. Bulk import must reject — or skip with audit — any edge whose `relation` field does not
   parse to a valid `EdgeRelation`.
2. **Pack-extensible endpoint rules (ADR-031)**: the base contract (`entity→entity` for 11
   relations, same-substrate for `supersedes`, `note→any` for `annotates`) can be broadened by
   packs but never tightened. Bulk import validation must consult the same
   `validate_edge_relation_endpoints` path that `link` uses, after installed pack rules are loaded.

## Decision

### D1 — Ingestion surface: generic `import` verb on the `kg` pack

Add a single `import` verb to `khive-pack-kg`. It accepts pre-typed records and edges in a
standardised JSON shape, validates everything (endpoint contract, relation legality, referential
integrity), and writes atomically.

**Wire shape:**

Function-call form (ADR-020):

```
import(
  records=[{"id":"local:paper-1","kind":"document","name":"FlashAttention: Fast and Memory-Efficient Exact Attention","description":"...","properties":{"arxiv_id":"2205.14135","authors":"Tri Dao, Daniel Y. Fu, ...","year":2022,"doi":"10.48550/arXiv.2205.14135"},"tags":["attention","efficiency"]}],
  edges=[{"source_ref":"local:concept-flash-tiling","target_ref":"local:paper-1","relation":"introduced_by","weight":1.0}],
  dry_run=false,
  on_conflict="error"
)
```

JSON request form (equivalent):

```json
{
  "tool": "import",
  "args": {
    "namespace": "string (optional, default: caller's namespace)",
    "records": [
      {
        "id": "local:paper-1",
        "kind": "document",
        "name": "FlashAttention: Fast and Memory-Efficient Exact Attention",
        "description": "...",
        "properties": {
          "arxiv_id": "2205.14135",
          "authors": "Tri Dao, Daniel Y. Fu, ...",
          "year": 2022,
          "doi": "10.48550/arXiv.2205.14135"
        },
        "tags": ["attention", "efficiency"]
      }
    ],
    "edges": [
      {
        "source_ref": "local:concept-flash-tiling",
        "target_ref": "local:paper-1",
        "relation": "introduced_by",
        "weight": 1.0
      }
    ],
    "dry_run": false,
    "on_conflict": "error"
  }
}
```

**Field semantics:**

- `records`: array of entity descriptors. `id` is a _local ref_ — a caller-assigned string valid
  only within this import payload (format: any string, used to resolve `source_ref` / `target_ref`
  in `edges`). It is **not** stored; the runtime assigns a fresh UUID on creation. Alternatively,
  `id` may be a full UUID; if a record with that UUID already exists in the namespace the
  `on_conflict` policy applies.
- `edges`: array of edge descriptors. `source_ref` and `target_ref` are either a local ref from
  the `records` array (resolved by position) or a UUID already live in the namespace. The
  `relation` field must be a valid `EdgeRelation` string (ADR-002, ADR-021); invalid values cause
  the entire import to fail. Direction follows ADR-002 conventions — for example,
  `introduced_by` runs from concept (source) to paper (target), not the reverse; the example
  above shows `local:concept-flash-tiling → local:paper-1` with `"introduced_by"`. Bulk import
  enforces the same direction rules as the `link` verb.
- `dry_run` (optional, default `false`): validate everything, return what would be inserted, but
  do not commit. Returns `{would_insert: {entities: N, edges: M}, errors: [...]}`.
- `on_conflict` (optional, default `"error"`): how to handle a record whose `(kind, name)` matches
  an existing entity — `"error"` (reject entire import), `"skip"` (omit this record and its
  incident edges; continue), or `"update"` (patch the existing entity with the new record's fields
  via the same `EntityPatch` semantics as `update(kind=entity)`).

### D2 — Atomicity: all-or-nothing per call

The entire `import` call is transactional. If any validation gate fails — illegal `relation`,
dangling endpoint, referential integrity violation, `on_conflict=error` collision — no records and
no edges are written. The caller receives a structured error identifying the first failing item.

Rationale: partial writes are harder to reason about than full rollback. Callers can split large
payloads into smaller batches if they want partial progress.

**Batch size cap**: maximum 10,000 records per call (configurable via `RuntimeConfig.import_max_records`,
default 10,000). Exceeding the cap returns `InvalidInput` before any validation begins.

### D3 — Edge endpoint validation (closes issue #28)

Every edge in the `edges` array goes through `validate_edge_relation_endpoints` — the same
function that `link` calls at lines 760-769 of `handlers.rs`. Both endpoints must resolve to a
live record: either present in the current `records` array (resolved by local ref) or already
present in the target namespace (resolved by UUID lookup). A dangling reference causes the entire
import to fail (honoring D2 atomicity).

This also retroactively fixes the existing `import_kg` path in `portability.rs`: the new `import`
verb replaces `import_kg` as the sanctioned bulk write entry point; `import_kg` should gain the
same validation pass in a follow-up patch before the next release.

### D4 — Idempotency properties

| `on_conflict`     | Re-running same payload                                   | Notes                                        |
| ----------------- | --------------------------------------------------------- | -------------------------------------------- |
| `error` (default) | Fails on second run — name collision detected             | Intentionally non-idempotent                 |
| `skip`            | Idempotent — existing records are skipped, no errors      | Safe for repeated seeding                    |
| `update`          | Idempotent only if content is unchanged; writes on change | Content-equality not checked; always patches |

Agents building reproducible seeding pipelines should use `on_conflict=skip`.

### D5 — Audit event

Each successful `import` dispatch (including dry-run) emits a structured tracing event via the
same `tracing::info!` mechanism as ADR-033 gate checks. The event carries:

```json
{
  "verb": "import",
  "dry_run": false,
  "on_conflict": "skip",
  "records_submitted": 42,
  "records_written": 41,
  "records_skipped": 1,
  "edges_written": 38,
  "content_hash": "sha256:4a7f..."
}
```

`content_hash` is the SHA-256 of the canonical JSON of the `records` + `edges` arrays (same
content-addressing scheme as ADR-015 snapshots). It allows re-import detection and
cross-instance traceability without storing the full payload.

### D6 — Format compatibility with ADR-015 export/import

The `records` + `edges` payload shape is intentionally designed as a strict subset of the
`KgArchive` format defined in `crates/khive-runtime/src/portability.rs`. The `import` verb
accepts a bare `{records: [...], edges: [...]}` envelope, or a full `KgArchive` JSON (the runtime
detects the `"format": "khive-kg"` header and routes to the same handler). This means:

- A future `export_kg` snapshot is directly consumable by `import` without transformation.
- The portability layer (ADR-015 §C) gains `import` as its write-side counterpart.

Field mapping: `KgArchive.entities` → `records`, `KgArchive.edges` → `edges`. Property names in
the record match `Entity` field names from `khive-types`.

### D7 — Reference adapters (separate `khive-import` crate, separate PR)

The `import` verb defines the _runtime contract_. External adapters transform source data into
the `{records, edges}` envelope. Two reference adapters are specified here — implementation is a
separate PR scope:

**Adapter A: arxiv**

- Input: list of arxiv IDs (e.g., `["2205.14135", "2307.08691"]`)
- Process: for each ID, call the arxiv API (`export.arxiv.org/abs/<id>` or OAI-PMH feed) to
  retrieve title, authors, year, abstract, DOI
- Output record shape:
  ```json
  {
    "kind": "document",
    "name": "<title>",
    "description": "<abstract (truncated to 2000 chars)>",
    "properties": {
      "arxiv_id": "2205.14135",
      "title": "<full title>",
      "authors": "Dao et al.",
      "year": 2022,
      "doi": "10.48550/arXiv.2205.14135",
      "url": "https://arxiv.org/abs/2205.14135"
    },
    "tags": []
  }
  ```
- Edges: none generated automatically (relationship extraction is out of scope; see issue #3's
  "NER + relation extraction — defer to a separate research-extractor MCP" note)
- Error mode: per-ID failure is logged and skipped; the adapter returns all successfully-fetched
  records as a single payload
- Rate limiting: respects arxiv's polite use policy (1 req/s, 3s back-off on 429)

**Adapter B: BibTeX**

- Input: `.bib` file path or BibTeX string
- Process: parse each entry using a BibTeX parser (e.g., `nom_bibtex` crate or equivalent)
- Output record shape: same as arxiv adapter; maps BibTeX fields: `title` → `name`, `abstract` →
  `description`, `author` → `properties.authors`, `year` → `properties.year`, `doi` →
  `properties.doi`, `url` → `properties.url`, `journal`/`booktitle`/`howpublished` →
  `properties.venue`
- Entry type `@article`, `@inproceedings`, `@misc`, `@preprint` → `kind=document`; unknown types
  → `kind=concept` with `properties.type="bib_entry"` and the raw entry type recorded
- Error mode: malformed entries are skipped with a warning; valid entries proceed

Both adapters live in a new `crates/khive-import/` crate (or equivalently, as separate binaries
at `crates/khive-import-arxiv/` and `crates/khive-import-bibtex/`), and are NOT dependencies of
`khive-pack-kg`. They call `import(...)` via the standard MCP `request` tool — no privileged
access to internal runtime state.

The community may ship additional adapters (DOI/Crossref, Semantic Scholar, Zotero JSON, JSONL
dumps) without requiring changes to the `khive-pack-kg` crate or any published permissions.

## Rationale

### Why generic `import` verb + reference adapters (not built-in adapters)

The alternative of embedding arxiv/BibTeX logic inside `khive-pack-kg` couples the network access
pattern (HTTP requests to external APIs) to the storage layer. This violates the principle that
packs are pure validators and storage orchestrators. The generic verb approach:

- Keeps `khive-pack-kg` compilable in offline environments.
- Lets the community ship adapters for sources we haven't anticipated without crate.io publishing
  rights to the `khive-pack-kg` namespace.
- Separates the transformation concern (adapter) from the validation concern (runtime).

The tradeoff is one additional crate (`khive-import`). Acceptable given the benefit.

### Why all-or-nothing and not per-record

Per-record atomicity (write successes, accumulate failures) sounds more resilient, but it
produces a harder-to-reason-about final state: the caller must diff what was written against what
was submitted to find the failures. All-or-nothing with a clear error message is operationally
simpler. Callers who need partial progress can split payloads — the 10,000-record cap encourages
batching already.

### Why `on_conflict=error` as default

Defaulting to `skip` would silently succeed on duplicate runs, masking ingestion errors.
Defaulting to `update` would silently mutate existing records when the same-named entity in the
archive has slightly different metadata. `error` is the loudest default: it forces the caller to
make an explicit choice on second import. This aligns with the "no silent coercion" principle in
`CLAUDE.md`.

### Why local refs instead of pre-assigned UUIDs

Callers building an import payload from an external source (e.g., BibTeX) don't have khive UUIDs
for newly-created records. Forcing UUID pre-assignment would require a round-trip to reserve IDs
before sending the payload — eliminating the batching benefit. Local refs are a single-payload
concept: they resolve within the import call and are discarded; the runtime owns UUID assignment.

### Why content_hash in the audit event

An import is not a mutation on a known ID — it creates N new records. Without a content hash,
re-import detection requires either re-running with `dry_run=true` (expensive for large payloads)
or storing the full payload (prohibitive). The SHA-256 hash is cheap to compute and gives
operators a stable token for "did this payload already land?" without storing raw data.

## Alternatives Considered

| Alternative                                                              | Pros                              | Cons                                                                                                                                             | Why rejected                                                       |
| ------------------------------------------------------------------------ | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------ |
| Built-in adapters inside `khive-pack-kg`                                 | Single crate installation         | Couples HTTP/network deps to storage layer; offline environments fail to compile; community adapters require core maintainer involvement         | Wrong coupling                                                     |
| Separate crate per adapter (`khive-import-arxiv`, `khive-import-bibtex`) | Maximum separation                | Every new source = new crate; install burden multiplies                                                                                          | Prefer one `khive-import` crate with subcommands                   |
| Out-of-tree only (community responsibility, no reference adapter)        | Zero new code in repo             | Leaves new users with an empty KG and no example                                                                                                 | Poor onboarding; issue #3 explicitly asks for a built-in path      |
| Per-record atomicity (write successes, collect failures)                 | Partial progress on large imports | Ambiguous final state; caller must reconcile                                                                                                     | All-or-nothing with clear errors is simpler                        |
| `on_conflict=skip` as default                                            | Re-running is always safe         | Silent duplicate masking                                                                                                                         | Loudest default; caller must opt into skip                         |
| Validate endpoints post-import (integrity sweep)                         | Decoupled from write path         | Transient inconsistent state exists between write and sweep                                                                                      | Transient dangles violate CLAUDE.md invariant immediately          |
| Dedicated `_imports` audit table (new DB migration)                      | Queryable import history          | ADR-033 tracing events already cover the audit requirement for v0.1; a table adds migration and query surface for marginal benefit at this stage | Defer; ADR-022 migration mechanism is available if needed in v0.2+ |

## Consequences

### Positive

- Researchers can seed a khive instance from existing reading lists in one call.
- Issue #28 is closed: the `import` verb enforces the same endpoint validation as `link`.
- The payload shape is compatible with ADR-015 `KgArchive`, making export/import symmetric.
- Community adapters (DOI, Semantic Scholar, Notion, Roam) compose with the generic verb without
  touching core crates.
- `dry_run=true` gives safe preview before committing bulk writes.
- The `on_conflict` axis gives agents explicit idempotency control.

### Negative

- All-or-nothing atomicity means a single bad edge in a 1,000-record payload fails the entire
  call. Callers must pre-validate or split payloads.
- The 10,000-record cap may require multi-call batching for large corpus imports. This is
  intentional: it bounds per-call memory pressure and gives progress reporting at batch
  boundaries.
- Adding `import` to `khive-pack-kg` extends the pack's handler surface by one verb. Acceptable;
  the 11 existing verbs set the pattern.

### Neutral

- The `khive-import` crate has no version coupling to `khive-pack-kg` — it is a standalone CLI
  that calls the runtime via the `request` tool. Breaking changes to the `import` verb wire shape
  are versioned independently.
- The existing `import_kg` function in `portability.rs` is not removed by this ADR; it is
  superseded as the recommended bulk write path. A follow-up patch should add endpoint validation
  to `import_kg` for defensive depth.

## Implementation

### Files that change (this ADR's scope — no code in this PR)

| File                                          | Change                                                                                                                                                                                                  |
| --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/khive-pack-kg/src/handlers.rs`        | Add `handle_import` handler following the `handle_create` / `handle_link` pattern (lines 332-769 are the model); deser `ImportParams`, validate records + edges, dispatch to `runtime.import_bulk(...)` |
| `crates/khive-runtime/src/operations.rs`      | Add `import_bulk(namespace, records, edges, on_conflict, dry_run) -> ImportResult`; orchestrates `create_entity` + `link` loops inside a single SQLite transaction                                      |
| `crates/khive-pack-kg/src/lib.rs`             | Register `"import"` verb in `VERBS` const                                                                                                                                                               |
| `crates/khive-types/src/lib.rs` (or new file) | Add `ImportRecord`, `ImportEdgeSpec`, `ImportResult`, `OnConflict` types                                                                                                                                |

### New crate (separate PR)

`crates/khive-import/` — reference adapters. Not a dependency of any existing crate. Calls the
runtime via the `request` MCP tool. Ships as an optional binary alongside `khive-mcp`.

### Schema migrations

No new tables required for v0.1. If a queryable import history becomes necessary in v0.2+, add a
`_import_runs` table via the ADR-022 `VersionedMigration` mechanism:

```sql
-- hypothetical v{N}
CREATE TABLE _import_runs (
    id          TEXT PRIMARY KEY,
    namespace   TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    dry_run     INTEGER NOT NULL,
    on_conflict TEXT NOT NULL,
    records_written INTEGER NOT NULL DEFAULT 0,
    edges_written   INTEGER NOT NULL DEFAULT 0,
    records_skipped INTEGER NOT NULL DEFAULT 0,
    imported_at     INTEGER NOT NULL
);
```

This table is deferred; the ADR-033 tracing event carries the same information for v0.1.

## Amendments

### Amendment 2026-05-20 — D1 superseded; scope narrowed to CLI-level import semantics

**Status**: partially superseded (D1 MCP verb rescinded; CLI import/export implemented at file level)\
**Date**: 2026-05-20\
**Rationale**: Ocean directive (2026-05-20) established that bulk import is a CLI-level
operation, not an MCP-exposed verb. Additionally, the `khive kg import/export` CLI commands
are Phase C2 deferred (not yet implemented), so any claim that they are implemented is
incorrect. The original D1 decision (adding `import` to the MCP surface) is rescinded.\
**Affected sections**: §D1 (import verb — rescinded); §Status field (remove "implemented" claim
for CLI import/export); ADR-023 amendment to add `import` to KG pack verb set (rescinded)

**Changed**: The original D1 decision — adding an `import` verb to `khive-pack-kg` exposed
through the MCP `request` tool — is superseded by Ocean's directive (2026-05-20): bulk import
is a CLI-level operation, not an MCP-exposed verb. It is inappropriate to route large file-based
batch operations through the agent MCP surface.

**What this means**:

- The `import` verb is NOT added to `khive-pack-kg` or dispatched via the `request` tool.
- The original ADR-023 amendment text below (which added `import` to the KG pack verb set) is
  rescinded. ADR-023's verb surface remains 11 verbs; this ADR does not amend it.
- The remaining scope of ADR-039 is: CLI-level import semantics — the validation pipeline, the
  `{records, edges}` payload contract, atomicity guarantees (D2), endpoint validation (D3),
  idempotency options (D4), and the reference adapter design (D7). These apply to `khive kg import`
  as a CLI command (Phase C2, deferred) per ADR-048's command surface.
- The `import` verb wire shape defined in D1 (function-call form, JSON request form, field
  semantics) is preserved as the contract for the CLI command's input format; only the transport
  layer changes (CLI stdin/file argument, not MCP request).

**Status of original ADR-023 amendment**: rescinded. The sentence below beginning "This ADR
amends ADR-023..." no longer applies. ADR-023's verb count stays at 11.

## References

- GitHub issue #3: Bulk import — arxiv / BibTeX / DOI → entities (this ADR closes it)
- GitHub issue #28: `import_kg` accepts dangling edges from untrusted archives (this ADR closes
  it; the `import` verb enforces endpoint validation)
- ADR-001: Entity Kind Taxonomy — `document` is the correct kind for paper records; no new kind
  is introduced
- ADR-002: Closed Edge Ontology — 13 canonical relations; bulk import must produce only legal
  triples
- ADR-014: Curation Operations — `create_entity` + `link` are the primitive operations `import`
  composes; `EntityPatch` semantics apply for `on_conflict=update`
- ADR-015: KG Versioning and Portability — `import` payload is a subset of `KgArchive`; the
  `export_kg` / `import_kg` symmetry is extended here
- ADR-021: EdgeRelation Enum — `EdgeRelation::from_str` is the validation gate for all relation
  strings in the `edges` array
- ADR-022: Schema Migrations — pattern for the optional `_import_runs` table if added in v0.2+
- ADR-031: Pack-Extensible Edge Endpoints — `validate_edge_relation_endpoints` is the shared
  gate; bulk import consults it after pack rules are installed (same path as `link`)
- ADR-033: Audit Envelope — `tracing::info!` emission pattern for the per-import audit event
- `crates/khive-pack-kg/src/handlers.rs` lines 757-769: `handle_link` — endpoint validation
  pattern that `handle_import` must replicate
- `crates/khive-runtime/src/portability.rs`: existing `import_kg` (bypasses endpoint validation;
  issue #28 target)
