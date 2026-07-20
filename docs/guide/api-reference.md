# API Reference

khive exposes exactly one MCP tool, `request`. Everything else, 50 verbs across 9
production packs, is dispatched through that single tool via a small request DSL.
This page documents the DSL grammar, the response envelope, and every verb's full
parameter contract, so an agent can call khive correctly without reading Rust source.

This page is verified against the live registry (`request(ops="verbs()")`) and the pack
source (`crates/khive-pack-*/src/*.rs` `HandlerDef`/`ParamDef` struct literals). Verb
count: **50**, matching both the live registry `total` field and the sum of the 9 pack
counts below. If your server reports a different total, your `KHIVE_PACKS` configuration
loads a different pack set than the default — run `request(ops="verbs()")` against your
own server to get the authoritative list.

An always-machine-readable copy of this page is at
[`/md/api-reference.md`](md/api-reference.md). The site also publishes
[`/llms.txt`](llms.txt) (a short index) and [`/llms-full.txt`](llms-full.txt)
(every guide page concatenated) for agents that prefer one fetch over several.

## Packs at a glance

| Pack        | Verbs | Load with                                  | Optional?           |
| ----------- | ----- | ------------------------------------------ | ------------------- |
| `kg`        | 18    | `KHIVE_PACKS=kg` (default)                 | No — base substrate |
| `gtd`       | 5     | `KHIVE_PACKS=kg,gtd`                       | Yes                 |
| `memory`    | 5     | `KHIVE_PACKS=kg,memory`                    | Yes                 |
| `comm`      | 7     | `KHIVE_PACKS=kg,comm`                      | Yes                 |
| `schedule`  | 4     | `KHIVE_PACKS=kg,schedule`                  | Yes                 |
| `session`   | 4     | `KHIVE_PACKS=kg,session`                   | Yes                 |
| `git`       | 4     | `KHIVE_PACKS=kg,git`                       | Yes                 |
| `workspace` | 0     | `KHIVE_PACKS=kg,git,gtd,session,workspace` | Yes                 |
| `blob`      | 3     | `KHIVE_PACKS=kg,blob`                      | Yes                 |

`git` also registers the `commit` / `issue` / `pull_request` note kinds and the shared
`run_ingest` core (`crates/khive-pack-git/src/ingest.rs`) that both `git.digest` and the
`kkernel git-ingest` CLI drive. Its four verbs are `git.digest` (read/ingest) plus three
write verbs, `git.commit` / `git.branch` / `git.push` (ADR-108), that shell to system git
with hardened, allowlisted argv construction.

`workspace` requires `kg`, `git`, `gtd`, and `session` to be loaded alongside it (the runtime rejects a pack set that omits a declared dependency), so its minimal example lists all four.

`schedule` requires `kg`. `schedule.remind` additionally requires `comm.send` at
creation time and persists nothing when that delivery capability is absent; the other
three schedule verbs remain available without `comm`.

`blob` registers no note or entity kinds; its three verbs (`blob.put` / `blob.get` /
`blob.stat`) dispatch over the `BlobStore` content-addressed storage trait (ADR-111). A
normal file-backed boot installs a default `FsBlobStore` rooted beside the database file
even with no `[storage.blob]` section and no `KHIVE_BLOB_ROOT` set; the verbs only stay
unconfigured (erroring until a backend is installed) when the server boots against an
in-memory backend, which has no directory to default a root beside.

The default binary (no `KHIVE_PACKS`/`--pack` override) loads all 9 packs: 18 + 5 + 5 +
7 + 4 + 4 + 4 + 0 + 3 = **50 verbs**.

Verb names in the `kg` pack are bare (`create`, `search`, `link`, …). Every other pack
namespaces its verbs with a `pack.` prefix (`gtd.assign`, `memory.recall`,
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
  `properties.status` is set (e.g. a `gtd` task's lifecycle status, or a `comm` message's
  delivery state), the row's substrate-level `status` (normally `"active"`) is renamed to
  `lifecycle`, and the top-level `status` is replaced with the `properties.status` value,
  so a `gtd`/`comm` consumer reads the pack-level status directly off the row instead of
  digging into `properties`. When no `properties.status` is set, `status` stays the raw
  substrate value and there is no `lifecycle` key.
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
(4) hybrid search over the namespace. Returns one of
`Resolved{id,confidence}` | `Ambiguous{candidates}` | `NotFound` per ref — never a
silent pick among close candidates. Read-only: performs no mutation.

| Param   | Type            | Required | Notes                                                                                                           |
| ------- | --------------- | -------- | --------------------------------------------------------------------------------------------------------------- |
| `refs`  | array\<string\> | yes      | Natural-language references to resolve (UUID, hex prefix, exact entity name, or free text).                     |
| `kind`  | string          | no       | Restricts the exact-name and hybrid-search stages to an entity kind. No effect on the id-string or ring stages. |
| `limit` | integer         | no       | Max candidates returned per ref from the stage 4 hybrid-search fallback. Default 5, max 20.                     |

```
request(ops="resolve(refs=[\"the old record\", \"<uuid>\"])")
```

### `verbs` — Assertive

List all MCP-callable verbs registered on this server. Internal subhandlers are
excluded.

| Param      | Type   | Required | Notes                                                                              |
| ---------- | ------ | -------- | ---------------------------------------------------------------------------------- |
| `category` | string | no       | Filter: `Assertive`\|`Commissive`\|`Declaration`\|`Directive`.                     |
| `pack`     | string | no       | Filter by pack name (`kg`, `gtd`, `memory`, `comm`, `schedule`, `session`, `git`). |

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

The `brain` pack (`brain.*` verbs — recall-tuning profiles, Beta-posterior scoring,
feedback-driven ranking) is a commercially licensed extension distributed separately;
it is not part of this distribution.

## `comm` pack — 7 verbs

Actor-to-actor messaging with threading. Optional; load with `KHIVE_PACKS=kg,comm`.

### `comm.send` — Commissive

Send a message, optionally threaded.

| Param       | Type   | Required | Notes                                                                                                                                                                                           |
| ----------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `to`        | string | yes      | Actor label, e.g. `"lambda:leo"`. Both copies land in the caller's namespace; no cross-namespace write occurs.                                                                                  |
| `content`   | string | yes      | Non-empty message body.                                                                                                                                                                         |
| `subject`   | string | no       | Optional subject line.                                                                                                                                                                          |
| `thread_id` | uuid   | no       | Groups the message into an existing thread.                                                                                                                                                     |
| `self_send` | bool   | no       | Default false. Required when `to` matches the configured sender actor; otherwise the send is rejected. The anonymous `local` fallback is exempt. Use true only for an intentional note to self. |

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
healthy bool. Health judgment belongs to the caller. Rows are read from the caller's
injected namespace (`namespace=`, defaulting to `local` like every other comm verb) —
`comm.heartbeat` is the only handler pinned to the fixed `local` operational namespace.
The response echoes the namespace actually read in a `namespace` field, so an empty
`channels` array is unambiguous even under a scoped read. See the
[communication guide](communication.md) for the full response contract.

No parameters.

```
request(ops="comm.health()")
```

---

## `schedule` pack — 4 verbs

Time-triggered reminders and deferred verb dispatch. Optional; load with
`KHIVE_PACKS=kg,schedule`. Add `comm` to create reminders.

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

## `git` pack — 4 verbs

Git-history ingester plus a hardened write surface (ADR-088, ADR-088 Amendment 1,
ADR-108). Optional; load with `KHIVE_PACKS=kg,git`. Also registers the `commit` /
`issue` / `pull_request` note kinds, used by `git.digest` below and by the `kkernel
git-ingest` CLI (both drive the same underlying ingest core, so ingest enrichment —
readable `name`s, `Closes #N` reference edges, parent→child commit `precedes` edges —
applies identically either way).

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

### `git.commit` / `git.branch` / `git.push` — Commissive (ADR-108)

Thin write verbs that shell to system git (`std::process::Command::args`, no shell
interpolation). Branch/ref names, remotes, messages, and authors are validated before they
enter fixed argv shapes. Commit paths are bounded, repository-relative, traversal-free,
and internally converted to Git literal pathspecs, so characters such as `*`, `?`, brackets,
Unicode, and caller text such as `:(top)` remain literal filename text. `force` on
`git.push` is always rejected when `true` — no policy or argument combination authorizes a
force-push through this surface.

The handler-level `[git_write]` allowlist is mandatory and independent of Gate policy
(ADR-018). With no `[[git_write.allowed]]` entries, all three write verbs deny every request,
including under `AllowAllGate`. Repository paths are compared after canonicalization, so an
entry names exactly one real repository; branch patterns are exact names or a glob containing
at most one `*` wildcard.

```toml
[[git_write.allowed]]
repo = "/abs/path/repo"
branches = ["main", "feat/*", "release-*"]
```

| Verb         | Param     | Type            | Required | Notes                                                                                                                                                                          |
| ------------ | --------- | --------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `git.commit` | `repo`    | string          | yes      | Absolute local path to a git repository (must contain a `.git` entry).                                                                                                         |
|              | `message` | string          | yes      | Commit message, passed as a single `-m` argument value.                                                                                                                        |
|              | `paths`   | array\<string\> | no       | Relative paths to stage and scope the commit to. Absent commits everything currently staged/modified in tracked files (`git commit -a`) — never auto-adds new untracked files. |
|              | `author`  | string          | no       | Override the commit author, e.g. `"Name <email>"`.                                                                                                                             |
| `git.branch` | `repo`    | string          | yes      | Same as above.                                                                                                                                                                 |
|              | `name`    | string          | yes      | New branch name.                                                                                                                                                               |
|              | `from`    | string          | no       | Ref or SHA to branch from. Absent uses the repo's current HEAD.                                                                                                                |
| `git.push`   | `repo`    | string          | yes      | Same as above.                                                                                                                                                                 |
|              | `branch`  | string          | yes      | Branch to push.                                                                                                                                                                |
|              | `remote`  | string          | no       | Remote to push to (default `origin`).                                                                                                                                          |
|              | `force`   | bool            | no       | Always rejected when `true` (ADR-108 hard rule 1) — present only so an explicit `force=true` request fails loudly instead of being silently ignored.                           |

```
request(ops="git.commit(repo=\"/abs/path/repo\", message=\"fix: thing\") | git.push(repo=\"/abs/path/repo\", branch=\"main\")")
```

---

## `blob` pack — 3 verbs

Content-addressed binary object storage (ADR-111). Optional; load with
`KHIVE_PACKS=kg,blob`. Registers no note or entity kinds. A normal file-backed boot
installs a default `FsBlobStore` rooted beside the database file even with no
`[storage.blob]` section in `khive.toml` and no `KHIVE_BLOB_ROOT` set; the verbs stay
unconfigured (erroring until a backend is installed) only when the server boots against
an in-memory backend, which has no directory to default a root beside.

### `blob.put` — Commissive

Store bytes (base64) in the content-addressed blob store; returns the BLAKE3
`ContentRef`. Idempotent: identical content returns the same ref without a re-write.

| Param   | Type   | Required | Notes                                                                                                   |
| ------- | ------ | -------- | ------------------------------------------------------------------------------------------------------- |
| `bytes` | string | yes      | Base64-encoded object content. Decoded size is capped at 64 MiB per call (ADR-111's v1 object ceiling). |

### `blob.get` — Assertive

Read an object back by `content_ref`, base64-encoded in the response, with an optional
byte range. The object is rejected before any bytes are hydrated if it exceeds the
64 MiB ceiling this verb will fetch, or if the requested slice would base64-encode to a
response exceeding the daemon's IPC frame cap. Concurrent `blob.get` hydration is bounded
by a small pack-level semaphore.

| Param         | Type   | Required | Notes                                                                                                                             |
| ------------- | ------ | -------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `content_ref` | string | yes      | 64-char lowercase-hex BLAKE3 content reference returned by `blob.put`.                                                            |
| `range`       | object | no       | `{offset, length}`, both non-negative integers when present. Applied to the fetched object as a slice, not a streamed range read. |

### `blob.stat` — Assertive

Report whether an object exists and its size, answered by a single metadata read with
no bytes hydrated.

| Param         | Type   | Required | Notes                                                                  |
| ------------- | ------ | -------- | ---------------------------------------------------------------------- |
| `content_ref` | string | yes      | 64-char lowercase-hex BLAKE3 content reference returned by `blob.put`. |

```
request(ops="blob.put(bytes=\"aGVsbG8=\")")
request(ops="blob.stat(content_ref=\"<64-char-hex>\")")
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
- [ADR-002: Closed Edge Ontology](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-002-edge-ontology.md)
