# khive KG Studio

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

Open <http://localhost:3000>.

Generate a headless report with the npm-distributed CLI, then use **Import JSON** in the editor:

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
npm run build
```

The production build is a statically rendered App Router page. There is intentionally no hosting
or deployment configuration in this open-source slice.

## Adapter boundary

`ReviewInput` in `src/lib/review-bundle.ts` is the browser boundary. Its closed Zod model, the
checked-in Draft 2020-12 schema, and the Rust-produced golden report are tested together. Future
adapters stay server-only:

- Git reads canonical `.khive/kg/*.ndjson` and repository refs;
- khive is invoked as the npm-distributed executable using argv arrays without a shell;
- GitHub reads use a least-privilege GitHub App after authorization is designed.

Browser code never opens khive's SQLite database, receives GitHub tokens, or accepts arbitrary
repository paths.
