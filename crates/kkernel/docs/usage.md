# kkernel — usage patterns

`kkernel` is the single khive Rust binary. It is both the **admin/management CLI**
(sync, schema migrations, pack/backend introspection, reindex) and the **MCP server**
(`kkernel mcp`). There is no separate `khive-mcp` binary — `khive-mcp` is now a
library crate consumed by `kkernel`.

All subcommands emit JSON on stdout by default; pass `--human` where supported for a
readable table. `--log <level>` (env `KHIVE_LOG`, default `warn`) is global and goes
to stderr — stdout stays clean for JSON / MCP traffic.

```
kkernel <command> [flags]

  sync      Build a working SQLite DB from .khive/kg/*.ndjson sources
  pack      Introspect registered packs (list, handler <name>)
  kg        KG validation, init, hook management
  db        Schema migration lifecycle (migrate, check)
  engine    Embedding model lifecycle (list, status, migrate, drift-check)
  vector    Vector store capabilities and orphan sweep
  reindex   Re-embed all entities and notes
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

# pick packs explicitly (default loads all 7 production packs)
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
kkernel exec 'knowledge.stats()'
kkernel exec 'knowledge.stats()' --db ~/.khive/khive.db
kkernel exec '[knowledge.list(limit=5), stats()]'                 # parallel batch
kkernel exec 'create(kind="entity", entity_kind="concept", name="X") | link(source_id=$prev.id, target_id="<id>", relation="extends")'   # chain ($prev)
kkernel exec 'knowledge.index(help=true)'                         # param schema for any verb
kkernel exec 'knowledge.search(query="...", rerank=true)' --presentation verbose
```

Flags: `--db`, `--namespace`, `--presentation <agent|verbose|human>`.

---

## Reindex workflows

Embeddings/FTS are rebuilt by two distinct paths, because entity/note vectors and
knowledge atoms live in different stores:

### Entities + notes — `kkernel reindex`

Walks all entities and notes and (re-)embeds them. Namespace-scoped, so run once per
namespace your data spans.

```bash
kkernel reindex --db ~/.khive/khive.db --namespace local
kkernel reindex --db ~/.khive/khive.db --namespace khive
# flags: --model, --batch-size (default 100), --keep-existing, --human
```

`--keep-existing` skips records that already have a vector (incremental top-up).
Omit it to drop-and-rebuild.

### Knowledge atoms — `kkernel exec 'knowledge.index(...)'`

The knowledge corpus is reindexed through its own handler. It embeds atoms and
(optionally) rebuilds the Vamana ANN. Also namespace-scoped via `--namespace`.

```bash
# embed all atoms in a namespace + rebuild the ANN snapshot
kkernel exec 'knowledge.index(rebuild_ann=true)' --db ~/.khive/khive.db --namespace local

# embed only specific atoms (by slug or id), no ANN rebuild
kkernel exec 'knowledge.index(ids=["my-slug", "<uuid>"])' --db ~/.khive/khive.db

# batch sizing (clamped 1..1000, default 500)
kkernel exec 'knowledge.index(batch_size=1000, rebuild_ann=true)' --db ~/.khive/khive.db
```

`knowledge.index` indexes **atoms** (not sections) and reports
`{indexed, skipped, total, ann_vectors}`. It is a no-op returning
`"no embedding model configured"` when no embedder is set.

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
