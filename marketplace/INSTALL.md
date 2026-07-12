# khive Marketplace Plugin Installation

This document covers how to install the khive plugin for Claude Code and verify it is wired
correctly to a running `kkernel mcp` server.

## Version compatibility

| Plugin | Version | kkernel |
| ------ | ------- | ------- |
| khive  | 0.3.1   | ≥ 0.2.4 |

`khive` is a single umbrella plugin — one pattern skill per pack plus the kg stewardship agents,
all over the one `kkernel mcp` server.

## Step 1 — Install kkernel

```bash
cargo install kkernel
```

`kkernel` is the single shipped binary; `kkernel mcp` serves the MCP `request` surface (the npm
package additionally installs `khive` / `khive-mcp` shims that forward to `kkernel mcp`). Add the
MCP server config (Step 2), and you are ready to go.

Verify:

```bash
kkernel --version
```

### Daemon (warm startup)

`kkernel mcp` automatically spawns a background daemon on first use. The daemon keeps embedding
models and the ANN index warm in memory so that subsequent requests start instantly instead of
reloading from disk. No user action is needed — install, configure, and the daemon lifecycle is
handled for you.

If you need manual control:

- **Start explicitly**: `kkernel mcp --daemon` launches the daemon in the foreground.
- **Stop**: send `SIGTERM` (the daemon shuts down cleanly).

## Step 2 — Register the MCP server in Claude Code

### Option A: project-scoped `.mcp.json`

Create or update `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": [
        "mcp",
        "--pack",
        "kg",
        "--pack",
        "gtd",
        "--pack",
        "memory",
        "--pack",
        "brain",
        "--pack",
        "comm",
        "--pack",
        "schedule",
        "--pack",
        "knowledge"
      ],
      "env": {
        "KHIVE_ACTOR": "lambda:your-id"
      }
    }
  }
}
```

### Option B: per-session CLI registration

```bash
# KG only
claude mcp add --transport stdio khive -- kkernel mcp --pack kg

# GTD only
claude mcp add --transport stdio khive -- kkernel mcp --pack gtd

# Memory only
claude mcp add --transport stdio khive -- kkernel mcp --pack memory

# Brain only (kg dependency resolved automatically)
claude mcp add --transport stdio khive -- kkernel mcp --pack brain

# Comm only
claude mcp add --transport stdio khive -- kkernel mcp --pack comm

# Schedule only
claude mcp add --transport stdio khive -- kkernel mcp --pack schedule

# Knowledge only
claude mcp add --transport stdio khive -- kkernel mcp --pack knowledge

# All packs (recommended)
claude mcp add --transport stdio khive -- kkernel mcp \
  --pack kg --pack gtd --pack memory --pack brain \
  --pack comm --pack schedule --pack knowledge
```

### Set your actor identity (attribution)

Every record you write — messages, tasks, memories — is stamped with **who you are**
(`from_actor`). That identity comes from the MCP server's actor config. If it is unset it
silently defaults to `"local"`, which leaves your messages unattributed and your `comm.inbox`
unscoped (it returns every `"local"` message, not just yours). Set it once, per agent.

The `env` block in Option A is the simplest place — set `KHIVE_ACTOR` to this agent's id (e.g.
`lambda:khive`, `lambda:lattice`). For per-session CLI registration, pass the flag instead:

```bash
claude mcp add --transport stdio khive -- kkernel mcp --actor lambda:your-id --pack kg --pack gtd
```

Resolution order (highest to lowest):

1. `--actor <id>` flag (or `KHIVE_ACTOR` env) — sets both attribution and the default namespace
2. `--namespace <id>` / `KHIVE_NAMESPACE` — legacy alias
3. `[actor] id` in a config file — attribution only (does not move the write namespace)
4. Default: `"local"`

The config-file form (searched in order `./khive.toml`, `./.khive/config.toml`,
`~/.khive/config.toml`, or an explicit `--config` / `KHIVE_CONFIG`):

```toml
[actor]
id = "lambda:your-id"
```

When the `comm` pack is loaded and the actor is still `"local"`, the server logs a startup
warning — that warning means your mail will be unattributed until you set an id.

## Step 3 — Install the plugin

```bash
# From the repo root
claude plugin install marketplace/khive
```

Or manually copy the plugin directory into `~/.claude/plugins/`:

```bash
cp -r marketplace/khive ~/.claude/plugins/khive
```

## Step 4 — Verify installation

Start a Claude Code session and confirm the MCP server responds.

### KG pack smoke tests

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"test-install\")")
request(ops="search(kind=\"entity\", query=\"test-install\")")
request(ops="delete(kind=\"entity\", id=\"<id-from-create>\")")
```

### GTD pack smoke tests

```text
request(ops="gtd.assign(title=\"install-test task\", priority=\"p3\", status=\"next\")")
request(ops="gtd.next(limit=3)")
request(ops="gtd.transition(id=\"<id-from-assign>\", status=\"active\")")
request(ops="gtd.complete(id=\"<id-from-assign>\")")
```

### Memory pack smoke tests

```text
request(ops="memory.remember(content=\"install verification note\", memory_type=\"episodic\", salience=0.1)")
request(ops="memory.recall(query=\"install verification\", limit=1)")
```

### Brain pack smoke tests

```text
request(ops="brain.profiles()")
request(ops="brain.profile(id=\"balanced-recall-v1\")")
request(ops="brain.resolve(consumer_kind=\"recall\")")
```

### Comm pack smoke tests

```text
request(ops="comm.send(to=\"local\", content=\"install verification\")")
request(ops="comm.inbox(limit=1)")
```

### Schedule pack smoke tests

```text
request(ops="schedule.agenda()")
```

### Knowledge pack smoke tests

```text
request(ops="knowledge.stats()")
request(ops="knowledge.search(query=\"test\", limit=1)")
```

## Step 5 — Run the example validator

```bash
uv run python marketplace/_validators/check_examples.py
```

All examples should report `invalid=0`.

## Troubleshooting

| Symptom                          | Fix                                                           |
| -------------------------------- | ------------------------------------------------------------- |
| `kkernel: command not found`     | Run `cargo install kkernel` or add `~/.cargo/bin` to `PATH`   |
| MCP tool not appearing in Claude | Check `.mcp.json` is in the project root; restart Claude Code |
| `Unknown verb` error             | Confirm `--pack` flag includes the right pack for the verb    |
| `Pack not loaded` error          | Verify `kkernel --version` matches the plugin version         |

## Links

- Repository: <https://github.com/ohdearquant/khive>
- ADR-016 (request DSL):
  <https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md>
- Releases: <https://github.com/ohdearquant/khive/releases>
