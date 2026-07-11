# Getting Started

This guide walks you from zero to a productive khive session: install the
binary, connect it to your MCP client, create your first entities, search the
graph, and link concepts together.

## What khive gives you

khive is a research knowledge graph runtime. When you read papers, form
concepts, link ideas, record decisions, or track tasks, khive gives that work a
typed, queryable graph that persists across sessions. Everything is accessible
through 78 verbs across 10 packs, dispatched through a single MCP tool.

## Install

### From crates.io (Rust)

```bash
cargo install kkernel
```

`kkernel` is the single shipped binary; `kkernel mcp` serves the MCP `request`
surface.

### From npm

```bash
npm install -g khive
# or
npm install -g @khive-ai/cli
```

The npm package installs `khive` / `khive-mcp` shims that forward to `kkernel mcp`. The npm
release can lag the crates.io release, so run `khive --version` after install and compare against
[crates.io/crates/khive-mcp](https://crates.io/crates/khive-mcp) if you need the latest verbs
documented here.

### From source

```bash
git clone https://github.com/ohdearquant/khive
cd khive/crates
cargo build --release -p kkernel
# Binary at target/release/kkernel (relative to crates/)
```

## Connect to your MCP client

### Claude Code

Add to your MCP configuration (`.claude/settings.json` or equivalent):

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

### Claude Desktop

Add to `claude_desktop_config.json`:

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

khive auto-spawns a background daemon on first request to keep the ANN index and
embedding model warm. You do not need to manage this: it starts automatically
and cleans up on exit.

## The single-tool interface

khive exposes one MCP tool: `request`. Every operation goes through it:

```
request(ops="verb(arg=value, arg=value)")
```

This is the only syntax you need. The `ops` string contains a verb call (or a
batch of them), and khive dispatches it to the appropriate pack handler.

## Your first session

### 1. Create an entity

Entities are the nodes in your knowledge graph. khive has 9 entity kinds:
`concept`, `document`, `dataset`, `project`, `person`, `org`, `artifact`,
`service`, `resource`.

```
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention\", description=\"IO-aware exact attention algorithm\", properties={\"domain\": \"attention\", \"year\": 2022})")
```

Response:

```json
{
  "ok": true,
  "result": {
    "id": "a1b2c3d4",
    "kind": "concept",
    "name": "FlashAttention",
    "description": "IO-aware exact attention algorithm"
  }
}
```

### 2. Create a related entity

```
request(ops="create(kind=\"entity\", entity_kind=\"document\", name=\"FlashAttention: Fast and Memory-Efficient Exact Attention\", properties={\"authors\": \"Dao et al.\", \"year\": 2022, \"source\": \"arxiv:2205.14135\"})")
```

### 3. Link them

Edges express typed relationships. `introduced_by` means "this concept was
introduced by that document":

```
request(ops="link(source_id=\"<flash_id>\", target_id=\"<paper_id>\", relation=\"introduced_by\", weight=1.0)")
```

### 4. Search the graph

Search uses hybrid FTS5 + vector similarity with RRF fusion:

```
request(ops="search(kind=\"entity\", query=\"memory efficient attention\")")
```

Returns a scored list of matching entities.

### 5. Explore neighbors

See what connects to an entity:

```
request(ops="neighbors(node_id=\"<flash_id>\", direction=\"both\")")
```

### 6. Create a note

Notes are temporal observations about your work: what you noticed, concluded,
or decided. They can annotate entities:

```
request(ops="create(kind=\"note\", note_kind=\"observation\", content=\"FlashAttention reduces memory from O(N^2) to O(N) by tiling and recomputation\", annotates=[\"<flash_id>\"])")
```

### 7. Batch operations

Run multiple independent operations in one call:

```
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention-2\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"FlashAttention-3\")]")
```

Batched ops run in parallel with no ordering guarantee. If op B depends on op
A's output, use two separate `request` calls.

### 8. Query the graph

For complex pattern matching, use GQL:

```
request(ops="query(query=\"MATCH (a:concept)-[:introduced_by]->(b:document) RETURN a.name, b.name LIMIT 10\")")
```

Or SPARQL:

```
request(ops="query(query=\"SELECT ?a ?b WHERE { ?a :introduced_by ?b . } LIMIT 10\")")
```

Both compile to the same SQL backend.

## What to read next

- [Knowledge Graph Modeling](knowledge-graph.md): how to think about entity
  kinds, edge relations, and modeling decisions
- [Prompt Cookbook](prompt-cookbook.md): 20+ ready-to-use verb patterns
- [Search and Retrieval](search.md): how hybrid search, reranking, and
  decompose work
