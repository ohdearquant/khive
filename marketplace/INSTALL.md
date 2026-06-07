# khive Marketplace Plugin Installation

This document covers how to install khive plugins for Claude Code and verify they are wired
correctly to a running `kkernel mcp` server.

## Version compatibility

| Plugin    | Version | kkernel |
| --------- | ------- | ------- |
| kg        | 0.2.4   | ≥ 0.2.4 |
| gtd       | 0.2.4   | ≥ 0.2.4 |
| memory    | 0.2.4   | ≥ 0.2.4 |
| brain     | 0.2.4   | ≥ 0.2.4 |
| comm      | 0.2.4   | ≥ 0.2.4 |
| schedule  | 0.2.4   | ≥ 0.2.4 |
| knowledge | 0.2.4   | ≥ 0.2.4 |

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
      ]
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

## Step 3 — Install the plugins

```bash
# From the repo root
claude plugin install marketplace/kg
claude plugin install marketplace/gtd
claude plugin install marketplace/memory
claude plugin install marketplace/brain
claude plugin install marketplace/comm
claude plugin install marketplace/schedule
claude plugin install marketplace/knowledge
```

Or manually copy each plugin directory into `~/.claude/plugins/`:

```bash
cp -r marketplace/kg        ~/.claude/plugins/kg
cp -r marketplace/gtd       ~/.claude/plugins/gtd
cp -r marketplace/memory    ~/.claude/plugins/memory
cp -r marketplace/brain     ~/.claude/plugins/brain
cp -r marketplace/comm      ~/.claude/plugins/comm
cp -r marketplace/schedule  ~/.claude/plugins/schedule
cp -r marketplace/knowledge ~/.claude/plugins/knowledge
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
request(ops="gtd.assign(title=\"install-test task\", priority=\"p3\")")
request(ops="gtd.next(limit=3)")
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
