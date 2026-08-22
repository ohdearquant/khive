# kkernel Operator Guide

This guide is for the human or agent **operating** a khive deployment: running migrations,
moving data between databases, reindexing embeddings, and diagnosing drift. It is not typical
workflow tooling: day-to-day agents talk to khive through the MCP `request` surface (see the
[README](../README.md#the-mcp-verb-surface) and [AGENTS.md](../AGENTS.md)), never through this
CLI. `kkernel` is the admin binary underneath that surface (`kkernel mcp` serves `request`), plus
a set of subcommands for the operations that don't belong on the agent-facing wire.

Every claim in this document was checked against the source in `crates/kkernel/src/`; file
references are given per section so behavior can be re-verified after a change. Where the code's
actual behavior differs from what `--help` or a doc comment implies, this guide says so
explicitly and describes what the code does.

---

## 1. What kkernel is, and how to navigate it

`kkernel` is one binary with a clap subcommand tree (`crates/kkernel/src/main.rs:34-88`):

| Subcommand                           | Mutates data?                          | Purpose                                                                                 |
| ------------------------------------ | -------------------------------------- | --------------------------------------------------------------------------------------- |
| `sync`                               | yes (target DB)                        | Rebuild a SQLite DB from `.khive/kg/{entities,edges}.ndjson`                            |
| `pack list` / `pack handler <name>`  | no                                     | Introspect registered packs (verbs, note/entity kinds)                                  |
| `kg validate`                        | no (mutates only with `--fix`)         | Structural + rule-based lint of tracked `.khive/kg/*.ndjson`                            |
| `kg init`                            | yes (repo scaffolding)                 | Create `.khive/kg/`, `khive.toml`, pre-commit hook, optional CI workflow                |
| `kg hook install\|uninstall\|status` | yes (`.git/hooks/`)                    | Wire/unwire the pre-commit hook                                                         |
| `kg fetch` (alias `kg sync`)         | yes (cache dir)                        | Pull a remote KG archive with SHA-256 pin verification                                  |
| `kg export`                          | no (writes an output file, not the DB) | Dump a namespace's entities+edges to one JSON archive                                   |
| `kg import`                          | yes (target DB)                        | Load an archive/JSON/NDJSON file into a DB                                              |
| `kg status`                          | no                                     | Compare DB content hash against tracked NDJSON content hash                             |
| `kg commit`                          | yes (a separate local-only git repo)   | Validate + git-commit a staged tier-2 change-set (ADR-102 Amendment to ADR-020)         |
| `db migrate` / `db check`            | `migrate` yes, `check` no              | Apply or report pending schema migrations                                               |
| `engine list` / `status`             | no                                     | Inspect the `_embedding_models` table                                                   |
| `engine migrate` / `drift-check`     | n/a                                    | **Not implemented**, always return an error (see §3)                                    |
| `vector capabilities`                | no                                     | Print the sqlite-vec backend's static capability set                                    |
| `vector sweep`                       | n/a                                    | **Not implemented**, always returns an error (see §3)                                   |
| `reindex`                            | yes (vectors + FTS)                    | Re-embed entities/notes/knowledge, fanning out across configured engines                |
| `exec`                               | depends on the ops given               | Run a verb DSL expression, or drain due `scheduled_event` notes with `--pending-events` |
| `mcp`                                | yes (serves writes)                    | Serve the MCP `request` surface (stdio/daemon/transport)                                |
| `backend list` / `info`              | yes (see caveat below)                 | Enumerate configured backends                                                           |

Read-only vs. mutating is a useful mental split when deciding what's safe to run against a
production database without a backup first: `kg status`, `kg validate` (no `--fix`), `pack list`,
`engine list`/`status`, `vector capabilities`, and `db check` never write. Everything else can.

`backend list` / `backend info` are **not** read-only today, despite only "enumerating" backends
on their surface. The v1 implementation builds a synthetic single-backend registry from
`RuntimeConfig::default()` and constructs a real `KhiveRuntime` over it (`main.rs:532-547`).
`KhiveRuntime::new` creates the parent directory of the default DB path if missing, opens or
creates the SQLite file, and runs all pending migrations (`khive-runtime/src/runtime.rs:80-106`).
So `kkernel backend list` (and `info`) can create/open/migrate the default runtime database
(`~/.khive/khive.db`) today, on a machine where that file is absent or stale, purely as a side
effect of listing backends.

### Shared conventions

- **Output**: most reporting commands print one line of JSON to stdout by default (most also
  accept `--human` for a readable table/summary instead). This is not universal: `kg validate`
  defaults to text output, not JSON (`OutputFormat::Text` is the `--format` default,
  `kg/types.rs:55-57`, `kg/validate.rs:134-140`); `kg init` and `kg hook install`/`uninstall`
  print human status lines, not JSON (`kg/init.rs:112-130`, `kg/init.rs:191-195`); `kg export`
  writes an archive file rather than emitting a JSON summary line; and the pending-events drain
  prints its summary as pretty-printed (multi-line) JSON, not a single line
  (`pending_events.rs:731-745`). Logs (tracing) always go to stderr, so piping stdout never mixes
  log noise into the JSON (`kkernel/src/cli.rs:init_tracing`). Stderr logging is best-effort: a
  failed or closed stderr consumer does not terminate the stdin/stdout MCP serving loop.
- **Log level**: `--log <level>` or `KHIVE_LOG` (global arg, default `warn`,
  `main.rs:41-43`). The `lattice_inference` tokenizer-size warning is filtered to `error`
  regardless of the requested level (`main.rs:456`).
- **`--db` / `KHIVE_DB` resolution**: shared across `reindex`, `exec`, and the pending-events
  drain via `crate::dbpath::resolve_db_override` (`dbpath.rs:13-19`). `:memory:` is a sentinel for
  the ephemeral in-memory database (`db_path: None`), not a file literally named `:memory:`:
  SQLite would otherwise treat that string as a real (per-connection, effectively empty-schema)
  file. Omitting `--db` leaves `RuntimeConfig::default().db_path` in place, which resolves to
  `~/.khive/khive.db`. Several `kg` subcommands (`export`, `import`, `status`) instead require
  `--db` explicitly with **no default**, specifically so an operator command never silently
  targets `~/.khive/khive.db` (`kg/types.rs:147, 162`, comment: "so this command never defaults to
  `~/.khive`").
- **Config / namespace resolution parity with `kkernel mcp`**: `reindex` calls the exact same
  `khive_mcp::serve::resolve_runtime_config` function that `kkernel mcp` uses to build its
  `RuntimeConfig` (`reindex.rs:450-465`, `khive-mcp/src/serve.rs:1259-1266`, the doc comment on
  that function states it was "extracted from `build_server` so `kkernel reindex` reuses the exact
  engine and db resolution, otherwise an admin reindex writes vectors for the default/env model
  set while the MCP server serves recall from the config-file `[[engines]]` set"). The precedence
  is: explicit `--namespace`/`KHIVE_NAMESPACE` (skips the config tier entirely) → `[actor] id` in
  the resolved `khive.toml` → default `"local"`. `exec` and the pending-events drain resolve
  namespace independently (a plain `Namespace::parse(&args.namespace)`, default `"local"`, no
  config-file `[actor] id` tier), so `exec`/`--pending-events` do not pick up `[actor] id` the way
  `reindex`/`mcp` do. `kg export`/`import`/`status`/`fetch` take `--namespace` directly with no
  config-file fallback at all.
- **`~/.khive/.env`**: loaded once at process start via `dotenvy` before argument parsing
  (`main.rs:198-215`). Real environment variables always win over the file; a missing file is not
  an error.

---

## 2. Data import and export

### `kg export` / `kg import`: portable archive files

**Format** (`kg/archive.rs`): a single JSON file, not NDJSON. The envelope (`KgArchive`) carries
`format: "khive-kg"`, `version: "0.1"`, `namespace`, `exported_at`, and `entities`/`edges` arrays:
metadata and data live in one file, unlike `kg fetch`'s cache (which writes a separate `meta.json`
sidecar; see below). Export always includes edges; there is no entities-only mode.

```bash
kkernel kg export /tmp/my-namespace.khive-kg.json --db ~/.khive/khive.db --namespace my-project
kkernel kg import /tmp/my-namespace.khive-kg.json --db /path/to/target.db --namespace my-project
```

- `kg export <output> --db <path> [--namespace local]`, `output` and `--db` are both required
  (no defaults). Export refuses to run if `--output`, after canonicalizing through symlinks,
  resolves to the same path as `--db`: "would overwrite the database" (`archive.rs:20-38`).
  The write itself is atomic: it creates `<output>.<pid>.inprogress` with `O_EXCL` (refusing to
  follow a pre-existing symlink), `fsync`s it, then renames it into place (`archive.rs:54-80`).
- `kg import <source> --db <path> [--namespace local] [--format archive|json|ndjson] [--verbose]`:
  `source` and `--db` are required, `--format` defaults to `archive`.
  - `--format archive` (default): parses `source` directly as a `KgArchive` JSON envelope and
    completes format/version, entity kind/name, timestamp, and edge-weight validation before the
    target runtime is constructed. Kind validation uses the **full merged pack kind registry**
    (including pack-registered kinds such as `resource`) discovered against an in-memory validation
    runtime, so malformed input cannot create or migrate `--db`.
  - `--format json` / `--format ndjson`: parsed through `khive_vcs_adapters::JsonFormatAdapter`,
    a flat array of entity/edge records in the adapter's own wire shape (a `json` file is one JSON
    array; an `ndjson` file is one record per line, joined into an array before parsing). These
    formats pass the merged pack kind registry into the adapter, so pack-registered kinds such as
    `resource` use the same installed taxonomy as archive import. Canonical `source`+`target`
    identifies an edge; `from`/`to` remain entity metadata; a complete dual entity/edge signature
    is rejected as ambiguous. Required names must be non-blank and present timestamps must be valid
    RFC 3339 strings. Entity labels retain their original nonblank bytes. Edge properties and the
    two timestamps remain top-level portable fields and persist as storage metadata/provenance.
  - A malformed record anywhere in an archive, `json`, or `ndjson` input aborts before the target
    runtime is constructed; `--db` is neither created nor migrated and earlier valid records are
    not partially applied.
  - The positional `source` argument points at an arbitrary file the operator names. It is **not** the same as reading
    the repo's tracked `.khive/kg/{entities,edges}.ndjson`; that directory-reading path
    (`archive_from_ndjson_repo`) is used only by `kg status`, and separately by `kkernel sync` /
    `kg fetch` for the NDJSON-in-a-git-repo workflow. Whether `import_kg` itself is
    upsert/idempotent or replaces existing records is decided inside `khive-runtime`
    (`runtime.import_kg`, not in this crate); this guide does not assert either way; verify
    against `crates/khive-runtime` before relying on repeatable-import behavior.
  - `kg validate`'s rule pipeline is never invoked by export/import; they are a fully separate
    code path from the tracked-NDJSON validate/status pipeline described below.

### `kg fetch` (alias: `kg sync`): pull a remote KG archive

`kg fetch` is a thin CLI-args adapter over the same `run_sync_remote` in `khive-vcs` that performs
SHA-256 pin verification (`kg/fetch.rs:8-32`, `khive-vcs/src/sync.rs`). It clones the remote
sparsely (`--filter=blob:none`, checking out only `entities.ndjson`/`edges.ndjson`), hashes the
result, and, if a pin is supplied, fails closed on a mismatch:

```bash
kkernel kg fetch upstream --url https://github.com/org/kg-data.git --ref main \
  --pin sha256:<64-hex> --namespace shared
```

- `remote` is positional (the cache directory name under `.khive/kg/remotes/<remote>/`), not
  `--remote`.
- `--url` is required; `--ref` defaults to `main`; `--namespace` defaults to `local`.
- `--pin sha256:<hex>` triggers fail-closed comparison against the archive's computed content
  hash; omit it to fetch unconditionally (the hash is still computed and written to `meta.json`
  for later pinning).
- `--repin` accepts the fetched content regardless of the existing pin and returns the new hash so
  the caller can update `schema.yaml`/config with it.
- Output: `.khive/kg/remotes/<remote>/{entities.ndjson, edges.ndjson, meta.json}`, published via an
  atomic staging-directory swap (`khive-vcs/src/sync.rs:341-395`); a crash mid-publish never
  leaves a reader-visible mix of old and new files. `meta.json` records `fetched_at`, the resolved
  `git_ref`, `commit_sha`, and `content_hash`.
- Git remote URLs and any embedded credentials are redacted from error messages before they reach
  stdout/stderr (`khive-vcs/src/sync.rs:462-523`).
- Remote validation uses the same full deterministic gate as local sync. Edge properties are part
  of `content_hash`, so a metadata-only remote change fails an old pin; independent edge creation
  and update timestamps are preserved through archive conversion.

**Do not confuse `kg fetch`/`kg sync` with the top-level `kkernel sync`.** They share a name
fragment but are different commands: `kkernel sync --repo . --db <path> [--namespace local]`
rebuilds a **local** SQLite DB from the repo's own tracked `.khive/kg/*.ndjson` (no remote, no
pin), atomically, via a `.tmp` file and rename, checkpointing the WAL first so no committed rows
are left behind by the rename (`khive-vcs/src/sync.rs:822-882`). `kg fetch`/`kg sync` instead
populate a **remote cache directory** with pin verification; it does not touch any SQLite file by
itself. Both validate NDJSON before writing anything (fail-closed): blank names, malformed entity
or edge timestamps, duplicate ids, dangling endpoints, out-of-range edge weights, unsorted files,
and unknown entity kinds/edge relations all abort before publication. Local sync maps edge
`properties` to storage `metadata` and preserves `created_at` and `updated_at` independently.

### `kg status`: drift between DB and tracked NDJSON

```bash
kkernel kg status --repo . --db ~/.khive/khive.db --namespace local
```

Exports the DB's current namespace content, hashes it (`snapshot_id_for_archive`), independently
hashes the repo's tracked `.khive/kg/{entities,edges}.ndjson`, and reports:

```json
{ "db_hash": "sha256:...", "ndjson_hash": "sha256:...", "clean": true }
```

`clean` is a pure content-hash equality check: not a field-by-field diff, so it tells you _that_
the DB and tracked files disagree, not _what_ disagrees (use `kg export` + a diff tool for that).
Edge properties contribute to this hash after recursive key canonicalization; a metadata-only edge
change therefore makes `clean` false.
**`kg status` never calls `std::process::exit`**: a "dirty" result is reported only via
`clean: false` in the JSON, not a nonzero exit code (`kg/status.rs`). This is the opposite of `kg
validate` (below), which does hard-exit on failure. A cron/CI check against drift must parse
`clean` from the JSON, not the process exit code. A repo with no `.khive/kg/` directory at all
does not error here; missing NDJSON files are treated as empty sets.

### `kg validate`: pre-commit gate for tracked NDJSON

```bash
kkernel kg validate --repo . [--strict] [--format text|json|github] [--fix] [--rules path] [--no-rules]
```

Bails immediately (not just a warning) if `.khive/kg/` doesn't exist: "run `kkernel kg init`
first" (`kg/validate.rs:76-81`). Runs seven unconditional built-in structural checks: six at
`error` severity and `sort-order` at `warning` severity. An eighth error-severity check,
`valid-note-kinds`, runs only when `notes.ndjson` exists. `--no-rules` cannot silence any
applicable structural check: `schema-compliance` (every NDJSON line must parse and carry required
fields, malformed lines are reported, not skipped, "so corrupt NDJSON cannot pass `kg validate`
only to fail later in `kkernel sync`/`kg import`"), `no-duplicate-uuids`, `sort-order`,
`referential-integrity` (every edge `source_id`/`target_id` must resolve, note this is the NDJSON
wire field name, distinct from the `source`/`target` Rust struct fields used by the export/import
archive types), `valid-entity-kinds` (against the merged pack taxonomy), `valid-edge-relations`
(against the closed `EdgeRelation` enum, not the pack registry, edge relations are compile-time
closed per ADR-002), and conditional `valid-note-kinds`. `required-input-files` runs first and
fails closed when mandatory `entities.ndjson` or `edges.ndjson` cannot be read. `notes.ndjson` is
optional when absent, but if present it must also be readable UTF-8.

On top of those, an optional `rules.toml` (default `.khive/kg/rules.toml`, override with
`--rules`, skip entirely with `--no-rules`) adds configurable `warning`/`info`/`error` rules;
`.yaml`/`.yml` is explicitly rejected with a "rename to `.toml`" error. Two rule shapes live in
`rules.toml`: the generic ad-hoc `[[rules]]` array (`id`/`severity`/`kind`/`condition`/
`require_field`/`message` — a single-field-equality-and-presence check over `entity` or `edge`
records), and five built-in, individually-configurable rule classes, each its own top-level
section. Every one of these sections is **opt-in**: a section absent from `rules.toml` does not run at
all (an existing `rules.toml` predating these sections is unaffected), and each section accepts
`enabled = true|false` (default `true` once the section is present) alongside `severity =
"error"|"warning"|"info"` (matching the same severity enum the `[[rules]]` array uses — there is
no separate `"warn"`/`"off"` spelling; use `severity` for the warning/error/info choice and
`enabled = false` to turn a class off entirely):

- **`edge_endpoint_types`** (default severity `error`): checks every edge's `(source kind,
  relation, target kind)` triple against the same canonical endpoint contract the `link`/`update`
  verbs enforce — the ADR-002 base allowlist plus every loaded pack's `EDGE_RULES` (ADR-017),
  fetched live from the pack registry on each run, never a hand-copied table. Edges whose
  endpoints don't resolve within the dataset are skipped (that's `dangling-refs`'s job).
- **`edge_direction_conventions`** (default severity `warning`): flags edges that match a
  configured relation's _reversed_ kind pattern but not its forward pattern — a heuristic for
  likely-inverted directional edges, not a hard contract check. Declare one or more
  `[[edge_direction_conventions.relations]]` entries; a relation with no entry is never checked.
- **`dangling_refs`** (default severity `error`): the configurable counterpart to the always-on
  `referential-integrity` structural check above. `kg validate` has no `--db` flag and never opens
  a live graph connection, so every reference is resolved only within the validated NDJSON dataset
  itself; every violation message says so explicitly ("not in dataset ... no live-graph check
  available in this build") rather than silently passing.
- **`naming_conventions`** (default severity `warning`): entity `name` hygiene — non-empty, no
  leading/trailing whitespace, no parenthetical suffix (e.g. `"Foo (2024 paper)"`, a qualifier
  that belongs in `properties`), and a configurable `max_length` (default 200). Per-entity-kind
  overrides live under `[naming_conventions.kinds.<kind>]`.
- **`citation_date_lint`** (default severity `warning`): flags configured `properties` field
  names (default `year`, `date`, `published_at`, `publication_date`) whose value is a year or
  ISO-ish date after validation time — catching forward-dated citation typos (`year = 2124`).
  Recognises a bare 4-digit year and RFC-3339 / `YYYY-MM-DD` strings; anything else is left
  unchecked rather than guessed at.

```toml
# .khive/kg/rules.toml — built-in rule-class sections (all optional; shown here all enabled)

[edge_endpoint_types]
enabled = true
severity = "error"

[edge_direction_conventions]
enabled = true
severity = "warning"

[[edge_direction_conventions.relations]]
relation = "introduced_by"
forward_source_kinds = ["concept", "artifact", "service"]
forward_target_kinds = ["document", "person"]
# Illustrative, not the canonical endpoint allowlist: the runtime's endpoint
# table is authoritative for which pairs a live write accepts.

[dangling_refs]
enabled = true
severity = "error"

[naming_conventions]
enabled = true
severity = "warning"
max_length = 200
no_leading_trailing_whitespace = true
no_parenthetical_suffix = true

[naming_conventions.kinds.concept]
max_length = 120

[citation_date_lint]
enabled = true
severity = "warning"
fields = ["year", "date", "published_at", "publication_date"]
```

- `--strict` makes warnings count toward failure too (`passed = errors==0 && (warnings==0 if
  strict)`).
- `--fix` applies the one fixable rule (sort order) **after** the report is printed, so the
  printed pass/fail reflects pre-fix state, and it refuses to touch a file containing malformed
  JSON rather than guessing at a fix (fail-closed).
- **Exit code**: a failing validation calls `std::process::exit(1)` directly
  (`kg/validate.rs:147-149`); `kg commit` (below) shares this hard-exit-on-failure behavior for its
  own pre-commit report, so between them `kg validate` and `kg commit` are the two `kg`
  subcommands that hard-exit on a failing report rather than only reporting it in JSON.

`kg init`'s generated pre-commit hook (below) runs exactly `kkernel kg validate` with no extra
flags whenever staged files touch `entities.ndjson`/`edges.ndjson`; it is bypassed the normal git
way (`git commit --no-verify`).

### `kg commit`: the tier-2 change-set commit primitive

```bash
kkernel kg commit <changeset.ndjson> --rules <rules.toml> --repo <path> -m "<message>" [--format text|json|github]
```

Restores the `kg commit` verb ADR-020 §5 specified (`export + validate + git add + git commit`)
but never shipped, scoped per [ADR-102](adr/ADR-102-tiered-validate-and-merge.md)'s "Amendment to
ADR-020" to the tier-2 flow that ADR defines: landing an already-staged, already-reviewed
[ADR-101](adr/ADR-101-kg-changeset-model.md) NDJSON-delta change-set into ADR-102's own
**local-only** staged-change-set/snapshot repository (D6) — this is a _different_ repository from
the project-repository-embedded `.khive/kg/` layout every other `kg` verb above operates on.
`--repo` here is that separate repository's root, not a project checkout.

**Flow**:

1. Parse `<changeset.ndjson>` via `khive_changeset::from_ndjson` — a malformed file (bad JSON, an
   unrecognized `schema_version`, an op missing a required field such as a `delete`/`merge`
   preimage) fails loud before any validation or git operation runs.
2. Project the change-set's `create` and `link` ops into synthetic `entities.ndjson` /
   `notes.ndjson` / `edges.ndjson` content and run a **subset** of the same rule pass `kg
   validate` uses against them: a local duplicate-stage-id check, `valid-entity-kinds`,
   `valid-note-kinds`, and (if `--rules` enables them) `edge_endpoint_types`,
   `edge_direction_conventions`, `naming_conventions`, `citation_date_lint`, and any generic
   `[[rules]]` entries. Any `error`-severity finding refuses the commit — the report is printed
   (respecting `--format`) and the process hard-exits `1`, exactly like `kg validate`, before any
   git command runs.
3. On a clean pass: refuses (fail-loud, before any git mutation) if `--repo` has **any** configured
   git remote (`git remote` returns a non-empty list) — ADR-102 D6's local-only constraint,
   enforced in code, not by convention. Otherwise stages the change-set file into the repo (in
   place if it already lives under `--repo`, or copied to
   `<repo>/.khive/kg/changesets/<file-name>` if it was staged elsewhere), `git add`s it, and
   `git commit -F <message-file>` with `-m`'s value as the body plus two trailers: `Change-Set-Producer:
   <envelope.producer>` and `Change-Set-Producer-Batch: <envelope.producer>@<staged_at
   microseconds>us` (ADR-101 D4's "producer-assigned batch identifier" trailer — see the crate note
   below for why it is derived rather than read verbatim from a dedicated field). Prints a JSON
   `CommitReport` (`commit_sha`, `changeset_path`, `ops`, `producer`, `producer_batch`) on success.

**Why `referential-integrity`/`dangling-refs` are excluded from step 2**: a change-set is a
_partial_ view of the graph — most `link` ops target entities or notes created by an earlier,
already-committed change-set, not by this one. Running either of those two rule classes against
this change-set alone would flag the overwhelming majority of ordinary edges as broken, a
false-positive storm rather than a real finding, so they are deferred to stage time (where the
producer/reviewer has, or can obtain, full graph context) instead of re-run here.
`edge_endpoint_types`/`edge_direction_conventions` need no such exclusion: both already skip any
edge whose endpoint fails to resolve within the given NDJSON dataset, so restricting them to this
change-set's own `create` ops degrades gracefully rather than false-flagging.
`referential-integrity` is always-on and unaffected (it never runs over `link` ops here — only
`no-duplicate-uuids`/`valid-entity-kinds`/`valid-note-kinds` are always-on at commit time; see
`run_commit_time_rules`). The `dangling-refs` exclusion is implemented by never invoking the
built-in dangling-ref evaluator in the first place (`validate::configurable_rule_checks_partial_view`)
— **not** by filtering the returned findings by id after the fact. A post-hoc `id ==
"dangling-refs"` filter would also swallow a malformed `[dangling_refs] severity = "..."`
config-validation error (which is always real, partial-view or not) and any generic `[[rules]]`
entry a rules author happens to name `"dangling-refs"`, silently letting error-severity findings
through. Both are checked-in regressions (`kg_commit_refuses_malformed_dangling_refs_severity`,
`kg_commit_refuses_generic_rule_named_dangling_refs` in `kg_commit_tier2.rs`).

**Why `update`/`delete`/`merge` ops are not re-projected**: they patch or remove records that
already exist outside this change-set, so `kg commit` has no fresh kind/name/relation data to
check for them beyond what ADR-102 D2 already routes to tier-2 review by construction (`delete`,
`merge`, and any edge-relation/weight change are _always_ tier-2, so a reviewer has already looked
at them before a change-set reaches this command).

**Batch-identifier trailer, a documented gap**: ADR-101 D4 specifies a commit trailer carrying "a
producer-assigned batch identifier," read from the change-set envelope. The shipped
`khive-changeset::Envelope` (schema version 1) carries `producer`, `producer_model_family`, and
`staged_at`, but no separate batch-identifier field. `kg commit` derives the trailer value as
`<producer>@<staged_at_micros>us` — unique per staged change-set and round-trippable, matching
ADR-101 D4's only stated contract for the field — rather than inventing a new envelope field on a
crate this branch does not modify. A future `khive-changeset` schema revision adding an explicit
batch-id field would let this trailer read it directly instead.

**Topology**: no SQLite handle is opened anywhere in this command. `valid-entity-kinds`/
`valid-note-kinds` build their pack-metadata taxonomy the same way `kg validate` does (`db_path:
None`), and every other check reads only the synthetic NDJSON projection or the change-set file
itself — matching ADR-102 D5's MCP-client-only / no-second-DB-handle constraint on live-graph
access.

**Local-repository leak guard**: this repository's entire purpose is committing exported KG
NDJSON, which trips the machine-wide `check-json-data.sh` pre-commit leak guard
(`core.hooksPath`) by default. `kg commit`'s own `git commit` invocation sets
`KHIVE_ALLOW_DATA=1` — that hook's documented, auditable bypass — rather than `--no-verify`.

### `kg init`: repo scaffolding

```bash
kkernel kg init [--repo .] [--ci] [--add-hooks]
```

Creates `.khive/kg/`, `.khive/kg/hooks/`, empty `entities.ndjson`/`edges.ndjson`, `.khive/.gitignore`,
a default `.khive/khive.toml`, and the tracked pre-commit hook script, every artifact is written
only `if !path.exists()`, so re-running is safe and never clobbers existing content
(`kg/init.rs:82-145`, confirmed by an explicit non-overwrite regression test). `--ci` additionally
writes `.github/workflows/kg-validate.yml` (also existence-gated). `--add-hooks` short-circuits to
the same logic as `kg hook install` below and skips the rest of scaffolding; use it to (re-)wire
the git hook without touching anything else.

`kg hook install|uninstall|status [--repo .]` manage `.git/hooks/pre-commit` independently of
`kg init`: `install` requires the tracked hook script to already exist (run `kg init` first),
removes any existing hook file/symlink, then on Unix **symlinks**
`.git/hooks/pre-commit → .khive/kg/hooks/pre-commit` (on non-Unix platforms it copies the file
instead; there is no symlink fallback there). `status` reports `{symlink_exists,
symlink_target, target_valid}` as JSON.

### `knowledge.import(...)` for corpus ingest

For ingesting a knowledge corpus (atoms/sections), use `kkernel exec` with the `knowledge.import`
verb; `exec` has no special-casing for any verb name; it forwards the DSL string to whichever
pack owns it, exactly like the MCP `request` tool:

```bash
kkernel exec 'knowledge.import(path="/path/to/atlas-markdown", format="atlas_md")' --db ~/.khive/khive.db
kkernel exec 'stats()' --config /absolute/path/to/config.toml
```

See §3 below for `exec`'s general resolution rules, and the `knowledge` pack's own docs for
`import`'s argument shape; that handler lives outside `kkernel` (in the pack crate), not in this
binary.

---

## 3. Reindex

`kkernel reindex` rebuilds embedding vectors and FTS documents for entities, notes, and (unless
excluded) the knowledge corpus. The graph entity/note pass fans out across every embedding engine
registered in the resolved config; knowledge atoms and sections use the default engine that their
read paths query. Engine resolution is the same one `kkernel mcp` uses (§1). Full flag reference
(`reindex.rs:134-194`):

| Flag                              | Default                                     | Effect                                                                                  |
| --------------------------------- | ------------------------------------------- | --------------------------------------------------------------------------------------- |
| `--db` / `KHIVE_DB`               | `~/.khive/khive.db`                         | Target database (`:memory:` sentinel supported)                                         |
| `--config` / `KHIVE_CONFIG`       | home-fallback search                        | TOML config path                                                                        |
| `--namespace` / `KHIVE_NAMESPACE` | `"local"` (or `[actor] id` if not explicit) | Namespace to reindex                                                                    |
| `--model <name>`                  | unset → every registered model              | Restrict the graph (entity/note) pass to one embedding model                            |
| `--batch-size <n>`                | `128`                                       | Clamped at runtime to `[1, 500]`, **silently**, no warning printed on clamp             |
| `--keep-existing`                 | off                                         | See below                                                                               |
| `--knowledge-only`                | off                                         | Skip the entity/note graph pass entirely; run only the knowledge corpus pass            |
| `--no-knowledge`                  | off                                         | Skip the knowledge corpus pass (atoms + sections) entirely                              |
| `--sections-only`                 | off                                         | Narrowest scope: skip the graph pass AND atom re-embedding, only knowledge sections run |
| `--no-sections`                   | off                                         | Skip knowledge sections, still re-embed atoms                                           |
| `--best-effort`                   | off                                         | See below                                                                               |
| `--human`                         | off                                         | Human-readable summary instead of JSON                                                  |

`--knowledge-only`/`--no-knowledge`, `--sections-only`/`--no-knowledge`, and
`--no-sections`/`--sections-only` are declared as clap `conflicts_with` pairs, so invalid
combinations are rejected at parse time, before any of the scope logic below runs
(`reindex.rs:169-188`).

**Actual scope derivation** (`reindex.rs:478-481`):

```rust
let do_graph     = !args.knowledge_only && !args.sections_only;
let do_knowledge = !args.no_knowledge;
let do_atoms     = do_knowledge && !args.sections_only;
let do_sections  = do_knowledge && !args.no_sections;
```

So `--sections-only` forces `do_atoms = false` regardless of `--no-sections`'s state; it is the
narrowest possible scope, not just "skip the graph pass."

**`--keep-existing`**: without it (default), every staged record is re-embedded and handed to
`VectorStore::insert_batch`. Each subject's old vector is replaced with the new vector in one
per-record savepoint; there is no committed pre-delete, so a failed embed or insert leaves the
prior vector stale rather than absent. With `--keep-existing`, the batch is narrowed to subjects
**not already present for that specific model + namespace** (`filter_unembedded`). Within that
narrowing, only `StorageError::Unsupported` (a backend that does not implement existence checks)
falls back to the conservative "assume nothing is embedded, re-embed everything" path. Any other
`batch_exists` error skips that model's batch and counts it as a failure; it does not silently
re-embed. In both modes the selected graph pass still backfills FTS. There is no
`--embeds-only` mode.

The default JSON report includes `truncation_by_model`. Each model entry reports `truncated`
(the number of inputs that were bounded) and `discarded_bytes` (source bytes not sent to that
embedder). These counters come from the embedding result itself; entity, note, atom, and section
source text remains complete in SQL and FTS.

**`--best-effort` vs. the fail-closed default**: `ReindexReport::has_failures()`
(`reindex.rs`) is a single predicate covering eight categories: vector embed/insert
errors, entity FTS failures, note FTS failures, knowledge atom failures, a knowledge pass that
didn't complete, Vamana ANN build/persist failure, and knowledge section failures. Without
`--best-effort`, any of these causes `run_reindex` to `bail!`: "reindex completed with failures;
recall/search state may be stale. Re-run, or pass `--best-effort` to accept a partial rebuild."
With `--best-effort`, the same conditions only print a stderr warning and the process still exits
0 (`reindex.rs:741-758`). **All eight categories are treated uniformly**; there is no failure
class that's exempt from `--best-effort` on one side or immune to it on the other. A failed
completion epoch bump is the eighth category. What
`--best-effort` cannot paper over are structural/setup failures that occur _before_ a report even
exists: a bad `--namespace` value, a config resolution failure, a failed runtime open, a failed
`authorize`, or a failed page-list call abort the whole run via `?` regardless of the flag.

**Engine fan-out**: omitting `--model` reindexes graph entities and notes against
`rt.registered_embedding_model_names()`, whatever engines the resolved runtime config actually
registers, not a hardcoded list. If that list is empty (no embedder configured at all), a warning
prints but FTS backfill for entities and notes still runs. Knowledge atoms and sections use only
the default model because every knowledge-search vector path reads only that model. `--model`
still restricts only the graph pass.

**When to reindex** (genuine in-code rationale, not doc-comment fluff): after relabeling a
namespace (vector rows would otherwise be stranded under the wrong namespace on next write,
`reindex.rs:239-250`); after adding or removing a graph embedding model in config (so entity/note
vectors match the currently configured engine set, `reindex.rs:483-490`); and to force a stale
Vamana ANN snapshot rebuild, since reindex explicitly invalidates ANN snapshots so the next
warm-load rebuilds against the freshly re-embedded vectors (`reindex.rs:627-636`).

### `kkernel engine`: read-only inspection only, today

`engine list [--human] [--db <path>]` and `engine status <name> [--human] [--db <path>]` are the
only implemented subcommands; both open a **read-only** runtime (`new_readonly`) and never
create a missing `~/.khive/khive.db`. `status`'s notion of "drift"/"migration in progress" is
purely a row-state check: does an `_embedding_models` row for that engine have `status="pending"`
(`engine.rs:164-195`); there is no comparison of actual embedding distributions.

`engine migrate <name> [--to <model> | --resume | --abort]` and `engine drift-check <name>
[--sample <n>]` parse and validate their flags but **always return an error**: "not yet
implemented (... deferred to follow-up #380)", regardless of which options are passed. Their args
are accepted by clap and even mutually validated (`--to`/`--resume`/`--abort` are pairwise
`conflicts_with`), but the handlers do nothing: no DB mutation, no re-embedding, no drift
computation happens today. Do not script against these as if they perform work.

### `kkernel vector`: capabilities is live, sweep is a stub

`vector capabilities [--human] [--engine <name>] [--db <path>]` prints a **static** capability
record matching the sqlite-vec backend's compiled-in `OnceLock` (`supports_filter` /
`supports_batch_search` / `supports_quantization` / `supports_update` / `supports_orphan_sweep` /
`supports_multi_field` all `false`, `max_dimensions: 8192`, `index_kinds: ["sqlite_vec"]`). It does
**not** open the database named by `--db` or inspect the configured backend at all; the output is
identical regardless of which `--db`/`--engine` you pass, so treat it as a reference for the
current sqlite-vec baseline, not a live probe (`vector.rs:95-136`).

`vector sweep [--namespace <ns>...] [--max-delete <n>] [--dry-run] [--engine <name>] [--db <path>]`
parses all its flags but **always returns an error**: "not yet implemented (backend orphan-sweep
deferred to follow-up #381)". No orphan detection, dry-run behavior, or deletion happens today;
none of its flags do anything yet.

---

## 4. Maintenance

### Blob-GC attachment rollout: Phase4a before Phase4b

Phase4a is a compatibility release, not the attachment migration. It leaves the database at V20:
it does not create `attachments`, register or run V21, backfill records, change pack/runtime
readers or writers, or drop `entities.content_ref`. Its filesystem
`transactional_orphan_sweep` intentionally returns typed `Unsupported` for both report-only and
destructive calls on V20, pending/incomplete V21, missing required objects, retained legacy
objects, or a schema newer than the exact V21 contract. Malformed schema/evidence or nonfunctional
named fences also fail closed, using the applicable validation, storage, or typed `Unsupported`
error before claim cleanup or deletion. There is no bypass through caller-snapshot `orphan_sweep`:
that API is disabled outright in this release (typed `Unsupported` for every call, both `dry_run`
modes) because it has no way to prove a completed V21 epoch and cannot account for the moodboard
FANN object missing from V20 SQL liveness.

**What you can do today, right now, in this release:** back up, drain old binaries, and deploy
Phase4a (steps 1-3 below). If a database's `attachment_cutover_state` marker is anything other than
the exact completed V21 epoch — including "incomplete", missing, or partially applied — GC simply
stays refused on both APIs, in both modes; the database and blob root are otherwise untouched.
There is no restore, rollback, or marker-repair command shipped in this release, and none is
required for the compatibility fence itself: an incomplete marker is not an error state to recover
from, it is V20's normal, permanently-supported epoch as far as Phase4a GC is concerned.

Use this rollout order for what Phase4a actually ships:

1. Back up the canonical main database and inventory every process that can open it or the shared
   blob root, including daemons, one-shot admin jobs, and independently supervised replicas.
2. Drain every pre-Phase4a process. Install a restart fence (deployment revision pin, disabled old
   unit, or equivalent) so an older binary cannot return and run V20 transactional GC.
3. Deploy Phase4a everywhere while the database remains V20. A typed V20 GC refusal in either mode
   is the expected safe state. Phase4a `db migrate` still knows only the V20 prefix and does not
   perform the attachment cutover.

The following is **future design, not a runnable procedure in this release.** No Phase4b
migration/serving tooling, boot gate, or durable-marker completion path exists in the committed
migration registry today (it ends at V20) — do not treat the steps below as operator instructions;
they describe what a later Phase4b release must do, once it ships:

4. Before introducing Phase4b, quiesce every Phase4a application reader and writer. Only the
   Phase4a GC implementation can interpret an exact completed V21 attachment epoch; its serving,
   pack, runtime, and ordinary migration paths remain V20 consumers. Do not perform a rolling
   Phase4a/Phase4b serving cutover against one database.
5. Run the future Phase4b boot-gated migration only with Phase4b tooling, keep serving closed until
   its durable marker is complete, and then admit only Phase4b-or-newer application processes.

Phase4b migration/serving support is a follow-up and is not present in the Phase4a release. The
positive completed-V21 Phase4a GC regression exists to prove the compatibility fence itself; it is
not an operator command or authorization to keep Phase4a application traffic live during cutover.

### `db migrate` / `db check`

```bash
kkernel db migrate [--db <path>] [--backend <name>] [--dry-run] [--check] [--human]
kkernel db check   [--db <path>] [--strict] [--human]
```

`db migrate` opens the database via `KhiveRuntime::new`, which applies any pending migrations as a
side effect of construction; there is no separate "apply" step. `--dry-run`/`--check` on
`migrate` redirect to `db check` instead of touching anything. `db check` is deliberately
read-only: it inspects `_schema_migrations`' `MAX(version)` directly rather than opening a runtime
(which would migrate-on-open and mask the very state the command exists to report,
`main.rs:385-397`). A missing database file is reported as version 0 without being created.
`--strict` turns "behind" or "ahead of the latest known migration" (a schema newer than this
binary knows, or corresponding to a pre-consolidated-baseline version) into a nonzero exit via
`anyhow::bail!` (`main.rs:433-445`).

### `exec --save-file` / `exec --ops-file`: daemon coexistence

When they execute operations, both file-oriented `exec` modes deliberately build a local runtime
instead of forwarding through the warm daemon (`--ops-file --dry-run` stops before runtime
construction). `--save-file` needs a trusted local result sink; `--ops-file` needs bulk execution,
including optional whole-file atomic behavior, that the daemon protocol does not expose. If a live
daemon has the same database open, the command and daemon are independent SQLite clients.
`KHIVE_WRITE_QUEUE=1` does not combine them into one writer because that queue is process-local.

SQLite serializes their writes through the WAL write lock. Each process waits for
`KHIVE_BUSY_TIMEOUT_SECS` (30 seconds by default) and then reports `database is locked` if the
other writer still owns the lock. The CLI does not retry automatically. Configure the environment
for the daemon and the CLI separately if changing the timeout; setting it for one process does not
change the other.

For non-atomic `--ops-file`, a busy op can fail after earlier ops committed. Inspect the printed
failure list and use `--strict` when any failed op must produce a non-zero exit. For
`--ops-file --atomic`, the whole commit pass holds one bounded write transaction; run large units
against an idle daemon or in a maintenance window. A plan-level rollback prints
`atomic.committed=false` but currently exits zero even with `--strict`; inspect that field rather
than relying on process status. Admissibility, prepare, and atomic-unit seam errors instead exit
non-zero before printing an atomic result envelope. A deferred reindex or result-rendering failure
after commit is different: it exits zero with `atomic.committed=true`,
`atomic.status="committed_degraded"`, and `atomic.retryable=false`; repair or re-read as directed by
the typed `atomic.degradations` stage, and do not replay the durable mutation. With atomic
`--save-file`, a successful manifest preserves that entire `atomic` block. A sink write, flush, or
rename failure after commit prints the full envelope with
`atomic.degradations[].stage="save_file_publish"` and `atomic.retryable=false`, then exits non-zero;
the exit reports file-publication failure, while `atomic.committed=true` remains authoritative and
forbids replay. For combined non-atomic
`--ops-file --save-file`, file publication is atomic but database chunks commit incrementally.
Every exit after dispatch prints a reconciliation manifest: success uses the ordinary shape; a
failure before the manifest is finalized uses `status="aborted"`, lists confirmed
`committed_chunks`, and identifies any unverified `dispatched_chunk` that may still have database
effects. Its `summary` covers confirmed rows and `unconfirmed_ops` accounts for the remainder. An
abort discards the incomplete temp file and leaves any prior destination unchanged. A `--strict`
failure or an all-failed file exits non-zero after the ordinary manifest has been published, and
keeps that manifest; those paths have a known outcome for every op, so inspect the printed manifest
rather than expecting an aborted one. For successful `--save-file`, the manifest `summary`
carries failure counts and the saved JSONL rows carry per-op error details. Retry only after
checking the manifest and the result file when one was published, and only when the operations are
known to be idempotent.
The normative rationale and mode-by-mode exit contract are in
[the kkernel design note](../crates/kkernel/docs/design.md#exec-daemon-bypass-second-writer-contract-548-adr-067-adr-099).

### `exec --pending-events`: cron drain for scheduled events

```bash
kkernel exec --pending-events --db ~/.khive/khive.db --namespace local --verbose
```

`--pending-events` is mutually exclusive with both the positional `ops` string and `--ops-file`
(clap `conflicts_with`, `exec.rs`) and, when set, bypasses result-presentation and sink handling
(no `--presentation`/`--output-format`/`--save-file` handling). It still honors `--db` and the
explicit `--config` / `KHIVE_CONFIG` tier, then calls
`pending_events::run_pending_events_with_config` directly.

"Pending events" are **notes of kind `scheduled_event`** (the same `notes` substrate as everything
else, created by the `schedule` pack's `remind`/`schedule` verbs), each carrying `trigger_at`,
`status` (`pending`/`firing`/`fired`/`failed`/`missed`/`cancelled`), `event_type` (`remind` default, or `schedule`),
and, for `schedule`-type events, a `payload` DSL action string. Despite taking a `--namespace`
flag, the scan itself is **global across all namespaces**; the flag only sets the drain's home
namespace for authorization; `discover_pending_namespaces` finds every namespace with due events
first.

For `event_type="remind"`, the drain delivers through `comm.send` to the immutable
creator-provenance actor (with the documented legacy fallback). For `event_type="schedule"`, the
stored `payload` DSL is re-parsed and dispatched through the live registry with the event namespace
and provenance-verified actor. Mutable note metadata never grants replay authority.

Delivery is claim- and receipt-based. The pending CAS durably records deterministic occurrence
identity, a fresh invocation id, and a renewable lease before the action begins. The lease defaults
to 300 seconds (`KHIVE_SCHEDULE_LEASE_SECS`) and renews through durable outcome persistence. Outcome and
lifecycle finalization are separate claim-bound writes: an expired durable success resumes
finalization without reinvocation; an expired `invoking` receipt becomes indeterminate and is not
automatically replayed. A known failed one-shot returns to `pending`; a failed named repeat advances
to its next occurrence. A structured ambiguous action result such as `side_effects_unknown` is
terminally indeterminate rather than retryable, and its receipt retains the original
`error_payload` (including `details.outbound_id` when supplied) for reconciliation. A claim that
expires before invocation is recorded `not_invoked`, returned to `pending`, and is not counted as
an action failure. Recovery isolates each expired-row write failure so one poisoned row is counted
and logged without blocking later rows or newly due work. Legacy pre-receipt claims retain the
historical five-minute reclaim path.
Repeat creation accepts only `daily`/`weekly`/`monthly`; five-field cron is rejected because it
cannot be advanced safely.

There is no `--limit`/batch-size flag for the drain (pagination is a hardcoded internal constant)
and no dry-run mode. **Exit code caveat**: per-event failures accumulate into the printed JSON
summary's `failed` count but do not cause a nonzero process exit; `run_pending_events` only
returns `Err` for structural failures (bad namespace, runtime-open failure). A cron wrapper that
wants to alert on drain failures must parse `failed` out of the JSON summary
(`{scanned, invoked, outcomes_persisted, finalized, fired, advanced, failed, retry_pending,
indeterminate, skipped_not_due, skipped_race, reclaimed, missed_count, missed_ids}`), not rely on the
exit code.

### `pack list` / `pack handler <name>`

```bash
kkernel pack list [--human]
kkernel pack handler kg [--human]
```

Introspection only; builds an in-memory `VerbRegistry` from every self-registered pack and
returns each pack's verb list (each with `name`, `description`, `visibility`, `"verb"` for
externally callable, MCP-wire verbs, vs. `"subhandler"` for internal DSL-addressable pipeline
steps that never appear on the `request` wire), plus its `note_kinds`, `entity_kinds`, and
`requires` list. An unregistered pack name exits nonzero with `pack "<name>" is not registered`.
This introspection deliberately does **not** enforce the strict-actor gate
(`KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1`) since it never dispatches a verb or touches tenant data; it
works even in a strict-actor deployment with no actor configured, unlike `exec` and the
pending-events drain, both of which do enforce that gate before dispatching anything.

### `backend list` / `backend info <name>`

Enumerates configured backends. The current implementation lists the single default backend
constructed from `RuntimeConfig::default()`; the full multi-backend enumeration (reading
`khive.toml`'s `[[backends]]` entries) is documented in
[docs/multi-backend.md](multi-backend.md), which this admin surface does not yet reflect for
`list`/`info` specifically (`main.rs:174-196`).

---

## 5. Configuration reference

Config file discovery order, `--db` vs. `[[backends]]` semantics, and the full multi-backend
activation rule are covered in [docs/configuration.md](configuration.md) and
[docs/multi-backend.md](multi-backend.md); this guide does not duplicate that content.
