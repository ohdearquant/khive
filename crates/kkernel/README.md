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

| Subcommand | Purpose                                                                                                |
| ---------- | ------------------------------------------------------------------------------------------------------ |
| `mcp`      | Serve the MCP `request` surface — stdio, `--daemon`, or a registered transport                         |
| `exec`     | Run a verb DSL expression (or `--ops-file batch.jsonl`) through the pack registry                      |
| `sync`     | Build a working SQLite database from `.khive/kg/*.ndjson` sources                                      |
| `kg`       | `validate` / `init` / `fetch` / `update` / `export` / `import` / `status` / `hook` — KG versioning ops |
| `db`       | `migrate` / `check` — apply or report pending schema migrations                                        |
| `pack`     | `list` / `handler <name>` — introspect registered packs and their verb surface                         |
| `engine`   | Embedding-model lifecycle: list, status, migrate, drift-check                                          |
| `vector`   | Vector store capabilities and orphan sweep                                                             |
| `reindex`  | Rebuild embedding vectors and FTS documents for entities, notes, and knowledge atoms                   |
| `backend`  | `list` / `info <name>` — inspect registered storage backends                                           |

All subcommands emit JSON on stdout by default (for piping/parsing); pass `--human`
where supported for a readable table. `kkernel kg`, `kkernel sync`, and the NDJSON-to-SQLite
rebuild logic they wrap live in [`khive-vcs`](https://crates.io/crates/khive-vcs) and
[`khive-vcs-adapters`](https://crates.io/crates/khive-vcs-adapters); `kkernel`'s own
`kg/` module is a thin CLI wrapper over those libraries.

### Updating a remote commit

`kkernel kg update origin --ref v2 --repo .` resolves a remote Git ref to its full
40-character commit SHA and writes only that remote's `commit` in the existing
`.khive/kg/schema.yaml`. The JSON receipt includes `remote`, `requested_ref`,
`previous_commit`, `commit`, and `updated`. Annotated tags are peeled to a commit;
tags that name trees or blobs refuse.

```yaml
format_version: "2.0.0"
remotes:
  - name: origin
    url: /path/to/remote.git
    ref: main
    commit: "1111111111111111111111111111111111111111"
```

Use either `url` (a Git URL or a local repository path) or `repo: owner/name` (a
GitHub shorthand), never both. Relative local paths are resolved against `--repo`,
which defaults to the current directory. Omitted `--ref` uses the selected remote's
`ref`, then its `HEAD`; it does not assume a branch named `main`. Ref input must be
one branch/tag/ref name or full SHA, without refspec destinations, wildcards or
revision expressions. A malformed or unsupported schema, missing/duplicate remote,
unresolvable ref, or non-commit object leaves the original schema intact.

This Git `commit` is independent of the optional SHA-256 archive `pin`. Update does
not change that pin, `khive_version`, other schema values, NDJSON, cached archives,
or a database. Current `kg fetch` still takes explicit `--url`/`--ref`/`--pin`
arguments; it does not load schema remotes. `kg init` does not create schema.yaml.

Successful edits preserve YAML values but normalize formatting and discard comments.
An unchanged commit returns `updated: false` without rewriting any bytes. Updates
use a unique temporary Git repository and a sibling staged schema file. Concurrent
updates refuse while `.khive/state/kg-update.lock` is held; retry after the other
command completes. Detected intervening schema edits refuse too, but external
editors that ignore that lock are not covered by an atomic compare-and-swap. Files
are synced before publication, and the schema directory is synced on Unix. If
durability confirmation fails after publication, the error explicitly says the new
commit was published; inspect the schema before retrying.

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
| `KHIVE_PACKS`                     | Pack-list override (after `--pack`, before `[runtime].packs`) |
| `KHIVE_CONFIG`                    | Path to the TOML config file (also `--config`)                |
| `KHIVE_LOG`                       | Log level for stderr (JSON results on stdout are unaffected)  |
| `KHIVE_BRAIN_PROFILE`             | Brain profile for feedback routing and recall boosting        |

Two feature flags gate optional functionality, both pass-through to `khive-mcp`:
`bench-embedder` (deterministic hash embedder for benchmarking, never enabled in release
builds) and `channel-email` (SMTP/IMAP polling loop, inert without `KHIVE_EMAIL_*` env vars).

The agent, telemetry and web packs are the optional features `pack-agent`, `pack-telemetry` and
`pack-web`. All three are on by default, so a default build links the packs it always has. A build
without one (`--no-default-features`, adding back only the packs you want) does not link that pack:
it is absent from the pack list the server reports, and naming it in `--pack` or `KHIVE_PACKS` is
refused as an unknown pack.

## Where this sits

`kkernel` sits at the top of the storage dependency chain — it depends on every pack crate
(`khive-pack-kg`, `-gtd`, `-memory`, `-brain`, `-comm`, `-schedule`, `-formal`, `-knowledge`,
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

Apache-2.0.
