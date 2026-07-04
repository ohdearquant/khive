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
  log noise into the JSON (`main.rs:449-461`).
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
  - `--format archive` (default): parses `source` directly as a `KgArchive` JSON envelope, checks
    edge weights are finite and in `[0.0, 1.0]`, then calls `runtime.import_kg`. Entity/note kind
    validation runs against the **full merged pack kind registry** (every loaded pack's kinds,
    including pack-registered ones like `resource`), because `cmd_import` installs that registry
    before importing (`archive.rs:93, 233-255`).
  - `--format json` / `--format ndjson`: parsed through `khive_vcs_adapters::JsonFormatAdapter`,
    a flat array of entity/edge records in the adapter's own wire shape (a `json` file is one JSON
    array; an `ndjson` file is one record per line, joined into an array before parsing). **These
    two formats are not the same code path as `--format archive`** and validate entity kind
    earlier, against only the base 8 `khive_types::EntityKind` variants, not the merged pack
    registry. A pack-registered kind such as `resource` imports successfully with `--format
    archive` but is rejected by `--format json`/`--format ndjson`; this asymmetry is a known,
    explicitly-commented gap (`archive.rs:609-620`), not a bug to work around locally.
  - A malformed record anywhere in a `json`/`ndjson` array aborts the entire import before any DB
    write; earlier well-formed records in the same file are not partially applied
    (`archive.rs:823-891`).
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

**Do not confuse `kg fetch`/`kg sync` with the top-level `kkernel sync`.** They share a name
fragment but are different commands: `kkernel sync --repo . --db <path> [--namespace local]`
rebuilds a **local** SQLite DB from the repo's own tracked `.khive/kg/*.ndjson` (no remote, no
pin), atomically, via a `.tmp` file and rename, checkpointing the WAL first so no committed rows
are left behind by the rename (`khive-vcs/src/sync.rs:822-882`). `kg fetch`/`kg sync` instead
populate a **remote cache directory** with pin verification; it does not touch any SQLite file by
itself. Both validate NDJSON before writing anything (fail-closed): duplicate ids, dangling edge
endpoints, out-of-range edge weights, unsorted files, and unknown entity kinds/edge relations all
abort before any DB write (`khive-vcs/src/sync.rs:707-810`).

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
first" (`kg/validate.rs:76-81`). Runs seven built-in structural checks, always at `error`
severity, that `--no-rules` cannot silence: `schema-compliance` (every NDJSON line must parse and
carry required fields, malformed lines are reported, not skipped, "so corrupt NDJSON cannot pass
`kg validate` only to fail later in `kkernel sync`/`kg import`"), `no-duplicate-uuids`,
`sort-order`, `referential-integrity` (every edge `source_id`/`target_id` must resolve, note this
is the NDJSON wire field name, distinct from the `source`/`target` Rust struct fields used by the
export/import archive types), `valid-entity-kinds` (against the merged pack taxonomy),
`valid-edge-relations` (against the closed `EdgeRelation` enum, not the pack registry, edge
relations are compile-time closed per ADR-002), and `valid-note-kinds` (only if `notes.ndjson`
exists).

On top of those, an optional `rules.toml` (default `.khive/kg/rules.toml`, override with
`--rules`, skip entirely with `--no-rules`) adds configurable `warning`/`info`/`error` rules;
`.yaml`/`.yml` is explicitly rejected with a "rename to `.toml`" error.

- `--strict` makes warnings count toward failure too (`passed = errors==0 && (warnings==0 if
  strict)`).
- `--fix` applies the one fixable rule (sort order) **after** the report is printed, so the
  printed pass/fail reflects pre-fix state, and it refuses to touch a file containing malformed
  JSON rather than guessing at a fix (fail-closed).
- **Exit code**: a failing validation calls `std::process::exit(1)` directly
  (`kg/validate.rs:147-149`); this is the one `kg` subcommand that hard-exits on failure rather
  than only reporting it in JSON.

`kg init`'s generated pre-commit hook (below) runs exactly `kkernel kg validate` with no extra
flags whenever staged files touch `entities.ndjson`/`edges.ndjson`; it is bypassed the normal git
way (`git commit --no-verify`).

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
kkernel exec 'knowledge.import(path="/path/to/corpus.jsonl", format="ndjson")' --db ~/.khive/khive.db
```

See §3 below for `exec`'s general resolution rules, and the `knowledge` pack's own docs for
`import`'s argument shape; that handler lives outside `kkernel` (in the pack crate), not in this
binary.

---

## 3. Reindex

`kkernel reindex` rebuilds embedding vectors and FTS documents for entities, notes, and (unless
excluded) the knowledge corpus, fanning out across every embedding engine registered in the
resolved config, the same resolution `kkernel mcp` uses (§1). Full flag reference
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

**`--keep-existing`**: without it (default), every existing vector row for the staged
`subject_id`s in a model's table is deleted up front (`drop_vectors_for_subjects`,
`reindex.rs:305-310`); necessary because the vector table's primary key is `(subject_id)` only,
not `(subject_id, namespace)`, so a namespace relabel followed by reindex would otherwise collide.
Every staged record is then re-embedded unconditionally. With `--keep-existing`, no delete runs at
all, and the batch is narrowed to subjects **not already present for that specific model +
namespace** (`filter_unembedded`, `reindex.rs:326-338`). Within that narrowing, only the specific
case of `StorageError::Unsupported` (a backend that doesn't implement existence checks at all)
falls back to the conservative "assume nothing is embedded, re-embed everything" path
(`reindex.rs:425-440`); any other `batch_exists` error instead skips that model's batch entirely
and counts it as a failure (`reindex.rs:327-340`); it does not silently re-embed.

**`--best-effort` vs. the fail-closed default**: `ReindexReport::has_failures()`
(`reindex.rs:228-236`) is a single predicate covering seven categories: vector embed/insert
errors, entity FTS failures, note FTS failures, knowledge atom failures, a knowledge pass that
didn't complete, Vamana ANN build/persist failure, and knowledge section failures. Without
`--best-effort`, any of these causes `run_reindex` to `bail!`: "reindex completed with failures;
recall/search state may be stale. Re-run, or pass `--best-effort` to accept a partial rebuild."
With `--best-effort`, the same conditions only print a stderr warning and the process still exits
0 (`reindex.rs:741-758`). **All seven categories are treated uniformly**; there is no failure
class that's exempt from `--best-effort` on one side or immune to it on the other. What
`--best-effort` cannot paper over are structural/setup failures that occur _before_ a report even
exists: a bad `--namespace` value, a config resolution failure, a failed runtime open, a failed
`authorize`, or a failed page-list call abort the whole run via `?` regardless of the flag.

**Engine fan-out**: omitting `--model` reindexes against `rt.registered_embedding_model_names()`,
whatever engines the resolved runtime config actually registers, not a hardcoded list. If that
list is empty (no embedder configured at all), a warning prints but FTS backfill for entities and
notes still runs. The knowledge-corpus pass is **not** fanned out across this model list at all:
it always uses the config's default embedder, independent of `--model`.

**When to reindex** (genuine in-code rationale, not doc-comment fluff): after relabeling a
namespace (vector rows would otherwise be stranded under the wrong namespace on next write,
`reindex.rs:239-250`); after adding or removing an embedding model in config (so on-disk vectors
match the currently-configured engine set, `reindex.rs:483-490`); and to force a stale Vamana ANN
snapshot rebuild, since reindex explicitly invalidates ANN snapshots so the next warm-load
rebuilds against the freshly re-embedded vectors (`reindex.rs:627-636`).

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

### `exec --pending-events`: cron drain for scheduled events

```bash
kkernel exec --pending-events --db ~/.khive/khive.db --namespace local --verbose
```

`--pending-events` is mutually exclusive with both the positional `ops` string and `--ops-file`
(clap `conflicts_with`, `exec.rs:105-106`) and, when set, bypasses the rest of `ExecArgs` entirely
(no `--presentation`/`--output-format`/`--save-file` handling); it calls
`pending_events::run_pending_events` directly.

"Pending events" are **notes of kind `scheduled_event`** (the same `notes` substrate as everything
else, created by the `schedule` pack's `remind`/`schedule` verbs), each carrying `trigger_at`,
`status` (`pending`/`firing`/`fired`/cancelled), `event_type` (`remind` default, or `schedule`),
and, for `schedule`-type events, a `payload` DSL action string. Despite taking a `--namespace`
flag, the scan itself is **global across all namespaces**; the flag only sets the drain's home
namespace for authorization; `discover_pending_namespaces` finds every namespace with due events
first.

What actually fires: for `event_type="remind"` (the default), **nothing**, the drain only marks
the note fired to acknowledge the trigger; the module's own comment states the notification
channel itself is out of scope for this drain. For `event_type="schedule"`, the stored `payload`
DSL string is re-parsed and dispatched through the same verb registry `exec` uses, with the
event's own namespace injected into every op so writes land where the event was created, not in
the drain's namespace.

Delivery is claim-based, not naive at-least-once: each event is claimed with a conditional
`UPDATE ... WHERE status='pending'` (a CAS), then finalized with a second CAS bound to that
specific claim token; a stale claimant can never clobber a fresh reclaim of the same row. Rows
stuck in `firing` for more than 5 minutes are reclaimed back to `pending` on every drain pass
before the scan. **Dispatch failures are terminal, not retried**: if the DSL action itself fails,
the event is still marked `fired` so a permanently broken action doesn't retry forever; only a
claim-checkout timeout (contention on the writer pool) leaves a row `pending` and retryable.
Repeat events (`daily`/`weekly`/`monthly`) advance `trigger_at` and reset to `pending` instead of
firing terminally; 5-field cron expressions are stored but **not** advanced by this drain (logged
as a warning, treated as one-shot); full cron support is tracked separately.

There is no `--limit`/batch-size flag for the drain (pagination is a hardcoded internal constant)
and no dry-run mode. **Exit code caveat**: per-event failures accumulate into the printed JSON
summary's `failed` count but do not cause a nonzero process exit; `run_pending_events` only
returns `Err` for structural failures (bad namespace, runtime-open failure). A cron wrapper that
wants to alert on drain failures must parse `failed` out of the JSON summary
(`{scanned, fired, advanced, failed, skipped_not_due, skipped_race, reclaimed}`), not rely on the
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
