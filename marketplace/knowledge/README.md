# khive knowledge plugin

Structured knowledge management on top of [khive-mcp](https://github.com/ohdearquant/khive).

The knowledge pack provides three focused verbs for registering concepts, recording provenance, and
browsing by domain — all built on the kg substrate without duplicating storage.

## Why this pack exists

The `kg` pack gives you direct CRUD for any entity kind (`create`, `link`, `search`). The knowledge
pack adds **opinionated sugar** for the specific pattern of managing research concepts:

- `learn` = `create(kind="concept")` with automatic `domain` → tag promotion (makes domain
  filterable via FTS and the `domain=` parameter on `topic`).
- `cite` = `link(relation="introduced_by")` with weight clamped to [0, 1] and a cleaner parameter
  name (`concept_id` / `source_id` instead of `source_id` / `target_id`).
- `topic` = `search(kind="concept")` with optional post-filter on the domain tag.

Use the knowledge pack when you want the auto-promotion and concise API. Use `kg` verbs directly
when you need other entity kinds, relations, or full parameter control.

## Verbs

All verbs are dispatched through the single MCP `request` tool.

| Verb                                                  | What it does                                                                                                                        |
| ----------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| `knowledge.learn(name, description?, domain?, tags?)` | Register a concept entity. `domain` is stored in `properties.domain` and automatically added to `tags` for FTS discoverability.     |
| `knowledge.cite(concept_id, source_id, weight?)`      | Create an `introduced_by` edge from a concept to its source document or person. `weight` is clamped to [0.0, 1.0]; defaults to 1.0. |
| `knowledge.topic(domain?, query?, limit?)`            | List or search concept entities, optionally filtered by domain tag. `limit` max is 100; defaults to 20.                             |

## What's New in 0.2.3

- **Score normalization**: all search/topic scores are now normalized to `[0, 1]` for consistent
  cross-query comparison.
- **`rerank=true` default**: `knowledge.topic` and `knowledge.search` now default to `rerank=true`,
  producing cleaner relevance ordering out of the box.
- **FTS5 special character hardening**: queries containing parentheses, colons, quotes, and other
  FTS5 metacharacters are escaped automatically instead of returning parse errors.

## Skills

- **learn** — register a concept with domain and tags.
- **cite** — create a provenance-tracked citation from a concept to its source.
- **topic** — browse concepts by domain or free-text query.

## Install

`khive-mcp` ships with the knowledge pack bundled. The knowledge pack requires the `kg` pack as a
dependency — both must be listed explicitly when launching the server:

```bash
cargo install khive-mcp

# Claude Code
claude mcp add --transport stdio khive -- khive-mcp --pack kg --pack knowledge
```

Or using the `KHIVE_PACKS` environment variable:

```bash
claude mcp add --transport stdio khive -- env KHIVE_PACKS=kg,knowledge khive-mcp
```

Or add to `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": ["--pack", "kg", "--pack", "knowledge"]
    }
  }
}
```

## Plugin install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install knowledge
```

## Presentation modes (ADR-045)

The `request` tool accepts an optional `presentation` field per op: `agent` (default,
token-efficient), `verbose` (canonical full JSON), or `human` (same as `verbose` over MCP). Agents
should use the default `agent` mode.

## License

Apache-2.0
