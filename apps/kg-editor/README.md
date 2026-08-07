# khive repository atlas + KG Studio

The default route is ADR-147's static-first repository showcase. Entering a public
repository URL performs a local lookup against the curated set; a hit renders the exact
checked-in `khive.repo.v1` golden bytes, and a miss performs no clone, forge request, or
server-side execution. The original ADR-145 semantic review workbench remains available
at `/review`.

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
