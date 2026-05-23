# ADR-067: Marketplace Adaptation Strategy — Patterns from the CC Ecosystem

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-065 (Plugin Intent Routing), ADR-066 (Cross-Plugin Workflows)

## Context

The Claude Code marketplace is the distribution channel for khive's OSS plugins. The plugins
that get adopted share patterns worth adapting. The plugins that don't get used share
anti-patterns worth avoiding.

khive's plugins are technically strong — the kg plugin has a closed ontology, hybrid search,
GQL queries, and a self-coordinating agent swarm. But technical strength doesn't guarantee
adoption. The marketplace is a discovery problem, not a capability problem.

### Patterns from successful CC marketplace plugins

**1. One-intent entry points** (create-pdf, sequential-thinking)\
The most-used plugins have a single, obvious entry point. "I need a PDF" → create-pdf.
"I need to think step by step" → sequential-thinking. khive's kg plugin has 6 skills —
none of them is the obvious "I need a knowledge graph" entry point.

**2. Zero-config first experience** (Context7, various memory tools)\
Install → use immediately. No setup, no configuration, no "first run these migrations."
khive requires `cargo install khive-mcp` and has an embedded SQLite database that
auto-initializes — this is good. But the plugin.json `installHint` just says
`cargo install khive-mcp` without mentioning that it works out of the box.

**3. Progressive disclosure** (well-structured READMEs)\
Best plugins show the simplest use first, then reveal depth. khive's kg README leads with
the verb table (17 verbs) before showing the skills. The skills are the entry point; the
verbs are the implementation detail.

**4. Composability signals** (plugins that advertise what they work with)\
The best plugins explicitly say "works great with X." khive's plugins mention each other
in passing but don't make composition a first-class selling point.

### Anti-patterns from unsuccessful plugins

**1. Tool catalog syndrome** — listing every function without explaining when to use any of
them. khive's verb tables risk this.

**2. Expert-only descriptions** — "hybrid FTS5 + vector search" means nothing to an agent
that just wants to find stuff. Lead with the job-to-be-done.

**3. Missing examples** — skills that explain the workflow abstractly without showing the
exact tool calls. khive's skills are good here — they show real `request(ops="...")` calls.

**4. Placeholder plugins** — empty marketplace entries that promise future functionality.
This was khive's lambda and leo plugins (now removed per this sweep).

## Decision

### 1. README structure: progressive disclosure

Every plugin README follows this structure:

```markdown
# {plugin name}

{One sentence: what job this plugin does, in intent language}

## Quick start

{3-5 lines: install + first useful command. Copy-paste ready.}

## Skills (what you can do)

{Table: skill name, slash command, one-line trigger description}

## Verbs (API reference)

{Table: verb, parameters, what it returns}

## Agents (for swarm/autonomous use)

{Table: agent, purpose, pickup/handoff protocol}

## Works with

{Which other khive plugins compose with this one and how}
```

The ordering is intentional: job → quick start → skills → verbs → agents → composition.
An agent scanning the README hits the useful information first and the reference material
last.

### 2. Quick start as the zero-config proof

Each plugin's quick start must demonstrate value in under 5 lines:

```markdown
## Quick start

Install:
/plugin install kg

Try it:
/kg:explore quantum computing

That's it. The graph starts empty and grows as you use it.
```

No "first configure your database" or "set these environment variables." The embedded
SQLite + auto-migration means khive genuinely works out of the box — the quick start
should prove it.

### 3. One-intent entry skill per plugin

Each plugin designates a **primary skill** — the one an agent should try first:

| Plugin | Primary skill | Intent trigger             |
| ------ | ------------- | -------------------------- |
| kg     | explore       | "What do I know about X?"  |
| gtd    | capture       | "I need to track this"     |
| memory | recall        | "What happened last time?" |

The primary skill is listed first in the README's skills table and is the `entry_skill`
in the capability map (ADR-065). It's the answer to "I just installed this, now what?"

### 4. "Works with" section in every README

Each plugin README ends with a composition section:

```markdown
## Works with

- **gtd**: kg agents hand off follow-up work via `assign`. Install both for swarm mode.
- **memory**: `remember` your research sessions; `recall` before the next one.
- **workflows**: Install all three for cross-plugin workflows (/research, /audit, /onboard).
```

This makes composition discoverable without requiring agents to read multiple READMEs.

### 5. Marketplace description as elevator pitch

The root `marketplace.json` description becomes the 10-second pitch:

```json
{
  "description": "Knowledge graph, task tracking, and persistent memory for AI agents.
    Build a knowledge base that grows with every session. Track work with GTD lifecycle.
    Remember context across conversations. All local, all composable, all via MCP."
}
```

Intent language, not technical language. The agent reading this should immediately know
whether khive is relevant to its current task.

### 6. Forward-deployed crates as integration targets, not dead code

Crates like `khive-vcs` and `khive-merge` exist in the workspace but have no current
callers. These are forward-deployed for ADR-042/043 — the KG versioning and three-way
merge capabilities that will power `kg sync`, `kg diff`, and collaborative workflows.

The marketplace strategy for these crates:

- **Do not delete** — they are implementation-ready, not stubs.
- **Do not advertise** — they are not user-facing until wired into a pack.
- **Do document** — each crate's README should state which ADR it implements and which
  pack will consume it, so agents and contributors understand the intent.
- **Usage discovery** — the real value is in finding patterns where these crates solve
  existing problems. `khive-merge` enables conflict resolution in collaborative KG
  editing. `khive-vcs` enables branching workflows for experimental knowledge. These
  are features agents should discover and request, not code to be cleaned up.

## Consequences

- READMEs become intent-first, progressive-disclosure documents.
- Each plugin has a clear primary skill for first-time users.
- Cross-plugin composition is advertised, not hidden.
- The marketplace description serves as an agent-readable elevator pitch.
- Forward-deployed crates are protected from cleanup sweeps and documented for future
  integration.
- These changes are documentation and metadata only — no runtime behavior changes.

## Alternatives considered

1. **Auto-generated capability map from pack metadata** — have the runtime introspect
   registered packs and produce the map. Attractive but premature: the intent vocabulary
   requires human curation, not mechanical extraction.
2. **Video/GIF demos** — common in marketplace listings. Not applicable to MCP plugins
   (text-only interaction), but worth considering for the web dashboard (khive.ai).
3. **Plugin ratings/usage metrics** — useful for discovery at scale. Deferred to the
   cloud tier where telemetry exists.
