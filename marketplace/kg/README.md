# kg — Knowledge Graph Plugin

Persistent knowledge graph for AI agents. Typed entities, closed edge ontology, hybrid search,
GQL/SPARQL queries — all via MCP.

Part of the [khive](https://github.com/ohdearquant/khive) marketplace.

## Prerequisites

This plugin provides skills and agents only — it does **not** bundle an MCP server. You must install
the `khive-mcp` binary and register it as an MCP server in your harness **before** using any of the
skills or agents below.

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

| Verb        | Key params                                                                                                          | What it does                                |
| ----------- | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------- |
| `create`    | `kind` (req), `name?`, `entity_kind?`, `note_kind?`, `content?`, `description?`, `tags?`, `properties?`, `annotates?` | Create entities or notes                    |
| `get`       | `id` (req, UUID)                                                                                                    | Fetch any record by UUID                    |
| `list`      | `kind` (req), `limit?`, `offset?`, `entity_kind?`, `tags?`, `note_kind?`, `source_id?`, `target_id?`, `relations?`, `min_weight?`, `max_weight?`, `direction?` | Browse with filters                         |
| `update`    | `id` (req), `kind?`, `name?`, `description?`, `content?`, `relation?`, `weight?`, `properties?`, `tags?`           | Patch entity, note, or edge fields          |
| `delete`    | `id` (req), `kind?`, `hard?`                                                                                        | Soft (default) or hard delete               |
| `merge`     | `into_id` (req), `from_id` (req)                                                                                    | Deduplicate two entities                    |
| `search`    | `kind` (req), `query` (req), `limit?`, `entity_kind?`, `note_kind?`, `tags?`, `properties?`, `include_superseded?`, `min_score?` | Hybrid FTS5 + vector search                 |
| `link`      | `source_id` (req), `target_id` (req), `relation` (req), `weight?`                                                  | Create typed directed edge (self-loops rejected) |
| `neighbors` | `node_id` (req), `direction?` (default `"out"`), `relations?`, `min_weight?`                                        | Immediate graph neighbors                   |
| `traverse`  | `roots` (req, array), `max_depth?` (default 3), `relations?`, `direction?`                                          | Multi-hop BFS                               |
| `query`     | `query` (req, GQL string), `limit?` (default 500, cap 10000)                                                        | GQL/SPARQL pattern matching                 |
| `propose`   | `title` (req), `description` (req), `changeset` (req), `reviewers?`, `expiry?`, `parent_id?`                        | Create an event-sourced change proposal     |
| `review`    | `proposal_id` (req), `decision` (req), `comment?`                                                                   | Review a proposal                           |
| `withdraw`  | `proposal_id` (req), `rationale?`                                                                                   | Withdraw an open proposal                   |

**Proposal lifecycle**: `open → approved → applying → applied` (happy path). Terminal states:
`rejected`, `withdrawn`. `applying` is a transient in-flight state; `withdraw` is rejected while the
apply worker holds it. `propose` returns `proposal_id` — pass this to `review` and `withdraw`, not
an `id` field. `review` is rejected if the proposal is not in `open` or `changes_requested` state.

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

The kg agents are designed to collaborate **via the GTD pack's task queue**, not by direct
orchestration. Each agent on completion `assign`s follow-up tasks to the next agent in the pipeline,
and at start runs `gtd.next(assignee=<self>)` to pull its queue.

Pipeline shape:

```
digester ──► polisher ──► gap-analyst ──► expander ──► polisher (verify)
                            │                    │
                            └─► librarian        └─► digester (prior art)
                                (taxonomy gaps)
```

To enable the swarm: install **both** `kg` and `gtd` plugins, and ensure your MCP server loads both
packs:

```bash
/plugin install kg
/plugin install gtd
```

MCP server config (both packs):

```json
{ "args": ["--pack", "kg", "--pack", "gtd"] }
```

Each agent file documents its `Pickup protocol` and `Handoff protocol` sections — read those to
understand which tasks land in your queue and which you assign on completion. A scheduled (or
hook-triggered) `gtd.next(assignee=<agent>)` poll is enough to keep the swarm moving; no central
orchestrator required.

## Namespace Rule (ADR-007)

KG entities live in the **shared** namespace (`local` by default). Even when your MCP server runs
with `--actor lambda:myproject`, KG operations (`create`, `link`, `search`, `list`, `get`,
`neighbors`, `traverse`, `query`) use the shared namespace — not the actor namespace.

This is by design: the knowledge graph is cross-project shared knowledge. "LoRA" is one entity that
multiple projects link to via `implements`/`depends_on` edges. If each project wrote to its own
namespace, entities would be invisible across projects, duplicates would proliferate, and
cross-project edges would be impossible.

Scoped packs (memory, GTD, comm, brain, schedule) correctly use the actor namespace — those are
per-agent operational data.

## Schema

**8 entity kinds**: concept, document, dataset, project, person, org, artifact, service

**15 edge relations**: contains, part_of, instance_of, extends, variant_of, introduced_by,
supersedes, derived_from, precedes, depends_on, enables, implements, competes_with, composed_with,
annotates

**5 note kinds**: observation, insight, question, decision, reference

All closed sets — enforced at compile time.

## What's New in 0.2.3

- **Entity tags filter**: `search` and `list` now accept a `tags` parameter to filter results by tag
  values.
- **Warm startup**: `KgPack` initializes eagerly on server start, reducing cold-call latency for the
  first verb dispatch.

## Links

- [crates.io](https://crates.io/crates/khive-mcp)
- [GitHub](https://github.com/ohdearquant/khive)
- [AGENTS.md](https://github.com/ohdearquant/khive/blob/main/AGENTS.md)
