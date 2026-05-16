# khive — Claude Code Plugin

Persistent knowledge graph for AI agents. Typed entities, closed edge ontology,
hybrid search, GQL/SPARQL queries — all via MCP.

## Install

```bash
# Option 1: Plugin (includes skills)
/plugin marketplace add https://github.com/ohdearquant/khive
/plugin install khive

# Option 2: Direct MCP (server only, no skills)
claude mcp add --transport stdio khive -- khive-mcp

# Option 3: From crates.io first
cargo install khive-mcp
claude mcp add --transport stdio khive -- khive-mcp
```

## What You Get

### 11 MCP Tools

| Tool | What it does |
|------|-------------|
| `create` | Create entities or notes |
| `get` | Fetch any record by UUID (or 8-char prefix) |
| `list` | Browse with filters |
| `update` | Patch entity/edge fields |
| `delete` | Soft or hard delete |
| `merge` | Deduplicate two entities |
| `search` | Hybrid FTS5 + vector search |
| `link` | Create typed directed edges |
| `neighbors` | Immediate graph neighbors |
| `traverse` | Multi-hop BFS |
| `query` | GQL/SPARQL pattern matching |

### 7 Skills (slash commands)

| Skill | Command | Purpose |
|-------|---------|---------|
| kg-digest | `/khive:kg-digest` | Ingest research into the graph |
| retrieve | `/khive:retrieve` | Choose the right retrieval verb |
| orient | `/khive:orient` | Explore graph structure + health |
| assign | `/khive:assign` | Create typed notes |
| search | `/khive:search` | Hybrid semantic search |
| curate | `/khive:curate` | Merge, dedup, supersede, delete |
| link | `/khive:link` | Edge ontology reference |

### 1 Agent

| Agent | Purpose |
|-------|---------|
| researcher | Context-aware research with KG persistence |

## Schema

**6 entity kinds**: concept, document, dataset, project, person, org

**13 edge relations**: contains, part_of, instance_of, extends, variant_of,
introduced_by, supersedes, depends_on, enables, implements, competes_with,
composed_with, annotates

**5 note kinds**: observation, insight, question, decision, reference

All closed sets — enforced at compile time.

## Links

- [crates.io](https://crates.io/crates/khive-mcp)
- [GitHub](https://github.com/ohdearquant/khive)
- [AGENTS.md](https://github.com/ohdearquant/khive/blob/main/AGENTS.md)
