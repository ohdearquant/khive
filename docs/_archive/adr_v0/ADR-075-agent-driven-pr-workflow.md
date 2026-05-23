# ADR-075: Agent-Driven PR Workflow on KG Versioning

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive

## Context

KG versioning (ADR-042, ADR-048) gives the knowledge graph git-native branching: entities live
as NDJSON files tracked by git. This enables a PR-based workflow where agents propose knowledge
graph changes through branches and pull requests, just like code changes.

Today, agents modify the graph directly via MCP verbs. There is no review step, no diff
visibility, and no way to reject a batch of changes. For high-stakes knowledge bases (research
papers, architectural decisions, compliance records), unreviewed direct mutation is a liability.

## Decision

### Branch-per-session workflow

When an agent starts a research or ingestion session, it can opt into the PR workflow:

```
request(ops="branch(name='research/quantum-2026')")
```

This creates a git branch in the KG repository. All subsequent `create`, `link`, `update`,
`delete` operations write to the branch, not main.

### Commit and PR

When the agent finishes its session:

```
request(ops="commit(message='Add 12 quantum computing entities from arxiv survey')")
request(ops="pr(title='Research: quantum computing survey', reviewers=['human'])")
```

This creates a git commit on the branch and opens a pull request. The PR diff shows:

- New entities added (NDJSON lines)
- New edges created
- Entities modified (property changes, tag additions)
- Entities deleted

### Review interface

The PR is a standard git PR — reviewable in GitHub, GitLab, or any git forge. Reviewers see
entity-level diffs, not raw JSON:

```diff
+ entity: "Quantum Error Correction" (concept)
+   extends: "Quantum Computing"
+   introduced_by: "Shor 1995"
+   properties: {domain: "quantum", status: "researched"}
```

The `kg diff` command (implemented in the Deno CLI, issue #238) provides this formatting.

### Merge strategies

| Strategy          | When                                    | How                             |
| ----------------- | --------------------------------------- | ------------------------------- |
| Fast-forward      | No conflicts, reviewer approves         | `git merge --ff-only`           |
| Three-way merge   | Concurrent changes, no entity conflicts | `khive-merge` (ADR-043)         |
| Manual resolution | Same entity modified on both sides      | Reviewer resolves via `kg diff` |

### Autonomous mutator agents

Agents that operate autonomously (swarm digester, polisher, expander) MUST use the PR workflow
when operating on shared knowledge bases. Their branches are auto-named:

```
agent/digester/2026-05-21-arxiv-batch
agent/polisher/2026-05-21-density-pass
```

The agent opens the PR and assigns it to the configured reviewer (human or another agent).
Auto-merge is allowed only when the PR passes quality gates (ADR-074 quality scores).

## Consequences

- Knowledge graph changes become reviewable, reversible, and auditable.
- Agents can propose changes without direct write access to the main branch.
- The existing `khive-vcs` (snapshots, hashing) and `khive-merge` (three-way merge) crates
  provide the implementation foundation.
- The Deno CLI's `kg diff`, `kg log`, and `kg doctor` commands (from the sweep) provide the
  review tooling.
- Requires: `branch`, `commit`, and `pr` verbs in a new VCS pack or as extensions to the KG
  pack.

## Alternatives considered

1. **In-database branching** — branch at the SQLite level using shadow tables. Rejected: loses
   git's distribution, review, and audit capabilities. ADR-048 chose git-native for a reason.
2. **Approval queue** — changes go to a queue instead of branches. Rejected: queues don't
   compose with existing git tooling. PRs are the universal review primitive.
3. **Post-hoc review** — let agents write directly, review via `kg log` after. Rejected: no
   rollback mechanism without branches. By the time you review, the damage is done.
