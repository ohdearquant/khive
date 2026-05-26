# khive knowledge plugin

Structured knowledge management on top of [khive-mcp](https://github.com/ohdearquant/khive).

The knowledge pack provides three focused verbs for registering concepts, recording provenance,
and browsing by domain — all built on the kg substrate without duplicating storage.

## Verbs

All verbs are dispatched through the single MCP `request` tool ([ADR-020](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-request-dsl.md)).

| Verb | What it does |
| ---- | ------------ |
| `knowledge.learn(name, description?, domain?, tags?)` | Register a concept entity. Domain is stored as a property and promoted to tags for search. |
| `knowledge.cite(concept_id, source_id, weight?)` | Link a concept to the document or person that introduced it (`introduced_by` edge). |
| `knowledge.topic(domain?, query?, limit?)` | List or search concept entities, optionally filtered by domain. |

## Skills

- **learn** — register a concept with domain and tags.
- **cite** — create a provenance-tracked citation from a concept to its source.
- **topic** — browse concepts by domain or free-text query.

## Prerequisites

This plugin provides skills only — it does **not** bundle an MCP server.
Install `khive-mcp` and register it with the knowledge pack:

```bash
cargo install khive-mcp

# Claude Code
claude mcp add --transport stdio khive -- khive-mcp --pack knowledge
```

Or add to `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": ["--pack", "knowledge"]
    }
  }
}
```

The runtime loads the `kg` pack as a dependency automatically.

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install knowledge
```

## License

Apache-2.0
