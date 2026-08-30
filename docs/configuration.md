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
3. **`<db-dir>/.khive/config.toml`**: anchored beside an explicit resolved
   database path; when no database is explicit, this is
   `./.khive/config.toml` in the current working directory. If the database
   itself lives in a directory named `.khive`, the file is directly beside it
   as `<db-dir>/config.toml`.
4. **`~/.khive/config.toml`**: user-global, under `$HOME`.

If none of the four exist, khive starts with no config file. Embedding engines
fall back to the `KHIVE_EMBEDDING_MODEL` / `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
env pair (or built-in defaults), and storage falls back to the single-file
`--db` / `KHIVE_DB` resolution described below.

A malformed file at whichever tier is found is always an error. A parse
failure is never silently skipped in favor of a lower tier.

### Authorization configuration is reserved

The current runtime does not expose an operator `[gate]` configuration
surface. Any present `[gate]` table, including an empty one, is rejected during
startup. This is deliberate: accepting caller-enrollment keys that the runtime
does not enforce would give operators a false authorization boundary.

The accepted authorization direction remains ADR-129's fail-closed gate and
ADR-143's store-held caller grants. ADR-143 supersedes a steady-state
configuration roster; its one-time legacy import is not implemented in this
build. Until that store-held model ships, do not add `[gate]` to `khive.toml`.
Embedders can still install a `Gate` implementation programmatically through
`RuntimeConfig::gate`.

(Source: `KhiveConfig::load_with_home_fallback` and the inner
`load_with_roots`, `crates/khive-runtime/src/engine_config.rs`, the `load`
family starting around line 298.)

### The naming wrinkle: `khive.toml` vs `config.toml`

The two accepted filenames are not interchangeable at every tier, and this
trips people up:

- **Project root** (tier 2) only looks for a file literally named
  `khive.toml`. `./config.toml` at the project root is not read.
- **Both the database-anchored/project-local hidden dir (tier 3) and the
  user-global dir (tier 4)** only look for a file named `config.toml`.
  `.khive/khive.toml` is not read.

So the common global file in practice is `~/.khive/config.toml`, not
`~/.khive/khive.toml`. Database-override refusals report the exact selected
file path. A source comment that uses "khive.toml" generically still means
"the selected config file," not a literal filename outside tier 2.

### Daemon working-directory sensitivity

`kkernel mcp --daemon` (the warm daemon auto-spawned behind stdio clients,
ADR-049) resolves its config the same way. Tier 2 is relative to _the daemon's_
cwd. Tier 3 is database-anchored when a database is explicit and cwd-anchored
otherwise. A daemon started from an unexpected directory can therefore select
a different project config when neither an explicit config nor an explicit
database anchors discovery. The reliable ways to pin this down:

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
served_kinds = ["note", "event"]

[packs.session]
backend = "sessions"
```

The full field reference (`name`, `kind`, `path`, `served_kinds`, `read_only`, and the
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
kkernel mcp --db :memory:                  # ephemeral in-memory storage
```

### `[[backends]]` declared

Once one or more `[[backends]]` entries exist in the resolved config, the
backend topology and every backend's file path are considered authoritative.
Three cases:

- **`--db :memory:` / `KHIVE_DB=:memory:`**: accepted as a deliberate,
  documented escape hatch. It forces _every_ declared backend to an in-memory
  database for that invocation, logged loudly at `warn` level. This is for
  ephemeral test runs where you want the declared pack-to-backend topology
  exercised without touching any real file on disk. `kkernel exec` forwards
  `:memory:` (and an explicit `--config`) to a warm daemon it spawns, so the
  daemon it binds is just as ephemeral; it never reuses a daemon already
  bound to the persistent files, because `:memory:` produces a distinct
  `config_id` that no persistent-storage daemon can match. A concrete `--db`
  override on a single-backend invocation (no `[[backends]]` declared) is
  likewise forwarded to the spawned daemon, so the child binds the operator's
  file instead of the default database.

- **A concrete path equal to the declared `main` backend path**: accepted as
  a redundant no-op after canonical path comparison. It does not collapse or
  replace any backend.

- **Any other concrete `--db` path**: rejected at startup, fail-loud, with an
  error that names the exact selected config file when one was loaded:

  ```
  --db "<path>" (or KHIVE_DB) cannot be combined with [[backends]]: N
  backend(s) are already declared in the selected config, so applying this override
  here is ambiguous (it could silently collapse distinct declared backends
  onto a single file). Edit the selected config file at <resolved-path> to
  change persistent backend paths. Use --config <path> or
  KHIVE_CONFIG=<path> to select a different config, or use --db :memory: only
  for an ephemeral all-in-memory invocation.
  ```

  `kkernel exec` additionally writes a compact JSON refusal envelope to stdout:

  ```json
  {
    "ok": false,
    "invocation": { "started": false },
    "error": {
      "code": "database_override_conflict",
      "message": "...",
      "db_override": "/path/to/scratch.db",
      "declared_backends": 2,
      "config_path": "/path/to/config.toml"
    }
  }
  ```

  The process remains nonzero, but automation can distinguish a no-run
  configuration refusal from a dispatched batch whose operations all failed.
  MCP startup keeps stdout protocol-clean and reports the actionable message on
  stderr only. The reported `config_path` is the canonicalized selected file
  path; under symlinks it can differ from the path you typed.

**Why this fails loud instead of silently applying `--db` to `main` only, or
to every backend:** with two or more distinct declared backend files, a
concrete `--db` override is inherently ambiguous. It could mean "route
everything to this one file instead" (silently collapsing physically
separated substrates back together, defeating the entire point of declaring
them) or "just override `main`, leave the others alone" (a different,
unstated, and equally plausible interpretation). Rather than guess and risk
silent data mis-routing, khive refuses to start and tells you to either edit
the selected config, select another one with `--config` / `KHIVE_CONFIG`, or
use the explicit `:memory:` escape hatch.

**If your config previously had no `[[backends]]` and you now add some:** the
first thing to check for any client config that still passes a concrete
`--db`/`KHIVE_DB` value is whether that value needs to be removed. Once
backends are declared, the file paths live in the config, not on the command
line.

---

## MCP client configuration

`kkernel mcp` (or, in multi-backend mode, the same command backed by a config
file) is the entry point for every MCP client. When your config declares
`[[backends]]`, omit `--db`/`KHIVE_DB` unless it canonically names the
declared `main` backend or is the explicit `:memory:` escape hatch. The config
file is authoritative for backend paths.

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
      "args": ["mcp", "--config", "/absolute/path/to/config.toml"]
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
[Troubleshooting](#troubleshooting-a-connect-failure) below. The config
file's `[[backends]]` paths are authoritative once declared; there is no
partial-override mode.

---

## Stdio bridge session lifetime

Four environment variables bound how long a stdio bridge session and its
individual writes may live, or how many requests it can admit at once. They
are read once at serve time.

| Variable                                | Default      | Effect                                                                                                                                                                                                |
| --------------------------------------- | ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS`        | **disabled** | When set to a positive number, a session that receives no request for this many seconds closes, releasing its reader-pool admission and DB connection. `0`, absent, or unparsable leaves it disabled. |
| `KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS`   | `300`        | The longest a single response write may stay pending before it is abandoned and the session closes. `0` is rejected at startup rather than treated as an opt-out.                                     |
| `KHIVE_BRIDGE_REQUEST_OBLIGATION_SECS`  | `3600`       | How long an admitted request whose response has not been written keeps deferring the idle close. Only reached when the idle timeout is enabled. `0` restores an unbounded defer.                      |
| `KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS` | `1024`       | Maximum requests admitted to rmcp while their responses remain outstanding. A full session closes before another handler is spawned. `0` or an unparsable value uses the default.                     |

**Idle reaping is off by default, and that is deliberate.**
[ADR-091](adr/ADR-091-wal-snapshot-lifetime.md) rejects closing long-lived
reader sessions by age, on the ground that they are live clients and that
bounding what they hold underneath them is the better fix. This transport has
no signal separating an abandoned pipe from a live client that has simply not
been asked anything, so enabling the idle timeout by default would reverse that
decision. Turn it on where session churn is cheap and a pinned WAL connection
is not: a supervised deployment, a CI harness, a batch runner.

The outstanding-request limit applies per stdio session and is independent of
the idle and response deadlines. The default of 1024 allows ordinary
concurrent MCP traffic while ensuring that a peer that stops reading cannot
make the session's handler and request-obligation state grow without bound.
Raise or lower it with `KHIVE_BRIDGE_MAX_OUTSTANDING_REQUESTS` when the
deployment's concurrency and memory budget require a different bound.

**A session also closes on a duplicate outstanding request id.** MCP requires a
request id to be unused within a session, and this transport tracks outstanding
requests by id in order to decide whether a quiet session still has work
running. Two live requests sharing an id make that undecidable: a completing
response could discharge either, and picking wrong either keeps a finished
session alive indefinitely or closes a live one. The second such request is
therefore refused and the session closes, with the id logged at `WARN`. A
conforming client never reaches this.

An id counts as outstanding for this purpose while its entry is still tracked,
including after it has passed the obligation TTL. Ageing past that TTL means the
request no longer defers the idle close; it does not mean the request finished,
and the handler may well still be running. Reuse is refused in that state too.

**A session also closes when an outbound message cannot be written at all.** A
write that fails is the same fact as one that outlives its deadline: the peer is
not receiving what the session tried to send it. It is worth stating separately
because the deadline does not cover it. The deadline bounds a write left
_pending_, which is what a peer that stops reading with its pipe still open
produces; a peer that closes the side it reads from fails the write immediately
instead, well inside any deadline.

This applies to responses, server-initiated requests and notifications alike.
The underlying library does not close the session for any of them: a failed send
is reported to whoever was awaiting that particular message, and its serve loop
exits only on receive EOF, cancellation, or a task join error. A session left
running against a writer that cannot write is one that will never answer
anything, so the transport ends it here. A write that succeeds changes nothing.

One narrow class of write error is excepted: an error saying the operation was
interrupted and may simply be repeated, on a message that is not a response. The
writer behind such an error is still usable, so ending the session would trade
one lost message for a session that could have gone on serving. The message is
still lost, and for these classes the error still reaches whoever was awaiting
it. Only the session survives.

A response gets no such exception, however healthy the writer is. When a
response cannot be written, the failure is recorded on the server side and
nothing is sent to the client, which is left waiting on an answer that will not
arrive and has no way to tell that from a slow one. Ending the session is what
turns that into an end-of-input the client can act on, so an interrupted
response closes the session exactly as any other failed response does.

**Known gap.** The response-delivery deadline covers responses this transport
writes. It does not cover parse-error responses, which the underlying line
transport writes directly through its own framed writer without passing through
the deadline. A peer that sends malformed input and then stops reading can leave
that one write pending.

What bounds that pending write depends on what else the session has
outstanding, and the idle window alone is not the answer:

- Idle timeout disabled: nothing bounds it.
- Idle timeout enabled, nothing else outstanding: the idle window bounds it.
- Idle timeout enabled with a request still awaiting its response: the idle
  close defers while that obligation is fresh, so the bound is the request
  obligation TTL rather than the idle window. A peer can reach this deliberately
  by admitting a request and then sending malformed input.

Closing the gap requires replacing or adapting the line transport and is tracked
separately.

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

For direct scripted execution, `kkernel exec --config <path> ...` and
`KHIVE_CONFIG=<path> kkernel exec ...` use the same explicit-config tier as
`kkernel mcp`. A database-override conflict also emits the structured no-run
envelope documented above before returning nonzero.

If the probe succeeds (you get back a JSON-RPC `initialize` response), the
server itself is healthy and the problem is elsewhere in the client's
transport setup (working directory, PATH resolution for the `kkernel` binary,
permissions on the socket/config paths).

---

## `[display]` — rendering timezone (ADR-169)

`[display] timezone` names the IANA zone (e.g. `"America/New_York"`) khive
anchors date-only input to. Absent → the host's local zone, resolved once and
falling back to UTC when it cannot be determined. An unrecognized zone name is
a config-load error, not a silent fallback.

Storage stays instant-based regardless of this setting: a date-only value like
`gtd.assign(due="2026-08-23")` is anchored to midnight in the configured zone
and stored with that zone's UTC offset (e.g. `2026-08-23T00:00:00-04:00` for a
caller anchored at UTC-4) rather than midnight UTC. A value that already
carries an explicit offset or `Z` is unaffected. See
[ADR-169](adr/ADR-169-timezone-correct-timestamps.md) for the full decision
record, including which rendering-surface work this setting does and does not
cover yet.

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
