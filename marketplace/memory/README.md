# khive memory plugin

Persistent agent memory on top of [khive-mcp](https://github.com/ohdearquant/khive).

A memory is a note with `kind = "memory"`. The memory pack adds two focused verbs: `remember` for storing durable context and `recall` for retrieving memory notes with decay-aware ranking. Memories can be tagged, typed as `episodic` or `semantic`, assigned a salience score, and optionally linked to a source entity or note.

## Verbs

All verbs are dispatched through the single MCP `request` tool ([ADR-020](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-request-dsl.md)).

| Verb                                                                                                      | What it does                                                             |
| --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `remember(content, memory_type?, salience?, decay_factor?/decay?, source_id?/source?, namespace?, tags?)` | Store a memory note with salience and decay metadata.                    |
| `recall(query, limit?, memory_type?, min_score?, min_salience?, config?, namespace?)`                     | Search memory notes only, then rank by relevance, salience, and recency. |

Memory types:

| Type       | Use for                                                                        |
| ---------- | ------------------------------------------------------------------------------ |
| `episodic` | Event-like memories tied to a session, conversation, decision, or observation. |
| `semantic` | Stable facts, preferences, project context, and reusable knowledge.            |

## Skills

- **remember** - store durable context intentionally, with the right memory type and salience.
- **recall** - retrieve prior context before acting, planning, or answering from memory.

## Prerequisites

This plugin provides skills only — it does **not** bundle an MCP server.
You must install the `khive-mcp` binary and register it as an MCP server in your
harness **before** using any of the skills below.

```bash
# Install the binary
cargo install khive-mcp

# Register in your harness (Claude Code example)
claude mcp add --transport stdio khive -- khive-mcp --pack memory
```

Or add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": ["--pack", "memory"]
    }
  }
}
```

The runtime resolves the memory pack's `kg` dependency, so memory notes are stored
in the same substrate as the knowledge graph.

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install memory
```

## License

Apache-2.0
