# khive — Claude Code plugin

One plugin for the khive knowledge graph surface, served by a single MCP server
(`kkernel mcp`) exposing one tool — `request` — that dispatches the 18 verbs of the
`kg` pack.

This plugin is **guidance, not a second runtime**. It ships the pattern skill that teaches an
agent how to use the `kg` pack well, plus the kg stewardship agents. The data and verbs live in
the MCP server; install and configure that per [INSTALL.md](../INSTALL.md).

## Configure your actor first

Every record you write is stamped with **who you are** (`from_actor`), resolved from the MCP
server's actor config. If it is unset it silently defaults to `"local"`. Set
`KHIVE_ACTOR=lambda:<your-id>` in the server env (or pass `--actor`) before you start. See
[INSTALL.md → Set your actor identity](../INSTALL.md#set-your-actor-identity-attribution).

## Pattern skill

The skill teaches the _reusable pattern_ for the `kg` pack, not a how-to for every verb.
Per-verb parameter detail is always one call away at runtime:
`request(ops="<verb>(help=true)")`.

| Skill | Pack | The pattern it teaches                                                       |
| ----- | ---- | ------------------------------------------------------------------------------ |
| `kg`  | kg   | Search before you create; model as typed entities + edges; explore; propose  |

## kg stewardship agents

Bulk graph work — ingestion, gap analysis, hygiene — is owned by dedicated agents rather than
hand-rolled. Four pair with a workflow skill (their operating contract); two run standalone.

| Agent         | Paired skill | Role                                                                          |
| ------------- | ------------ | ----------------------------------------------------------------------------- |
| `digester`    | `digest`     | Turn source material (ADRs, papers, docs, code) into entities + edges + notes |
| `gap-analyst` | `gap`        | Survey the graph's structural gaps; read-only frontier ranking                |
| `expander`    | `expand`     | Grow the graph to close one strategic gap, with hard create caps              |
| `polisher`    | `polish`     | Fix orphans, under-linked nodes, duplicates, wrong-direction edges            |
| `librarian`   | —            | Swarm health monitor; watches the agent task queue, files taxonomy questions  |
| `researcher`  | —            | Context-aware investigation grounded in the persistent graph                  |

Once installed, invoke them as `khive:digester`, `khive:polisher`, and so on.

## Requirements

The `kg` pack is the base (entities, edges, notes) and the default server config loads it
alone. Task management, memory, inter-agent communication, scheduling, session continuity,
workspace linking, blob storage, and brain profiles (`brain.*` verbs — Bayesian
recall-tuning) are provided by commercially licensed extensions and are not part of this
distribution; this plugin ships a pattern skill only for `kg`. Git provenance ingestion and
write verbs (`git.digest`, `git.commit`, `git.branch`, `git.push`) are likewise a
commercially licensed extension.
See [INSTALL.md](../INSTALL.md) for setup, the actor config, and the smoke test.

## Links

- Repository: <https://github.com/ohdearquant/khive>
- Install guide: [INSTALL.md](../INSTALL.md)
- License: BUSL-1.1
