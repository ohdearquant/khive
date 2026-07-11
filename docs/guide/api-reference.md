# API Reference

khive exposes exactly one MCP tool, `request`. Everything else, 78 verbs across 11
production packs, is dispatched through that single tool via a small request DSL.
This page documents the DSL grammar, the response envelope, and every verb's full
parameter contract, so an agent can call khive correctly without reading Rust source.

This page is verified against the live registry (`request(ops="verbs()")`, run
2026-07-10) and the pack source (`crates/khive-pack-*/src/*.rs` `HandlerDef`/`ParamDef`
struct literals). Verb count: **78**, matching both the live registry `total` field and
the sum of the 11 pack counts below. If your server reports a different total, your
`KHIVE_PACKS` configuration loads a different pack set than the default, run
`request(ops="verbs()")` against your own server to get the authoritative list.

An always-machine-readable copy of this page is at
[`/md/api-reference.md`](md/api-reference.md). The site also publishes
[`/llms.txt`](llms.txt) (a short index) and [`/llms-full.txt`](llms-full.txt)
(every guide page concatenated) for agents that prefer one fetch over several.

## Packs at a glance

| Pack        | Verbs | Load with                  | Optional?           |
| ----------- | ----- | -------------------------- | ------------------- |
| `kg`        | 18    | `KHIVE_PACKS=kg` (default) | No — base substrate |
| `gtd`       | 5     | `KHIVE_PACKS=kg,gtd`       | Yes                 |
| `memory`    | 5     | `KHIVE_PACKS=kg,memory`    | Yes                 |
| `brain`     | 15    | `KHIVE_PACKS=kg,brain`     | Yes                 |
| `comm`      | 7     | `KHIVE_PACKS=kg,comm`      | Yes                 |
| `schedule`  | 4     | `KHIVE_PACKS=kg,schedule`  | Yes                 |
| `knowledge` | 19    | `KHIVE_PACKS=kg,knowledge` | Yes                 |
| `session`   | 4     | `KHIVE_PACKS=kg,session`   | Yes                 |
| `git`       | 1     | `KHIVE_PACKS=kg,git`       | Yes                 |
| `code`      | 0     | `KHIVE_PACKS=kg,code`      | Yes                 |
| `workspace` | 0     | `KHIVE_PACKS=kg,git,gtd,session,workspace` | Yes                 |

`git` also registers the `commit` / `issue` / `pull_request` note kinds and the shared
`run_ingest` core (`crates/khive-pack-git/src/ingest.rs`) that both `git.digest` and the
`kkernel git-ingest` CLI drive.

`workspace` requires `kg`, `git`, `gtd`, and `session` to be loaded alongside it (the runtime rejects a pack set that omits a declared dependency), so its minimal example lists all four.

`code` registers the `finding` note kind and edge rules only; its `code.ingest` verb is
accepted but unimplemented (ADR-085), and `findings.json` ingest runs through the
`kkernel code-ingest` admin CLI path, not the MCP verb surface — so it contributes 0
verbs to the total below.

The default binary (no `KHIVE_PACKS`/`--pack` override) loads all 11 packs: 18 + 5 + 5 +
15 + 7 + 4 + 19 + 4 + 1 + 0 + 0 = **78 verbs**.

Verb names in the `kg` pack are bare (`create`, `search`, `link`, …). Every other pack
namespaces its verbs with a `pack.` prefix (`gtd.assign`, `memory.recall`,
`brain.feedback`, `comm.send`, `schedule.remind`, `knowledge.search`, `session.store`).

---

## DSL syntax

The `request` tool takes one string argument, `ops`, in one of four forms.

### Single op

```
request(ops="search(kind=\"entity\", query=\"LoRA\")")
```

### Parallel batch

Up to 100 ops, run with no ordering guarantee between them:

```
request(ops="[memory.recall(query=\"x\"), memory.remember(content=\"y\")]")
```

### Chain

Ops separated by `|` run sequentially; `$prev` resolves against the immediately
preceding op's result (not any earlier op — non-adjacent dependencies require splitting
into separate `request` calls):

```
request(ops="create(kind=\"concept\", name=\"X\") | link(source_id=$prev.id, target_id=\"<uuid>\", relation=\"extends\")")
```

`$prev` path extraction:

| Form                | Meaning                |
| ------------------- | ---------------------- |
| `$prev`             | the full prior result  |
| `$prev.field`       | a nested object field  |
| `$prev.items[0].id` | array index then field |
| `$prev[2]`          | top-level array index  |

A quoted string containing `$prev` is promoted to a substitution automatically
(`id="$prev.id"` behaves the same as `id=$prev.id`). To pass the literal four
characters `$prev`, escape it: `"\\$prev"`.

### JSON form

Equivalent to parallel batch, for callers that prefer to build JSON directly:

```
request(ops="[{\"tool\":\"search\",\"args\":{\"kind\":\"entity\",\"query\":\"LoRA\"}}]")
```

JSON form only supports independent ops — a literal `$prev` anywhere in JSON form is a
parse error (`DslError::PrevRefInJsonForm`), since JSON form has no chain syntax.

### Parser constraints (source: `khive-request`, ADR-016)

- **`MAX_OPS` = 100** per request; exceeding it is `DslError::TooManyOps`.
- **`$prev` is chain-only.** Using it outside a `|` chain, or anywhere in JSON form, is
  rejected at parse time.
- **Write-key conflict detection**: a parallel batch where two ops target the same UUID
  via `update`/`delete` (`id`), `merge` (`into_id`/`from_id`), or `link`
  (`source_id`/`target_id`) is rejected before any op dispatches, rather than racing.
- **`RESERVED_ENVELOPE_ARGS`** (`presentation`, `presentation_per_op`) are envelope-level
  fields on the `request` tool call itself; passing them inside a verb's own argument
  list is rejected (`DslError::ReservedEnvelopeArg`).
- Mixing `,` and `|` at the top level is rejected (`DslError::MixedSeparators`).
- Only single-level `pack.verb` names are supported — `a.b.c` is
  `DslError::UnsupportedVerbNesting`.
- Argument values are JSON literals. Strings must be double-quoted, including inside
  DSL function-call form — a bare word as a value fails at the assignment, even
  standalone.

## Response envelope

Every op returns its own `ok`/`error` outcome; a batch's per-op failure does not abort
its siblings (chain failures do abort the remainder of the chain):

```json
{
  "results": [
    { "ok": true, "tool": "search", "result": { "...": "..." } },
    { "ok": false, "tool": "get", "error": "not found: ..." }
  ],
  "summary": { "total": 2, "succeeded": 1, "failed": 1, "aborted": 0 }
}
```

`aborted` counts ops skipped after an earlier failure in a `|` chain; it is always 0 for
parallel batches, since parallel failures do not cascade.

---

## `kg` pack — 18 verbs

Base substrate verbs, bare names (no `kg.` prefix). Category is the illocutionary act
(Searle 1976): Assertive = retrieves state, Commissive = commits a persistent change,
Declaration = changes institutional status by fiat.

### `create` — Commissive

Create an entity or note (singleton) or a batch of entities (bulk via `items`).

| Param               | Type            | Required    | Notes                                                                                                                                                                                                                                                                                      |
| ------------------- | --------------- | ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `kind`              | string          | conditional | Substrate (`entity`\|`note`) or granular kind (`concept`, `document`, `observation`, …). Required for the singleton path; not required when `items` is present.                                                                                                                            |
| `name`              | string          | no          | Entity name (singleton).                                                                                                                                                                                                                                                                   |
| `entity_kind`       | string          | no          | concept\|document\|dataset\|project\|person\|org\|artifact\|service\|resource (when `kind="entity"`).                                                                                                                                                                                      |
| `note_kind`         | string          | no          | observation\|insight\|question\|decision\|reference (when `kind="note"`).                                                                                                                                                                                                                  |
| `content`           | string          | no          | Note body text (singleton notes).                                                                                                                                                                                                                                                          |
| `embedding_content` | string          | no          | Singleton `kind="note"` only. A non-empty proper prefix of `content` sent to the vector embedder instead of the full text, for content that exceeds an embedder's input cap. Stored and FTS-indexed content are always the full `content`; this only overrides the vector-embedding input. |
| `description`       | string          | no          | Entity free-text description.                                                                                                                                                                                                                                                              |
| `tags`              | array\<string\> | no          | Tag list.                                                                                                                                                                                                                                                                                  |
| `entity_type`       | string          | no          | First-class type tag, e.g. `"paper"`, `"algorithm"`, `"tool"`.                                                                                                                                                                                                                             |
| `properties`        | object          | no          | Arbitrary JSON properties.                                                                                                                                                                                                                                                                 |
| `items`             | array\<object\> | no          | Bulk entity creation, each `{kind, name, entity_kind?, entity_type?, description?, properties?, tags?}`. Capped at 1000/request. Bulk-created entities skip embedding until a later `reindex`.                                                                                             |
| `atomic`            | bool            | no          | Bulk path. Default true = all-or-nothing; false = per-item errors collected.                                                                                                                                                                                                               |
| `verbose`           | bool            | no          | Bulk path. When true, response includes full entity objects.                                                                                                                                                                                                                               |

```
request(ops="create(kind=\"concept\", name=\"RoPE\", description=\"Rotary position embedding\")")
```

### `get` — Assertive

Fetch any record by UUID (auto-detects entity/note/edge/event/proposal).

| Param             | Type | Required | Notes                                                                  |
| ----------------- | ---- | -------- | ---------------------------------------------------------------------- |
| `id`              | uuid | yes      | Full UUID or short hex prefix (min 8 chars).                           |
| `include_deleted` | bool | no       | Return soft-deleted records too (default false); requires a full UUID. |

```
request(ops="get(id=\"3f2a9c1e\")")
```

### `list` — Assertive

List records with optional filtering.

| Param                        | Type                     | Required | Notes                                                                         |
| ---------------------------- | ------------------------ | -------- | ----------------------------------------------------------------------------- |
| `kind`                       | string                   | yes      | `entity`\|`note`\|`edge`\|`event`\|`proposal`\|`message`, or a granular kind. |
| `limit`                      | integer                  | no       | Default 20.                                                                   |
| `offset`                     | integer                  | no       | Default 0.                                                                    |
| `entity_kind`                | string                   | no       | Filter when `kind="entity"`.                                                  |
| `entity_type`                | string                   | no       | Filter by type field when `kind="entity"`.                                    |
| `note_kind`                  | string                   | no       | Filter when `kind="note"`.                                                    |
| `tags`                       | array\<string\>          | no       | OR-match, `kind="entity"` only.                                               |
| `source_id` / `target_id`    | uuid                     | no       | Edge endpoint filters, `kind="edge"` only.                                    |
| `relations`                  | array\<string\>          | no       | Edge relation filter, `kind="edge"` only.                                     |
| `min_weight` / `max_weight`  | number                   | no       | Edge weight bounds, `kind="edge"` only.                                       |
| `event_kind` / `event_kinds` | string / array\<string\> | no       | `kind="event"` only; additive.                                                |
| `thread_id`                  | string                   | no       | `kind="message"` only; full UUID or 8-char prefix.                            |
| `direction`                  | string                   | no       | `kind="message"` only: `inbound`\|`outbound`.                                 |
| `from` / `to`                | string                   | no       | `kind="message"` only, sender/recipient filter.                               |
| `read`                       | bool                     | no       | `kind="message"` only.                                                        |
| `delivered`                  | bool                     | no       | `kind="message"` only.                                                        |

```
request(ops="list(kind=\"entity\", entity_kind=\"concept\", limit=20)")
```

### `stats` — Assertive

Return aggregate KG substrate counts (entities, edges, notes). No params.

```
request(ops="stats()")
```

### `update` — Declaration

Patch entity, note, or edge fields. Field set depends on substrate: entities accept
`name`/`description`/`properties`/`tags`; notes accept
`name`/`content`/`salience`/`decay_factor`/`properties`; edges accept
`relation`/`weight`/`properties`.

| Param          | Type            | Required | Notes                                                                     |
| -------------- | --------------- | -------- | ------------------------------------------------------------------------- |
| `id`           | uuid            | yes      | Record to patch.                                                          |
| `kind`         | string          | no       | Substrate hint (`entity`\|`note`\|`edge`); omit to resolve from the UUID. |
| `name`         | string          | no       | Entities and notes.                                                       |
| `description`  | string          | no       | Entities only.                                                            |
| `content`      | string          | no       | Notes only (body text).                                                   |
| `salience`     | number          | no       | Notes only, 0.0–1.0.                                                      |
| `decay_factor` | number          | no       | Notes only, >= 0.                                                         |
| `relation`     | string          | no       | Edges only, one of the 17 canonical relations.                            |
| `weight`       | number          | no       | Edges only, 0.0–1.0.                                                      |
| `properties`   | object          | no       | Shallow-merged in.                                                        |
| `tags`         | array\<string\> | no       | Replaces the tag list.                                                    |

```
request(ops="update(id=\"<uuid>\", salience=0.7)")
```

### `delete` — Declaration

Soft or hard delete a record.

| Param  | Type   | Required | Notes                                                                    |
| ------ | ------ | -------- | ------------------------------------------------------------------------ |
| `id`   | uuid   | yes      | Record to delete.                                                        |
| `kind` | string | no       | Substrate hint; omit to resolve from the UUID.                           |
| `hard` | bool   | no       | Default false (soft delete). True permanently removes with edge cascade. |

```
request(ops="delete(id=\"<uuid>\")")
```

### `merge` — Declaration

Deduplicate two entities. Returns `{kept_id, removed_id, edges_rewired,
properties_merged, tags_unioned, content_appended, dry_run}` — chain with
`$prev.kept_id`, **not** `$prev.id` (merge has no top-level `id` field).

| Param     | Type | Required | Notes                                       |
| --------- | ---- | -------- | ------------------------------------------- |
| `into_id` | uuid | yes      | Entity that survives the merge (canonical). |
| `from_id` | uuid | yes      | Entity merged from; soft-deleted afterward. |

```
request(ops="merge(into_id=\"<canonical-uuid>\", from_id=\"<dup-uuid>\")")
```

### `search` — Assertive

Hybrid FTS + vector search with RRF fusion.

| Param                | Type    | Required | Notes                                                                                                                                  |
| -------------------- | ------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `kind`               | string  | yes      | Substrate or granular kind to search.                                                                                                  |
| `query`              | string  | yes      | Free-text query.                                                                                                                       |
| `limit`              | integer | no       | Default 10.                                                                                                                            |
| `entity_kind`        | string  | no       | `kind="entity"` only.                                                                                                                  |
| `entity_type`        | string  | no       | `kind="entity"` only.                                                                                                                  |
| `note_kind`          | string  | no       | `kind="note"` only.                                                                                                                    |
| `include_superseded` | bool    | no       | `kind="note"` only; default false excludes notes targeted by a `supersedes` edge.                                                      |
| `properties`         | object  | no       | Match records whose properties contain all listed key=value pairs, applied before result truncation inside a bounded candidate window. |
| `tags`               | array   | no       | OR-match against tags; entity tags matched at the SQL level, note tags read from `properties.tags`.                                    |
| `min_score`          | number  | no       | Score floor 0.0–1.0. No server default; RRF rank-1 scores on small corpora are typically 0.013–0.033.                                  |

```
request(ops="search(kind=\"entity\", query=\"knowledge graph runtime\", limit=10)")
```

### `link` — Commissive

Create a typed directed edge.

| Param       | Type   | Required | Notes                                                                                                                                                                                                                                                                     |
| ----------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `source_id` | uuid   | yes      | Source node.                                                                                                                                                                                                                                                              |
| `target_id` | uuid   | yes      | Target node.                                                                                                                                                                                                                                                              |
| `relation`  | string | yes      | One of the 17 canonical relations: `contains`\|`part_of`\|`instance_of`\|`extends`\|`variant_of`\|`introduced_by`\|`supersedes`\|`derived_from`\|`precedes`\|`depends_on`\|`enables`\|`implements`\|`competes_with`\|`composed_with`\|`annotates`\|`supports`\|`refutes`. |
| `weight`    | number | no       | Default 1.0. 1.0=definitional, 0.7-0.9=strong, 0.4-0.6=plausible.                                                                                                                                                                                                         |

```
request(ops="link(source_id=\"<uuid-a>\", target_id=\"<uuid-b>\", relation=\"extends\")")
```

### `neighbors` — Assertive

Immediate graph neighbors.

| Param        | Type            | Required | Notes                                            |
| ------------ | --------------- | -------- | ------------------------------------------------ |
| `node_id`    | uuid            | yes      | Node whose neighbors to return.                  |
| `direction`  | string          | no       | `outgoing`\|`incoming`\|`both` (default `both`). |
| `relations`  | array\<string\> | no       | Restrict to these relation types.                |
| `min_weight` | number          | no       | Exclude edges below this weight.                 |

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"both\")")
```

### `traverse` — Assertive

Multi-hop BFS traversal.

| Param       | Type            | Required | Notes                                  |
| ----------- | --------------- | -------- | -------------------------------------- |
| `roots`     | array\<uuid\>   | yes      | Starting node UUIDs.                   |
| `max_depth` | integer         | no       | Default 3.                             |
| `relations` | array\<string\> | no       | Restrict traversal to these relations. |

```
request(ops="traverse(roots=[\"<uuid>\"], max_depth=2)")
```

### `context` — Assertive

Entity-anchored graph context in one call ([ADR-089](../adr/ADR-089-context-verb.md)).
Resolves anchors from `query` and/or `entity_ids`, expands 1-2 hops via the same
runtime op behind `neighbors`, and assembles a budgeted, deterministically-ordered
response — replacing a caller-side `search | neighbors` chain with a single
round-trip. `direction` defaults to `"both"`, matching `neighbors` and `traverse`
(`outgoing`/`incoming` on request). At least one of `query`/`entity_ids` is required.
One embedding inference when `query` is used; zero for a pure `entity_ids` call.

| Param        | Type            | Required | Notes                                                                                 |
| ------------ | --------------- | -------- | ------------------------------------------------------------------------------------- |
| `query`      | string          | no\*     | Semantic anchor selection via hybrid search; adds anchors after `entity_ids`.         |
| `entity_ids` | array\<string\> | no\*     | Explicit anchor UUIDs/prefixes/slugs. Honored in full, never clamped by `limit`.      |
| `hops`       | integer         | no       | Expansion depth, clamped 0..=2 (default 1).                                           |
| `budget`     | integer         | no       | Output budget in Unicode scalars of compact JSON, clamped 256..=65536 (default 4096). |
| `relations`  | array\<string\> | no       | Edge-relation filter applied during expansion.                                        |
| `direction`  | string          | no       | `outgoing`\|`incoming`\|`both` (default `both`).                                      |
| `limit`      | integer         | no       | Max anchors from the `query` leg, clamped 1..=20 (default 5).                         |
| `fanout`     | integer         | no       | Max neighbors per expanded node per hop, clamped 1..=50 (default 10).                 |

\* at least one of `query`/`entity_ids` required.

```
request(ops="context(query=\"rotary position embedding\", hops=1, budget=4096)")
```

Response shape:

```json
{
  "anchors": [
    {
      "entity": { "id": "…", "name": "…", "kind": "concept", "description": "…", "properties": {} },
      "neighbors": [
        {
          "id": "…",
          "name": "…",
          "relation": "extends",
          "direction": "outgoing",
          "weight": 0.9,
          "hop": 1,
          "via": null,
          "description": "…"
        }
      ]
    }
  ],
  "truncated": false,
  "dropped": { "anchors": 0, "neighbors": 0 }
}
```

### `query` — Assertive

GQL or SPARQL pattern matching (read-only). Write-shaped input (SPARQL
INSERT/DELETE/LOAD/WITH…DELETE, GQL/Cypher CREATE/DELETE/DETACH DELETE/SET/MERGE) is
rejected — use `create`/`update`/`link`/`merge`/`delete` to mutate the graph. Queries
that mix fixed-length and variable-length chains are not compiled in one call; split
them into separate `query()` calls.

| Param   | Type    | Required | Notes                                    |
| ------- | ------- | -------- | ---------------------------------------- |
| `query` | string  | yes      | GQL or SPARQL pattern string, read-only. |
| `limit` | integer | no       | Default 500, hard cap 10,000.            |

```
request(ops="query(query=\"MATCH (c:concept)-[:extends]->(d:concept) RETURN c, d LIMIT 20\")")
```

### `propose` — Commissive

Create an event-sourced change proposal. Returns `{id, status, proposer, title}` —
chain with `$prev.id`, not `$prev.proposal_id`. The `changeset` field has nested
objects and cannot be expressed in function-call DSL form; use JSON form.

| Param         | Type            | Required | Notes                                                                                                                                              |
| ------------- | --------------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `title`       | string          | yes      | Non-empty short title.                                                                                                                             |
| `description` | string          | yes      | Non-empty full description.                                                                                                                        |
| `changeset`   | object          | yes      | Discriminated by `kind`: `add_entity`, `update_entity`, `add_edge`, `add_note`, `merge_entities`, `supersede_entity`, `compound` (nested `steps`). |
| `reviewers`   | array\<string\> | no       | Actor IDs requested as reviewers.                                                                                                                  |
| `expiry`      | integer         | no       | Expiry timestamp, microseconds since epoch.                                                                                                        |
| `parent_id`   | uuid            | no       | Parent proposal this supersedes or extends.                                                                                                        |

```
request(ops="[{\"tool\":\"propose\",\"args\":{\"title\":\"Add GQE\",\"description\":\"Register the GQE concept\",\"changeset\":{\"kind\":\"add_entity\",\"entity\":{\"kind\":\"concept\",\"name\":\"GQE\"}}}}]")
```

### `review` — Declaration

Approve, reject, comment, or request changes on a proposal.

| Param      | Type   | Required | Notes                                              |
| ---------- | ------ | -------- | -------------------------------------------------- |
| `id`       | uuid   | yes      | Full UUID or 8-char short ID of the proposal.      |
| `decision` | string | yes      | `approve`\|`reject`\|`comment`\|`request_changes`. |
| `comment`  | string | no       | Reviewer comment.                                  |

```
request(ops="review(id=\"<proposal-id>\", decision=\"approve\")")
```

### `withdraw` — Commissive

Withdraw an open proposal (proposer-only).

| Param       | Type   | Required | Notes                                              |
| ----------- | ------ | -------- | -------------------------------------------------- |
| `id`        | uuid   | yes      | Full UUID or 8-char short ID of the open proposal. |
| `rationale` | string | no       | Reason for withdrawing.                            |

```
request(ops="withdraw(id=\"<proposal-id>\")")
```

### `resolve` — Assertive

Resolve natural-language references to ids. Each ref in `refs` is resolved through:
(1) id-string passthrough (UUID or 8+ hex prefix) via the existing by-ID path; (2) this
actor's recently-referenced ring; (3) hybrid search over the namespace. Returns one of
`Resolved{id,confidence}` | `Ambiguous{candidates}` | `NotFound` per ref — never a
silent pick among close candidates. Read-only: performs no mutation.

| Param   | Type            | Required | Notes                                                                                                        |
| ------- | --------------- | -------- | ------------------------------------------------------------------------------------------------------------ |
| `refs`  | array\<string\> | yes      | Natural-language references to resolve (UUID, hex prefix, exact entity name, or free text).                  |
| `kind`  | string          | no       | Restricts the hybrid-search fallback (stage 3) to an entity kind. No effect on the id-string or ring stages. |
| `limit` | integer         | no       | Max candidates returned per ref from the hybrid-search fallback. Default 5, max 20.                          |

```
request(ops="resolve(refs=[\"the old record\", \"<uuid>\"])")
```

### `verbs` — Assertive

List all MCP-callable verbs registered on this server. Internal subhandlers are
excluded.

| Param      | Type   | Required | Notes                                                                                                    |
| ---------- | ------ | -------- | -------------------------------------------------------------------------------------------------------- |
| `category` | string | no       | Filter: `Assertive`\|`Commissive`\|`Declaration`\|`Directive`.                                           |
| `pack`     | string | no       | Filter by pack name (`kg`, `gtd`, `memory`, `brain`, `comm`, `schedule`, `knowledge`, `session`, `git`). |

```
request(ops="verbs()")
```

---

## `gtd` pack — 5 verbs

GTD task lifecycle over notes (`kind="task"`). Optional; load with
`KHIVE_PACKS=kg,gtd`.

### `gtd.assign` — Directive

Create a GTD task (note with `kind=task`).

| Param               | Type            | Required | Notes                                                                                                                                                                |
| ------------------- | --------------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `title`             | string          | yes      | Task title.                                                                                                                                                          |
| `status`            | string          | no       | `inbox`\|`next`\|`waiting`\|`someday`\|`active` (default `inbox`). Aliases: `todo`=inbox, `in_progress`=active, `blocked`=waiting, `later`=someday, `finished`=done. |
| `priority`          | string          | no       | `p0`\|`p1`\|`p2`\|`p3` (default `p2`).                                                                                                                               |
| `assignee`          | string          | no       | Assignee identifier.                                                                                                                                                 |
| `due`               | string          | no       | ISO-8601 due date.                                                                                                                                                   |
| `depends_on`        | array\<uuid\>   | no       | Blocking task UUIDs.                                                                                                                                                 |
| `context_entity_id` | uuid            | no       | Full UUID of a related KG entity.                                                                                                                                    |
| `tags`              | array\<string\> | no       | Tag list.                                                                                                                                                            |

```
request(ops="gtd.assign(title=\"Ship API reference\", priority=\"p1\", assignee=\"agent:docs\")")
```

### `gtd.next` — Assertive

List actionable tasks (status `next` or `active`) by priority.

| Param      | Type    | Required | Notes                    |
| ---------- | ------- | -------- | ------------------------ |
| `limit`    | integer | no       | Default 10.              |
| `assignee` | string  | no       | Filter to this assignee. |

```
request(ops="gtd.next(assignee=\"agent:docs\", limit=10)")
```

### `gtd.complete` — Declaration

Mark a task done (or cancelled) with an optional result note.

| Param    | Type   | Required | Notes                                             |
| -------- | ------ | -------- | ------------------------------------------------- |
| `id`     | uuid   | yes      | Task to complete.                                 |
| `result` | string | no       | Completion note.                                  |
| `status` | string | no       | Terminal status: `done` (default) or `cancelled`. |

```
request(ops="gtd.complete(id=\"<task-id>\", result=\"shipped in PR #600\")")
```

### `gtd.tasks` — Assertive

List tasks filtered by status, assignee, priority.

| Param      | Type    | Required | Notes                                                                                              |
| ---------- | ------- | -------- | -------------------------------------------------------------------------------------------------- |
| `status`   | string  | no       | `inbox`\|`next`\|`waiting`\|`someday`\|`active`\|`done`\|`cancelled` (aliases as in `gtd.assign`). |
| `assignee` | string  | no       | Filter by assignee.                                                                                |
| `priority` | string  | no       | `p0`\|`p1`\|`p2`\|`p3`.                                                                            |
| `limit`    | integer | no       | Default 20.                                                                                        |
| `offset`   | integer | no       | Default 0.                                                                                         |

```
request(ops="gtd.tasks(status=\"active\", assignee=\"agent:docs\")")
```

### `gtd.transition` — Declaration

Explicit GTD status transition with lifecycle validation.

| Param    | Type   | Required | Notes                                      |
| -------- | ------ | -------- | ------------------------------------------ |
| `id`     | uuid   | yes      | Task to transition.                        |
| `status` | string | yes      | Target status (same set/aliases as above). |
| `note`   | string | no       | Note attached to the transition.           |

```
request(ops="gtd.transition(id=\"<task-id>\", status=\"active\")")
```

---

## `memory` pack — 5 verbs

Salience- and decay-weighted memory notes. Optional; load with
`KHIVE_PACKS=kg,memory`.

### `memory.remember` — Commissive

Create a memory note with salience and decay.

| Param             | Type   | Required | Notes                                                                                                                       |
| ----------------- | ------ | -------- | --------------------------------------------------------------------------------------------------------------------------- |
| `content`         | string | yes      | Memory content.                                                                                                             |
| `salience`        | number | no       | 0.0–1.0. Type-differentiated default: episodic=0.3, semantic=0.5.                                                           |
| `decay_factor`    | number | no       | >= 0. Type-differentiated default: episodic=0.02 (~35d half-life), semantic=0.005 (~139d half-life). Higher = faster decay. |
| `memory_type`     | string | no       | `episodic`\|`semantic` (default `episodic`); no other values accepted.                                                      |
| `source_id`       | string | no       | UUID or 8-char short ID of the entity/note this memory annotates.                                                           |
| `embedding_model` | string | no       | Registered model name; defaults to pack config.                                                                             |
| `tags`            | array  | no       | Stored in `properties.tags`.                                                                                                |
| `namespace`       | string | no       | Write namespace override. Default: episodic → caller's namespace, semantic → `local`.                                       |

```
request(ops="memory.remember(content=\"ADR-016 fixes the DSL grammar\", salience=0.7, memory_type=\"semantic\")")
```

### `memory.recall` — Assertive

Recall memory notes with decay-aware hybrid ranking. Each hit carries resolved
(read-model) values — `memory_type` defaults to `episodic` when unset; `salience` and
`decay_factor` reflect the effective defaults used for ranking.

| Param               | Type    | Required | Notes                                                                     |
| ------------------- | ------- | -------- | ------------------------------------------------------------------------- |
| `query`             | string  | yes      | Semantic recall query.                                                    |
| `limit`             | integer | no       | Default 10.                                                               |
| `top_k`             | integer | no       | Overrides `limit` (max 100).                                              |
| `min_score`         | number  | no       | Composite score floor, always in [0,1]. Typical production floor 0.3–0.7. |
| `score_floor`       | number  | no       | Alias for `min_score`.                                                    |
| `min_salience`      | number  | no       | Salience floor.                                                           |
| `memory_type`       | string  | no       | Filter to this type.                                                      |
| `fusion_strategy`   | string  | no       | `rrf`\|`weighted`\|`union`\|`vector_only`\|`keyword_only`.                |
| `embedding_model`   | string  | no       | Registered model name; defaults to pack config.                           |
| `include_breakdown` | bool    | no       | Include per-component score breakdown.                                    |
| `entity_names`      | array   | no       | Names to boost; matches get a 1.3x multiplier.                            |
| `full_content`      | bool    | no       | Default true; false truncates content to 200 chars.                       |
| `tags`              | array   | no       | Filter by `properties.tags`.                                              |
| `tag_mode`          | string  | no       | `any` (default, OR) or `all` (AND).                                       |

```
request(ops="memory.recall(query=\"ADR-016 DSL grammar\", limit=5, min_score=0.3)")
```

### `memory.feedback` — Commissive

Emit explicit feedback on a recalled entity; updates recall-domain posteriors.

| Param       | Type   | Required | Notes                                                                                                                              |
| ----------- | ------ | -------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `target_id` | string | yes      | UUID of the recalled entity or memory.                                                                                             |
| `signal`    | string | yes      | `useful`\|`not_useful`\|`wrong`\|`explicit_positive`\|`explicit_negative`\|`implicit_positive`\|`implicit_negative`\|`correction`. |

```
request(ops="memory.feedback(target_id=\"<uuid>\", signal=\"useful\")")
```

### `memory.prune` — Commissive

Soft-delete memories below a salience threshold and/or past `expires_at`
(curation-layer, ADR-014).

| Param          | Type    | Required | Notes                                                                                                               |
| -------------- | ------- | -------- | ------------------------------------------------------------------------------------------------------------------- |
| `min_salience` | number  | no       | Soft-delete memories strictly below this value.                                                                     |
| `before`       | integer | no       | Soft-delete memories expired at/before this Unix microsecond timestamp; defaults to now; 0 skips the expiry filter. |
| `namespace`    | string  | no       | Defaults to `local`.                                                                                                |
| `dry_run`      | bool    | no       | Default false; when true, counts candidates without deleting.                                                       |

```
request(ops="memory.prune(min_salience=0.2, dry_run=true)")
```

### `memory.vacuum` — Commissive

Run SQLite `VACUUM` to reclaim space freed by soft-deleted rows. No params.

```
request(ops="memory.vacuum()")
```

---

## `brain` pack — 15 verbs

Recall-tuning profiles: Beta-posterior scoring, profile lifecycle, and the actor/
namespace/consumer-kind resolution table that picks which profile serves a given
caller. Optional; load with `KHIVE_PACKS=kg,brain`.

### `brain.event_counts` — Assertive

Windowed event counts grouped by kind and actor over the event plane (ADR-103 Stage 1,
#724 Ask A). `feedback_explicit` events are additionally split by `served_by_profile_id`.
Events carrying a `work_class` (today: `phase_started`/`phase_completed`/`phase_cancelled`
payloads) split by `counts_by_work_class`. `cost_unit` is not surfaced: it does not exist
on any event payload yet (ADR-103 Stage 0 is design-only) and will be added once a
resource payload carries it.

| Param   | Type   | Required | Notes                                                                  |
| ------- | ------ | -------- | ---------------------------------------------------------------------- |
| `since` | string | yes      | Window start, ISO-8601/RFC-3339 datetime. Inclusive.                   |
| `until` | string | no       | Window end, ISO-8601/RFC-3339 datetime. Exclusive. Defaults to now.    |
| `actor` | string | no       | Filter to a single actor. Omit for all actors.                         |
| `kind`  | string | no       | Filter to a single EventKind (e.g. `"recall_executed"`). Omit for all. |

```
request(ops="brain.event_counts(since=\"2026-07-01T00:00:00Z\")")
```

### `brain.profiles` — Assertive

List profiles, optionally filtered by lifecycle.

| Param       | Type   | Required | Notes                                           |
| ----------- | ------ | -------- | ----------------------------------------------- |
| `lifecycle` | string | no       | `active`\|`inactive`\|`archived`; omit for all. |

```
request(ops="brain.profiles(lifecycle=\"active\")")
```

### `brain.profile` — Assertive

Profile metadata, latest snapshot, current state summary.

| Param        | Type   | Required | Notes                                                         |
| ------------ | ------ | -------- | ------------------------------------------------------------- |
| `profile_id` | string | yes      | Profile ID string (e.g. `"balanced-recall-v1"`) — not a UUID. |

```
request(ops="brain.profile(profile_id=\"implementer-recall-v1\")")
```

### `brain.resolve` — Assertive

Show which profile would serve a caller context.

| Param           | Type   | Required | Notes                                                        |
| --------------- | ------ | -------- | ------------------------------------------------------------ |
| `consumer_kind` | string | yes      | Verb/operation type about to be performed (e.g. `"recall"`). |
| `actor`         | string | no       | Default `*` (wildcard match).                                |
| `namespace`     | string | no       | Default `*` (wildcard match).                                |

```
request(ops="brain.resolve(consumer_kind=\"recall\", actor=\"agent:docs\")")
```

### `brain.activate` — Commissive

Move a profile to Active (starts the live update loop).

| Param        | Type   | Required | Notes                |
| ------------ | ------ | -------- | -------------------- |
| `profile_id` | string | yes      | Profile to activate. |

```
request(ops="brain.activate(profile_id=\"implementer-recall-v1\")")
```

### `brain.deactivate` — Commissive

Move a profile to Inactive (stop live updates, retain state).

| Param        | Type   | Required | Notes                  |
| ------------ | ------ | -------- | ---------------------- |
| `profile_id` | string | yes      | Profile to deactivate. |

```
request(ops="brain.deactivate(profile_id=\"implementer-recall-v1\")")
```

### `brain.archive` — Declaration

Move a profile to Archived (read-only, audit-retained).

| Param        | Type   | Required | Notes               |
| ------------ | ------ | -------- | ------------------- |
| `profile_id` | string | yes      | Profile to archive. |

```
request(ops="brain.archive(profile_id=\"deprecated-recall-v0\")")
```

### `brain.reset` — Declaration

Reset posteriors to priors (preserves event history).

| Param        | Type   | Required | Notes                                                         |
| ------------ | ------ | -------- | ------------------------------------------------------------- |
| `profile_id` | string | no       | Must exist and be active. Defaults to `"balanced-recall-v1"`. |

```
request(ops="brain.reset(profile_id=\"implementer-recall-v1\")")
```

### `brain.feedback` — Commissive

Emit a `FeedbackExplicit` event into the shared log.

| Param                  | Type   | Required | Notes                                                                                                      |
| ---------------------- | ------ | -------- | ---------------------------------------------------------------------------------------------------------- |
| `target_id`            | uuid   | yes      | Memory note or entity the feedback applies to.                                                             |
| `signal`               | string | yes      | Same signal set as `memory.feedback`.                                                                      |
| `served_by_profile_id` | string | no       | Profile that served the rated result.                                                                      |
| `section_signals`      | object | no       | Per-section signals for `knowledge_compose` profiles: `{"section_name": "useful"\|"not_useful"\|"wrong"}`. |
| `scorer_run_id`        | string | no       | ADR-081 scorer-pass id; must pair with `serve_ledger_id`.                                                  |
| `serve_ledger_id`      | string | no       | ADR-081 `brain_serve_ledger` row id; must pair with `scorer_run_id`.                                       |

```
request(ops="brain.feedback(target_id=\"<uuid>\", signal=\"useful\")")
```

### `brain.auto_feedback` — Commissive

Emit implicit feedback for recall results supplied by an agent — the convenience verb
to call right after `memory.recall` instead of hand-building `brain.feedback`.

| Param                  | Type   | Required | Notes                                                                 |
| ---------------------- | ------ | -------- | --------------------------------------------------------------------- |
| `query`                | string | yes      | The recall query that produced the results.                           |
| `results`              | array  | yes      | Recall result objects; the first object's `id` is credited.           |
| `signal`               | string | no       | Defaults to `implicit_positive`.                                      |
| `served_by_profile_id` | string | no       | Profile that served the recall.                                       |
| `scorer_run_id`        | string | no       | Forwarded verbatim to `brain.feedback`; pairs with `serve_ledger_id`. |
| `serve_ledger_id`      | string | no       | Forwarded verbatim to `brain.feedback`; pairs with `scorer_run_id`.   |

```
request(ops="memory.recall(query=\"x\", limit=5) | brain.auto_feedback(query=\"x\", results=[{\"id\": \"$prev.items[0].id\"}])")
```

### `brain.bind` — Declaration

Write a row in the profile resolution table.

| Param           | Type    | Required | Notes                                        |
| --------------- | ------- | -------- | -------------------------------------------- |
| `profile_id`    | string  | yes      | Must exist.                                  |
| `actor`         | string  | no       | Default `*` (all actors).                    |
| `namespace`     | string  | no       | Default `*` (all namespaces).                |
| `consumer_kind` | string  | no       | Default `*` (all kinds).                     |
| `priority`      | integer | no       | Higher wins on multiple matches (default 0). |

```
request(ops="brain.bind(profile_id=\"implementer-recall-v1\", actor=\"role:implementer\")")
```

### `brain.unbind` — Declaration

Remove rows from the profile resolution table. At least one filter is required.

| Param           | Type   | Required | Notes                            |
| --------------- | ------ | -------- | -------------------------------- |
| `profile_id`    | string | no       | AND-combined with other filters. |
| `actor`         | string | no       |                                  |
| `namespace`     | string | no       |                                  |
| `consumer_kind` | string | no       |                                  |

```
request(ops="brain.unbind(actor=\"role:implementer\")")
```

### `brain.bindings` — Assertive

List rows in the profile resolution table, optionally filtered.

| Param           | Type   | Required | Notes |
| --------------- | ------ | -------- | ----- |
| `profile_id`    | string | no       |       |
| `actor`         | string | no       |       |
| `namespace`     | string | no       |       |
| `consumer_kind` | string | no       |       |

```
request(ops="brain.bindings(consumer_kind=\"recall\")")
```

### `brain.create_profile` — Declaration

Create a new brain profile with a given name and optional seed priors.

| Param           | Type   | Required | Notes                                                                                                                                                               |
| --------------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `name`          | string | yes      | Profile ID (alphanumeric + hyphens), must be unique.                                                                                                                |
| `description`   | string | no       | Human-readable description.                                                                                                                                         |
| `consumer_kind` | string | no       | Default `"recall"`.                                                                                                                                                 |
| `seed_priors`   | object | no       | For `knowledge_compose`: `{"section_posteriors": {"overview": {"alpha": 2.0, "beta": 2.0}, ...}}`; for `recall`: `{"relevance": {"alpha": 7.0, "beta": 3.0}, ...}`. |

```
request(ops="brain.create_profile(name=\"implementer-recall-v2\", consumer_kind=\"recall\")")
```

### `brain.register_adapter` — Declaration

Register an adapter integrity record so the router only composes adapters matching the
active base model revision.

| Param                 | Type   | Required | Notes                                                       |
| --------------------- | ------ | -------- | ----------------------------------------------------------- |
| `adapter_id`          | string | yes      | Stable adapter identifier (used as the entity name).        |
| `content_hash`        | string | yes      | Content hash of the adapter weights.                        |
| `base_model_revision` | string | yes      | Must match the active revision or registration is rejected. |
| `metadata`            | object | no       | Merged into entity properties.                              |

```
request(ops="brain.register_adapter(adapter_id=\"lora-v3\", content_hash=\"<sha256>\", base_model_revision=\"2026-07-01\")")
```

---

## `comm` pack — 7 verbs

Actor-to-actor messaging with threading. Optional; load with `KHIVE_PACKS=kg,comm`.

### `comm.send` — Commissive

Send a message, optionally threaded.

| Param       | Type   | Required | Notes                                                                                                          |
| ----------- | ------ | -------- | -------------------------------------------------------------------------------------------------------------- |
| `to`        | string | yes      | Actor label, e.g. `"lambda:leo"`. Both copies land in the caller's namespace; no cross-namespace write occurs. |
| `content`   | string | yes      | Non-empty message body.                                                                                        |
| `subject`   | string | no       | Optional subject line.                                                                                         |
| `thread_id` | uuid   | no       | Groups the message into an existing thread.                                                                    |

```
request(ops="comm.send(to=\"lambda:leo\", subject=\"PR ready\", content=\"#600 is open for review\")")
```

### `comm.inbox` — Assertive

List inbound messages for the caller.

| Param    | Type    | Required | Notes                              |
| -------- | ------- | -------- | ---------------------------------- |
| `limit`  | integer | no       | Default 20, max 200.               |
| `status` | string  | no       | `unread` (default)\|`read`\|`all`. |

```
request(ops="comm.inbox(limit=10)")
```

### `comm.read` — Declaration

Mark an inbound message as read. Outbound messages cannot be marked read.

| Param | Type   | Required | Notes                                              |
| ----- | ------ | -------- | -------------------------------------------------- |
| `id`  | string | yes      | 8-char prefix or full UUID of the inbound message. |

```
request(ops="comm.read(id=\"<message-id>\")")
```

### `comm.reply` — Commissive

Reply to a message, threading linkage.

| Param     | Type   | Required | Notes                                                       |
| --------- | ------ | -------- | ----------------------------------------------------------- |
| `id`      | string | yes      | 8-char prefix or full UUID of the message being replied to. |
| `content` | string | yes      | Non-empty reply body.                                       |

```
request(ops="comm.reply(id=\"<message-id>\", content=\"On it.\")")
```

### `comm.thread` — Assertive

Retrieve all messages in a conversation thread, ordered chronologically.

| Param   | Type    | Required | Notes                                                               |
| ------- | ------- | -------- | ------------------------------------------------------------------- |
| `id`    | string  | yes      | Thread root: 8-char prefix or full UUID of the originating message. |
| `limit` | integer | no       | Default 100, max 500.                                               |

```
request(ops="comm.thread(id=\"<thread-root-id>\")")
```

### `comm.probe` — Assertive

Strictly read-only poll for new inbound message metadata and a stale-unread count. No
read-flag mutation, no writes: designed for monitors polling every ~30 seconds, served by
a single cheap indexed query. Returns a `cursor_us` high-water mark, a `stale_unread_count`
of inbound messages unread past the staleness window, and a `new_messages` array of up to
100 inbound rows `{id, created_at_us, from_actor, subject?}` newer than `since_us`.

`cursor_us`/`since_us` is an opaque, monotonically increasing token, not a Unix microsecond
timestamp: round-trip whatever the previous `comm.probe` response returned as the next
call's `since_us`, and omit it for a baseline-first probe.

| Param           | Type    | Required | Notes                                               |
| --------------- | ------- | -------- | --------------------------------------------------- |
| `actor`         | string  | yes      | Actor label whose inbound mail is probed.           |
| `since_us`      | integer | no       | Opaque cursor from a prior response's `cursor_us`.  |
| `stale_minutes` | integer | no       | Staleness window for the unread count (default 20). |

```
request(ops="comm.probe(actor=\"lambda:leo\")")
request(ops="comm.probe(actor=\"lambda:leo\", since_us=42)")
```

### `comm.health` — Assertive

Read-only per-channel health snapshot. Returns the daemon-persisted heartbeat row for
every known channel: timestamps and consecutive-failure counts only, never a computed
healthy bool. Health judgment belongs to the caller. Rows are read from the pinned
operational namespace (`local`) unconditionally, regardless of the caller's dispatch
namespace. An empty `channels` array cannot distinguish "no daemon running" from
"channels configured but never polled". See the
[communication guide](communication.md) for the full response contract.

No parameters.

```
request(ops="comm.health()")
```

---

## `schedule` pack — 4 verbs

Time-triggered reminders and deferred verb dispatch. Optional; load with
`KHIVE_PACKS=kg,schedule`.

### `schedule.remind` — Commissive

Create a time-triggered reminder.

| Param     | Type   | Required | Notes                                                                                                                                            |
| --------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `content` | string | yes      | Non-empty reminder message.                                                                                                                      |
| `at`      | string | yes      | RFC 3339 trigger time, e.g. `"2026-06-01T09:00:00Z"`.                                                                                            |
| `repeat`  | string | no       | `daily`\|`weekly`\|`monthly`, or a limited 5-field cron form using only `*` or one in-range integer per field (steps/ranges/lists not accepted). |

```
request(ops="schedule.remind(content=\"check PR #600 CI\", at=\"2026-07-05T09:00:00Z\")")
```

### `schedule.schedule` — Commissive

Schedule a future verb dispatch.

| Param    | Type   | Required | Notes                                                               |
| -------- | ------ | -------- | ------------------------------------------------------------------- |
| `action` | string | yes      | Verb dispatch payload, e.g. `"schedule.remind(content=\"hello\")"`. |
| `at`     | string | yes      | RFC 3339 trigger time.                                              |
| `repeat` | string | no       | Same recurrence grammar as `schedule.remind`.                       |

```
request(ops="schedule.schedule(action=\"gtd.next(assignee=\\\"agent:docs\\\")\", at=\"2026-07-05T09:00:00Z\")")
```

### `schedule.agenda` — Assertive

List upcoming scheduled events.

| Param   | Type    | Required | Notes                                                                 |
| ------- | ------- | -------- | --------------------------------------------------------------------- |
| `from`  | string  | no       | RFC 3339 window start; omit to start from the earliest pending event. |
| `to`    | string  | no       | RFC 3339 window end; omit for all future events.                      |
| `limit` | integer | no       | Default 20, max 200.                                                  |

```
request(ops="schedule.agenda(limit=10)")
```

### `schedule.cancel` — Declaration

Cancel a scheduled event.

| Param | Type   | Required | Notes                             |
| ----- | ------ | -------- | --------------------------------- |
| `id`  | string | yes      | Full UUID of the scheduled event. |

```
request(ops="schedule.cancel(id=\"<event-id>\")")
```

---

## `knowledge` pack — 19 verbs

The knowledge-atom corpus: bulk ingest, TF-IDF + embedding search, domain composition,
section-level review/dispute, and KG-sugar verbs for citing sources. Optional; load
with `KHIVE_PACKS=kg,knowledge`.

### `knowledge.upsert_atoms` — Commissive

Bulk insert or update knowledge atoms by slug.

| Param        | Type            | Required | Notes                                                             |
| ------------ | --------------- | -------- | ----------------------------------------------------------------- |
| `atoms`      | array\<object\> | yes      | `{slug, name, content, tags?, properties?, finalized?}` per atom. |
| `chunk_size` | integer         | no       | Client-side chunking hint, max 5000.                              |

```
request(ops="[{\"tool\":\"knowledge.upsert_atoms\",\"args\":{\"atoms\":[{\"slug\":\"rope\",\"name\":\"RoPE\",\"content\":\"Rotary position embedding...\"}]}}]")
```

### `knowledge.upsert_domains` — Commissive

Bulk insert or update domain groupings of atoms.

| Param     | Type            | Required | Notes                                                     |
| --------- | --------------- | -------- | --------------------------------------------------------- |
| `domains` | array\<object\> | yes      | `{slug, name, description?, tags?, members?}` per domain. |

```
request(ops="[{\"tool\":\"knowledge.upsert_domains\",\"args\":{\"domains\":[{\"slug\":\"attention\",\"name\":\"Attention mechanisms\"}]}}]")
```

### `knowledge.get` — Assertive

Fetch a single atom or domain by UUID or slug.

| Param              | Type   | Required | Notes                                                                                                                                                                                                                                                                           |
| ------------------ | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`               | string | yes      | Atom/domain UUID or slug.                                                                                                                                                                                                                                                       |
| `include_sections` | bool   | no       | Include the atom's sections under a `sections` key (ignored for domains). Each section: `id, atom_id, namespace, section_type, heading, content, content_hash, status, tokens, sort_order, created_at, updated_at`, ordered by `sort_order`, `created_at`, `id`. Default false. |

```
request(ops="knowledge.get(id=\"rope\", include_sections=true)")
```

### `knowledge.list` — Assertive

Paginated listing of atoms or domains.

| Param    | Type    | Required | Notes                              |
| -------- | ------- | -------- | ---------------------------------- |
| `type`   | string  | no       | `atom`\|`domain` (default `atom`). |
| `limit`  | integer | no       | Default 20, max 500.               |
| `offset` | integer | no       | Pagination offset.                 |

```
request(ops="knowledge.list(type=\"domain\", limit=50)")
```

### `knowledge.delete_atoms` — Commissive

Soft-delete atoms by slug or ID.

| Param | Type            | Required | Notes                |
| ----- | --------------- | -------- | -------------------- |
| `ids` | array\<string\> | yes      | Atom slugs or UUIDs. |

```
request(ops="knowledge.delete_atoms(ids=[\"stale-atom-slug\"])")
```

### `knowledge.stats` — Assertive

Corpus statistics: atom count, domain count, coverage. No params.

```
request(ops="knowledge.stats()")
```

### `knowledge.index` — Commissive

Backfill embeddings + FTS for atoms/domains.

| Param         | Type            | Required | Notes                                                   |
| ------------- | --------------- | -------- | ------------------------------------------------------- |
| `ids`         | array\<string\> | no       | Atom slugs/IDs to index; omit to index all.             |
| `batch_size`  | integer         | no       | Default 500, max 1000.                                  |
| `insert_only` | bool            | no       | Deprecated no-op, accepted for API compatibility only.  |
| `rebuild_ann` | bool            | no       | Rebuild the in-memory Vamana ANN index (default false). |

```
request(ops="knowledge.index(rebuild_ann=true)")
```

### `knowledge.fold` — Assertive

Budget-constrained knapsack selection of scored candidates.

| Param              | Type            | Required | Notes                                                   |
| ------------------ | --------------- | -------- | ------------------------------------------------------- |
| `candidates`       | array\<object\> | yes      | `{id, score, size, content?, category?}` per candidate. |
| `budget`           | integer         | yes      | Token/size budget for the selected set.                 |
| `min_score`        | number          | no       | Default 0.0.                                            |
| `category_weights` | object          | no       | Per-category score multipliers.                         |

```
request(ops="[{\"tool\":\"knowledge.fold\",\"args\":{\"candidates\":[{\"id\":\"a\",\"score\":0.8,\"size\":400}],\"budget\":4000}}]")
```

### `knowledge.search` — Assertive

TF-IDF ranked search over the knowledge corpus with embedding rerank (default when an
embedder is configured). Draft and deprecated atoms are excluded by default. Score
bands: `score>=0.46` reliably on-target, `0.42<=score<0.46` mixed quality, `score<0.42`
mostly off-target.

| Param                 | Type    | Required | Notes                                                                                   |
| --------------------- | ------- | -------- | --------------------------------------------------------------------------------------- |
| `query`               | string  | yes      | Search query text.                                                                      |
| `type`                | string  | no       | `atom`\|`domain` (default both).                                                        |
| `include_drafts`      | bool    | no       | Default false; no-op when `status` is set.                                              |
| `status`              | string  | no       | Exact status filter: `draft`\|`reviewed`\|`deprecated`; overrides `include_drafts`.     |
| `exclude_status`      | string  | no       | Exclude an exact status; only used when `status` unset.                                 |
| `role`                | string  | no       | Agent role hint, prepended to the query for scoring.                                    |
| `limit`               | integer | no       | Default 10, max 100.                                                                    |
| `min_score`           | number  | no       | Default 0.0.                                                                            |
| `weights`             | object  | no       | `{w_name, w_tags, w_content, w_exact_name, w_bigram, expand_discount, coverage_alpha}`. |
| `decompose`           | bool    | no       | Default false; enables query decomposition.                                             |
| `decompose_threshold` | integer | no       | Default 4 non-stop terms to trigger decomposition.                                      |
| `intersection_bonus`  | number  | no       | Default 0.25; score multiplier for multi-sub-query hits.                                |
| `rerank`              | bool    | no       | Default true; embedding rerank; no-op with no embedder configured.                      |
| `rerank_alpha`        | number  | no       | Default 0.7 (TF-IDF-dominant blend).                                                    |

```
request(ops="knowledge.search(query=\"FastAPI JWT middleware\", rerank=true, limit=10)")
```

### `knowledge.suggest` — Assertive

Suggest relevant knowledge domains for a query. Draft/deprecated domain atoms excluded
by default.

| Param   | Type    | Required | Notes                   |
| ------- | ------- | -------- | ----------------------- |
| `query` | string  | yes      | Orientation query text. |
| `role`  | string  | no       | Agent role hint.        |
| `limit` | integer | no       | Default 8, max 100.     |

```
request(ops="knowledge.suggest(query=\"async middleware retry circuit breaker patterns\", role=\"implementer\")")
```

### `knowledge.compose` — Assertive

Compose a markdown briefing from selected knowledge domains and atoms.

| Param        | Type            | Required | Notes                                             |
| ------------ | --------------- | -------- | ------------------------------------------------- |
| `domain_ids` | array\<string\> | no       | Domain UUIDs/slugs whose member atoms to include. |
| `atom_ids`   | array\<string\> | no       | Atom UUIDs/slugs to include directly.             |
| `query`      | string          | yes      | Reranks the selected atom bodies.                 |

```
request(ops="knowledge.compose(query=\"FastAPI JWT middleware validation patterns\", domain_ids=[\"attention\"])")
```

### `knowledge.edit` — Commissive

Upsert sections for an atom without wiping other sections.

| Param      | Type            | Required | Notes                                                                                                                                                                                                                                                                             |
| ---------- | --------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`       | string          | yes      | Atom UUID or slug.                                                                                                                                                                                                                                                                |
| `sections` | array\<object\> | yes      | `[{section_type, content, heading?, sort_order?}]`. `section_type` is a closed enum: `overview`\|`core_model`\|`boundary_conditions`\|`formalism`\|`operational_guidance`\|`examples`\|`failure_modes`\|`expert_lens`\|`references`\|`other`. `content` must be >= 80 characters. |

```
request(ops="[{\"tool\":\"knowledge.edit\",\"args\":{\"id\":\"rope\",\"sections\":[{\"section_type\":\"overview\",\"content\":\"Rotary position embedding rotates query/key vectors by an angle proportional to position...\"}]}}]")
```

### `knowledge.import` — Commissive

Ingest atlas markdown file(s) as atoms with parsed sections.

| Param            | Type   | Required | Notes                                                                         |
| ---------------- | ------ | -------- | ----------------------------------------------------------------------------- |
| `path`           | string | yes      | Filesystem path to a markdown file or directory.                              |
| `format`         | string | no       | Only `atlas_md` supported (default).                                          |
| `chunk_strategy` | string | no       | `section` (default, one section per atom) or `atom` (whole file as one atom). |

```
request(ops="knowledge.import(path=\"/path/to/atlas/rope.md\")")
```

### `knowledge.challenge` — Commissive

Mark a section as disputed and increment the atom's `dispute_count`.

| Param          | Type   | Required | Notes                                                             |
| -------------- | ------ | -------- | ----------------------------------------------------------------- |
| `atom_id`      | string | yes      | Atom UUID or slug.                                                |
| `section_type` | string | yes      | Section type to challenge.                                        |
| `content_hash` | string | no       | Required when more than one eligible section of that type exists. |
| `reason`       | string | no       | Optional challenge reason.                                        |

```
request(ops="knowledge.challenge(atom_id=\"rope\", section_type=\"formalism\", reason=\"formula sign error\")")
```

### `knowledge.adjudicate` — Commissive

Resolve a disputed section and decrement the atom's `dispute_count`.

| Param          | Type   | Required | Notes                                                             |
| -------------- | ------ | -------- | ----------------------------------------------------------------- |
| `atom_id`      | string | yes      | Atom UUID or slug.                                                |
| `section_type` | string | yes      | Section type to adjudicate.                                       |
| `content_hash` | string | no       | Required when more than one disputed section of that type exists. |
| `resolution`   | string | yes      | `accept` (marks verified) or `reject` (marks reviewed).           |

```
request(ops="knowledge.adjudicate(atom_id=\"rope\", section_type=\"formalism\", resolution=\"accept\")")
```

### `knowledge.learn` — Commissive

Register a concept entity with optional domain and tags.

| Param         | Type            | Required | Notes                            |
| ------------- | --------------- | -------- | -------------------------------- |
| `name`        | string          | yes      | Concept name.                    |
| `description` | string          | no       | Optional description.            |
| `domain`      | string          | no       | Folded into `properties.domain`. |
| `tags`        | array\<string\> | no       | Optional tag list.               |

```
request(ops="knowledge.learn(name=\"GQA\", domain=\"attention\", description=\"Grouped-query attention\")")
```

### `knowledge.cite` — Commissive

Link a concept to the paper or source that introduced it.

| Param        | Type  | Required | Notes                                                                                                |
| ------------ | ----- | -------- | ---------------------------------------------------------------------------------------------------- |
| `concept_id` | uuid  | yes      | Concept entity ID.                                                                                   |
| `source_id`  | uuid  | yes      | Source entity ID; must be `kind=document`, `kind=person`, or `kind=org` (`introduced_by` edge rule). |
| `weight`     | float | no       | Defaults to 1.0.                                                                                     |

```
request(ops="knowledge.cite(concept_id=\"<concept-uuid>\", source_id=\"<paper-uuid>\")")
```

### `knowledge.topic` — Assertive

List concepts filtered by domain or free-text query.

| Param    | Type    | Required | Notes                                       |
| -------- | ------- | -------- | ------------------------------------------- |
| `domain` | string  | no       | Filter to concepts tagged with this domain. |
| `query`  | string  | no       | Free-text search across name + description. |
| `limit`  | integer | no       | Default 20, max 100.                        |

```
request(ops="knowledge.topic(domain=\"attention\")")
```

### `knowledge.feedback` — Commissive

Apply per-section feedback signals to update section posterior weights.

| Param             | Type   | Required | Notes                                                                                                                         |
| ----------------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `section_signals` | object | yes      | `{section_type: signal}`, e.g. `{"overview": "useful", "formalism": "not_useful"}`. Signals: `useful`\|`not_useful`\|`wrong`. |
| `target_id`       | string | no       | UUID of the rated atom/entity. When paired with a configured brain profile, also forwards to `brain.feedback`.                |

```
request(ops="knowledge.feedback(target_id=\"rope\", section_signals={\"overview\": \"useful\"})")
```

---

## `session` pack — 4 verbs

Cross-provider agent-session continuity records. Optional; load with
`KHIVE_PACKS=kg,session`.

### `session.store` — Directive

Persist an agent-session record as a session note.

| Param                 | Type            | Required | Notes                                                  |
| --------------------- | --------------- | -------- | ------------------------------------------------------ |
| `content`             | string          | yes      | Verbatim transcript or summary content.                |
| `title`               | string          | no       | Stored as `note.name`.                                 |
| `provider`            | string          | no       | Provider label, e.g. `codex`, `claude_code`, `openai`. |
| `provider_session_id` | string          | no       | Provider-native continuity anchor.                     |
| `tags`                | array\<string\> | no       | Stored in `properties.tags`.                           |

```
request(ops="session.store(content=\"...\", provider=\"claude_code\", title=\"pages revamp session\")")
```

### `session.list` — Assertive

List stored sessions newest first.

| Param      | Type    | Required | Notes                                  |
| ---------- | ------- | -------- | -------------------------------------- |
| `limit`    | integer | no       | 1–200, default 20.                     |
| `offset`   | integer | no       | Default 0.                             |
| `provider` | string  | no       | Exact filter on `properties.provider`. |

```
request(ops="session.list(provider=\"claude_code\", limit=10)")
```

### `session.resume` — Assertive

Fetch one session's full content by UUID or 8+ hex prefix.

| Param | Type   | Required | Notes                             |
| ----- | ------ | -------- | --------------------------------- |
| `id`  | string | yes      | Full UUID or 8+ hex short prefix. |

```
request(ops="session.resume(id=\"<session-id>\")")
```

### `session.export` — Assertive

Serialize one stored session as json or markdown.

| Param    | Type   | Required | Notes                               |
| -------- | ------ | -------- | ----------------------------------- |
| `id`     | string | yes      | Full UUID or 8+ hex short prefix.   |
| `format` | string | no       | `json`\|`markdown`, default `json`. |

```
request(ops="session.export(id=\"<session-id>\", format=\"markdown\")")
```

---

## `git` pack — 1 verb

Batch, cursor-based git-history ingester (ADR-088, ADR-088 Amendment 1). Optional; load
with `KHIVE_PACKS=kg,git`. Also registers the `commit` / `issue` / `pull_request` note
kinds, used by `git.digest` below and by the `kkernel git-ingest` CLI (both drive the
same underlying ingest core, so ingest enrichment — readable `name`s, `Closes #N`
reference edges, parent→child commit `precedes` edges — applies identically either way).

### `git.digest` — Commissive

Walk a local repository path or clone/fetch a remote `https://` URL, then ingest commits
and (when the source is a github.com repo and the `gh` CLI is available) issues and pull
requests as provenance notes, resolving or auto-creating the repo-anchor `project` entity.
Bounded and cursor-resumable: call again with the same `source`/`project` while the
response's `done` field is `false`.

| Param       | Type            | Required | Notes                                                                                                                                                                                                                                     |
| ----------- | --------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `source`    | string          | yes      | A local filesystem path (must contain `.git`) or an `https://` URL. Any `https` host is accepted; non-github.com hosts degrade to commits-only. `ssh://`, `git://`, `http://`, and scp-shorthand (`user@host:path`) sources are rejected. |
| `project`   | string          | no       | UUID or 8+ hex prefix of the repo-anchor `project` entity. When absent, resolved by matching `properties.repo_url` or `name`, or created if none is found (see the response's `project_id` and `project_created`).                        |
| `max_items` | integer         | no       | Bounded work for this call, counted across commits + issues + PRs (default 500, clamped to 1..=2000). Cursor-resumable: call again while the response's `done` field is `false`.                                                          |
| `include`   | array\<string\> | no       | Which record kinds to ingest this call: any of `commits` \| `issues` \| `pull_requests` (default: all three).                                                                                                                             |

```
request(ops="git.digest(source=\"https://github.com/org/repo\", max_items=500)")
```

---

## Further reading

- [Getting Started](getting-started.html): install and connect an MCP client.
- [Knowledge Graph Modeling](knowledge-graph.html): entity kinds, edge relations, patterns.
- [Memory and Recall](memory.html): salience, decay, and recall internals.
- [Search and Retrieval](search.html): FTS, vector, hybrid fusion, reranking.
- [GTD Task Management](tasks.html): task lifecycle in depth.
- [Prompt Cookbook](prompt-cookbook.html): ready-to-use verb patterns.
- [ADR-016: request DSL](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)
- [ADR-002: edge ontology](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-002-edge-ontology.md)
