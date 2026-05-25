# kg — Knowledge Graph Plugin

Persistent knowledge graph for AI agents. Typed entities, closed edge ontology,
hybrid search, GQL/SPARQL queries — all via MCP.

Part of the [khive](https://github.com/ohdearquant/khive) marketplace.

## Prerequisites

This plugin provides skills and agents only — it does **not** bundle an MCP server.
You must install the `khive-mcp` binary and register it as an MCP server in your
harness **before** using any of the skills or agents below.

```bash
# Install the binary
cargo install khive-mcp

# Register in your harness (Claude Code example)
claude mcp add --transport stdio khive -- khive-mcp --pack kg
```

Or add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "khive-mcp",
      "args": ["--pack", "kg"]
    }
  }
}
```

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install kg
```

## What You Get

### 1 MCP tool (`request`), 14 verbs inside it

The MCP server exposes a single tool, `request`, that takes the verb call as a string:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\")")
request(ops="[search(kind=\"entity\", query=\"LoRA\"), neighbors(node_id=\"<id>\")]")  # parallel batch
```

| Verb        | What it does                                |
| ----------- | ------------------------------------------- |
| `create`    | Create entities or notes                    |
| `get`       | Fetch any record by UUID (or 8-char prefix) |
| `list`      | Browse with filters                         |
| `update`    | Patch entity, note, or edge fields          |
| `delete`    | Soft or hard delete                         |
| `merge`     | Deduplicate two entities                    |
| `search`    | Hybrid FTS5 + vector search                 |
| `link`      | Create typed directed edges                 |
| `neighbors` | Immediate graph neighbors                   |
| `traverse`  | Multi-hop BFS                               |
| `query`     | GQL/SPARQL pattern matching                 |
| `propose`   | Create an event-sourced change proposal     |
| `review`    | Review a proposal                           |
| `withdraw`  | Withdraw an open proposal                   |

### 9 Skills (workflow-shaped, not verb docs)

| Skill    | Command        | What it does                                                                                     |
| -------- | -------------- | ------------------------------------------------------------------------------------------------ |
| digest   | `/kg:digest`   | Ingest material into the graph — extract entities, link them, verify density                     |
| explore  | `/kg:explore`  | Discover what the graph knows about a topic — traverse, narrate, surface gaps                    |
| connect  | `/kg:connect`  | Wire a new concept into existing knowledge — find relations, reach density                       |
| polish   | `/kg:polish`   | Audit and fix — orphans, low-degree nodes, duplicates, stale edges                               |
| gap      | `/kg:gap`      | Strategic-gap survey — researched-but-unbuilt, decision debt, frontier ranking for planning      |
| expand   | `/kg:expand`   | Self-expansion — take a gap and grow the graph to close it (promote / bridge / extend / resolve) |
| propose  | `/kg:propose`  | Draft event-sourced KG changes for review                                                        |
| review   | `/kg:review`   | Approve, reject, comment on, or request changes for proposals                                    |
| withdraw | `/kg:withdraw` | Withdraw an open proposal with rationale                                                         |

### 6 Agents (specialized + a generic backstop)

| Agent       | Purpose                                                                                            |
| ----------- | -------------------------------------------------------------------------------------------------- |
| digester    | Bulk ingestion of source material → typed entities + edges + notes (batch-parallel friendly)       |
| polisher    | Graph hygiene — orphans, under-linked, duplicates, wrong-direction edges                           |
| gap-analyst | Strategic-gap survey — produces `gap_inventory.md` + frontier ranking (read-only)                  |
| expander    | Self-expansion — closes a specific gap by adding new entities/edges with citation discipline       |
| librarian   | Swarm health monitor — watches the agent task queue, surfaces stuck work, owns taxonomy escalation |
| researcher  | Generic backstop — open-ended KG-aware research when no specialized agent fits                     |

### Swarm coordination via GTD pack

The kg agents are designed to collaborate **via the GTD pack's task queue**, not by
direct orchestration. Each agent on completion `assign`s follow-up tasks to the next
agent in the pipeline, and at start runs `next(assignee=<self>)` to pull its queue.

Pipeline shape:

```
digester ──► polisher ──► gap-analyst ──► expander ──► polisher (verify)
                            │                    │
                            └─► librarian        └─► digester (prior art)
                                (taxonomy gaps)
```

To enable the swarm: install **both** `kg` and `gtd` plugins, and ensure your
MCP server loads both packs:

```bash
/plugin install kg
/plugin install gtd
```

MCP server config (both packs):

```json
{ "args": ["--pack", "kg", "--pack", "gtd"] }
```

Each agent file documents its `Pickup protocol` and `Handoff protocol` sections —
read those to understand which tasks land in your queue and which you assign on
completion. A scheduled (or hook-triggered) `next(assignee=<agent>)` poll is enough
to keep the swarm moving; no central orchestrator required.

## Schema

**8 entity kinds**: concept, document, dataset, project, person, org, artifact, service

**15 edge relations**: contains, part_of, instance_of, extends, variant_of,
introduced_by, supersedes, derived_from, precedes, depends_on, enables,
implements, competes_with, composed_with, annotates

**5 note kinds**: observation, insight, question, decision, reference

All closed sets — enforced at compile time.

## Links

- [crates.io](https://crates.io/crates/khive-mcp)
- [GitHub](https://github.com/ohdearquant/khive)
- [AGENTS.md](https://github.com/ohdearquant/khive/blob/main/AGENTS.md)
