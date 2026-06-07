# khive

A research knowledge graph runtime — 63 verbs, 7 packs, one MCP tool.

[![GitHub](https://img.shields.io/github/stars/ohdearquant/khive?style=flat)](https://github.com/ohdearquant/khive)
[![crates.io](https://img.shields.io/crates/v/khive-mcp.svg)](https://crates.io/crates/khive-mcp)

## Install

```bash
npm install -g khive
```

## Configure

Add to `.mcp.json` (project-level or `~/.claude/mcp.json` for global):

```json
{ "mcpServers": { "khive": { "command": "khive", "args": ["mcp"] } } }
```

All 7 packs load by default. A background daemon auto-spawns to keep the runtime warm.

## What you get

| Pack          | Verbs | What it does                                     |
| ------------- | ----- | ------------------------------------------------ |
| **kg**        | 16    | Entities, edges, notes, graph queries, proposals |
| **gtd**       | 5     | Task lifecycle (inbox → next → active → done)    |
| **memory**    | 2     | Salience-weighted remember / decay-ranked recall |
| **brain**     | 13    | Bayesian user profiles + feedback loop           |
| **comm**      | 5     | Threaded messaging                               |
| **schedule**  | 4     | Reminders and scheduled verb execution           |
| **knowledge** | 18    | Atom-based KB with embedding rerank search       |

## Usage

```text
khive mcp                   # Start MCP server (used by Claude Code)
khive kg init               # Initialize .khive/kg/ in a git repo
khive kg validate           # Check NDJSON files against schema
khive kg commit -m "msg"    # Validate + stage + git commit
```

## Documentation

- [GitHub](https://github.com/ohdearquant/khive)
- [AGENTS.md](https://github.com/ohdearquant/khive/blob/main/AGENTS.md) — full verb reference
- [Marketplace plugins](https://github.com/ohdearquant/khive/tree/main/marketplace) — Claude Code
  skills
