# Multi-Backend Deployment Guide

This guide covers how to configure `kkernel mcp` with multiple SQLite files so
that different packs write to physically separate databases. The headline use
case is isolating audit or operational records from agent memory: a deployment
can store operator-specific data in its own file without that data mixing into
the shared agent-memory database.

References: [ADR-028](adr/ADR-028-pack-scoped-backends.md) (per-pack backend
config), [ADR-029](adr/ADR-029-substrate-coordinator.md) (SubstrateCoordinator
and cross-backend operations).

See also [docs/configuration.md](configuration.md) for the `--db` / `[[backends]]`
interaction (including the fail-loud rejection when both are combined), MCP
client configuration, and troubleshooting a connect failure.

---

## What it is and when to use it

Multi-backend mode assigns each pack its own `KhiveRuntime`, which in turn owns
a separate SQLite file. The assignment is declared in `khive.toml` using
`[[backends]]` entries and `[packs.<name>]` sections. The server resolves verb
calls to the correct runtime based on which pack owns the verb.

Use multi-backend when you need:

- **Physical file isolation.** Audit, operational, or custom-pack records must not share a
  WAL, page cache, or backup cadence with the agent-memory corpus.
- **Independent backup granularity.** `sqlite3 .backup` can target one file
  without locking the other.
- **Read-only archives.** A frozen historical corpus can be opened
  `read_only = true`; writes are rejected at the backend level.
- **Per-domain failure isolation.** Corruption in one file does not corrupt the
  other.

If none of these apply, the default single-backend path (one
`~/.khive/khive.db` file) is simpler and identical in behavior.

---

## Activation rule

Multi-backend mode engages **only** when at least one `[[backends]]` entry is
declared in the resolved config file. From `crates/khive-mcp/src/serve.rs`:

```rust
if khive_cfg.backends.is_empty() {
    // Single-backend path — identical to pre-ADR-028 behavior.
    let runtime = KhiveRuntime::new(config)?;
    return KhiveMcpServer::new(runtime).map_err(|e| anyhow::anyhow!("{e}"));
}

// Multi-backend path (ADR-028).
build_server_multi_backend(config, &khive_cfg)
```

With no `[[backends]]` section the server takes the byte-identical
single-backend path: existing databases, MCP clients, and agent code are
unaffected.

### The `main` backend is required

When `[[backends]]` is non-empty, one entry must have `name = "main"`. The
server uses `main` as the fallback for any pack not listed under `[packs.*]`.
If `main` is absent, the server exits at startup with:

```
[[backends]] is declared but no backend named "main" was found; \
add a [[backends]] entry with name = "main"
```

(Source: `build_server_multi_backend` and `build_registry_for_multi_backend` in
`crates/khive-mcp/src/serve.rs`, lines 369-377 and 141-148.)

### Pack default

Any pack not listed under `[packs.*]` defaults to `main`. From
`build_server_multi_backend`:

```rust
let backend_name = khive_cfg
    .packs
    .get(pack_name.as_str())
    .map(|pc| pc.backend.as_str())
    .unwrap_or(BackendId::MAIN);
```

---

## Config syntax

The config file is `khive.toml` at the project root (or `.khive/config.toml`,
or `~/.khive/config.toml`; see [Config resolution](#config-resolution) below).

```toml
# ── Actor identity ─────────────────────────────────────────────────────────────
[actor]
id = "lambda:app"                   # attribution label (optional)
display_name = "khive app node"     # advisory; not used by the runtime

# ── Embedding engines ──────────────────────────────────────────────────────────
[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true

# ── Backends ───────────────────────────────────────────────────────────────────
# Every [[backends]] entry declares one SQLite file (or in-memory db for testing).
# Exactly one entry must be named "main".

[[backends]]
name   = "main"
kind   = "sqlite"
path   = "~/.khive/agent.db"   # tilde is expanded to $HOME at boot
# read_only = false             # default; set true to reject all writes

[[backends]]
name   = "records"
kind   = "sqlite"
path   = "~/.khive/records.db"

# ── Per-pack routing ───────────────────────────────────────────────────────────
# Packs not listed here fall back to "main".
# The value of `backend` must match a [[backends]].name above.

[packs.records]           # hypothetical operator-supplied records pack
backend = "records"

# All built-in packs (kg, memory, brain, gtd, knowledge, schedule, comm,
# session, git, code) are left unlisted and therefore default to "main".
```

Fields on `[[backends]]`:

| Field       | Type    | Required | Description                                            |
| ----------- | ------- | -------- | ------------------------------------------------------ |
| `name`      | string  | yes      | Unique name; referenced by `[packs.*].backend`.        |
| `kind`      | string  | no       | `"sqlite"` (default) or `"memory"` (testing only).     |
| `path`      | path    | sqlite   | Filesystem path; tilde expanded. Created if absent.    |
| `read_only` | boolean | no       | Open read-only. Writes return an error. Default false. |

`cache_mb` and `journal_mode` fields are parsed but immediately rejected at
startup:

```
[[backends]] entry "main": field `cache_mb` is not yet supported; \
remove it from the config or wait for a future release that implements it
```

(Source: `ConfigError::UnsupportedBackendField` in
`crates/khive-runtime/src/engine_config.rs`, lines 59-62, enforced in
`validate()` at lines 446-457.)

---

## The custom-pack isolation pattern

The recommended split assigns all built-in agent packs to `main` and routes
a custom operator pack to its own file.

```toml
[[backends]]
name = "main"
kind = "sqlite"
path = "~/.khive/agent.db"

[[backends]]
name = "records"
kind = "sqlite"
path = "/var/lib/your-app/records.db"

# Built-in packs (kg, memory, brain, gtd, knowledge, schedule, comm,
# session, git, code) are NOT listed here; they fall back to "main" automatically.

[packs.records]
backend = "records"
```

The guarantee: each pack's substrate writes are confined to its assigned
backend file. This is enforced structurally because `build_server_multi_backend`
constructs one `KhiveRuntime` per pack, each wrapping its own
`Arc<StorageBackend>`. The custom pack's runtime holds only its assigned
backend; it has no handle to `agent.db`. The isolation test
(`multi_backend_isolates_pack_data_to_separate_files` in
`crates/khive-mcp/src/serve.rs`) verifies this directly: it pins the `comm` pack
to a second on-disk backend, writes through both packs, then opens each SQLite
file independently and asserts that each file holds only its own pack's rows and
none of the other's.

### Namespace is not isolation

Assigning different namespaces to records is not a substitute for backend
separation. ADR-007 Rule 0 states explicitly:

> "Actor identity... is attribution only: stamped on write records and
> gate-request context, available for logging, filtering, and operator policy
> input. It never silently becomes the storage namespace and never gates by-ID
> access."

And Rule 1:

> "Stores are unscoped database connections... No inline namespace == checks in
> handlers or stores are permitted. No per-namespace storage partitioning."

(Source: `docs/adr/ADR-007-namespace.md`, Rules 0 and 1.)

Physical backend separation is the only mechanism that guarantees records owned
by one pack cannot appear in another pack's query, regardless of namespace
values.

---

## External pack integration

No domain-specific operator packs are included in the khive repository. The design
assumes that custom operator packs (such as `your-custom-pack`) are maintained in
a separate crate and linked into a custom binary.

Because packs self-register at link time via the `inventory` crate (ADR-027),
once a custom pack crate is compiled into the binary it is available to the
server without any change to khive itself. From
`crates/khive-runtime/src/pack.rs`:

```rust
pub fn register_packs_with_runtimes(
    names: &[String],
    runtimes: &HashMap<String, KhiveRuntime>,
    default_runtime: &KhiveRuntime,
    builder: &mut VerbRegistryBuilder,
) -> Result<(), PackLoadError> {
    let all: Vec<&'static dyn PackFactory> = inventory::iter::<PackRegistration>
        .into_iter()
        .map(|r| r.0)
        .collect();
    // ...
    for name in names {
        let factory = factory_for(name.as_str()).unwrap();
        let runtime = runtimes
            .get(name.as_str())
            .cloned()
            .unwrap_or_else(|| default_runtime.clone());
        builder.register_boxed(factory.create(runtime.clone()));
```

The registry walks `inventory::iter::<PackRegistration>` to enumerate every
pack linked into the process. Any pack crate that calls `inventory::submit!` at
link time is discoverable without a code change to khive. Once the binary
links in `your-custom-pack`, the config entry `[packs.records] backend =
"records"` is sufficient to route that pack to its own backend.

The pack author is responsible for ensuring their pack name matches the string
used in `[packs.*]`. No khive change is required.

---

## Config resolution

`KhiveConfig::load_with_home_fallback` searches for the config file in this
order (first match wins):

1. Explicit path from `--config` flag or `KHIVE_CONFIG` env var.
2. `./khive.toml` in the current working directory (project root).
3. `<db-dir>/.khive/config.toml`, anchored beside an explicit resolved
   database path; with no explicit database this is
   `./.khive/config.toml`. When the database directory itself is named
   `.khive`, the file is `<db-dir>/config.toml`.
4. `~/.khive/config.toml` (user-global).

If none exist, the server starts with no config and uses the single-backend
default path.

(Source: `KhiveConfig::load_with_home_fallback` and `load_with_roots` in
`crates/khive-runtime/src/engine_config.rs`, lines 332-374.)

### The daemon and config discovery

The daemon (ADR-049) is auto-spawned as `kkernel mcp --daemon` and builds its
server from the same config resolution logic. Its working directory determines
tier 2 and tier 3 only when no explicit database anchors tier 3. If the daemon
is launched from a different directory than the MCP client expects, it may pick
up a different config file (or none).

To avoid this, use one of:

- `--config <absolute-path>` on the `kkernel mcp` invocation.
- A user-global `~/.khive/config.toml` (tier 4), which the daemon always finds
  regardless of working directory.

The daemon caches a `config_id` fingerprint that folds in the backend topology.
Two configs that differ only in pack-to-backend routing produce different
`config_id` values. A client whose `config_id` does not match the running
daemon's does not use, kill, or restart that daemon. It silently falls back to
local in-process dispatch (cold, without the daemon's warm ANN indexes), while
the daemon keeps serving its original config to clients that still match it.

This matters operationally. After you change `[[backends]]` or `[packs.*]`
routing, kill the running daemon so the next client spawns a fresh one on the
new topology. `make local` does this as part of install; otherwise stop it
manually (for example `pkill -f "kkernel mcp --daemon"`). Until the daemon is
replaced, clients on the new config run cold via local dispatch. The daemon is
killed and respawned automatically only for a stale binary or a
protocol-version mismatch, never for config-id drift.

---

## Operational notes and current limits

### Full schema on every backend

Every writable declared backend receives the full khive schema via
`run_migrations()` at startup. Packs do not write to tables outside their
substrate; the schema is applied uniformly for simplicity. There is no
per-backend schema trimming. A read-only backend is validated against the
build's exact current core-schema version instead; boot never attempts a
migration or pack-auxiliary DDL against it.

### Canonical-path deduplication

Two `[[backends]]` entries that resolve to the same canonical filesystem path
share one `Arc<StorageBackend>` and run migrations once. This prevents
`SQLITE_BUSY` from double-opening the same file and avoids running migrations
twice. The deduplication applies only to SQLite backends; `kind = "memory"`
backends are never deduplicated (each gets its own in-process database).

### Read-only backends

Setting `read_only = true` opens every connection with
`SQLITE_OPEN_READ_ONLY` and also applies `PRAGMA query_only = ON` to the pool's
writer slot, so any write attempt (DDL or DML) through that backend returns an
error. Normal single-backend boot also selects this mode when an existing
SQLite file has no filesystem write bits. Explicit read-only configuration
does not create a missing database or its parent directory.

Read-only boot is an inspection path, not a migration path. It requires the
snapshot's core migration ledger to be the exact contiguous canonical sequence
through the build's latest migration; a missing middle row or foreign version
is rejected even when `MAX(version)` matches. Core-ledger and optional-table
inspection use genuine read-only reader connections. This remains true for a
rollback-journal snapshot: a read-only file pool retains a dedicated reader
instead of degrading `reader()` onto its query-only writer slot. It skips
configured embedding-model registration, lazy store-schema/repair writes, pack
schema application, writer-task startup, checkpointing, and WAL sweeps. Optional
vector, sparse, and text tables are checked through reader connections and must
already exist. Read-only ANN cache misses do not enqueue registration or
rebuild work; memory uses exact sqlite-vec fallback and knowledge retains FTS
plus its load-only fresh-tail path. Prepare and migrate a writable copy before
inspection if validation fails.

Daemon background work follows the assigned backend, not a blanket main-mode
switch. Writer-bearing memory/knowledge ANN warm and session mirroring do not
run for a read-only pack runtime, and warm phase telemetry does not append to a
read-only event store; neither does lazy embedder-initialization telemetry. The
schedule ticker is omitted only when the schedule
pack's own runtime is read-only: it remains enabled on a writable secondary
beside a read-only main, and remains disabled on a read-only secondary beside a
writable main.

Feature-enabled email and Telegram background work is also admitted by the
runtime that serves each loop's verbs, not by `--daemon` alone. Inbound polling
requires the assigned `comm` runtime to be writable because it dispatches
`comm.ingest`, heartbeat, and cursor writes. Outbound delivery requires the
assigned `kg` runtime to be writable because it uses generic `list`/`update` to
claim and mark a message; otherwise no external send task starts, avoiding a
send followed by an inevitably failed `delivered_at` write. These gates are
independent, so a mixed topology can admit one direction without admitting the
other.

The explicit `read_only` value participates in the backend-topology portion of
the warm-daemon `config_id`. In multi-backend configuration it must agree with
the file mode: a path with no filesystem write bits and `read_only = false` is
rejected with a remedy to declare the inspection mode. Normal single-backend
boot can detect that mode from the existing file and folds the effective mode
into the same fingerprint, preventing a read-only client from reusing a daemon
that opened the path while it was writable.

Because the main event store is itself read-only, successful MCP operations do
not attempt the per-dispatch audit append. Each successful non-help operation
instead carries a stable machine-readable warning beside its canonical result:

```json
{
  "ok": true,
  "tool": "stats",
  "result": { "entities": 42 },
  "advisories": [
    {
      "code": "audit_persistence_skipped_read_only",
      "severity": "warning",
      "component": "audit_event_store",
      "reason": "read_only_backend"
    }
  ]
}
```

The warning does not weaken mutation checks: mutating verbs still return a
per-operation error from the read-only SQLite contract. Operators that require
durable audit or accounting must use a writable backend; this inspection mode
does not claim those durability guarantees.

### Cross-backend link and federated search

Cross-backend `link` operations work: the coordinator stores the edge on the
source's backend with a `target_backend` column naming the target's backend.
Federated `search(kind=entity)` and `search(kind=note)` fan out across all
backends hosting that substrate kind and fuse results with unweighted RRF.

### Deferred operations

The following are deferred per ADR-029 (D5 and D6):

- **Cross-backend `traverse` and `neighbors(direction=In)`:** the target design
  (BFS following `target_backend` edges) is specified but not shipped. Callers
  receive results from the pack's own backend only.
- **Cross-backend `merge`:** returns `CrossBackendMergeUnsupported`. The
  workaround is to export, delete, and re-import on the target backend.
- **Props/tags federated-search filters:** filtering on entity properties or
  tags across multiple backends is tracked in issue #176 and is not yet
  implemented.
- **Partition tolerance health map (D6):** the coordinator does not maintain
  per-backend health state in the current release.

### Unsupported config fields

As noted above, `cache_mb` and `journal_mode` are parsed to give operators an
early error rather than silently ignoring misconfiguration. Remove them from
your config until a future release implements them.

### Validation at load time

The parser validates that every `[packs.*].backend` value references a declared
`[[backends]].name`. A forward reference (a pack pointing at a backend name not
listed in `[[backends]]`) fails at config load with:

```
[packs.records].backend = "records" references an unknown backend; \
defined backends: main
```

(Source: `ConfigError::UnknownPackBackend` in
`crates/khive-runtime/src/engine_config.rs`, lines 48-56.)
