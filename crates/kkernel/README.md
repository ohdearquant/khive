# kkernel

The khive kernel — a single Rust binary that serves the MCP `request` surface and
provides the admin CLI for database, pack, and versioning operations.

`kkernel` is the only binary khive ships. `kkernel mcp` serves the
[`khive-mcp`](https://crates.io/crates/khive-mcp) `request` tool over stdio (or a
persistent Unix-socket daemon); the rest of the subcommand tree covers everything an
operator needs outside of agent dispatch — schema migrations, KG sync/validate/fetch,
pack introspection, embedding-model lifecycle, and reindexing.

## Install

```bash
cargo install kkernel
```

or build from source:

```bash
git clone https://github.com/ohdearquant/khive.git && cd khive
cd crates && cargo build --release -p kkernel
```

The npm package `khive` installs a thin `khive` / `khive-mcp` shim that forwards to
`kkernel mcp` for users who prefer `npm install -g khive` over Rust tooling; the two
install paths produce the same binary underneath.

## Usage

Point an MCP client at the binary's `mcp` subcommand:

```json
{ "mcpServers": { "khive": { "command": "kkernel", "args": ["mcp"] } } }
```

```bash
kkernel mcp                            # stdio, default ~/.khive/khive.db
kkernel mcp --daemon                   # persistent warm daemon over a Unix socket
kkernel mcp --db :memory:              # ephemeral in-memory database
kkernel mcp --pack kg --pack gtd       # explicit pack list (default: full production set)
kkernel mcp --actor my-project         # default namespace for unscoped ops
```

Run a verb DSL expression directly — the same syntax the `request` tool accepts —
without going through an MCP client:

```bash
kkernel exec 'knowledge.stats()'
kkernel exec 'knowledge.index(rebuild_ann=true)'
kkernel exec '[knowledge.list(limit=5), knowledge.stats()]'
kkernel exec --pending-events          # cron-friendly: fire due scheduled_event notes
```

## Subcommands

| Subcommand | Purpose                                                                                     |
| ---------- | ------------------------------------------------------------------------------------------- |
| `mcp`      | Serve the MCP `request` surface — stdio, `--daemon`, or a registered transport              |
| `exec`     | Run a verb DSL expression (or `--ops-file batch.jsonl`) through the pack registry           |
| `sync`     | Build a working SQLite database from `.khive/kg/*.ndjson` sources                           |
| `kg`       | `validate` / `init` / `fetch` / `export` / `import` / `status` / `hook` — KG versioning ops |
| `db`       | `migrate` / `check` — apply or report pending schema migrations                             |
| `pack`     | `list` / `handler <name>` — introspect registered packs and their verb surface              |
| `engine`   | Embedding-model lifecycle: list, status, migrate, drift-check                               |
| `vector`   | Vector store capabilities and orphan sweep                                                  |
| `reindex`  | Rebuild embedding vectors and FTS documents for entities, notes, and knowledge atoms        |
| `backend`  | `list` / `info <name>` — inspect registered storage backends                                |

All subcommands emit JSON on stdout by default (for piping/parsing); pass `--human`
where supported for a readable table. `kkernel kg`, `kkernel sync`, and the NDJSON-to-SQLite
rebuild logic they wrap live in [`khive-vcs`](https://crates.io/crates/khive-vcs) and
[`khive-vcs-adapters`](https://crates.io/crates/khive-vcs-adapters); `kkernel`'s own
`kg/` module is a thin CLI wrapper over those libraries.

## Configuration

Resolution precedence for the default namespace: `--actor` > `--namespace` (legacy alias) >
`[actor] id` in a `khive.toml` config file > `"local"`. Config file search order when
`--config` is not given: `./khive.toml`, `./.khive/config.toml`, `~/.khive/config.toml`.
`~/.khive/.env` is loaded into the process environment at startup if present (real env vars
take precedence).

| Environment variable              | Effect                                                        |
| --------------------------------- | ------------------------------------------------------------- |
| `KHIVE_DB`                        | Database path (also `kkernel mcp --db`)                       |
| `KHIVE_ACTOR` / `KHIVE_NAMESPACE` | Default namespace (also `--actor` / `--namespace`)            |
| `KHIVE_NO_EMBED`                  | Disable local embedding model                                 |
| `KHIVE_PACKS`                     | Comma/whitespace-separated pack list (also repeated `--pack`) |
| `KHIVE_CONFIG`                    | Path to the TOML config file (also `--config`)                |
| `KHIVE_LOG`                       | Log level for stderr (JSON results on stdout are unaffected)  |
| `KHIVE_BRAIN_PROFILE`             | Brain profile for feedback routing and recall boosting        |

Two feature flags gate optional functionality, both pass-through to `khive-mcp`:
`bench-embedder` (deterministic hash embedder for benchmarking, never enabled in release
builds) and `channel-email` (SMTP/IMAP polling loop, inert without `KHIVE_EMAIL_*` env vars).

## Where this sits

`kkernel` sits at the top of the storage dependency chain — it depends on every pack crate
(`khive-pack-kg`, `-gtd`, `-memory`, `-comm`, `-schedule`, `-formal`, `-knowledge`,
`-session`), `khive-mcp` (the server library it serves), `khive-vcs` / `khive-vcs-adapters`
(KG versioning and import/export), and the core storage stack (`khive-runtime`,
`khive-db`, `khive-storage`, `khive-types`, `khive-score`). Its `_pack_links` module force-
references each pack crate so the linker keeps their `inventory::submit!` verb registrations
in the final binary — dependency alone is not enough for that to happen.

Governed by [ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)
(request DSL), [ADR-049](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-049-khived-daemon.md)
(the `--daemon` warm runtime), and [ADR-027](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-027-dynamic-pack-loading.md)
(pack self-registration via `inventory`).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
