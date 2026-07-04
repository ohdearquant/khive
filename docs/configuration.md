# Configuration

This page is the canonical reference for how khive resolves its configuration:
which config file gets loaded, what `--db` / `KHIVE_DB` do and do not let you
override, how MCP clients should be pointed at `kkernel mcp`, and how to
diagnose a connection failure that a client only reports as a generic error.

For the full annotated field-by-field reference (every `[[engines]]`,
`[[backends]]`, `[packs.*]` key and every environment variable), see
[docs/khive-config-example.toml](khive-config-example.toml). For the
multi-backend deployment model specifically, see
[docs/multi-backend.md](multi-backend.md). This page is the entry point that
ties those two together and covers the parts operators hit first: discovery
order and the `--db` interaction.

---

## Config file discovery order

khive's production entry points (`kkernel mcp`, `kkernel exec`, `kkernel
reindex`) all resolve configuration through
`KhiveConfig::load_with_home_fallback`, which searches, in order, and loads the
first file that exists:

1. **Explicit override**: the path given by `--config <path>` or the
   `KHIVE_CONFIG` env var. `--config` wins if both are set.
2. **`./khive.toml`**: in the current working directory (project root).
3. **`./.khive/config.toml`**: in the current working directory (hidden
   project-local dir).
4. **`~/.khive/config.toml`**: user-global, under `$HOME`.

If none of the four exist, khive starts with no config file. Embedding engines
fall back to the `KHIVE_EMBEDDING_MODEL` / `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
env pair (or built-in defaults), and storage falls back to the single-file
`--db` / `KHIVE_DB` resolution described below.

A malformed file at whichever tier is found is always an error. A parse
failure is never silently skipped in favor of a lower tier.

(Source: `KhiveConfig::load_with_home_fallback` and the inner
`load_with_roots`, `crates/khive-runtime/src/engine_config.rs`, the `load`
family starting around line 298.)

### The naming wrinkle: `khive.toml` vs `config.toml`

The two accepted filenames are not interchangeable at every tier, and this
trips people up:

- **Project root** (tier 2) only looks for a file literally named
  `khive.toml`. `./config.toml` at the project root is not read.
- **Both the project-local hidden dir (tier 3) and the user-global dir (tier
  4)** only look for a file named `config.toml`, i.e. `.khive/config.toml`
  and `~/.khive/config.toml`. `.khive/khive.toml` is not read.

So the common global file in practice is `~/.khive/config.toml`, not
`~/.khive/khive.toml`, even though most doc comments and error messages in
the source refer to the config generically as "khive.toml" (the annotated
reference file is literally named `docs/khive-config-example.toml`, and code
comments say things like "the resolved config file (`khive.toml`)" regardless
of which tier actually supplied it). When you see "khive.toml" in an error
message or a comment, treat it as shorthand for "your resolved config file,"
not as a literal filename requirement outside tier 2.

### Daemon working-directory sensitivity

`kkernel mcp --daemon` (the warm daemon auto-spawned behind stdio clients,
ADR-049) resolves its config the same way, from its own working directory.
Tiers 2 and 3 are relative to _the daemon's_ cwd, not the client's. If the
daemon is spawned from an unexpected directory it can pick up a different
project-local config than you expect, or none. The reliable ways to pin this
down:

- Pass `--config <absolute-path>` explicitly on the `kkernel mcp` invocation.
- Rely on tier 4 (`~/.khive/config.toml`), which is found regardless of
  working directory.

See [Multi-backend deployment guide § The daemon and config
discovery](multi-backend.md#the-daemon-and-config-discovery) for the
`config_id` fingerprinting behavior when a running daemon's config differs
from what a client expects.

---

## The `[[backends]]` model

`[[backends]]` entries assign each pack its own SQLite file (or an in-memory
database for testing) instead of the single implicit `main` backend every
pack shares by default. A backend entry looks like:

```toml
[[backends]]
name   = "main"
kind   = "sqlite"
path   = "~/.khive/khive.db"

[[backends]]
name   = "sessions"
kind   = "sqlite"
path   = "~/.khive/sessions.db"

[packs.session]
backend = "sessions"
```

The full field reference (`name`, `kind`, `path`, `read_only`, and the
currently-rejected `cache_mb` / `journal_mode` fields) and the pack-routing
model (which packs default to `main`, how a custom pack binds a backend, the
`main`-backend requirement, canonical-path deduplication, and cross-backend
operation limits) are documented in full in [docs/multi-backend.md](multi-backend.md)
and annotated inline in [docs/khive-config-example.toml](khive-config-example.toml).
This page does not repeat that material. Read those two for anything beyond
the `--db` interaction below.

---

## `--db` / `KHIVE_DB` semantics

`--db` (and its env equivalent `KHIVE_DB`) selects a single SQLite file (or
`:memory:`) for the implicit `main` backend. Its behavior depends entirely on
whether the resolved config file declares any `[[backends]]`:

### No `[[backends]]` declared (single-file mode)

This is the default for anyone who has never touched `[[backends]]`. `--db`
/ `KHIVE_DB` behaves exactly as it always has:

```bash
kkernel mcp                                # ~/.khive/khive.db (default)
kkernel mcp --db /path/to/my.db            # custom path
KHIVE_DB=/path/to/my.db kkernel mcp        # same, via env
kkernel mcp --db :memory:                  # ephemeral, in-process only
```

### `[[backends]]` declared

Once one or more `[[backends]]` entries exist in the resolved config, the
backend topology and every backend's file path are considered authoritative.
Two cases:

- **`--db :memory:` / `KHIVE_DB=:memory:`**: accepted as a deliberate,
  documented escape hatch. It forces _every_ declared backend to an in-memory
  database for that invocation, logged loudly at `warn` level. This is for
  ephemeral test runs where you want the declared pack-to-backend topology
  exercised without touching any real file on disk.

- **Any other concrete `--db` path**: rejected at startup, fail-loud, with:

  ```
  --db "<path>" (or KHIVE_DB) cannot be combined with [[backends]]: N
  backend(s) are already declared in khive.toml, so applying this override
  here is ambiguous (it could silently collapse distinct declared backends
  onto a single file). Edit khive.toml directly to change backend paths, or
  pass --db :memory: to force all backends in-memory for this invocation.
  ```

  (Source: `build_registry_for_multi_backend`, `crates/khive-mcp/src/serve.rs`.
  Verified live against the 0.3.0 binary: a config declaring two `[[backends]]`
  entries plus a concrete `--db`/`KHIVE_DB` override exits with this exact
  message and process exit code 1.)

**Why this fails loud instead of silently applying `--db` to `main` only, or
to every backend:** with two or more distinct declared backend files, a
concrete `--db` override is inherently ambiguous. It could mean "route
everything to this one file instead" (silently collapsing physically
separated substrates back together, defeating the entire point of declaring
them) or "just override `main`, leave the others alone" (a different,
unstated, and equally plausible interpretation). Rather than guess and risk
silent data mis-routing, khive refuses to start and tells you to either edit
`khive.toml` directly or use the explicit `:memory:` escape hatch.

**If your config previously had no `[[backends]]` and you now add some:** the
first thing to check for any client config that still passes a concrete
`--db`/`KHIVE_DB` value is whether that value needs to be removed. Once
backends are declared, the file paths live in the config, not on the command
line.

---

## MCP client configuration

`kkernel mcp` (or, in multi-backend mode, the same command backed by a config
file) is the entry point for every MCP client. When your config declares
`[[backends]]`, do not pass `--db`/`KHIVE_DB` at all. The config file is
authoritative for backend paths.

### Claude Code (`.mcp.json` or `.claude/settings.json`)

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": ["mcp"]
    }
  }
}
```

If you need a config file at a location the daemon's working directory won't
reliably find (see [Daemon working-directory sensitivity](#daemon-working-directory-sensitivity)
above), pin it explicitly:

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": ["mcp", "--config", "/absolute/path/to/khive.toml"]
    }
  }
}
```

### Codex CLI (`~/.codex/config.toml`)

```toml
[mcp_servers.khive]
command = "kkernel"
args = ["mcp"]
```

### Gemini CLI (`~/.gemini/settings.json`)

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": ["mcp"]
    }
  }
}
```

### Migration note

If you passed `--db` in any of the above configs before upgrading to 0.3.0
and your config file now declares `[[backends]]`, remove the `--db` argument
(and unset `KHIVE_DB` if it is set in the client's environment). Leaving it in
place is what produces the connect failure in
[Troubleshooting](#troubleshooting-a-32000-connect-failure) below. The config
file's `[[backends]]` paths are authoritative once declared; there is no
partial-override mode.

---

## Troubleshooting a connect failure

**Symptom:** an MCP client (Claude Code, Claude Desktop, Codex, Gemini) reports
a generic connection error such as `-32000` when it tries to start `kkernel
mcp`, with no further detail in the client UI.

**Cause:** most MCP clients treat "the server process exited before completing
the handshake" as an opaque transport error and do not surface the server's
own stderr output. If `kkernel mcp` exits at startup (a bad `--db` /
`[[backends]]` combination, a malformed config file, an invalid `--actor`
namespace, etc.) the client only shows you the transport-level symptom, not
the reason.

**Diagnosis:** run the exact same command your client would run, from the
same working directory, with a minimal MCP `initialize` request piped to
stdin. This surfaces the server's real stderr message directly in your
terminal instead of behind the client's error swallowing:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0.0.1"}}}' \
  | kkernel mcp
echo "exit: $?"
```

Add whatever flags/env your client config uses (`--config <path>`,
`--db <path>`, `KHIVE_DB=...`, etc.) to reproduce the exact failing
invocation. A startup failure (like the `--db` + `[[backends]]` conflict
above) prints its full error message to stderr and exits with a non-zero
status before the `initialize` response is ever produced. That message is
the actual root cause, whatever opaque code the client showed you.

If the probe succeeds (you get back a JSON-RPC `initialize` response), the
server itself is healthy and the problem is elsewhere in the client's
transport setup (working directory, PATH resolution for the `kkernel` binary,
permissions on the socket/config paths).

---

## References

- [docs/khive-config-example.toml](khive-config-example.toml): full annotated
  field and environment-variable reference.
- [docs/multi-backend.md](multi-backend.md): the `[[backends]]` /
  `[packs.*]` deployment model, pack routing, cross-backend operation limits.
- `crates/khive-runtime/src/engine_config.rs`: `KhiveConfig::load`,
  `load_with_home_fallback`, `load_with_roots`, `BackendConfig`, `PackConfig`.
- `crates/khive-mcp/src/serve.rs`: `build_registry_for_multi_backend` (the
  `--db` / `[[backends]]` fail-loud check), `resolve_runtime_config`.
- `crates/khive-mcp/src/args.rs`: the `kkernel mcp` CLI argument surface
  (`--db`, `--config`, `--actor`, `--namespace`, `--pack`, `--brain-profile`).
