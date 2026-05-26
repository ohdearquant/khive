# khive Marketplace Plugin Installation

This document covers how to install the `kg`, `gtd`, `memory`, and `brain` plugins for Claude Code
and verify they are wired correctly to a running `khive-mcp` server.

## Version compatibility

| Plugin | Version | khive-mcp |
| ------ | ------- | --------- |
| kg     | 0.2.2   | ≥ 0.2.2   |
| gtd    | 0.2.2   | ≥ 0.2.2   |
| memory | 0.2.2   | ≥ 0.2.2   |
| brain  | 0.2.2   | ≥ 0.2.2   |

## Step 1 — Install khive-mcp

```bash
cargo install khive-mcp
```

Verify:

```bash
khive-mcp --version
```

## Step 2 — Register the MCP server in Claude Code

### Option A: project-scoped `.mcp.json`

Create or update `.mcp.json` in your project root:

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": ["--pack", "kg", "--pack", "gtd", "--pack", "memory", "--pack", "brain"]
    }
  }
}
```

### Option B: per-session CLI registration

```bash
# KG only
claude mcp add --transport stdio khive -- khive-mcp --pack kg

# GTD only
claude mcp add --transport stdio khive -- khive-mcp --pack gtd

# Memory only
claude mcp add --transport stdio khive -- khive-mcp --pack memory

# Brain only (kg dependency resolved automatically)
claude mcp add --transport stdio khive -- khive-mcp --pack brain

# All four packs (recommended for kg-agent swarms with adaptive recall)
claude mcp add --transport stdio khive -- khive-mcp --pack kg --pack gtd --pack memory --pack brain
```

## Step 3 — Install the plugins

```bash
# From the repo root
claude plugin install marketplace/kg
claude plugin install marketplace/gtd
claude plugin install marketplace/memory
claude plugin install marketplace/brain
```

Or manually copy each plugin directory into `~/.claude/plugins/`:

```bash
cp -r marketplace/kg     ~/.claude/plugins/kg
cp -r marketplace/gtd    ~/.claude/plugins/gtd
cp -r marketplace/memory ~/.claude/plugins/memory
cp -r marketplace/brain  ~/.claude/plugins/brain
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
request(ops="remember(content=\"install verification note\", memory_type=\"episodic\", salience=0.1)")
request(ops="recall(query=\"install verification\", limit=1)")
```

### Brain pack smoke tests

```text
request(ops="brain.profiles()")
request(ops="brain.profile(id=\"balanced-recall-v1\")")
request(ops="brain.resolve(consumer_kind=\"recall\")")
```

## Step 5 — Run the example validator

```bash
uv run python marketplace/_validators/check_examples.py
```

All examples should report `invalid=0`.

## Troubleshooting

| Symptom                          | Fix                                                           |
| -------------------------------- | ------------------------------------------------------------- |
| `khive-mcp: command not found`   | Run `cargo install khive-mcp` or add `~/.cargo/bin` to `PATH` |
| MCP tool not appearing in Claude | Check `.mcp.json` is in the project root; restart Claude Code |
| `Unknown verb` error             | Confirm `--pack` flag includes the right pack for the verb    |
| `Pack not loaded` error          | Verify `khive-mcp --version` matches the plugin version       |

## Links

- Repository: <https://github.com/ohdearquant/khive>
- ADR-020 (request DSL): <https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-request-dsl.md>
- Releases: <https://github.com/ohdearquant/khive/releases>
