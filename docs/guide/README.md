# khive User Guide

Documentation for using khive as a research knowledge graph runtime.

| Guide                                               | What it covers                                     |
| --------------------------------------------------- | -------------------------------------------------- |
| [Getting Started](getting-started.md)               | Install, connect, first session                    |
| [Request and DSL](request-and-dsl.md)               | Single tool, syntax, batches, chains, errors       |
| [Knowledge Graph Modeling](knowledge-graph.md)      | Entity kinds, edge relations, modeling patterns    |
| [Prompt Cookbook](prompt-cookbook.md)               | 20+ real-world verb patterns with examples         |
| [Search and Retrieval](search.md)                   | FTS, vector, hybrid fusion, reranking              |
| [Query Cookbook](query-cookbook.md)                 | GQL/SPARQL question classes, verified idioms, gaps |
| [Memory and Recall](memory.md)                      | Episodic vs semantic, salience, decay              |
| [GTD Task Management](tasks.md)                     | Task lifecycle, priorities, dependencies           |
| [Tips and Tricks](tips-and-tricks.md)               | Query craft, DSL round-trips, param gotchas        |
| [Proof-Graph Case Study](proof-graph-case-study.md) | Mathlib as a khive-at-scale case study             |

## How to read these guides

Each guide is self-contained but cross-references related topics. Start with
[Getting Started](getting-started.md) if you have never used khive, then read
[Knowledge Graph Modeling](knowledge-graph.md) for the conceptual foundation.

The [Prompt Cookbook](prompt-cookbook.md) is a reference you return to — it shows
the exact `request(ops="...")` syntax for every common operation.

## What khive is

khive is a structured persistence layer for AI research agents. It provides a
typed knowledge graph (9 entity kinds, 17 edge relations, 5 note kinds), hybrid
search (FTS5 + vector + RRF fusion), GQL/SPARQL queries, task management, agent
memory with decay-weighted recall, inter-agent messaging, scheduling, and a
knowledge corpus with reranking.

All interaction goes through a single MCP tool: `request(ops="verb(args)")`.

## What khive is not

khive is not a general-purpose database, a vector DB, or a chat memory system.
It has opinions: closed taxonomies, a fixed edge ontology, namespace isolation.
If your data does not fit the schema, reconsider how you model it before
requesting schema changes.

## Further reading

- [AGENTS.md](../../AGENTS.md) — agent usage reference (verb tables, property conventions, edge density rules)
- [CLAUDE.md](../../CLAUDE.md) — developer guide for working on khive itself
- ADR index — architecture decision records (the design contract)
