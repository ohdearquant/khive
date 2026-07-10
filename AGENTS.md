# khive — Agent Usage Guide

This file is for AI agents (and the humans configuring them) using khive as the research runtime.

khive gives your agent:

1. **A knowledge graph** — typed entities + edges you build as you work
2. **Notes** — observations, insights, questions, decisions, references that persist across sessions
3. **Pattern matching queries** — GQL/SPARQL traverse over the graph
4. **Task management** — GTD lifecycle (inbox → next → active → done)
5. **Memory** — salience- and decay-weighted recall across sessions
6. **Communication** — namespaced message passing between agents
7. **Scheduling** — time-triggered reminders and future verb dispatch
8. **Knowledge corpus** — atom/domain CRUD, FTS + embedding search, compose briefings
9. **Brain** — Bayesian profile tuning from feedback signals
10. **Session** — persist and resume agent-session records

All 9 packs load by default. **78 public verbs** across the packs — the `git` pack
contributes the `git.digest` verb plus the commit/issue/pull_request provenance note kinds
and a batch ingester (regenerate via `request(ops="verbs()")` before editing this line).

If you're working on khive itself (writing code in this repo), see `CLAUDE.md` instead.

---

## Core verbs

All verbs are dispatched through a single MCP tool, `request`, which accepts a function-call DSL
or JSON form ([ADR-016](docs/adr/ADR-016-request-dsl.md),
[ADR-027](docs/adr/ADR-027-dynamic-pack-loading.md)). Verb semantics and namespace contract are
defined in [ADR-023](docs/adr/ADR-023-declarative-pack-format.md).

### KG pack — 18 verbs (bare names, no prefix)

| Verb        | What it does                                                                                     | When to use                                                             |
| ----------- | ------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------- |
| `create`    | Add an entity or note                                                                            | New concept, paper, observation, decision worth tracking                |
| `get`       | Fetch any record by UUID (auto-detects type)                                                     | When you have a UUID and need the full record                           |
| `search`    | Text + semantic search over entities or notes                                                    | Finding things by content similarity                                    |
| `list`      | Structured filtering (by kind, tags, etc.)                                                       | Browsing a category or namespace                                        |
| `stats`     | Entity/edge/note/event counts                                                                    | Dashboard, health check                                                 |
| `update`    | Patch properties, tags, or content (by UUID)                                                     | Correcting or enriching an existing record                              |
| `delete`    | Soft-delete (or hard-delete) a record (by UUID)                                                  | Removing stale or incorrect data                                        |
| `merge`     | Deduplicate two entities into one                                                                | "LoRA" and "Low-Rank Adaptation" are the same concept                   |
| `link`      | Connect two nodes with a typed relation                                                          | When relationships emerge from research                                 |
| `neighbors` | Immediate neighbors of a node                                                                    | "What connects to this entity?"                                         |
| `traverse`  | Multi-hop graph walk with depth/relation filters                                                 | Structural context — lineages, paths, clusters                          |
| `query`     | GQL/SPARQL query string → SQL                                                                    | Complex pattern matching over the graph                                 |
| `propose`   | Create an event-sourced change proposal                                                          | Staging changes for review before apply                                 |
| `review`    | Approve or reject a proposal                                                                     | Gating changes through a review workflow                                |
| `withdraw`  | Cancel an open proposal                                                                          | Abandoning a staged change                                              |
| `resolve`   | Resolve natural-language references to ids (id passthrough, recent-ring, hybrid search fallback) | Turning a fuzzy reference ("the old record") into an id before acting   |
| `verbs`     | List all registered verbs on this server                                                         | Discovery — see what's available                                        |
| `context`   | Entity-anchored graph context in one call (anchors + 1-2 hop expansion, budget-packed)           | Feeding an agent "everything relevant near X" in one call — see ADR-089 |

`get`, `update`, `delete` are by-ID — they auto-detect whether the record is an entity, note, or
edge. The `id` parameter is resolved in three steps: (1) a full UUID (36-char dashed or 32-char
plain hex) is parsed directly; (2) otherwise an 8-or-more hex-character string enters a short-ID
prefix lookup, but in practice only the exact 8-char compact ID returned by write verbs resolves,
because stored UUIDs carry a dash at position 9, so a 9+ pure-hex string never matches; (3) anything
else falls through to an exact, case-insensitive entity-name lookup. `create`, `list`, `search`
require `kind=entity|note` (or `kind=edge` for `list`; `kind=event` for audit events per
[ADR-022](docs/adr/ADR-022-events-query-surface.md)).

### GTD pack — 5 verbs (`gtd.` prefix, [ADR-019](docs/adr/ADR-019-gtd-pack.md))

| Verb             | What it does                                            | When to use                              |
| ---------------- | ------------------------------------------------------- | ---------------------------------------- |
| `gtd.assign`     | Create a task (note with kind=task)                     | New work item, bug, follow-up            |
| `gtd.next`       | List actionable tasks (status=next/active), by priority | "What should I work on?"                 |
| `gtd.complete`   | Mark a task done or cancelled                           | Finishing work                           |
| `gtd.tasks`      | Filtered task listing                                   | Browse tasks by status/assignee/priority |
| `gtd.transition` | Explicit lifecycle change (inbox→next→active→done)      | Moving a task through its lifecycle      |

`gtd.assign` accepts `context_entity_id` to anchor a task to a KG entity.

Full `gtd.transition` allowed transitions:
`inbox` → next | waiting | someday | active | done | cancelled;
`next` → active | waiting | someday | done | cancelled (skipping `active` with `next -> done` is valid);
`active` → next | waiting | done | cancelled;
`waiting` | `someday` → next | active | done | cancelled;
`done` and `cancelled` are terminal.

`gtd.transition` returns one of two shapes. On a real transition: `{transitioned: true, id, full_id,
from, to, is_terminal, title, priority, assignee, due}` — `is_terminal: true` when the task reaches
`done` or `cancelled`. On an idempotent no-op (the task is already in the requested status): `{transitioned:
false, id, full_id, from, to, note: "already in target status"}` — the task fields (`title`, `priority`,
`assignee`, `due`, `is_terminal`) are omitted. Branch on `transitioned` before reading those fields.

### Memory pack — 5 verbs (`memory.` prefix, [ADR-021](docs/adr/ADR-021-memory-pack.md))

| Verb              | What it does                                                  | When to use                                         |
| ----------------- | ------------------------------------------------------------- | --------------------------------------------------- |
| `memory.remember` | Store a memory with salience and decay                        | Cross-session context, agent state                  |
| `memory.recall`   | Hybrid FTS + vector recall with decay-weighted ranking        | Retrieve what you stored in prior sessions          |
| `memory.feedback` | Emit explicit feedback on a recalled memory                   | Signal useful/not_useful to tune posteriors         |
| `memory.prune`    | Soft-delete memories below a salience threshold               | Trim low-value memories to prevent unbounded growth |
| `memory.vacuum`   | Run SQLite VACUUM to reclaim space freed by soft-deleted rows | Periodic maintenance after heavy prune runs         |

`memory.recall` supports `tags` and `tag_mode` ("any"|"all") for tag-based post-filtering.
Composite scores are always in [0,1]. Typical production floor: 0.3-0.7.

### Brain pack — 15 verbs (`brain.` prefix)

| Verb                     | What it does                                                      | When to use                                                |
| ------------------------ | ----------------------------------------------------------------- | ---------------------------------------------------------- |
| `brain.event_counts`     | Windowed event counts by kind/actor (+ feedback by_profile split) | Flywheel metrics, feedback-coverage reporting              |
| `brain.profiles`         | List profiles (optionally filtered by lifecycle)                  | See what profiles exist                                    |
| `brain.profile`          | Full detail: metadata, snapshot, state summary                    | Inspect a specific profile                                 |
| `brain.create_profile`   | Create a new profile with optional seed priors                    | Custom tuning for a new consumer                           |
| `brain.resolve`          | Which profile serves a given consumer context?                    | Before recall — check active tuning                        |
| `brain.activate`         | Start live update loop for a profile                              | Enable feedback-driven tuning                              |
| `brain.deactivate`       | Stop live updates, retain state                                   | Pause tuning without losing progress                       |
| `brain.archive`          | Read-only, audit-retained                                         | Retire a profile permanently                               |
| `brain.reset`            | Reset posteriors to priors (preserves event history)              | Start tuning fresh                                         |
| `brain.feedback`         | Emit explicit feedback event                                      | Rate a recall result as useful/not_useful/wrong            |
| `brain.auto_feedback`    | Emit implicit feedback for recall results                         | Convenience: agents call after memory.recall               |
| `brain.bind`             | Bind a profile to an actor + consumer                             | Route a specific caller to a specific profile              |
| `brain.unbind`           | Remove a binding                                                  | Stop routing                                               |
| `brain.bindings`         | List binding rows                                                 | Audit profile routing                                      |
| `brain.register_adapter` | Register an adapter integrity record                              | Gate adapter composition to the active base-model revision |

### Comm pack — 7 verbs (`comm.` prefix)

| Verb          | What it does                                                           | When to use                                   |
| ------------- | ---------------------------------------------------------------------- | --------------------------------------------- |
| `comm.send`   | Send a message (optionally threaded)                                   | Inter-agent or inter-namespace messaging      |
| `comm.inbox`  | List inbound messages                                                  | Check what's waiting                          |
| `comm.read`   | Mark an **inbound** message as read                                    | Acknowledge receipt (recipient action)        |
| `comm.reply`  | Reply to a message (threading linkage)                                 | Respond in-thread                             |
| `comm.thread` | Retrieve full conversation thread                                      | Read the whole conversation                   |
| `comm.health` | Per-channel health snapshot (no args)                                  | Check daemon channel-poll state               |
| `comm.probe`  | Read-only poll for new inbound message metadata and stale unread count | Cheap wake-up check without a full inbox scan |

**Inbox shape (ADR-057).** `comm.inbox` is scannable: each entry carries top-level `from`, `to`,
`subject`, `read`, `direction`, and a derived `preview` (whitespace-collapsed, truncated to 80
chars) — triage without a `get` per message. Delivery is actor-addressed: `comm.send(to="lambda:x")`
stamps `from_actor` from the server's configured actor and lands in `x`'s inbox, and `comm.inbox`
returns only messages addressed to you. Set that actor via `--actor` / `KHIVE_ACTOR`; an unset actor
sends and reads as the shared `"local"` party line.

**`comm.read` is inbound-only.** It marks a received message as read; calling it on an outbound
(sent) message returns `read: message <uuid> is outbound; only received (inbound) messages can be
marked as read`. To confirm a sent message was received, read it from the recipient's `comm.inbox`
or `comm.thread`.

### Schedule pack — 4 verbs (`schedule.` prefix)

| Verb                | What it does                     | When to use                                    |
| ------------------- | -------------------------------- | ---------------------------------------------- |
| `schedule.remind`   | Create a time-triggered reminder | "Remind me to X at Y"                          |
| `schedule.schedule` | Schedule a future verb dispatch  | Deferred actions (action is a DSL verb string) |
| `schedule.agenda`   | List upcoming scheduled events   | "What's on the calendar?"                      |
| `schedule.cancel`   | Cancel a scheduled event         | Remove a pending reminder/action               |

### Knowledge pack — 19 verbs (`knowledge.` prefix)

| Verb                       | What it does                                            | When to use                                  |
| -------------------------- | ------------------------------------------------------- | -------------------------------------------- |
| `knowledge.upsert_atoms`   | Bulk insert/update atoms by slug                        | Ingesting knowledge corpus                   |
| `knowledge.upsert_domains` | Bulk insert/update domain groupings                     | Organizing atoms into domains                |
| `knowledge.get`            | Fetch atom/domain by UUID or slug                       | Read a specific knowledge entry              |
| `knowledge.list`           | Paginated listing of atoms or domains                   | Browse the corpus                            |
| `knowledge.search`         | TF-IDF search with embedding rerank (default on)        | Finding relevant knowledge                   |
| `knowledge.suggest`        | Orient query against domains for composition            | "Which domains cover topic X?"               |
| `knowledge.compose`        | Compose a markdown briefing from selected atoms/domains | Build a context briefing for an agent        |
| `knowledge.edit`           | Upsert sections for an atom                             | Update part of an atom without wiping others |
| `knowledge.import`         | Ingest markdown files as atoms                          | Batch import from filesystem                 |
| `knowledge.delete_atoms`   | Soft-delete atoms by slug or ID                         | Retire stale knowledge                       |
| `knowledge.stats`          | Corpus statistics: atom/domain/coverage counts          | Health check                                 |
| `knowledge.index`          | Backfill embeddings + FTS                               | After bulk import or reindex                 |
| `knowledge.fold`           | Budget-constrained knapsack selection                   | Token-aware subset picking                   |
| `knowledge.challenge`      | Mark a section as disputed                              | Flag incorrect content                       |
| `knowledge.adjudicate`     | Resolve a disputed section                              | Accept or reject a challenge                 |
| `knowledge.learn`          | Register a concept entity with domain/tags              | Quick concept creation                       |
| `knowledge.cite`           | Link concept → paper/person/org (introduced_by edge)    | Attribution                                  |
| `knowledge.topic`          | List concepts by domain or free-text                    | Explore the concept graph                    |
| `knowledge.feedback`       | Route feedback to brain for knowledge recall tuning     | Signal useful/not_useful on compose results  |

`knowledge.search` supports `decompose=true` for multi-concept query splitting (avoids FTS edge
cases). Scores are normalized to [0,1] when `rerank` is active (default).
Pass `kind=` (`"atom"` or `"domain"`) to filter by result type; `type=` is accepted as a legacy
alias. `knowledge.list` accepts the same `kind=`/`type=` discriminant.

`knowledge.edit` takes `sections=[{section_type, content, heading?, sort_order?}]`. `section_type`
is a **closed enum** — valid values: `overview` | `core_model` | `boundary_conditions` |
`formalism` | `operational_guidance` | `examples` | `failure_modes` | `expert_lens` |
`references` | `other`. Content must be **at least 80 characters**. Shorter content or an
unrecognized `section_type` returns a validation error listing the valid values.

### Session pack — 4 verbs (`session.` prefix)

| Verb             | What it does                                      | When to use                              |
| ---------------- | ------------------------------------------------- | ---------------------------------------- |
| `session.store`  | Persist an agent-session record as a session note | Checkpoint/save a completed session      |
| `session.list`   | List stored sessions, newest first                | "What sessions have I run?"              |
| `session.resume` | Fetch one session's full content by UUID/prefix   | Continue or reference a specific session |
| `session.export` | Serialize one session as JSON or markdown         | Share or archive a session outside khive |

### How to call a verb

Wrap the verb call in `request(ops="…")`:

```text
request(ops="create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\")")
request(ops="search(kind=\"entity\", query=\"memory efficient attention\")")
request(ops="link(source_id=\"<u>\", target_id=\"<v>\", relation=\"extends\", weight=0.9)")
```

Run several ops in parallel by passing a batch:

```text
request(ops="[create(kind=\"entity\", entity_kind=\"concept\", name=\"A\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"B\")]")
```

JSON form is equivalent (use this when the DSL string would be hard to escape):

```text
request(ops="[{\"tool\":\"create\",\"args\":{\"kind\":\"entity\",\"entity_kind\":\"concept\",\"name\":\"LoRA\"}}]")
```

Ops in a batch run in parallel and have no ordering guarantee. If op B depends on op A's output
(e.g. create-then-link), chain them with `|` instead (see _Efficient batching and round-trip reduction_ below).

**Deferred (not yet available):**

- `create(supersedes=<old-id>)` parameter shortcut — this convenience form (which would atomically
  create a new record and add a `supersedes` edge to the old one) is not yet in the wire surface.
  Use two ops, which you can run as one chained call: `create(...) | link(..., relation="supersedes")`.
- Note merge — only entity merge is implemented (`merge(into_id=..., from_id=...)`).
  Deduplicating two notes is not yet supported; add a `supersedes` edge manually.

### Efficient batching and round-trip reduction

Every `request` call is one round trip, and the server keeps a warm daemon between calls (see
_Daemon and warm startup_). Folding work into fewer calls cuts both the per-call round trip and
repeated access to that warm state. Two forms do this: a parallel batch for independent ops, and a
sequential chain for dependent ops.

Batch independent ops with `[ … ]`. They run in parallel, each returns its own `ok`/`error` so one
failure does not abort the rest, and the limit is 100 ops per batch. Orient at session start in one
call instead of three:

```text
request(ops="[gtd.next(limit=10), gtd.tasks(status=\"active\"), comm.inbox(limit=10)]")
```

The response carries each op's result alongside a `summary` with `total`, `succeeded`, and `failed`.

Chain dependent ops with `|` and `$prev`. When one op needs the previous op's output, separate them
with `|` and reference the prior result with `$prev`. A chain runs sequentially and skips the
remaining ops if a step fails. This collapses create-then-link into a single call:

```text
request(ops="create(kind=\"concept\", name=\"LoRA\") | link(source_id=$prev.id, target_id=\"<uuid>\", relation=\"extends\")")
```

`$prev` reads fields by dotted path and arrays by index, such as `$prev.id` or `$prev[0].id`.
Reference the field the previous verb actually returns; create, remember, and the other write verbs
return the new record's `id`. Recording a corrected memory and marking the old one superseded is then
one call:

```text
request(ops="memory.remember(content=\"corrected fact\", salience=0.8) | link(source_id=$prev.id, target_id=\"<old-uuid>\", relation=\"supersedes\")")
```

`$prev` refers to the immediately preceding op only. In a three-op chain `A | B | C`, the `$prev` in
`C` is `B`'s result, not `A`'s; there is no multi-step back-reference. If `C` needs a value from `A`,
split the work into two `request` calls.

The two forms cannot be combined in one `request`, and the JSON op form does not support `$prev`:

| Situation                       | Use                        | Note                                                                                  |
| ------------------------------- | -------------------------- | ------------------------------------------------------------------------------------- |
| Ops are independent             | batch `[a(), b()]`         | parallel, up to 100 ops                                                               |
| Op B needs op A's result        | chain `a() \| b(…=$prev…)` | sequential, aborts on failure                                                         |
| Two writes to the same record   | chain, not batch           | conflicting same-record writes get per-op errors and are skipped; other ops still run |
| Discovering a verb's parameters | `verb(help=true)`          | returns the parameter schema without running                                          |

Mixing `,` and `|` at the top level of one `request` is rejected, as is `$prev` inside the JSON op
form. Use the function-call form shown above for chaining.

### Output format

The `request` envelope accepts a `format` parameter that controls how the response is serialized
([ADR-078](docs/adr/ADR-078-output-format-shape-aware-rendering.md)):

| Value   | Description                                                                                                                                                                                      | Default for                                                        |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------ |
| `json`  | Compact JSON (`serde_json::to_string`). Lossless, parseable. The redundancy-drop in Agent presentation mode (properties dedup, `namespace=local` elision) already removes most duplicated bytes. | **all surfaces** (MCP `request`, `kkernel exec`, CLI, CI, scripts) |
| `auto`  | Shape-aware: markdown table for record arrays, flat key-value block for single records, compact JSON fallback. Lossy view (cells truncated ~120 chars).                                          | (opt-in)                                                           |
| `table` | Force markdown table regardless of shape. Lossy view.                                                                                                                                            | (opt-in)                                                           |

The format axis is `Json | Auto | Table` (three values). YAML was measured and dropped: minimal-YAML
costs ~11-17% more tokens than compact JSON for record arrays, so it never beats the default.

```text
# Default is compact json everywhere — parseable, lossless, $prev-safe
request(ops="gtd.tasks(limit=10)")

# Opt into a markdown table when an agent is reading (not parsing) the output
request(ops="gtd.tasks(limit=10)", format="auto")
```

`format=json` (the default) is the machine contract: any caller that chains `$prev`, parses the
response, or runs in a test harness gets it for free. `format=auto`/`table` are an opt-in token-lean
_view_ for agents reading rather than parsing — they truncate long cells and are not round-trippable.
In a compounded request (batch/chain) the format applies **per-op** to each op's `result`; the
`results`/`summary` envelope stays compact JSON, and error entries are never reformatted. The
per-op override is `format_per_op` (mirrors `presentation_per_op`). Verbs whose policy is
AlwaysVerbose (`get`, `link`, `query`, `traverse`, `neighbors`, `brain.feedback`) are exempt from
the redundancy-drop even under `auto`/`table`, so agents still get their full output.

**Precedence** (ADR-078 §2, highest to lowest):

1. Per-call `format` on the `request` envelope, or `kkernel exec --output-format <json|auto|table>`
2. `KHIVE_OUTPUT_FORMAT` env var
3. `[runtime] default_output_format` in `khive.toml`
4. Builtin `json`

A deployment that wants the lean view everywhere sets `[runtime] default_output_format = "auto"` once;
product/cloud surfaces leave the default `json`. Unknown values (e.g. `yaml`) warn and fall back to
the next tier. `kkernel exec` honors all four tiers, including the TOML tier on its in-process path.

### Notes vs entities

- **Entities** = things in the world: concepts, papers, people, projects, datasets, orgs,
  artifacts, services. Graph nodes with typed edges between them.
- **Notes** = your observations about the world: what you noticed, concluded, decided, asked, cited.
  Temporal records with salience and optional graph edges (via `annotates`).

Use `create(kind="entity", entity_kind="concept", ...)` for entities.
Use `create(kind="note", note_kind="observation", ...)` for notes.

---

## The 9 entity kinds (closed set — [ADR-001](docs/adr/ADR-001-entity-kind-taxonomy.md), [ADR-048](docs/adr/ADR-048-knowledge-section-profiles.md))

| Kind       | What it represents                                                         |
| ---------- | -------------------------------------------------------------------------- |
| `concept`  | Algorithms, techniques, architectures, theories, models                    |
| `document` | Papers, preprints, technical reports, blog posts, books                    |
| `dataset`  | Benchmarks, corpora, evaluation sets                                       |
| `project`  | Codebases, libraries, tools, frameworks                                    |
| `person`   | Researchers, engineers, authors                                            |
| `org`      | Labs, companies, institutions                                              |
| `artifact` | Binaries, model checkpoints, Docker images, packages                       |
| `service`  | APIs, hosted endpoints, SaaS products                                      |
| `resource` | Actionable content agents consume: atoms, domains, skills, tools, runbooks |

`concept` is the default. Use `properties` for finer distinctions (`type: "paper"`,
`domain: "attention"`, `status: "implemented"`).

---

## The 5 note kinds (closed set — [ADR-013](docs/adr/ADR-013-note-kind-taxonomy.md))

| Kind          | What it records                               |
| ------------- | --------------------------------------------- |
| `observation` | An empirical capture — what you noticed       |
| `insight`     | A synthetic conclusion from observations      |
| `question`    | An open inquiry or research direction         |
| `decision`    | A committed choice with rationale             |
| `reference`   | An external pointer with context (paper, URL) |

`observation` is the default. Notes can annotate entities via `create(kind="note",
annotates=[entity_id], ...)`.

---

## The 17-relation ontology (closed set — [ADR-002](docs/adr/ADR-002-edge-ontology.md) base 15; [ADR-055](docs/adr/ADR-055-epistemic-edge-relations.md) +2 epistemic)

When you `link` nodes, use ONLY these relations:

### Structure

- `contains` — parent → child (system contains module)
- `part_of` — inverse of contains
- `instance_of` — specific is a case of general

### Derivation

- `extends` — child builds on parent (Flash Causal extends Flash Tiled)
- `variant_of` — A is a modified version of B (QLoRA variant_of LoRA)
- `introduced_by` — concept first described in paper / by person or org; document authorship
- `supersedes` — new replaces old entirely

### Provenance

- `derived_from` — output derived from input (artifact from dataset, document, etc.)

### Temporal

- `precedes` — earlier comes before later (document → document, dataset → dataset, etc.)

### Dependency

- `depends_on` — consumer needs dependency at runtime/build
- `enables` — prerequisite makes outcome possible

### Implementation

- `implements` — code realizes algorithm/concept

### Lateral

- `competes_with` — alternative approaches
- `composed_with` — used together in a system

### Annotation

- `annotates` — a note observes/comments on an entity (or another note)

### Epistemic

- `supports` — evidence for a claim (evidence → claim; weight = strength); same-substrate
- `refutes` — evidence against a claim (evidence → claim; weight = strength); same-substrate

**Why closed**: a sparse ontology stays queryable. Ad-hoc relations (`uses`, `related_to`,
`loaded_by`) fragment the graph and make traversal useless. If your relationship doesn't fit, it's
probably a property on the entity, not an edge.

---

## Tool schemas (required → **bold**, optional → normal)

These are the KG pack verbs. Other packs are documented in their verb tables above.

| Tool        | Fields                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | Example                                                      |
| ----------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| `create`    | **kind** (entity\|note), **name** + **entity_kind** for entity, **content** + note_kind for note; entity_type, description, properties, tags, salience, annotates. Bulk shape: **items** (array of entity specs, each with **kind** + **name**, optional entity_kind/entity_type/description/properties/tags, capped at 1000) in place of a top-level kind; atomic (bool, default true — true fails/writes nothing on any invalid item, false attempts each item independently and collects per-item errors); verbose (bool, default false — includes full entity objects in the response). Bulk-created entities skip vector embedding until a subsequent `reindex`. | `{"kind":"entity","entity_kind":"concept","name":"LoRA"}`    |
| `get`       | **id** — full UUID, the exact 8-char compact ID returned by a write verb, or (fallback) an exact entity name. A 9+ pure-hex prefix does not resolve; see the by-ID resolution note above                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | `{"id":"<uuid>"}`                                            |
| `list`      | **kind** (entity\|edge\|note\|event\|proposal); entity_kind, entity_type, note_kind, tags, source_id, target_id, relations, min_weight, max_weight, limit, offset; event: event_kind, event_kinds; message: thread_id, direction, from, to, read. kind="edge" pages the full set at `offset` (capped at 1000/page); for O(1)-at-depth full-graph walks pass `after` (keyset cursor — the last edge `id`, or `""` to start) instead — the response then becomes `{"edges":[...],"next_after":<uuid-or-null>}` rather than a bare array.                                                                                                                                | `{"kind":"entity","entity_kind":"concept","tags":["ml"]}`    |
| `update`    | **id** (UUID); name, description, properties, tags (entity), relation, weight (edge)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | `{"id":"<uuid>","description":"Updated desc"}`               |
| `delete`    | **id** (UUID); hard (default: false)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | `{"id":"<uuid>","hard":true}`                                |
| `merge`     | **into_id**, **from_id**; strategy (prefer_into\|prefer_from\|union). Returns `{kept_id, removed_id, edges_rewired, …}` — no top-level `id` field. Chain as `merge(...) \| link(source_id=$prev.kept_id, …)`, **not** `$prev.id`.                                                                                                                                                                                                                                                                                                                                                                                                                                     | `{"into_id":"<uuid>","from_id":"<uuid>"}`                    |
| `search`    | **kind** (entity\|note), **query** (text); entity_kind, entity_type, note_kind, tags, include_superseded (note), properties (entity post-filter), min_score, limit                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | `{"kind":"entity","query":"attention mechanism"}`            |
| `link`      | **source_id**, **target_id**, **relation**; weight (0.0–1.0)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | `{"source_id":"<A>","target_id":"<B>","relation":"extends"}` |
| `neighbors` | **node_id**; direction (out\|in\|both), relations, min_weight, limit; `include_entity_type` (bool, default false — when true each neighbor carries its `entity_type` subtype field)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | `{"node_id":"<uuid>","direction":"both"}`                    |
| `traverse`  | **roots** (UUID list); max_depth, direction, relations, include_roots; `include_properties` (bool, default false — when true each path node carries its entity `properties` map)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | `{"roots":["<uuid>"],"max_depth":2}`                         |
| `query`     | **query** (GQL or SPARQL string) — **read-only**: write-shaped input (SPARQL `INSERT`/`DELETE`/`LOAD`, GQL/Cypher `CREATE`/`DELETE`/`SET`/`MERGE`) is rejected; use `create`, `update`, `link`, `merge`, `delete` to mutate                                                                                                                                                                                                                                                                                                                                                                                                                                           | `{"query":"MATCH (a:concept)-[:extends]->(b) RETURN a"}`     |
| `propose`   | **kind** (entity\|note\|edge), fields for the proposed change. Returns `{id, status, proposer, title}`. Chain as `propose(...) \| review(id=$prev.id, …)`, **not** `$prev.proposal_id`. Nested objects (e.g. `changeset`) are expressible in the function-call DSL using quoted JSON-style keys (`changeset={"kind":"update_entity",...}`); unquoted JS-style keys are rejected. The full JSON batch form also works.                                                                                                                                                                                                                                                 | `{"kind":"entity","entity_kind":"concept","name":"X"}`       |
| `review`    | **id** (proposal UUID), **decision** (approve\|reject\|comment\|request_changes); comment. Returns `{decision, id, reviewer, status}` (status = new proposal status)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | `{"id":"<uuid>","decision":"approve"}`                       |
| `withdraw`  | **id** (proposal UUID). Returns `{by, id, status}` with `status: "withdrawn"`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | `{"id":"<uuid>"}`                                            |
| `resolve`   | **refs** (array of natural-language references — UUID, 8+ hex prefix, exact name, or free text); kind (restricts the hybrid-search fallback stage to an entity kind), limit (max candidates per ref from the hybrid-search fallback, default 5, max 20). Read-only. Returns one of `Resolved{id,confidence}` \| `Ambiguous{candidates}` \| `NotFound` per ref                                                                                                                                                                                                                                                                                                         | `{"refs":["the old record","<uuid>"]}`                       |
| `context`   | query and/or **entity_ids** (at least one required); hops (0-2, default 1), budget (256-65536 chars, default 4096), relations, direction (out\|in\|both, default both), limit (1-20, default 5), fanout (1-50, default 10). Returns `{anchors: [{...entity, neighbors: [{...edge fields, hop, via}]}], truncated, dropped: {anchors, neighbors}}`. Query anchors run one hybrid search; entity_ids anchors resolve in full, never clamped by `limit`. See ADR-089.                                                                                                                                                                                                    | `{"entity_ids":["<uuid>"],"hops":1,"budget":4096}`           |

### When to use which retrieval verb

- **`get(id)`** — you have a UUID, fetch the record (any type)
- **`search(kind, query)`** — text similarity: "find things _about_ X"
- **`list(kind, filters)`** — structured browse: "all concepts" / "edges from node A"
- **`neighbors(node_id)`** — one-hop graph: "what connects to X?"
- **`traverse(roots)`** — multi-hop graph: "reachability within N hops"
- **`context(query|entity_ids)`** — anchor + bounded 1-2 hop expansion in one call, budget-packed: "everything relevant near X, ready to hand to an agent"
- **`query(gql)`** — pattern matching: "concepts that extend something introduced by a paper"

### Supersession via edges

To supersede a record, create a `supersedes` edge:

```
request(ops="link(source_id=\"<new_note>\", target_id=\"<old_note>\", relation=\"supersedes\")")
```

`search(kind="note")` already excludes notes targeted by a `supersedes` edge (implemented in
`khive_runtime::operations::search_notes`, per ADR-013 §"Supersession via edge, not field"). That
exclusion is a **view-layer filter**: superseding **keeps** the old note and its edges and
marks it superseded; it never deletes, copies, or transfers anything. "Show only current" is a
query concern. See CLAUDE.md §"Data vs. view — the principle most violated here" before
implementing any supersede / annotate / currency behavior.

---

## Research workflow pattern

Each step below is run as `request(ops="<verb_call>")`. The inner verb syntax is shown for
brevity — wrap it in `request(...)` when calling MCP.

```
1. search(kind="note", query="topic I'm investigating")
   → see what you already know

2. search(kind="entity", query="FlashAttention")
   → check what's already in the graph

3. For new concepts:
   create(kind="entity", entity_kind="concept", name="...", properties={...})

4. For relationships:
   link(source_id="<A>", target_id="<B>", relation="extends")

5. For observations/insights:
   create(kind="note", note_kind="observation", content="...", annotates=["<entity_id>"])

6. For structural queries:
   traverse(roots=["<entity_id>"], max_depth=3, relations=["extends", "variant_of"])
```

Independent ops can be batched in one call:

```
request(ops="[search(kind=\"entity\", query=\"LoRA\"), search(kind=\"note\", query=\"LoRA\")]")
```

---

## Entity naming conventions

- **Short canonical names**, not full titles: `LoRA` not
  `Low-Rank Adaptation of Large Language Models`
- **Papers**: entity name = short name (`Sinkhorn Distances`). Full title, authors, year, arxiv ID
  in `properties`
- **Algorithms**: the name people actually say: `GQA`, `RoPE`, `FlashAttention`
- **No prefixes/suffixes**: `Speculative Decoding` not `Speculative Decoding (concept)`

---

## Property conventions

Use these canonical property keys when applicable:

| Key       | Values                                                                                     | Purpose                          |
| --------- | ------------------------------------------------------------------------------------------ | -------------------------------- |
| `type`    | `paper`, `algorithm`, `technique`, `architecture`, `model`, `benchmark`, `dataset`, `tool` | Finer classification than `kind` |
| `domain`  | `attention`, `inference`, `training`, `fine-tuning`, `optimal-transport`, etc.             | Research area                    |
| `status`  | `concept`, `researched`, `prototyped`, `implemented`, `shipped`, `deprecated`              | Maturity                         |
| `source`  | `arxiv:2106.09685` or DOI/URL                                                              | Citation pointer                 |
| `summary` | One-paragraph description                                                                  | Human-readable explanation       |

For papers also include: `title`, `authors`, `year`.

---

## Edge density rules

Sparse graphs are useless. Every entity should have minimum edges:

| Entity kind                | Min edges | Required relations                                                                                                       |
| -------------------------- | --------- | ------------------------------------------------------------------------------------------------------------------------ |
| `concept` (algorithm)      | 4         | `instance_of` or `extends` (at least one parent), `introduced_by` if paper exists, `competes_with` if alternatives exist |
| `concept` (paper)          | 2         | `introduced_by` from concepts it introduced                                                                              |
| `project` (implementation) | 3         | `contains` or `part_of`, `implements` (what concept), `depends_on`                                                       |
| `person`                   | 1         | `introduced_by` from their work                                                                                          |

**Target**: 5+ edges per entity average. Below 3 = polish needed.

---

## GQL traverse examples

```gql
# What does LoRA derive from / what derives from LoRA?
MATCH (a)-[:extends|variant_of*1..3]->(b {name: 'LoRA'}) RETURN a, b

# Find all papers in the attention domain
MATCH (a:concept) WHERE a.domain = 'attention' AND a.type = 'paper' RETURN a

# What concepts does this implementation realize?
MATCH ({name: 'lattice-inference'})-[:implements]->(c:concept) RETURN c

# Multi-hop lineage: from a paper to current implementations
MATCH (p:concept)<-[:introduced_by]-(c)<-[:implements]-(impl)
WHERE p.name = 'Attention Is All You Need'
RETURN c, impl
```

## SPARQL traverse examples

```sparql
# Same as first GQL example, SPARQL syntax
SELECT ?a ?b WHERE { ?a :extends+ ?b . ?b :name 'LoRA' . } LIMIT 10

# Find concepts in a specific domain
SELECT ?a WHERE { ?a a :concept . ?a :domain 'attention' . } LIMIT 20
```

Both syntaxes compile to the same SQL. Use whichever is natural.

---

## Self-expansion: let the graph grow with your work

khive isn't a passive database — it's designed for the graph to grow as you research:

- **Extract**: feed papers in, entities + edges fall out automatically
- **Gap detection**: traverse finds structural holes — "these clusters should connect"
- **Frontier discovery**: identify leaf nodes worth exploring next
- **Annotate**: notes attach observations to entities, creating cross-substrate navigation

Don't think of yourself as a curator. Think of yourself as a researcher whose work happens to leave
structural traces.

---

## Common mistakes

| Mistake                                           | Why it's wrong                                     |
| ------------------------------------------------- | -------------------------------------------------- |
| Storing findings only as notes, never as entities | Notes are for context; entities are for structure  |
| Creating duplicate entities                       | Always `search` first — link to existing if found  |
| Using ad-hoc relations                            | Map to the closed 17-relation set or don't link    |
| Reversed `introduced_by` direction                | concept → paper (the paper introduces the concept) |
| One-hop neighbor queries when you need lineage    | Use `traverse` with `max_depth` for multi-hop      |
| Adding `version`/`date` to entity names           | Those are properties, not names                    |

---

## AI-assisted contribution policy

If you are an AI agent authoring PRs, issues, or comments via someone's CLI:

1. **Attribution**: start the body with a blockquote attribution line:
   `> _PR description authored by Claude (Anthropic agent) on behalf of @<handle>._`
2. **Verify claims**: every claim in your PR description must match the actual diff.
3. **Test evidence**: include `cargo test` output for behavior-changing code.
4. **ADR awareness**: link to relevant ADRs. Schema/interface changes require an ADR first.

---

## Pack authoring pattern (governance — [ADR-023](docs/adr/ADR-023-declarative-pack-format.md), [ADR-095](docs/adr/ADR-095-verb-surface-consolidation.md))

If you are adding a new pack or a new verb to an existing pack:

1. **Express CRUD-shaped operations through kg dispatch-by-kind.** A new pack that only
   creates, reads, updates, deletes, or searches records of some kind should route through
   the kg pack's `create`/`search`/`list`/`get`/`update`/`delete` verbs (dispatched by
   `kind`) rather than introducing bespoke pack-prefixed verbs for the same operation. A
   bespoke verb is justified only when the operation carries genuine non-CRUD domain logic —
   a state machine, an atomic multi-step guard, or side effects beyond storing a record —
   that dispatch-by-kind cannot express.
2. **Put per-kind create-time field validation in `KindHook::prepare_create`.** Enum checks,
   format checks, and cross-field guards for a kind belong in that kind's `KindHook`
   implementation ([ADR-017](docs/adr/ADR-017-pack-standard.md)), not in a parallel handler
   that duplicates the same checks outside the hook seam.

---

## Daemon and warm startup

The `kkernel` binary auto-spawns a background daemon (`kkernel mcp --daemon`) on the first request. The daemon
keeps the ANN index and embedding model warm so `knowledge.search` and `memory.recall` are fast on
subsequent calls. Users do not need to configure or manage the daemon — it starts automatically and
cleans up on exit.

The daemon communicates over a Unix socket (`khived.sock`). If you see stale-process errors after a
rebuild, kill zombie processes: `pkill -f kkernel` then reconnect.

---

## Namespace (attribution-only — ADR-007 Rev 6)

Namespace is a write-stamp on records. Every record is stored under namespace `"local"` by
default. It is attribution, not isolation: queryable and filterable as a data column, but never a
storage boundary.

By-ID operations (`get`, `update`, `delete`, `merge`) are namespace-agnostic. They resolve the
globally-unique UUID with no namespace check at any layer. Authorization is enforced at the Gate
(ADR-018), not in storage or by-ID post-fetch checks.

Multi-record operations (`list`, `search`, `recall`, `neighbors`, `traverse`, `query`) default to
`WHERE namespace='local'`. The only way to target a different namespace is an explicit `namespace=`
parameter in the verb call.

---

## Admin tooling

**kkernel** is an optional admin CLI for operators. It provides pack introspection, reindexing, and
engine management commands (`kkernel sync`, `kkernel vector`). Agents do not need kkernel — all
agent-facing operations go through the `request` tool.

---

## See also

- `CLAUDE.md` — for working on khive itself
- `docs/adr/` — Architecture Decision Records (the design contract)
- `docs/adr/README.md` — full ADR index
