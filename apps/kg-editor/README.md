# khive repository atlas + KG Studio

The default route is ADR-147's catalog-driven repository showcase. The browser first
discovers configured DB-backed analyses, merges them with its curated static set by
normalized repository URL, and offers the result in a native repository selector.
Entering a public repository URL still performs a local lookup only: a miss performs no
clone, forge request, or server-side execution. If the catalog is absent or unhealthy,
the curated checked-in `khive.repo.v1` golden remains usable and the degraded state is
visible. The original ADR-145 semantic review workbench remains available at `/review`.

The repository showcase renders all ten ADR-147 views at their declared granularity:
structure, history navigation, dependency topology, hotspots, hidden coupling, treemap,
cadence, ownership, de-facto API surface, and scorecard. Join-dependent views consume
only exporter-derived edges and aggregates. Symbol collections remain typed and empty;
unavailable, empty, derived, and truncated states are never collapsed.

Its browser boundary is a closed Zod model matching
`docs/schemas/khive-repo-v1.schema.json`. The public static asset is mechanically copied
from `docs/schemas/examples/khive-repo-v1-khive.json`, and tests assert byte identity.
The browser fetches only registry-owned same-origin assets, rejects bundles over 8 MiB,
and applies disclosed local display limits to large precomputed sections.

## Reproduce the showcase bundle

From the repository root:

```bash
scripts/generate-repo-showcase.sh
```

The underlying one-shot pipeline is `khive repo build`: pinned clone → cursor-exhausted
`git.digest` → separate `code.ingest` map → cross-store export.

For the DB-backed interview walkthrough, investigation path, and evidence boundaries, see
[DEMO.md](DEMO.md).

## KG review workbench

KG Studio is the first read-only vertical slice of khive's local-first semantic review
workbench. It presents an attributed KG change-set as graph changes, rule findings, evidence,
affected context, and an independent-review gate while Git and GitHub remain the version-control
and collaboration substrate.

The current slice is deliberately honest about its capabilities:

- deterministic `khive.review.v1` demo data and JSON import/export;
- both contract variants: the minimal Rust CLI `changeset` report and the enriched
  `pull_request` workbench view;
- semantic entity/edge changes with field-level before/after values;
- affected-subgraph exploration;
- captured khive search, recall, and traverse results;
- ADR-102 same-model-family approval refusal;
- no GitHub writes, live khive mutation, persistence, deployment, or claimed WASM parity.

Imports are schema-validated and capped at 2 MiB. Review pages are independently capped at 200
items, and core operation/finding arrays at 500 items; adapters must paginate anything larger.

The governing architecture is
[ADR-145](../../docs/adr/ADR-145-local-first-kg-workbench.md). The fixture is realistic but is not
a live repository or live graph connection.

## Run locally

Requires Node.js 20.19 or newer.

```bash
npm ci
npm run dev
```

Open <http://localhost:3000> for the repository showcase or
<http://localhost:3000/review> for KG review.

Generate a headless report with the npm-distributed CLI, then use **Import JSON** at `/review`:

```bash
khive kg review changes.ndjson --rules rules.toml --format json > review.json
```

The command emits its report before returning a non-zero status when validation or the independent
review gate blocks approval.

## Verify

```bash
npm run lint
npm run typecheck
npm test
npm run test:e2e
npm run build
```

The production build is a statically rendered App Router page. There is intentionally no hosting
or deployment configuration in this open-source slice.

## Optional DB-backed snapshot delivery

For a local analysis prepared with `kkernel repo build`, the Node server can expose a
server-private materialized report without placing it under `public/`:

```bash
KHIVE_SHOWCASE_ANALYSIS_ROOT=/absolute/path/to/analyses \
KHIVE_SHOWCASE_ANALYSES='[{"analysis_id":"khive","canonical_url":"https://github.com/ohdearquant/khive"}]' \
KHIVE_SHOWCASE_ACCESS_TOKEN=a-long-random-operator-secret \
npm run dev
```

The report must be located at
`$KHIVE_SHOWCASE_ANALYSIS_ROOT/khive/khive.repo.v1.json`. Discover configured entries at
`/api/showcase/analyses`, then fetch this report at
`/api/showcase/analyses/khive`. `KHIVE_SHOWCASE_ANALYSES` is a strict JSON array of one
to 64 `{analysis_id, canonical_url}` objects. IDs and normalized repository URLs must
both be unique; one invalid entry makes the entire catalog unavailable. The catalog is
sorted by analysis ID and exposes only those two public fields. It does not scan the
analysis root or read a report.

The default page consumes this catalog before resolving its initial `repo=` location.
Configured entries appear in **Repository analysis** and resolve to their opaque report
route. The browser accepts only the exact bounded v1 envelope. A catalog 404 means
static-only operation; transport, server, or validation failures produce a disclosed
degraded state while keeping curated static entries usable. Configured and static
entries with the same normalized URL are one selection: the DB snapshot is preferred,
and only its 404 may use the approved static asset. A 5xx, invalid bundle, provenance or
repository mismatch never falls back. A configured-only 404 is shown as an honest miss.

Both API routes require `Authorization: Bearer $KHIVE_SHOWCASE_ACCESS_TOKEN` on every
request, checked in constant time. To use the DB-backed setup through the showcase UI,
supply the same token to your own browser session before loading a repository:

```js
sessionStorage.setItem("khive.showcase.accessToken", "a-long-random-operator-secret");
```

The UI sends it as the bearer credential on snapshot requests. Without it the protected
route answers 404 and the UI falls back to the curated static bundle, so the token never
ships in the client build. An absent or mismatched token is indistinguishable
from an unconfigured catalog: both routes fail closed to the same sanitized 404. Without
`KHIVE_SHOWCASE_ACCESS_TOKEN` set, no request can be authorized, regardless of what
credentials it presents.

```bash
curl -H "Authorization: Bearer $KHIVE_SHOWCASE_ACCESS_TOKEN" \
  http://localhost:3000/api/showcase/analyses/khive
```

The report route rejects symlinks, reports above 8 MiB, malformed bundles, unknown IDs,
and bundles whose normalized `meta.repository.canonical_url` does not match the URL
configured for that ID. It never opens SQLite or starts a repository process. Responses
deliberately omit server paths and carry
`X-Khive-Analysis-Source: khive-db-snapshot` plus the analysis ID and a canonical byte
ETag. Both API routes use `private, no-store` and `nosniff` responses; an absent or
invalid operator catalog, and an absent or invalid credential, all return the same
sanitized 404.

The analysis root and its parent must be owned by the operator and unavailable for
untrusted local writes. Promoted analysis directories are immutable: build into a fresh
run directory, then publish only the completed report. The reader verifies path
containment and file identity after opening and reads at most 8 MiB plus one sentinel
byte.

This is a pinned DB-backed snapshot, not a live mutable query and not arbitrary URL
ingest. See ADR-147 Amendments 1–4 and the repository-showcase CLI guide.

## Adapter boundary

`RepoBundle` in `src/lib/repo-bundle.ts` and `ReviewInput` in
`src/lib/review-bundle.ts` are deliberately independent browser boundaries. Their closed
Zod models, checked-in schemas, and Rust-produced golden values are tested together. Future
review adapters stay server-only:

- Git reads canonical `.khive/kg/*.ndjson` and repository refs;
- khive is invoked as the npm-distributed executable using argv arrays without a shell;
- GitHub reads use a least-privilege GitHub App after authorization is designed.

Browser code never opens khive's SQLite database, receives GitHub tokens, or accepts arbitrary
repository paths.
