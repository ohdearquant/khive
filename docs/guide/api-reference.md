# API Reference

khive exposes exactly one MCP tool, `request`. Everything else, 19 verbs in the `kg`
pack, is dispatched through that single tool via a small request DSL. This page
documents the DSL grammar, the response envelope, and every verb's full parameter
contract, so an agent can call khive correctly without reading Rust source.

This page is verified against the live registry (`request(ops="verbs()")`) and the pack
source (`crates/khive-pack-kg/src/*.rs` `HandlerDef`/`ParamDef` struct literals). Verb
count: **19**, matching the live registry `total` field for the default `kg`-only pack
set. If your server reports a different total, your `KHIVE_PACKS` configuration loads
additional (commercially licensed) packs beyond the open-source default — run
`request(ops="verbs()")` against your own server to get the authoritative list.

An always-machine-readable copy of this page is at
[`/md/api-reference.md`](md/api-reference.md). The site also publishes
[`/llms.txt`](llms.txt) (a short index) and [`/llms-full.txt`](llms-full.txt)
(every guide page concatenated) for agents that prefer one fetch over several.

## Packs at a glance

| Pack | Verbs | Load with                  | Optional?           |
| ---- | ----- | -------------------------- | ------------------- |
| `kg` | 19    | `KHIVE_PACKS=kg` (default) | No — base substrate |

This distribution ships one production pack, `kg`, loaded by default. Task management,
memory, inter-agent communication, scheduling, session continuity, workspace linking,
blob storage, and the profile-oriented feedback/learning-loop pack (`brain.*`) are
provided by commercially licensed extensions and are not part of this distribution; when
installed, they load the same way, via `KHIVE_PACKS`/`--pack`. Git provenance ingestion
(`git.digest`, the `commit`/`issue`/`pull_request` note kinds) and the `git.commit` /
`git.branch` / `git.push` write verbs (ADR-108) are likewise a commercially licensed
extension. The code-quality and formal-methods ontology packs are also distributed as
commercially licensed extensions rather than as part of this repository.

The default binary (no `KHIVE_PACKS`/`--pack` override) loads the `kg` pack: **19 verbs**.

Verb names in the `kg` pack are bare (`create`, `search`, `link`, …). Extension packs
namespace their verbs with a `pack.` prefix (`gtd.assign`, `memory.recall`,
`comm.send`, `schedule.remind`, `session.store`).

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
request(ops="[search(kind=\"entity\", query=\"x\"), stats()]")
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

## `kg` pack — 19 verbs

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

| Param             | Type | Required | Notes                                                                 |
| ----------------- | ---- | -------- | --------------------------------------------------------------------- |
| `id`              | uuid | yes      | Full UUID or short hex prefix (min 8 chars).                          |
| `include_deleted` | bool | no       | Return soft-deleted records too (default false); full UUID or prefix. |

```
request(ops="get(id=\"3f2a9c1e\")")
```

The returned object has the full substrate shape documented under `list` below. For an edge,
`get` additionally returns `annotations: Note[]`. The array is always present (empty when no live
notes annotate the edge), and each full note object includes `annotation_edge_id`, the UUID of the
`annotates` edge connecting that note to the fetched edge. Because `get` is a by-ID operation,
annotation discovery is namespace-agnostic under ADR-007, matching the fetched edge itself.

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

Requests within the kind's server-side row cap keep the existing array response. If `limit`
exceeds the cap, the response is `{"items": [...], "requested_limit": N,
"effective_limit": CAP, "limit_clamped": true}`. This lets offset-based clients advance by
the effective limit instead of silently skipping rows. The caps are entity 500, note 200, edge
1000, event 1000, and proposal 500. Edge cursor mode keeps its existing `{"edges": [...],
"next_after": ...}` shape and adds the same limit metadata when clamped.

Row shape (each item in the array, or in `"items"`/`"edges"` when clamped) depends on `kind`.
For `kind="entity"`, `"note"`, `"edge"`, and `"event"`, the row is the **full stored record**
for that substrate, listed below in its **verbose** form (the shape returned with
`presentation="verbose"`, which is also the default for `kkernel exec` and the `khive` CLI).
This is the key difference from `search` and `neighbors` below, which both return narrow
projections regardless of presentation mode.

Every MCP call that omits `presentation` gets **Agent** mode instead (`list` is not on the
`AlwaysVerbose` verb list in `crates/khive-types/src/pack.rs`), which conditionally reshapes the
rows below (`crates/khive-runtime/src/presentation.rs`): a null field is dropped entirely rather
than returned as `null`, unless its name is on the lifecycle-preserve list (`deleted_at` among
others, but not `merged_into`/`merge_event_id`/`expires_at`); empty strings, arrays, and objects
are dropped; `id`/`source_id`/`target_id`/`merge_event_id` and other `_id`-suffixed fields are
shortened to an 8-character prefix; `created_at`/`updated_at`/`deleted_at` are
compacted to a relative or minute-truncated form; and `salience`/`decay_factor` are truncated to
3 significant figures. Pass `presentation="verbose"` to get the exact shapes below unconditionally.

- **`kind="entity"`**: `{id, namespace, kind, entity_type, name, description, properties, tags,
  created_at, updated_at, deleted_at, merged_into, merge_event_id, content_ref}`.
  `created_at`/`updated_at`/`deleted_at` are ISO-8601 strings (the store keeps them as
  epoch-microseconds internally; the handler converts before returning).
- **`kind="note"`**: `{id, namespace, kind, status, name, content, salience, decay_factor,
  expires_at, properties, created_at, updated_at, deleted_at}`. Notes have **no top-level
  `tags` field**: unlike entities, tags live inside `properties.tags`. If the note's
  `properties.status` is set (e.g. a task's lifecycle status, or a message's delivery
  state — fields set by note kinds that extension packs register), the row's
  substrate-level `status` (normally `"active"`) is renamed to `lifecycle`, and the
  top-level `status` is replaced with the `properties.status` value, so a consumer reads
  the pack-level status directly off the row instead of digging into `properties`. When no
  `properties.status` is set, `status` stays the raw substrate value and there is no
  `lifecycle` key.
- **`kind="edge"`**: `{id, namespace, source_id, target_id, relation, weight, created_at,
  updated_at, deleted_at, metadata, target_backend}`.
- **`kind="event"`**: `{id, namespace, verb, substrate, actor, kind, outcome, payload,
  payload_schema_version, profile_state_version, duration_us, target_id, session_id,
  aggregate_kind, aggregate_id, created_at}`.
- **`kind="proposal"`** is a supported `list` kind but is not a full stored record: it returns a
  purpose-built projection, `{id, proposer, title, status, created_at, updated_at, expiry,
  last_decision, review_count, approve_count, reject_count}` (built in
  `crates/khive-pack-kg/src/handlers/proposal.rs`). That field set is the
  `presentation="verbose"` projection; the default Agent mode applies the same generic
  reshaping as the other `list` rows: non-lifecycle null/empty fields are omitted (a null
  `expiry`, an empty `last_decision`), ids are shortened, and timestamps are compacted.

None of these match `search`'s `{id, entity_kind|note_kind, score, title, snippet}` rows or
`neighbors`'s flat `{origin_id, id, edge_id, relation, weight, name?, kind?, entity_type?}`
rows. `search` and `neighbors` are built for ranking and graph-walking, not display: fetch the
full record with `get(id=...)` (or `list`) when you need more than what they return.

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

For a symmetric-relation edge (`competes_with`, `composed_with`), setting `relation` or
`weight` can collide with an already-existing edge at the same canonical `(source, target,
relation)` triple. When that happens, the requested edge is dropped and the returned record
is the pre-existing survivor, left exactly as it was (ADR-039's edge-conflict contract is
`ON CONFLICT DO NOTHING` — the survivor's own attributes are never overwritten by the
discarded edge's patch). If that survivor was previously soft-deleted, the returned edge may
carry a non-null `deleted_at`: absorbing a conflicting update never resurrects a tombstone.

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

Response shape (`kind="entity"` rows, `presentation="verbose"`):

```json
[
  {
    "id": "3f2a9c1e-...",
    "entity_kind": "concept",
    "score": 0.0909,
    "title": "LoRA",
    "snippet": "matched text from the description/properties"
  }
]
```

`kind="note"` rows are identical except the kind field is named `note_kind` instead of
`entity_kind`. That kind field is present on every row in the verbose shape above but is `null`
in the rare case where the record was deleted between the search hit and the metadata lookup
that fills it in; `title`/`snippet` are `null` for the same reason, or when the underlying
FTS/vector hit carried no snippet text. `search` is not on the `AlwaysVerbose` verb list
(`crates/khive-types/src/pack.rs`), so a call that omits `presentation` gets Agent mode instead:
`entity_kind`/`note_kind`/`title`/`snippet` are omitted from the row entirely when `null` rather
than returned as `null` (they are not on the lifecycle-preserve list), and `id` is shortened to
an 8-character prefix (`crates/khive-runtime/src/presentation.rs`).

`score` is an implementation-defined ranking value, not a normalized 0.0-1.0 similarity, and its
construction differs by kind (see the `min_score` row above for typical magnitudes):

- **Entity** (`crates/khive-runtime/src/retrieval.rs`): each retrieval leg (lexical, vector) that
  returns the entity contributes `1 / (k + rank)` with `k = 10`; contributions from every leg
  that hit the entity are summed, then a flat `+0.5` boost is added when the entity's title is
  an exact case-insensitive match for the query. A single-leg rank-1 hit is `0.0909`; an entity
  hit by both legs at rank 1, with an exact title match, would score `1/11 + 1/11 + 0.5 ≈ 0.682`.
- **Note** (`crates/khive-runtime/src/operations.rs`): the same per-leg RRF sum, but with `k = 60`,
  is then multiplied by a salience-derived weight, `0.5 + 0.5 * salience` (salience defaults to
  `0.5` when unset), so the fused rank score is scaled down for low-salience notes and left
  closer to unscaled for high-salience ones.

This row shape never includes the full entity/note record (no `description`, `content`,
`properties`, `tags`, timestamps, …) in either presentation mode, only enough to rank and
identify the hit. It diverges from both `neighbors` and `list`'s row shapes above; see `list`'s
"Row shape" note above for the full comparison.

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

Each returned hit includes `origin_id`, the resolved queried node. This lets
batch callers verify that every result is associated with the submitted root.

| Param        | Type            | Required | Notes                                            |
| ------------ | --------------- | -------- | ------------------------------------------------ |
| `node_id`    | uuid            | yes      | Node whose neighbors to return.                  |
| `direction`  | string          | no       | `outgoing`\|`incoming`\|`both` (default `both`). |
| `relations`  | array\<string\> | no       | Restrict to these relation types.                |
| `min_weight` | number          | no       | Exclude edges below this weight.                 |

```
request(ops="neighbors(node_id=\"<uuid>\", direction=\"both\")")
```

Response shape:

```json
[
  {
    "origin_id": "<the queried node_id>",
    "id": "<neighbor node id>",
    "edge_id": "<uuid>",
    "relation": "extends",
    "weight": 0.9,
    "name": "LoRA",
    "kind": "concept",
    "entity_type": "paper"
  }
]
```

Flat rows, one per edge: never the neighbor's full entity/note record. `name`, `kind`, and
`entity_type` are filled in by a batch entity+note lookup performed after the graph query, and
are **omitted from the JSON entirely (not `null`)** when that lookup can't resolve the neighbor
id (a dangling/bogus id that never matched an entity or note row). Soft-deleted entity neighbors
are a separate case: the runtime filters them out before the response is built
(`crates/khive-runtime/src/operations.rs`, `neighbors_with_query`), so a soft-deleted neighbor
produces no row at all rather than a row with omitted fields. `neighbors` is `AlwaysVerbose`
(`crates/khive-types/src/pack.rs`), so this omission behavior is unconditional regardless of
`presentation`; `search`'s `entity_kind`/`note_kind` follows the opposite rule in verbose mode
(always present, only ever `null`), but is itself omitted-when-null under the default Agent mode.
`entity_type` is included only when `include_entity_type=true` was passed, and is never set for
a note neighbor (notes have no `entity_type`).

`kind` is overloaded: for an entity neighbor it is the entity's base kind (e.g. `concept`); for
a note neighbor it is the note's kind (e.g. `observation`); annotation edges routinely link an
entity to a note, so a `neighbors` result set can mix both. There is no separate field stating
which substrate a given neighbor belongs to; disambiguate by checking `kind` against the closed
entity-kind vocabulary (§"The 9 entity kinds" in AGENTS.md) vs. the note-kind vocabulary
(§"The 5 note kinds"), or call `get(id=...)` on the neighbor's `id`.

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

Entity-anchored graph context in one call (ADR-089).
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
them into separate `query()` calls. GQL string equality uses SQLite `COLLATE NOCASE`,
so `WHERE e.name = "LoRA"` matches both `LoRA` and ASCII case variants such as `lora`.

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
actor's recently-referenced ring; (3) a case-sensitive exact match on `entities.name`;
(4) hybrid search over the namespace, with vector hits below the server-side `0.3` raw
cosine-similarity floor discarded before RRF fusion. Returns one of
`Resolved{id,confidence}` | `Ambiguous{candidates}` | `NotFound` per ref — never a
silent pick among close candidates. Read-only: performs no mutation.

The id-string stage resolves entities only. Note, edge, and event UUIDs or hex prefixes
return `NotFound` through `resolve` under every `kind`; use `get` when the substrate should
be auto-detected. The similarity floor removes low-confidence ANN neighbors while preserving
lexical partial-name matches, whose RRF score encodes rank rather than textual relevance.

| Param   | Type            | Required | Notes                                                                                                           |
| ------- | --------------- | -------- | --------------------------------------------------------------------------------------------------------------- |
| `refs`  | array\<string\> | yes      | Natural-language references to resolve (UUID, hex prefix, exact entity name, or free text).                     |
| `kind`  | string          | no       | Restricts the exact-name and hybrid-search stages to an entity kind. No effect on the id-string or ring stages. |
| `limit` | integer         | no       | Max candidates returned per ref from the stage 4 hybrid-search fallback. Default 5, max 20.                     |

```
request(ops="resolve(refs=[\"the old record\", \"<uuid>\"])")
```

### `whoami` — Assertive

Report the caller's identity as the runtime already resolved it for this request:
`actor_id`, `actor_kind`, whether the actor is the unattributed/anonymous fallback,
the write namespace, and the read-visible namespace set. A projection of existing
per-request state, not new state; never returns tokens or credentials. Takes no
parameters.

```
request(ops="whoami()")
```

```json
{
  "actor_id": "local",
  "actor_kind": "anonymous",
  "unattributed": true,
  "namespace": "local",
  "visible_namespaces": ["local"]
}
```

### `verbs` — Assertive

List all MCP-callable verbs registered on this server. Internal subhandlers are
excluded.

| Param      | Type   | Required | Notes                                                                      |
| ---------- | ------ | -------- | -------------------------------------------------------------------------- |
| `category` | string | no       | Filter: `Assertive`\|`Commissive`\|`Declaration`\|`Directive`.             |
| `pack`     | string | no       | Filter by pack name (`kg` in this distribution; extension packs add more). |

```
request(ops="verbs()")
```

---

## Other packs

Task management (`gtd.*`), memory (`memory.*`), inter-agent communication (`comm.*`),
scheduling (`schedule.*`), session continuity (`session.*`), and content-addressed blob
storage (`blob.*`, ADR-111) are provided by commercially licensed extensions and are not
part of this distribution; when installed, they load the same way, via
`KHIVE_PACKS`/`--pack`.

---

## `git` pack

Git-history ingestion (`git.digest`) and the hardened write surface (`git.commit` /
`git.branch` / `git.push`, ADR-108), along with the `commit`/`issue`/`pull_request`
note kinds they register, are provided by a commercially licensed extension and are
not part of this distribution.

---

## Further reading

- [Getting Started](getting-started.html): install and connect an MCP client.
- [Knowledge Graph Modeling](knowledge-graph.html): entity kinds, edge relations, patterns.
- [Memory and Recall](memory.html): salience, decay, and recall internals.
- [Search and Retrieval](search.html): FTS, vector, hybrid fusion, reranking.
- [GTD Task Management](tasks.html): task lifecycle in depth.
- [Prompt Cookbook](prompt-cookbook.html): ready-to-use verb patterns.
- ADR-016: request DSL
- ADR-002: Closed Edge Ontology
