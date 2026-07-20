# khive

A research knowledge graph runtime — 65 verbs, 10 packs, one MCP tool.

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

All 11 packs load by default. A background daemon auto-spawns to keep the runtime warm.

## What you get

| Pack          | Verbs | What it does                                                                                                                                     |
| ------------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| **kg**        | 18    | Entities, edges, notes, graph queries, proposals                                                                                                 |
| **gtd**       | 5     | Task lifecycle (inbox → next → active → done)                                                                                                    |
| **memory**    | 5     | Salience-weighted remember / decay-ranked recall                                                                                                 |
| **brain**     | 15    | Bayesian user profiles + feedback loop                                                                                                           |
| **comm**      | 7     | Threaded messaging                                                                                                                               |
| **schedule**  | 4     | Reminders and scheduled verb execution                                                                                                           |
| **session**   | 4     | Session record persistence (store/list/resume/export)                                                                                            |
| **git**       | 4     | Git-lifecycle note kinds (commit/issue/pull_request) + batch ingester + `git.digest`; write verbs `git.commit`/`git.branch`/`git.push` (ADR-108) |
| **workspace** | 0     | Adds the `workspace` entity kind + `contains` endpoint rules to git/gtd/session notes (#873)                                                     |
| **blob**      | 3     | Content-addressed object put/get/stat over the `BlobStore` CAS trait (ADR-111)                                                                   |

## Usage

```text
khive mcp                   # Start MCP server (used by Claude Code)
khive kg init               # Initialize .khive/kg/ in a git repo
khive kg validate           # Check NDJSON files against schema
```

## Documentation

- [GitHub](https://github.com/ohdearquant/khive)
- [AGENTS.md](https://github.com/ohdearquant/khive/blob/main/AGENTS.md) — full verb reference
- [Marketplace plugins](https://github.com/ohdearquant/khive/tree/main/marketplace) — Claude Code
  skills
