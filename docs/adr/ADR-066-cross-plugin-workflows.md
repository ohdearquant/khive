# ADR-066: Cross-Plugin Workflow Skills

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-025 (Pack Standard), ADR-065 (Plugin Intent Routing)

## Context

Each khive plugin (kg, gtd, memory) works well in isolation. The kg README documents a swarm
pipeline (digester → polisher → gap-analyst → expander) that uses gtd for task handoff. But
there is no skill that orchestrates the _combined_ workflow.

Real agent work is cross-plugin by nature:

- **Research session**: recall prior context (memory) → explore the graph (kg) → web search →
  digest findings (kg) → capture follow-ups (gtd) → remember session insights (memory)
- **Knowledge audit**: gap analysis (kg) → prioritize gaps as tasks (gtd) → expand each gap
  (kg) → polish (kg) → remember patterns discovered (memory)
- **Project onboarding**: recall project context (memory) → explore domain graph (kg) →
  capture action items (gtd) → remember key decisions (memory)

These workflows are where khive's value compounds — each plugin alone is useful, but the
composition is the moat. Today, agents must discover this composition themselves. Most don't.

### What best CC marketplace plugins do

The most-used Claude Code plugins succeed through workflow-shaped skills, not tool catalogs:

- **create-pdf**: one skill, one intent ("I need a PDF"), orchestrates multiple tools internally.
- **Context7**: "resolve docs before coding" — a workflow pattern, not a tool description.
- **Sequential thinking**: structures multi-step reasoning into a reusable workflow.

khive's per-plugin skills are good (digest, explore, capture are all workflow-shaped). What's
missing is the _cross-plugin_ workflow skill — the one that chains kg + gtd + memory into a
coherent session.

## Decision

### 1. Workflow skills live at marketplace root, not inside plugins

Cross-plugin workflows are a marketplace concern, not a pack concern. They live in
`marketplace/workflows/` alongside the plugin directories:

```
marketplace/
  kg/
  gtd/
  memory/
  workflows/
    research/SKILL.md        # kg + memory + gtd
    audit/SKILL.md           # kg + gtd
    onboard/SKILL.md         # kg + memory + gtd
```

Each workflow skill declares its plugin dependencies in frontmatter:

```yaml
---
description: You're starting a research session — reading papers, exploring a domain, or
  investigating a technique. This workflow chains recall → explore → digest → capture.
requires: [kg, memory, gtd]
---
```

### 2. Three initial workflow skills

**research** — Full research session lifecycle:

```
Phase 1: Orient
  memory:recall(query="<topic>")           # What do I already know?
  kg:explore(topic="<topic>")              # What's in the graph?

Phase 2: Ingest
  <external research — web, papers, code>
  kg:digest(material=<findings>)           # Extract entities + link

Phase 3: Consolidate
  kg:connect(entity=<new>, graph=<existing>)  # Wire into existing knowledge
  kg:polish(scope=<session entities>)         # Verify density

Phase 4: Hand off
  gtd:capture(follow-ups from research)    # Track next actions
  memory:remember(session insights)        # Persist for next session
```

**audit** — Knowledge quality sweep:

```
Phase 1: Survey
  kg:gap(scope=<domain>)                   # Find what's missing
  kg:polish(scope=<domain>)                # Find what's broken

Phase 2: Prioritize
  gtd:capture(each gap as a task)          # Convert gaps to actionable work
  gtd:today()                              # Pick what to close now

Phase 3: Close
  kg:expand(gap=<selected>)               # Grow the graph
  kg:polish(scope=<expanded>)             # Verify the expansion
```

**onboard** — Project/domain ramp-up:

```
Phase 1: Context
  memory:recall(query="<project>")         # Prior session context
  kg:explore(topic="<project domain>")     # Existing knowledge

Phase 2: Map
  kg:digest(material=<project docs>)       # Ingest project documentation
  kg:connect(new entities to domain graph) # Wire into broader knowledge

Phase 3: Act
  gtd:capture(action items from exploration)
  memory:remember(key decisions, contacts, architecture)
```

### 3. Workflow skills are orchestration guides, not rigid scripts

Each workflow skill documents the phase structure and the verb calls, but the agent adapts
based on what each phase returns. If `kg:explore` returns rich results, Phase 2 might skip
external research. If `kg:gap` returns nothing, the audit is done. The skill teaches the
_pattern_, not a fixed sequence.

### 4. Agents can be workflow-aware

The kg plugin's agents (digester, polisher, gap-analyst, expander) already coordinate via
gtd task handoff. Workflow skills formalize this coordination for agents that aren't part of
the kg swarm — any Claude Code agent can follow the research workflow without knowing the
internal swarm protocol.

## Consequences

- Cross-plugin composition becomes discoverable via standard SKILL.md files.
- Agents learn multi-plugin patterns from workflow skills instead of reinventing them.
- The `marketplace/workflows/` directory establishes a clear home for composition logic.
- Plugin independence is preserved — workflows depend on plugins, not the reverse.
- The three initial workflows (research, audit, onboard) cover the most common multi-plugin
  patterns. More can be added as usage patterns emerge.

## Alternatives considered

1. **Embed cross-plugin skills inside kg** — kg already references gtd. Rejected: violates
   the principle that a plugin shouldn't assume other plugins exist. Workflow skills are
   explicitly multi-plugin.
2. **Orchestrator agent instead of skills** — a meta-agent that dispatches to plugins.
   Rejected for the OSS tier: adds complexity. Skills are simpler and work with any agent.
   The lambda tier (future) may add orchestrator agents that consume these same workflows.
3. **Implicit composition via tool chaining** — let the agent figure it out from tool
   descriptions alone. This is the current state and it doesn't work well enough —
   agents need the workflow pattern demonstrated, not just the tools available.
