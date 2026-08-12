# kkernel — usage patterns

`kkernel` is the single khive Rust binary. It is both the **admin/management CLI**
(sync, schema migrations, pack/backend introspection, reindex) and the **MCP server**
(`kkernel mcp`). There is no separate `khive-mcp` binary — `khive-mcp` is now a
library crate consumed by `kkernel`.

All subcommands emit JSON on stdout by default; pass `--human` where supported for a
readable table. `--log <level>` (env `KHIVE_LOG`, default `warn`) is global and goes
to stderr — stdout stays clean for JSON / MCP traffic. Stderr is diagnostic-only and
best-effort; disconnecting its consumer does not terminate the stdin/stdout MCP transport.

`kkernel -V` reports the package version; `kkernel --version` also reports the full source
revision (including `-dirty` when applicable) and UTC build time.

```
kkernel <command> [flags]

  sync      Build a working SQLite DB from .khive/kg/*.ndjson sources
  pack      Introspect registered packs (list, handler <name>)
  kg        KG validation, init, hook management
  db        Schema migration lifecycle (migrate, check)
  engine    Embedding model lifecycle (list, status, migrate, drift-check)
  vector    Vector store capabilities and orphan sweep
  reindex   Re-embed entities, notes, and the knowledge corpus (multi-engine)
  exec      Run a verb DSL expression through the pack registry
  mcp       Serve the MCP `request` surface (stdio / daemon / transports)
  backend   Inspect registered backends (list, info <name>)
```

The default database is `~/.khive/khive.db`. Override per-command with `--db`
(or `KHIVE_DB` for `mcp`/`exec`). Use `:memory:` for an ephemeral database.

---

## `kkernel mcp` — serve the MCP request surface

This is the production entrypoint. The deno/npm distribution invokes it:
`khive mcp …` → `kkernel mcp …`, and the `khive-mcp` command alias → `kkernel mcp …`.

```bash
# stdio MCP server (default transport) — what MCP clients spawn
kkernel mcp --db ~/.khive/khive.db

# pick packs explicitly (default loads all 12 production packs)
kkernel mcp --pack kg --pack gtd --pack knowledge

# warm Unix-socket daemon (owns ANN indexes; stdio clients auto-spawn + forward to it)
kkernel mcp --daemon

# ephemeral in-memory server, no embedding (fast tests)
kkernel mcp --db :memory: --no-embed
```

Key flags: `--db`, `--actor`/`--namespace`, `--no-embed`, `--pack` (repeatable),
`--config`, `--daemon`, `--transport <name>`, `--bind <addr>`.

### Transports are registerable

`--transport` selects a foreground transport by name from a registry
(`khive_mcp::transport::TransportRegistry`). `stdio` is the only built-in today;
additional transports (e.g. Streamable HTTP) register with `registry.register(...)`
before serving. An unknown name errors with the registered set. `--bind` is reserved
for network transports and is ignored by stdio.

`--daemon` is a deployment mode, not a transport: it runs the warm Unix-socket server
(`~/.khive/khived.sock`) and takes precedence over `--transport`. On first use, stdio
clients auto-spawn `kkernel mcp --daemon` and forward request frames to it; set
`KHIVE_NO_DAEMON=1` to force local dispatch (used by the smoke/contract tests).

---

## `kkernel exec` — run a verb directly through the registry

Same DSL as the MCP `request` tool, but in-process against a chosen DB and namespace —
ideal for admin verb calls without standing up an MCP client. Defaults to namespace
`local`.

```bash
kkernel exec 'stats()'
kkernel exec 'stats()' --db ~/.khive/khive.db
kkernel exec 'stats()' --config /absolute/path/to/config.toml
kkernel exec '[list(kind="entity", limit=5), stats()]'            # parallel batch
kkernel exec '[create(kind="concept", name="X"), stats()]' --strict  # nonzero exit if any op fails
kkernel exec 'create(kind="entity", entity_kind="concept", name="X") | link(source_id=$prev.id, target_id="<id>", relation="extends")'   # chain ($prev)
kkernel exec 'knowledge.index(help=true)'                         # param schema for any verb
kkernel exec 'knowledge.search(query="...", rerank=true)' --presentation verbose
```

Flags: `--db`, `--config` (env `KHIVE_CONFIG`), `--namespace`, `--actor`, `--expect-actor`,
`--presentation <agent|verbose|human>`, `--strict`.
A request in which every op failed or aborted always exits nonzero after printing the full
response. Without `--strict`, a _partially_ failed request (`status: "partial"` with at least
one success) retains its compatibility behavior and exits zero; `--strict` converts any failed
or aborted op into a nonzero process exit.

### Bulk JSONL execution

`kkernel exec --ops-file batch.jsonl` accepts one independent JSON operation per
non-blank line: `{"tool":"verb","args":{...}}`. It validates the complete
source before runtime construction, with a 96 MiB physical-line limit and a
512 MiB file limit, then dispatches ordered chunks bounded to 100 operations and
32 MiB. One operation larger than the chunk byte target runs alone, still under
the physical-line and total-file ceilings.

Validated chunks use the local typed JSON batch seam. They retain the same
handler, identity, presentation, strict-refusal, audit, ordered-row, and output
format pipeline as ordinary requests, without serializing the decoded values
back through the raw request parser. This separation is intentional: MCP, HTTP,
daemon, and inline `exec` strings retain their independent 1 MiB
`MAX_OPS_INPUT_LEN` safety boundary. The ops-file limits are not a global limit
increase.

### Stable refusal reasons

Refused invocations retain their existing human-readable error and exit-code
semantics, and additionally write `kkernel-refusal: <token>` to stderr. Failed
per-operation JSON entries carry the same token in a sibling `reason` field.
The following vocabulary is closed and append-only: new classifications may be
added, but an existing token's spelling and meaning never change.

| Token                   | Meaning                                                                              |
| ----------------------- | ------------------------------------------------------------------------------------ |
| `anonymous-actor`       | The resolved actor is anonymous while attributed execution is required.              |
| `expect-actor-mismatch` | The resolved actor differs from `--expect-actor`.                                    |
| `gate-refusal`          | Content was refused by the write-time secret gate.                                   |
| `strict-op-failure`     | `--strict` observed at least one otherwise-unclassified failed or aborted operation. |
| `parse-error`           | The operation expression or JSONL operation failed to parse.                         |
| `verb-refused`          | The requested verb is unknown or is not loaded.                                      |

For a multi-failure batch, stderr contains one line per classified failed or
aborted operation. A specific dispatch reason (`gate-refusal` or
`verb-refused`) takes precedence over the aggregate `strict-op-failure` reason
for that entry. An invocation-level actor refusal emits one line and returns
the same normal `results`/`summary` JSON shape over every parsed operation
without dispatching. Those failed rows and the `summary.failed` count describe
not-attempted operations, not per-operation execution failures. A malformed
input expression or source JSONL line has no operation list, so it preserves
the parse-before-envelope boundary and prints a dedicated invocation error
instead of inventing a tool:

```json
{
  "error": {
    "code": "invalid_params",
    "message": "<unchanged parser error>",
    "reason": "parse-error"
  },
  "invocation": { "started": false }
}
```

Carrier parsing precedes actor expectation, attributed-actor gates, and the
explicit `--db` versus multi-backend `database_override_conflict` check. If an
invocation is malformed and would also fail any of those later preflights,
`parse-error` is therefore the deterministic first classification for inline
DSL and JSONL alike.

For a completed, published `--strict --save-file` result, classification happens
before the save sink writes or hashes rows. The stdout manifest's
`failures[].reason` is consequently an exact projection of the corresponding
reason in the checksummed JSONL row.
The non-atomic ops-file path without `--save-file` retains its pre-existing
aggregate summary shape, so its compatibility `failures` objects do not gain a
`reason` field; stable classifications still appear on stderr. Use
`--save-file` when automation needs reasons correlated with durable rows.

Combined `--ops-file --save-file` has two separate commit boundaries. The
destination file is published by one atomic rename, while non-atomic database
chunks commit incrementally. After dispatch starts, success emits the ordinary
manifest unchanged. Any later error that stops the run before the manifest is
finalized, including malformed JSON or a structurally self-contradictory
response envelope, emits an aborted manifest before the non-zero exit. The
aborted manifest's `committed_chunks` are the structurally confirmed prefix;
its `dispatched_chunk`, when present, is unverified and may still have database
effects. Its `summary` covers confirmed rows, while `unconfirmed_ops` accounts
for the remainder without calling them aborted. Its `file_published=false`
means the incomplete temp JSONL was discarded and any previous destination was
preserved. A `--strict` failure or an all-failed file is a policy exit taken
after the ordinary manifest has already been published, so those runs keep the
ordinary manifest, carry none of the aborted-only fields above, and exit
non-zero without an aborted one.

Atomic ops-file preflight and prepare failures use the real per-operation
`results` shape: an unknown or unloaded verb receives `verb-refused`, while a
known verb that is merely ineligible for `--atomic` remains unclassified.
Secret-gate failures during atomic prepare receive `gate-refusal`. An atomic
rollback passed with `--strict` receives `strict-op-failure` on each
not-committed result without changing the atomic path's existing process-exit
semantics. Ordinary validation, transport, storage, and authorization-gate
failures remain unclassified unless `--strict` supplies the aggregate reason.

After `atomic.committed=true`, the write must not be replayed. If deferred
reindexing or canonical result rendering then fails, the command exits
successfully with `atomic.status="committed_degraded"`,
`atomic.retryable=false`, and one or more typed `atomic.degradations` entries.
Treat `post_commit_reindex` as an index-repair requirement and
`result_rendering` as a result re-read requirement; neither means the mutation
failed. A render-degraded result remains `ok=true` with `result=null` and its
own non-retryable degradation marker.

When `--atomic` and `--save-file` are combined, a successful stdout manifest
preserves that complete top-level `atomic` block. If JSONL write, flush, or
final publication fails after commit, stdout is instead the full atomic
envelope with `stage="save_file_publish"`, `committed=true`, and
`retryable=false`; the process then exits non-zero because the requested file
was not published. Reconcile from stdout and do not replay the durable unit.

When an explicit `--db` conflicts with a selected multi-backend config,
dispatch does not begin. The command emits
`error.code = "database_override_conflict"` with
`invocation.started = false` in a JSON envelope on stdout, retains the
actionable prose on stderr, and exits nonzero.

`--actor` is a highest-precedence identity pin: it overrides project config and
`KHIVE_ACTOR` for this invocation without changing the storage namespace, and
the configured authorization gate still checks the selected identity.
`--expect-actor` makes scripts fail before dispatch when identity resolution
produces anything else. The two flags compose:

```bash
kkernel exec 'create(kind="concept", name="X")' \
  --actor lambda:worker --expect-actor lambda:worker
kkernel exec 'stats()' --expect-actor lambda:worker  # validate config/env resolution
```

---

## Reindex — `kkernel reindex`

`kkernel reindex` re-embeds **entities, notes, and the knowledge corpus** in one
pass (namespace-scoped — run once per namespace your data spans). Progress prints
to stderr; the JSON/`--human` report goes to stdout.

```bash
kkernel reindex --db ~/.khive/khive.db --namespace local   # entities + notes + knowledge
kkernel reindex --db ~/.khive/khive.db --namespace khive
kkernel reindex --db ~/.khive/khive.db --sections-only      # backfill only section embeddings
```

| Flag               | Effect                                                                          |
| ------------------ | ------------------------------------------------------------------------------- |
| `--db <path>`      | database (env `KHIVE_DB`; `:memory:` for ephemeral) — parity with `mcp`/`exec`  |
| `--config <path>`  | khive TOML config (env `KHIVE_CONFIG`) — resolves engines like `kkernel mcp`    |
| `--knowledge-only` | only the knowledge corpus (skip entities/notes)                                 |
| `--no-knowledge`   | only entities/notes (skip knowledge)                                            |
| `--no-sections`    | within the knowledge pass, embed atoms but skip section embeddings (ADR-051)    |
| `--sections-only`  | embed only knowledge sections (skip entities/notes and atoms)                   |
| `--model <name>`   | entities/notes use this single engine instead of fanning out                    |
| `--keep-existing`  | skip records already embedded (incremental top-up) instead of replacing them    |
| `--batch-size <n>` | records per embedding batch (default 128, max 500)                              |
| `--best-effort`    | downgrade partial failures to a warning and still exit 0 (default fails closed) |
| `--human`          | readable report instead of JSON                                                 |

There is no `--embeds-only`, `--ids`, or `--dry-run` mode. `--keep-existing` narrows
vector work to missing records, but the selected graph pass still backfills FTS.

**Config resolution.** Engines, db path, and config file are resolved with the
**same precedence as `kkernel mcp`** — config-file `[[engines]]` (via `--config`
/ `KHIVE_CONFIG` / `./khive.toml` / `./.khive/config.toml` / `~/.khive/config.toml`)
win over the `KHIVE_EMBEDDING_MODEL` env vars and over `RuntimeConfig` defaults.
This guarantees reindex writes vectors for the SAME engine set the MCP server
serves recall from. `--namespace` is the explicit per-namespace target and
always wins over any config `[actor] id`.

**Fail-closed.** By default reindex returns a **non-zero exit** if any requested
engine failed, the knowledge pass errored, any knowledge atom vector insert
failed, the Vamana ANN build/snapshot persist failed, or any knowledge section
embed/write failed — a partial rebuild leaves stale recall/search state, so
automation must not see success. Pass `--best-effort` to downgrade failures to a
warning and exit 0. The report (JSON and `--human`) always reports
attempted/indexed/failed counts honestly (`errors_skipped`,
`knowledge_atoms_failed`, `knowledge_pass_errored`, `knowledge_ann_failed`,
`knowledge_sections_failed`). Note: `knowledge_ann_failed` and
`knowledge_sections_failed` are distinct failure dimensions from
`knowledge_atoms_failed` — atom vectors may have persisted successfully while the
ANN rebuild/snapshot persist or the section embed/write failed.

**Multi-engine semantics.** Entities and notes embed with **every registered
engine** (e.g. `all-minilm-l6-v2` + `paraphrase-multilingual-minilm-l12-v2`),
one vector record per engine — matching the runtime's create/update write path.
`--model` narrows to a single engine. **Knowledge is single-model**: knowledge
search retrieves via the default embedder's ANN, so the knowledge pass always
uses the default embedder (fanning out would write vectors search never reads).

The knowledge pass calls the `khive_pack_knowledge::reindex_knowledge` library
entry directly (the full-corpus `knowledge.index` handler) and rebuilds the
Vamana ANN snapshot — no verb-DSL shell required.

```bash
kkernel reindex --db ~/.khive/khive.db --knowledge-only      # just the corpus
kkernel reindex --db ~/.khive/khive.db --no-knowledge        # just graph substrate
```

For ad-hoc / scoped knowledge indexing (specific atoms, no ANN rebuild) the
low-level verb is still available via `exec`:

```bash
kkernel exec 'knowledge.index(ids=["my-slug", "<uuid>"])' --db ~/.khive/khive.db
```

> Stop the MCP daemon before a large reindex to avoid SQLite write contention:
> `pkill -f 'kkernel.*--daemon'` (or `KHIVE_NO_DAEMON=1`), then reindex, then let
> the next stdio client re-spawn the daemon.

---

## `kkernel db` — schema lifecycle

```bash
kkernel db check --db ~/.khive/khive.db --human     # report current vs latest version
kkernel db check --strict                            # exit nonzero if behind
kkernel db migrate --db ~/.khive/khive.db            # apply pending migrations
kkernel db migrate --dry-run                         # show pending without applying
```

The consolidated baseline is a single migration (V1, from `khive-db/sql/schema.sql`).
A database whose `_schema_migrations` version is **ahead** of the latest known
migration is rejected at open time — it predates the consolidation or was written by a
newer build. Recreate it from the current schema; in-place downgrade is unsupported.

---

## `kkernel sync` — build a DB from NDJSON sources

```bash
kkernel sync --repo . --db ~/.khive/working.db --namespace local
```

Reads `.khive/kg/{entities,edges}.ndjson`, builds a queryable SQLite DB, and replaces
the target atomically (tmp + rename). Consumed by the deno CLI's `khive kg sync`.

---

## Introspection

```bash
kkernel pack list --human                 # all packs: verbs, note kinds, entity kinds
kkernel pack handler knowledge --human     # full handler surface for one pack
kkernel backend list --human               # registered backends
kkernel backend info main --human
kkernel engine list                        # embedding engines + model history
kkernel engine status                      # active model + migration status
kkernel vector --help                      # vector store capabilities, orphan sweep
kkernel kg --help                          # KG validation, init, pre-commit hook
```

---

## Distribution model

`kkernel` is the only published binary. The npm package `khive` ships per-platform
`@khive-ai/kernel-<platform>` subpackages that each contain `bin/kkernel`. Two command
names route to it:

- `khive <cmd>` → `kkernel <cmd>` (and `khive mcp` → `kkernel mcp`)
- `khive-mcp [args]` → `kkernel mcp [args]` (compat alias for existing MCP configs)

Binary resolution order (npm shims and `cli/lib/kernel.ts` agree): `KKERNEL_BINARY`
env override → `@khive-ai/kernel-<platform>/bin/kkernel` → monorepo
`crates/target/{release,debug}/kkernel`.

### Local development

```bash
make local          # build release kkernel, kill stale procs, codesign, install to ~/.cargo/bin
make ci             # full gate (fmt, clippy -D warnings, tests, contract + smoke)
```

After `make local`, run `/mcp` in Claude Code to reconnect to the rebuilt server.
