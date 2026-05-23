# ADR-065: Plugin Intent Routing — Agent Self-Selection via Capability Map

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-025 (Pack Standard), ADR-063 (Dynamic Pack Loading)

## Context

khive ships three marketplace plugins (kg, gtd, memory) with a total of 13 skills, 6 agents,
and 26+ verbs. Each plugin has a good README and well-shaped skills. The problem is upstream:
when an agent encounters a task, how does it know which plugin to reach for?

Today, an agent must already know that `kg:explore` exists to use it. There is no capability
map that routes from _intent_ ("I need to research a topic") to _plugin_ ("use kg"). This is
the difference between a plugin that gets installed and one that gets used.

The Claude Code marketplace ecosystem surfaces plugins by name and description. The best
plugins (Context7, memory tools, task managers) succeed because their descriptions match
the vocabulary agents already think in. khive's descriptions are accurate but technical —
"typed entities, closed edge ontology, hybrid search" tells an expert what the tool is, but
doesn't tell an agent _when to reach for it_.

### Observed failure modes

1. **Intent mismatch**: Agent needs to "remember something for later" but doesn't connect
   that to `memory:remember` because the plugin description leads with "decay-aware ranking."
2. **Plugin blindness**: Agent has kg installed but uses raw file writes for research notes
   because nothing in the agent's context maps "take research notes" → `kg:digest`.
3. **Composition gap**: Agent uses kg for entities but manually tracks follow-ups instead of
   using gtd, because the two plugins don't advertise their composition surface.

## Decision

### 1. Intent vocabulary in plugin descriptions

Each plugin's `description` field in `plugin.json` must lead with **intent triggers** — the
natural-language phrases an agent would think when the plugin is relevant. Technical
capabilities follow.

```
# Before (technical-first)
"Persistent knowledge graph for AI agents — typed entities, closed edge ontology, hybrid search"

# After (intent-first)
"Research, organize, and connect knowledge — persistent graph with typed entities, semantic
search, and graph traversal. Use when: building a knowledge base, ingesting papers, tracking
concepts and their relationships, finding gaps in what you know."
```

The `"Use when:"` clause is the routing signal. It must use the same vocabulary an agent would
use in its internal reasoning.

### 2. Capability map at marketplace root

The root `marketplace.json` gains a `capabilities` field that maps intent categories to plugins:

```json
{
  "capabilities": {
    "research": {
      "plugin": "kg",
      "entry_skill": "explore",
      "description": "Find what the graph knows about a topic"
    },
    "ingest": {
      "plugin": "kg",
      "entry_skill": "digest",
      "description": "Add new knowledge from papers, docs, or conversations"
    },
    "track_work": {
      "plugin": "gtd",
      "entry_skill": "capture",
      "description": "Capture tasks, commitments, and follow-ups"
    },
    "plan_day": {
      "plugin": "gtd",
      "entry_skill": "today",
      "description": "Review actionable work and pick what to do"
    },
    "remember": {
      "plugin": "memory",
      "entry_skill": "remember",
      "description": "Store durable context for future sessions"
    },
    "recall": {
      "plugin": "memory",
      "entry_skill": "recall",
      "description": "Retrieve prior context before acting"
    },
    "audit_knowledge": {
      "plugin": "kg",
      "entry_skill": "polish",
      "description": "Fix graph quality — orphans, duplicates, weak links"
    },
    "find_gaps": {
      "plugin": "kg",
      "entry_skill": "gap",
      "description": "Strategic survey of what's missing from the knowledge base"
    }
  }
}
```

This is a static routing table. Agents (or an orchestrator) can scan it to find the right
entry point without reading every plugin's README.

### 3. Skill description frontmatter as trigger

Every `SKILL.md` already has a `description` field in frontmatter. This ADR mandates that the
description must be phrased as a **trigger condition** — the situation in which the skill
should be invoked:

```yaml
# Before
description: Ingest research material into the knowledge graph

# After
description: You have material to add to your knowledge base — a paper, concept, conversation,
  or implementation. This skill extracts entities, links them, and verifies graph density.
```

The "You have..." / "You need..." / "You want..." phrasing matches how Claude Code's skill
router evaluates relevance. Technical details belong in the skill body, not the trigger.

## Consequences

- Plugin descriptions become intent-shaped, improving Claude Code's automatic skill matching.
- The capability map provides a single lookup table for orchestrators and agents.
- Skill triggers use second-person situational phrasing for better router hit rates.
- Existing skill bodies and workflows are unchanged — this is a discovery layer, not a
  behavioral change.
- Third-party plugins (future) can register capabilities in the same map format.

## Alternatives considered

1. **LLM-based routing at runtime** — have the MCP server classify intent and dispatch.
   Rejected: adds latency, requires inference, and moves routing logic away from the agent
   where it belongs.
2. **Single mega-plugin** — combine kg+gtd+memory into one plugin with all skills.
   Rejected: violates pack composability (ADR-025). Users should install only what they need.
3. **Tagging system** — tag skills with intent keywords and let Claude Code search.
   Partially adopted: the capability map is effectively a curated tag index, but structured
   rather than free-form.
