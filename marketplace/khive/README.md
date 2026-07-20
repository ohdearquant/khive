# khive — Claude Code plugin

One plugin for the whole khive surface: a knowledge graph, GTD, memory, inter-agent comm,
scheduling, and a domain-knowledge corpus, all served by a single MCP server (`kkernel mcp`)
exposing one tool — `request` — that dispatches 70 verbs across 11 packs.

This plugin is **guidance, not a second runtime**. It ships the pattern skills that teach an
agent how to use each pack well, plus the kg stewardship agents. The data and verbs live in the
MCP server; install and configure that per [INSTALL.md](../INSTALL.md).

## Configure your actor first

Every record you write is stamped with **who you are** (`from_actor`), resolved from the MCP
server's actor config. If it is unset it silently defaults to `"local"`, which leaves your
messages unattributed and your `comm.inbox` unscoped. Set `KHIVE_ACTOR=lambda:<your-id>` in the
server env (or pass `--actor`) before you start. See
[INSTALL.md → Set your actor identity](../INSTALL.md#set-your-actor-identity-attribution).

## Pattern skills (one per pack)

Each skill teaches the _reusable pattern_ for its pack, not a how-to for every verb. Per-verb
parameter detail is always one call away at runtime: `request(ops="<verb>(help=true)")`.

| Skill       | Pack      | The pattern it teaches                                                                     |
| ----------- | --------- | ------------------------------------------------------------------------------------------ |
| `kg`        | kg        | Search before you create; model as typed entities + edges; explore; propose                |
| `gtd`       | gtd       | Capture with an assignee, process the inbox, advance the lifecycle, complete with evidence |
| `memory`    | memory    | Store-before-recall with honest salience; recall before acting                             |
| `comm`      | comm      | Be attributable, address by actor with a subject, triage, reply to thread                  |
| `schedule`  | schedule  | `remind` for prompts vs `schedule` for deferred verb dispatch; agenda; cancel              |
| `knowledge` | knowledge | Search (`rerank=true`) then suggest then compose; learn + cite to grow the corpus          |

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

The `kg` pack is the base (entities, edges, notes); every other pack builds on it. The default
server config loads all eleven (`kg`, `gtd`, `memory`, `comm`, `schedule`, `knowledge`,
`session`, `git`, `code`, `workspace`, `blob` — `git` contributes note kinds, a batch ingester, the
`git.digest` verb, and three write verbs — `git.commit`/`git.branch`/`git.push` — that shell to
system git with hardened, allowlisted argv construction and unconditional force-push denial
(ADR-108); `code` contributes the `code.ingest` L1/L1.5 source-ingest verb plus a `finding`
note kind whose `findings.json` batch ingestion is reached only through the
`kkernel code-ingest` admin CLI; `workspace` contributes entity/endpoint vocabulary, no verbs;
`blob` contributes content-addressed blob storage, `blob.put`/`blob.get`/`blob.stat`) —
this plugin currently ships pattern skills for the first six; `session`, `git`, `code`,
`workspace`, and `blob` have no skill yet (see the Pattern skills table above).
The `brain` pack (`brain.*` verbs — Bayesian recall-tuning profiles) is a commercially
licensed extension distributed separately; it is not part of this distribution.
See [INSTALL.md](../INSTALL.md) for setup, the actor config, and per-pack smoke tests.

## Links

- Repository: <https://github.com/ohdearquant/khive>
- Install guide: [INSTALL.md](../INSTALL.md)
- License: BUSL-1.1
