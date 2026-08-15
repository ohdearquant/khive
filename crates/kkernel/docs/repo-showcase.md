# Repository showcase CLI

`khive repo` is the offline producer for the read-only `khive.repo.v1`
bundle accepted in ADR-147. It does not add an MCP verb or accept browser-triggered
work.

## Build the khive golden target

Use an explicit timestamp and commits-only history when regenerating the durable
golden vector. Forge issues and pull requests are mutable observations and therefore
are not part of this reproducible command.

From the repository root, `scripts/generate-repo-showcase.sh` runs the command
below and also regenerates the JSON Schema first and synchronizes the resulting
golden bundle into the browser's static showcase assets.

```text
khive repo build \
  --source https://github.com/ohdearquant/khive \
  --revision c2979d2443738a075e55a170c772d1dc86cf0f91 \
  --work-dir /tmp/khive-repo-showcase \
  --include commits \
  --tags none \
  --default-branch main \
  --generated-at 2026-08-07T18:00:00Z \
  --out docs/schemas/examples/khive-repo-v1-khive.json
```

`repo build` resolves remote sources through the bounded `git.digest` clone
cache (or validates an absolute local clone), creates a separate clean checkout
with the canonical public origin at the selected revision under `--work-dir`,
loops `git.digest` until its cursor reports `done`, runs Rust-only `code.ingest`
into a separate code-map database, checks the two revisions, then exports canonical
JSON with an atomic replacement.

Before opening either database, the isolated checkout is preflighted from Git's
NUL-delimited tracked-file index. Tracked symlinks, gitlinks, special files,
non-stage-zero entries, unsafe paths, and files that do not canonically remain
inside the checkout are rejected. The Rust-only v1 input budget is 8 MiB per
tracked `.rs` file, 1 MiB per tracked `Cargo.toml`, and 256 MiB across those
relevant files combined. These fail-closed limits bound the files read by
`code.ingest` and the exporter's Rust module join.

Remote inputs are accepted only as public HTTPS repository URLs without
userinfo, credentials, query strings, fragments, or control characters. Only
the validated canonical URL is passed to Git and the bounded clone cache.

A commit does not pin the repository's mutable tag refs or `origin/HEAD`.
`--tags none` therefore makes the golden's release-tag series explicitly
unavailable, while `--default-branch main` supplies the label as a normalized
input rather than rediscovering it later. For a fresh interactive observation,
`--tags current` force-fetches and prunes the isolated clone's tag namespace;
the bundle records that source as completed at the explicit generation time.

Commits are mandatory. Issues and pull requests are enabled by default for an
interactive showcase build. When `gh` cannot resolve them, the command preserves
their `skipped` source states and the bundle reports them as unavailable. It never
turns an unavailable forge series into a zero-valued chart.

## Export existing stores

```text
khive repo export \
  --repo /absolute/path/to/clean/clone \
  --history-db /absolute/path/to/history.db \
  --map-db /absolute/path/to/code-map.db \
  --repository-url https://github.com/owner/repository \
  --default-branch main \
  --generated-at 2026-08-07T18:00:00Z \
  --out /absolute/path/to/khive.repo.v1.json
```

The clone must be a clean checkout at the revision represented by the code map.
Both database inputs are opened read-only by the exporter. `repo export` never runs
ingest or contacts a forge.
Omit `--default-branch` when it was not independently pinned; export encodes
the field as unavailable instead of reading mutable `origin/HEAD`. Tag coverage
is likewise unavailable in export-only mode because no pipeline report was
supplied.

Because an existing pair of databases has no embedded pipeline-identity
handshake, `repo build` refuses either store if it already exists. Use a fresh
work directory for each run. `repo export` accepts existing stores but marks
their ingest provenance as unknown; it never guesses completion from empty or
non-empty tables.

## Failure posture

The build refuses to export when commit history is incomplete, a digest cursor is
stalled, the secret gate refuses a history or code-map write, the repository moves
during the run, the code-map revision differs from the clone's pinned HEAD, or
`--generated-at` predates the HEAD commit timestamp.
Every bounded bundle section carries its own truncation disclosure; increasing a
bound is an exporter input change, not a renderer guess.

Section-specific ceilings are producer-enforced by `ExportBounds` validation:
2,048 packages, 10,000 modules, 5,000 commits, 2,000 issues, 2,000 pull requests,
50,000 edges per edge section, 5,000 residuals, 5,000 aggregate rows (with a
1,000-row default), 10,000 navigation entities, 50 links per navigation entity,
and 100 authors per scope. The generic JSON Schema `Page` definition retains a
50,000-item safety ceiling; each emitted page's `bound.max_items` records the
tighter producer limit that actually governed that section.

## Serve a completed local analysis

ADR-147 Amendment 1 permits an operator to serve a completed `repo build` result
through KG Studio without checking the report into Git or copying it under `public/`.
The directory layout is closed and server-private:

```text
<analysis-root>/
  khive/
    khive.repo.v1.json
  runs/
    khive-<opaque-run-id>/
      history.db
      code-map.db
      source/
```

Generate into a fresh run directory and publish the canonical report only after the
command succeeds:

```bash
kkernel repo build \
  --source /absolute/path/to/clean/khive \
  --repository-url https://github.com/ohdearquant/khive \
  --revision <40-hex-sha> \
  --work-dir <analysis-root>/runs/khive-<opaque-run-id> \
  --include commits \
  --tags none \
  --default-branch main \
  --generated-at <rfc3339> \
  --out <analysis-root>/khive/khive.repo.v1.json
```

KG Studio receives only the opaque `khive` ID. It never receives the paths above and
does not run this command in response to a browser request.

Configure the server with an explicit ID-to-repository binding:

```bash
KHIVE_SHOWCASE_ANALYSIS_ROOT=<analysis-root> \
KHIVE_SHOWCASE_ANALYSES='[{"analysis_id":"khive","canonical_url":"https://github.com/ohdearquant/khive"}]' \
npm run dev
```

`KHIVE_SHOWCASE_ANALYSES` accepts one to 64 strict objects. Both the ID and normalized
repository URL must be unique across the array; malformed JSON, unknown fields, invalid
IDs or URLs, and duplicate bindings make the complete catalog unavailable. The legacy
ID-only allowlist is not accepted because an ID without a repository identity cannot
bind the materialized report to operator intent.

`GET /api/showcase/analyses` returns the sorted
`khive.showcase.catalog.v1` catalog. It exposes only `analysis_id` and `canonical_url`,
does not enumerate the analysis root, and does not read reports, databases, or process
state. `GET /api/showcase/analyses/khive` reads the explicit bounded report and rejects
it when the bundle's normalized repository URL differs from the configured URL. Both
routes return sanitized, private, non-cacheable responses.
