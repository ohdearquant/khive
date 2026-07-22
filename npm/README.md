# khive

A research knowledge graph runtime — 19 verbs, 1 pack, one MCP tool.

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

The `kg` pack loads by default. A background daemon auto-spawns to keep the
runtime warm.

## What you get

| Pack   | Verbs | What it does                                     |
| ------ | ----- | ------------------------------------------------ |
| **kg** | 19    | Entities, edges, notes, graph queries, proposals |

Task management, memory recall, inter-agent messaging, scheduling, session
continuity, workspace linking, and blob storage are provided by commercially
licensed extensions and are not part of the open-source distribution.

## Usage

```text
khive mcp                   # Start MCP server (used by Claude Code)
khive kg init               # Initialize .khive/kg/ in a git repo
khive kg validate           # Check NDJSON files against schema
```

## Documentation

- [GitHub](https://github.com/ohdearquant/khive)
- [AGENTS.md](https://github.com/ohdearquant/khive/blob/main/AGENTS.md) — full
  verb reference
- [Marketplace plugins](https://github.com/ohdearquant/khive/tree/main/marketplace)
  — Claude Code skills
