# khive memory plugin

Persistent agent memory on top of [khive-mcp](https://github.com/ohdearquant/khive).

A memory is a note with `kind = "memory"`. The memory pack adds two focused verbs: `remember` for
storing durable context and `recall` for retrieving memory notes with decay-aware ranking. Memories
can be tagged, typed as `episodic` or `semantic`, assigned a salience score, and optionally linked
to a source entity or note.

## Verbs

All verbs are dispatched through the single MCP `request` tool
([ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)).

| Verb                                                                                                        | What it does                                                             |
| ----------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `memory.remember(content, memory_type?, salience?, decay_factor?, source_id?, namespace?, tags?)`           | Store a memory note with salience and decay metadata.                    |
| `memory.recall(query, limit?, memory_type?, min_score?, min_salience?, config?, namespace?, presentation?)` | Search memory notes only, then rank by relevance, salience, and recency. |

Memory types (`memory_type` values):

| Type       | Use for                                                                                  |
| ---------- | ---------------------------------------------------------------------------------------- |
| `episodic` | Event-like memories tied to a session, conversation, decision, or observation. (default) |
| `semantic` | Stable facts, preferences, project context, and reusable knowledge.                      |

`memory.recall` results include a score triplet: `score` (absolute relevance, `[0.0, 1.0]`),
`rank_score` (composite ordering score, `[0.0, 1.0]`), and `raw_score` (pre-fusion vector cosine
similarity, `[0.0, 1.0]` or `null` for text-only hits). All values are bounded to `[0.0, 1.0]`.
Passing `presentation="verbose"` adds a per-component `breakdown` field; `presentation` defaults to
`"agent"` when omitted.

## What's New in 0.2.3

- **Tags filter on recall**: `memory.recall` now accepts `tags` and `tag_mode` (`any` or `all`)
  parameters to filter recalled memories by tag values.
- **`include_embeddings` on recall**: pass `include_embeddings=true` to include raw embedding
  vectors in recall results (useful for downstream reranking or clustering).
- **`presentation` alias removed**: the deprecated `presentation` positional alias on `recall` is
  removed; use the standard `presentation` field on the `request` call instead.

## Skills

- **remember** - store durable context intentionally, with the right memory type and salience.
- **recall** - retrieve prior context before acting, planning, or answering from memory.

## Prerequisites

This plugin provides skills only — it does **not** bundle an MCP server. You must install the
`khive-mcp` binary and register it as an MCP server in your harness **before** using any of the
skills below.

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

The runtime resolves the memory pack's `kg` dependency, so memory notes are stored in the same
substrate as the knowledge graph.

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install memory
```

## License

Apache-2.0
