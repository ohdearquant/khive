# API Reference

khive exposes exactly one MCP tool, `request`. Every public verb from the production
packs is dispatched through that single tool via a small request DSL.
This page documents the DSL grammar, the response envelope, and every verb's full
parameter contract, so an agent can call khive correctly without reading Rust source.

The live registry is authoritative: run `request(ops="verbs()")` against your server to
discover its loaded pack set and total. The static sections below are audited against pack
`HandlerDef`/`ParamDef` declarations; the kg catalog was refreshed against the 20-entry
`KG_HANDLERS` table and its integration contract when `db_diagnostics` shipped.

An always-machine-readable copy of this page is at
[`/md/api-reference.md`](md/api-reference.md). The site also publishes
[`/llms.txt`](llms.txt) (a short index) and [`/llms-full.txt`](llms-full.txt)
(every guide page concatenated) for agents that prefer one fetch over several.

## Packs at a glance

| Pack        | Verbs | Load with                                  | Optional?           |
| ----------- | ----- | ------------------------------------------ | ------------------- |
| `kg`        | 20    | `KHIVE_PACKS=kg`                           | No — base substrate |
| `gtd`       | 5     | `KHIVE_PACKS=kg,gtd`                       | Yes                 |
| `memory`    | 5     | `KHIVE_PACKS=kg,memory`                    | Yes                 |
| `brain`     | 16    | `KHIVE_PACKS=kg,brain`                     | Yes                 |
| `comm`      | 10    | `KHIVE_PACKS=kg,comm`                      | Yes                 |
| `schedule`  | 4     | `KHIVE_PACKS=kg,schedule`                  | Yes                 |
| `knowledge` | 19    | `KHIVE_PACKS=kg,knowledge`                 | Yes                 |
| `session`   | 4     | `KHIVE_PACKS=kg,session`                   | Yes                 |
| `git`       | 4     | `KHIVE_PACKS=kg,git`                       | Yes                 |
| `code`      | 1     | `KHIVE_PACKS=kg,code`                      | Yes                 |
| `workspace` | 0     | `KHIVE_PACKS=kg,git,gtd,session,workspace` | Yes                 |
| `blob`      | 3     | `KHIVE_PACKS=kg,blob`                      | Yes                 |

`git` also registers the `commit` / `issue` / `pull_request` note kinds and the shared
`run_ingest` core (`crates/khive-pack-git/src/ingest.rs`) that both `git.digest` and the
`kkernel git-ingest` CLI drive. Its four verbs are `git.digest` (read/ingest) plus three
write verbs, `git.commit` / `git.branch` / `git.push` (ADR-108), that shell to system git
with hardened, allowlisted argv construction. A remote `git.digest` source whose initial
clone or fetch setup fails returns a typed `RemoteFetchError` naming the redacted remote
to in-process callers; the MCP `request` envelope renders it as a plain error message
rather than structured fields (ADR-088 Amendment 1, Remote-URL mode, point 5). A
clone/fetch failure hit later while repairing an already-cached clone surfaces as
`InvalidInput` only when the bounded refetch-then-reclone repair ultimately fails (a
successful repair earns one more snapshot attempt, and the digest completes only when
that attempt succeeds); a source that parses as neither a local path nor a remote URL
is `InvalidInput` as well.

`workspace` requires `kg`, `git`, `gtd`, and `session` to be loaded alongside it (the runtime rejects a pack set that omits a declared dependency), so its minimal example lists all four.

`schedule` requires `kg`. `schedule.remind` additionally requires `comm.send` at
creation time and persists nothing when that delivery capability is absent; the other
three schedule verbs remain available without `comm`.

`code` registers the `finding` note kind and edge rules, plus the `code.ingest` verb (L1
manifest + L1.5 import-scan source ingest, ADR-085 Amendment 2 — see below); its
`findings.json` batch ingest still runs only through the `kkernel code-ingest` admin CLI
path, not the MCP verb surface.
That admin path is history-preserving: a deterministic entity, finding-note,
or annotation-edge ID is skipped even when its row is soft-deleted, so neither
real re-ingest nor `--dry-run` treats a tombstone as a new record or resurrects
it.

`blob` registers no note or entity kinds; its three verbs (`blob.put` / `blob.get` /
`blob.stat`) dispatch over the `BlobStore` content-addressed storage trait (ADR-111). A
normal file-backed boot installs a default `FsBlobStore` rooted beside the database file
even with no `[storage.blob]` section and no `KHIVE_BLOB_ROOT` set; the verbs only stay
unconfigured (erroring until a backend is installed) when the server boots against an
in-memory backend, which has no directory to default a root beside.

Pack selection resolves as `--pack` > `KHIVE_PACKS` > discovered `[runtime].packs` > the
built-in production set. With no non-empty selection at any of the first three layers, the
default binary loads all 12 packs. Use `verbs()` for the current aggregate rather than carrying
a second hand-maintained total here.

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

A successful entry can also carry a transport-owned `advisories` array beside `result`.
These warnings describe execution context without changing the verb's canonical result or
the batch summary. Presentation and output-format transforms apply only to `result`, and
frame-budget degradation preserves advisories. For example, inspecting a read-only snapshot
returns normal verb data while making the missing durable dispatch audit explicit:

```json
{
  "ok": true,
  "tool": "stats",
  "result": { "entities": 42 },
  "advisories": [
    {
      "code": "audit_persistence_skipped_read_only",
      "severity": "warning",
      "component": "audit_event_store",
      "reason": "read_only_backend",
      "message": "operation completed, but its dispatch audit event was not persisted because the audit backend is read-only"
    }
  ]
}
```

That advisory appears on successful non-help operations only. Failed, aborted, and
`help=true` entries do not claim that an audit write was skipped.

---

## `kg` pack — 20 verbs

Base substrate verbs, bare names (no `kg.` prefix). Category is the illocutionary act
(Searle 1976): Assertive = retrieves state, Commissive = commits a persistent change,
Declaration = changes institutional status by fiat.

### `create` — Commissive

Create an entity or note (singleton) or a batch of entities (bulk via `items`).

Singleton writes preserve the complete source in storage and FTS. If a configured embedder
receives a UTF-8-safe bounded prefix, the successful response includes a `warnings` array; the
warning is derived from the embedding outcome, not from a separate registry prediction.

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

| Param                        | Type                     | Required | Notes                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ---------------------------- | ------------------------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `kind`                       | string                   | yes      | `entity`\|`note`\|`edge`\|`event`\|`proposal`\|`message`, or a granular kind.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `limit`                      | integer                  | no       | Default 20.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `offset`                     | integer                  | no       | Default 0; mutually exclusive with `after`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `after`                      | string                   | no       | Exact full entity/note/edge cursor UUID returned as `next_after`, or `""` to begin cursor mode. Prefixes are rejected because keyset pagination needs the stable insertion boundary.                                                                                                                                                                                                                                                                                                                                                                                                      |
| `entity_kind`                | string                   | no       | Filter when `kind="entity"`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `entity_type`                | string                   | no       | Filter by type field when `kind="entity"`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `note_kind`                  | string                   | no       | Filter when `kind="note"`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `tags`                       | array\<string\>          | no       | Case-insensitive OR-match over entity tags or note `properties.tags`; valid for `kind="entity"` and `kind="note"`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| `source_id` / `target_id`    | uuid                     | no       | Edge endpoint filters, `kind="edge"` only.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `relations`                  | array\<string\>          | no       | Edge relation filter, `kind="edge"` only.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `min_weight` / `max_weight`  | number                   | no       | Edge weight bounds, `kind="edge"` only.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `event_kind` / `event_kinds` | string / array\<string\> | no       | `kind="event"` only; additive.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `session_id`                 | uuid                     | no       | `kind="event"` only; exact full session UUID. Prefixes are rejected because the filter needs one stable record.                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `observed`                   | array\<uuid\>            | no       | `kind="event"` only; exact full observed-record UUIDs. Prefixes are rejected.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `selected`                   | array\<uuid\>            | no       | `kind="event"` only; exact full selected-record UUIDs. Prefixes are rejected.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `thread_id`                  | string                   | no       | `kind="message"` only; complete UUID or unique 8+ hex prefix resolved across stored thread roots in the caller's effective namespace. Missing or ambiguous prefixes fail explicitly. Legacy exceptions: input that is not hex, or shorter than 8 chars, is matched exactly against stored thread labels (no match → empty list, not an error); for all-hex ≥8-char input, a stored label byte-equal to the input takes precedence over any UUID-prefix match, and a stored label differing only in ASCII case is a final fallback, consulted only when no UUID-prefix candidate resolves. |
| `direction`                  | string                   | no       | `kind="message"` only: `inbound`\|`outbound`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `from` / `to`                | string                   | no       | `kind="message"` only, sender/recipient filter.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `read`                       | bool                     | no       | `kind="message"` only.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `delivered`                  | bool                     | no       | `kind="message"` only.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |

```
request(ops="list(kind=\"entity\", entity_kind=\"concept\", limit=20)")
```

Offset-mode responses always use `{"items": [...], "requested_limit": N,
"effective_limit": M, "limit_clamped": bool}`. The shape is identical whether or not the
server-side cap binds. Advance `offset` by `items.length`, not by either limit field;
`effective_limit` discloses the server cap but is not a guaranteed row count. The caps are entity
500, note 200, edge 1000, event 1000, and proposal 500. Entity, note, and edge cursor modes return
`{"entities": [...], "next_after": ...}`, `{"notes": [...], "next_after": ...}`, or
`{"edges": [...], "next_after": ...}` and always include the same limit metadata.

Set `after=""` to begin a stable cursor walk, then pass each non-null `next_after` value into the
next request with the same filters. The cursor's public value is a UUID; storage resolves it to an
immutable, database-assigned insertion sequence and performs a sequence seek. Every genuinely new id
committed after an issued boundary receives a greater sequence, so equal timestamps, backward clock
movement, and lower UUIDs cannot make it fall behind that boundary.

Cursor mode is a live walk, not an MVCC snapshot. Inserts committed before a later page query may
extend the walk. After a substrate/namespace query returns `next_after: null`, rows committed after
that terminal query require a new walk from `after=""`. Updates and deletes can change whether an
unvisited row matches the filters. A cursor that was hard-deleted, is outside the caller's visible
namespaces, or otherwise cannot be resolved returns an error instead of silently restarting. Cursor
mode and `offset` are mutually exclusive. Filtered note cursor walks may additionally return
`scan_incomplete: true` with the last safe continuation cursor when their 10,000-row safety
ceiling is reached before another matching note is proven.

Outcome-filtered event offset pages may also set `scan_incomplete: true` when their bounded
post-filter scan cannot prove exhaustion. Such a short page is not terminal: advance only by the
rows actually returned, and treat an incomplete empty page as non-resumable without a narrower
filter or a larger effective limit.

Row shape (each item in the offset or cursor envelope) depends on `kind`.
For `kind="entity"`, `"note"`, `"edge"`, and `"event"`, the row is the **full public record shape**
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
  `content_ref`, when present, is the compatibility projection of attachment role `content`;
  entities no longer store a writable same-named column.
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

The response carries a `count_scope` object stating what the counts range over:
`{"namespaces": "caller_visible", "rows": "live_only"}` — counts cover the namespaces visible to
the caller and exclude soft-deleted rows.

```
request(ops="stats()")
```

### `update` — Declaration

Patch entity, note, or edge fields. Field set depends on substrate: entities accept
`name`/`description`/`properties`/`tags`; notes accept
`name`/`content`/`salience`/`decay_factor`/`properties`; edges accept
`relation`/`weight`/`properties`.

Entity/note text updates use the same full-source storage and bounded embedding contract as
singleton `create`; a successful response includes `warnings` when embedding actually truncated.

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

Deduplicate two entities or notes. Returns `{kept_id, removed_id, edges_rewired,
edges_contract_skipped, edge_conflict_preimages, properties_merged,
tags_unioned, content_appended, dry_run}` — chain with `$prev.kept_id`, **not**
`$prev.id` (merge has no top-level `id` field). When rewiring collides with an
existing edge natural key, `edge_conflict_preimages` records the surviving edge
id, the complete dropped edge, and any incident annotation edges removed by the
hard-delete cascade. The same preimages are stored in the merge audit event.
When the surviving entity or note is reindexed and an embedder bounds its input, the successful
response also includes the standard embedding-truncation `warnings` advisory.

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
| `entity_kind`        | string  | no       | Entity-substrate searches only.                                                                                                        |
| `entity_type`        | string  | no       | Entity-substrate searches only.                                                                                                        |
| `note_kind`          | string  | no       | Note-substrate searches only.                                                                                                          |
| `include_superseded` | bool    | no       | Note-substrate searches only; default false excludes notes targeted by a `supersedes` edge.                                            |
| `properties`         | object  | no       | Match records whose properties contain all listed key=value pairs, applied before result truncation inside a bounded candidate window. |
| `tags`               | array   | no       | OR-match against tags; entity tags matched at the SQL level, note tags read from `properties.tags`.                                    |
| `min_score`          | number  | no       | Score floor 0.0–1.0. No server default; RRF rank-1 scores on small corpora are typically 0.013–0.033.                                  |

```
request(ops="search(kind=\"entity\", query=\"knowledge graph runtime\", limit=10)")
```

`entity_kind` and `note_kind` are compatibility filters for the corresponding
substrate-level `kind`. A granular discriminator such as `kind="concept"` or
`kind="observation"` may be paired with the same compatibility value, but a
contradiction is rejected. Entity-only fields on a note search and note-only
fields on an entity search are also rejected explicitly; they are never
ignored. `properties` must be an object and `tags` must be an array of strings.
The same validated request is used for single- and multi-backend execution.

In multi-backend mode a backend failure with surviving hits yields those hits
with `status: "partial"`, deprecated `partial: true`, `missing_backends`, and a
`backend_errors` object mapping each retained failed backend to its bounded,
credential-masked backend id and cause. Masked backend ids use a stable hash
suffix so distinct failed legs remain distinguishable. These fields sit beside `result` and survive
presentation and response-frame compaction. If no hit survives filtering, the
operation is `ok: false` with `error.kind="search_incomplete"`; the structured
error carries the same diagnostics. `backend_errors_truncated` plus
`backend_errors_omitted` explicitly report causes omitted by safety bounds.

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

Bounded multi-hop BFS traversal. Nodes are selected at their shallowest depth;
same-depth tie order is intentionally unspecified. Limits count non-root
first-visit nodes independently per distinct root.

| Param                | Type            | Required | Notes                                                                      |
| -------------------- | --------------- | -------- | -------------------------------------------------------------------------- |
| `roots`              | array\<uuid\>   | yes      | Starting UUIDs; maximum 100 raw entries, then de-duplicated after resolve. |
| `max_depth`          | integer         | no       | Default 3; maximum 10; values above 10 are rejected.                       |
| `direction`          | string          | no       | `out`/`outgoing`, `in`/`incoming`, or `both` (default `both`).             |
| `relations`          | array\<string\> | no       | Restrict traversal to these relations.                                     |
| `min_weight`         | number          | no       | Minimum edge weight, finite and within 0.0–1.0.                            |
| `limit`              | integer         | no       | Non-root results per root; default 100, maximum 1,000.                     |
| `include_roots`      | boolean         | no       | Include depth-0 roots (default `true`; they do not consume `limit`).       |
| `include_properties` | boolean         | no       | Include entity properties on path nodes (default `false`).                 |

One public request shares a 100,000-adjacency-row work budget and five-second
storage-expansion deadline across all roots and visible namespaces. Self-loops, parallel paths,
and rows rejected by first-visit de-duplication still consume work. Exceeding a
shape bound, work budget, or deadline returns an error and never partial paths.
Traversal reads use statement-scoped snapshots, so concurrent writes may become
visible between frontier expansions; a single old WAL snapshot is never held for
the full operation.

```
request(ops="traverse(roots=[\"<uuid>\"], max_depth=2)")
```

The response contains exactly one traversal object per distinct requested root. Each path node
has `id`, `via_edge`, and `depth`; resolvable entity and note nodes also carry `name` and `kind`.
Note enrichment matches `neighbors`, including its `[kind]` display-name fallback for a nameless
note reached through an annotation edge. `properties` remains entity-only and is included only
when `include_properties=true`.

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
GQL results have a deterministic identity order. When a response has `has_more: true`,
repeat the same query with `SKIP` set to its `next_offset`; keep `page_size` unchanged.
SPARQL `OFFSET` is not part of the supported dialect.

| Param       | Type    | Required | Notes                                                                     |
| ----------- | ------- | -------- | ------------------------------------------------------------------------- |
| `query`     | string  | yes      | GQL or SPARQL pattern string, read-only. GQL supports `SKIP n [LIMIT m]`. |
| `page_size` | integer | no       | Rows per call; minimum 1, default 500, clamped to hard cap 10,000.        |
| `limit`     | integer | no       | Deprecated alias for `page_size`; supplying both is an error.             |

```
request(ops="query(query=\"MATCH (c:concept)-[:extends]->(d:concept) RETURN c, d LIMIT 20\")")
```

For an exhaustive audit, omit query-text `LIMIT` so the server can report whether another
page exists. An explicit `LIMIT` at or below `page_size` is a terminal caller-chosen bound.

```text
request(ops="query(query=\"MATCH (a)-[r:depends_on]->(b) RETURN a, r, b\", page_size=500)")
# {"rows":[...],"offset":0,"page_size":500,"has_more":true,
#  "next_offset":500,"truncated":true,...}

request(ops="query(query=\"MATCH (a)-[r:depends_on]->(b) RETURN a, r, b SKIP 500\", page_size=500)")
```

`next_offset` is `offset + rows.length` and appears only on GQL pages with `has_more: true`.
`truncated` remains as a compatibility alias for `has_more`. Offset paging is stable while
the matched graph is unchanged; concurrent inserts or deletes can shift later pages.

### `propose` — Commissive

Create an event-sourced change proposal. Returns `{id, full_id, parent_id, status, proposer, title}`.
`full_id` and a non-null `parent_id` remain canonical 36-character UUIDs in Agent mode so
ancestry can be submitted again unchanged; `id` may be the ordinary 8-character Agent form.
Reuse the returned `full_id` as `parent_id` in a subsequent proposal request. The `changeset`
field has nested objects and cannot be expressed in function-call DSL form; use JSON form (whose
operations do not support `$prev`).

| Param         | Type            | Required | Notes                                                                                                                                                                                                                                                                                           |
| ------------- | --------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `title`       | string          | yes      | Non-empty short title.                                                                                                                                                                                                                                                                          |
| `description` | string          | yes      | Non-empty full description.                                                                                                                                                                                                                                                                     |
| `changeset`   | object          | yes      | Discriminated by `kind`: `add_entity`, `update_entity`, `add_edge`, `add_note`, `merge_entities`, `supersede_entity`, `compound` (nested `steps`). Every nested identifier is a full UUID; prefixes are rejected because resolution could miss, be ambiguous, or change stable proposal intent. |
| `reviewers`   | array\<string\> | no       | Actor IDs requested as reviewers.                                                                                                                                                                                                                                                               |
| `expiry`      | integer         | no       | Expiry timestamp, microseconds since epoch.                                                                                                                                                                                                                                                     |
| `parent_id`   | uuid            | no       | Full UUID of the parent proposal. Prefixes are rejected because ancestry is an explicit stable reference; responses preserve it canonically.                                                                                                                                                    |

```
request(ops="[{\"tool\":\"propose\",\"args\":{\"title\":\"Add GQE\",\"description\":\"Register the GQE concept\",\"changeset\":{\"kind\":\"add_entity\",\"entity\":{\"kind\":\"concept\",\"name\":\"GQE\"}}}}]")
```

### `review` — Declaration

Approve, reject, comment, or request changes on a proposal.

When approval immediately applies an embedding-bearing changeset, the response includes the
standard `warnings` advisory if that committed apply bounded any embedding input.

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

### `whoami` — Assertive

Report the caller identity and namespace scope the runtime already resolved for this request.
It takes no parameters and returns only identity labels, never tokens or credentials:
`{actor_id, actor_kind, unattributed, namespace, visible_namespaces}`.

```
request(ops="whoami()")
```

### `db_diagnostics` — Assertive

Report reader/writer contention, graph-edge integrity, and WAL/checkpoint diagnostics for the
main database: build identity, the checkpoint counters, a single PASSIVE checkpoint probe, the
`-wal` sidecar file size, page-level database size composition, and a WAL-pin holder census.
Takes no parameters.

`reader_contention` is scoped to the main `ConnectionPool` and resets only when that pool is
reconstructed. `reader_admission_capacity` and `available_reader_admission_slots` are the
configured total budget and its point-in-time availability; pooled reads and the explicit
raw-SQL deferred-transaction exception share it. `reader_acquisitions` is the sum of
`pooled_reader_checkouts` and request-path `standalone_reader_opens`, while
`infrastructure_standalone_reader_opens` is deliberately separate. Ordinary file-backed reads
must leave `standalone_reader_opens` flat. `reader_checkout_timeouts` counts admission waits that
exhausted `KHIVE_CHECKOUT_TIMEOUT_SECS` before work began, not cooperative request cancellation.
`active_pooled_reader_checkouts`, `peak_active_pooled_reader_checkouts`,
`completed_pooled_reader_checkouts`, and `max_completed_reader_hold_micros` expose concurrency
and lifecycle evidence; completed hold includes connection reset/replacement before reuse.
`reader_replacement_open_failures` counts a disqualified pooled-reader return whose replacement
connection then also failed to open, permanently shrinking the physical pool by one slot below
`max_readers`; non-zero here means the pool has fewer physical reader connections than
configured, and each occurrence is also logged at `warn`.

The timeout setting applies to each admission attempt. A verb that issues several sequential
reads can spend more than one configured timeout in total wall time, but each attempt is bounded
and saturation never falls back to opening a standalone connection.

`writer_contention` contains monotonic counters captured once per request:
`writer_acquisitions` is the total of `pooled_writer_acquisitions`,
`standalone_writer_acquisitions`, and `writer_task_acquisitions`. The first counts successful
finite-wait main-pool mutex checkouts, the second counts successful per-operation file-backed
standalone writer opens, and the third counts dequeued writer-task requests that acquired its
dedicated connection (or successfully completed `BEGIN IMMEDIATE`).
`writer_acquisition_timeouts` remains specific to the finite-wait main-pool mutex before SQLite
executes; SQLite `BEGIN`/statement failures are separate stages. `audit_append_failures` counts
process-wide best-effort audit appends whose storage error was logged and swallowed —
pure-observability rows only. An obligation-bearing row's commit failure (a dispatch outcome, an
unknown-verb row, a `git.digest` receipt, or a gate denial's own audit row) is never counted here:
it instead fails the dispatch that produced it directly, or — for a denial whose dispatch already
fails independent of the row — is tracked by a separate internal counter. `audit_append_failures`
and `audit_batch_flush_failures` are therefore disjoint for that case; summing them does not
double-count an obligation-bearing generation failure. Zero-wait
checkpoint skips, the diagnostics probe connection, the writer task's one-time lifetime
connection, and the checkpoint task's dedicated long-lived connection (opened once at startup
and reused across ticks) do not inflate the write-traffic acquisition total.

`writer_task_request_failures` counts dequeued writer-task requests whose processing at the
writer seam terminated in error, regardless of the specific terminal state; a request that never
reached the seam because an earlier request already closed the queue does not count.
`writer_task_side_effects_unknown` is the subset of those failures whose terminal state left the
request's side effects unprovable. Both are populated directly by `khive-db` and are therefore
identical for a runtime caller and a direct `khive-db` caller — unlike the `audit_*` fields below,
neither is ever `null`.

`audit_batch_flush_failures`, `audit_degraded_rows`, and `audit_degraded` are additive fields
supplied by the runtime's audit-batch control once one is registered: accepted batch generations
that reached a terminal non-commit outcome after retry, pure-observability rows released without a
commit, and a monotonic process-lifetime degradation flag, respectively. Each carries a matching
`_unavailable_reason` field and reports `null` — never a fabricated `0`/`false` — for a direct
`khive-db` caller or a runtime with no audit-batch control registered.

`checkpoint_counters` reports checkpoint pressure without making its telemetry another source of
WAL pressure. `checkpoint_pressure_elevated_ticks` and the episode start/recovery totals are
in-memory observations; `checkpoint_lifecycle_append_attempts`, append failures, and handoff drops
describe actual persistence work. The checkpoint task appends only episode elevation and recovery
transitions, so sustained pressure does not produce one primary-store write per checkpoint tick.

A finite-wait pooled checkout failure retains its compatibility display text in `message`, but
the MCP error is a stable object rather than a string:

```json
{
  "kind": "unavailable",
  "code": "writer_pool_checkout_timeout",
  "stage": "writer_pool_checkout_timeout",
  "message": "storage: ... timed out ... waiting for sqlite writer connection",
  "timeout_ms": 5000,
  "capability": "notes",
  "operation": "append_note"
}
```

`capability` and `operation` are `null` when the typed SQLite error reaches runtime directly
rather than through a storage capability wrapper. Callers should branch on `code` or `stage`,
never on `message`.

The PASSIVE probe may backfill WAL frames into the database — that is normal checkpoint
I/O and is what the reported `checkpointed_frames` counts. It never changes logical
state, never escalates to TRUNCATE, never creates a missing database file, and never
deletes WAL-pin sidecar evidence. `wal_pin.status` reports `complete`, `degraded`, or
`unavailable`; its tagged `census.status` is independently `complete`, `incomplete`, or
`unavailable`. An incomplete OS walk retains partial PID evidence but states why additional
holders cannot be ruled out. The legacy sibling booleans and PID arrays remain for compatibility.
The holder census is reconciled with a separate, bounded, read-only sidecar pass. A complete census
and conclusive sidecar walk can therefore produce `wal_pin.status: "complete"`; truncated walks,
unknown sidecar identities, and OS-confirmed holders missing sidecar evidence degrade explicitly.
`sidecar_listing_truncated` and `sidecar_entries_cleanup_would_reap` are measured by that pass. The
latter is a forecast: diagnostics never performs the cleanup it reports.

`size_composition` accounts SQLite pages by individual table or index using aggregate `dbstat`.
It reports file-wide page/freelist/accounted/unaccounted byte totals plus operational class totals
for ordinary row tables, indexes, FTS storage, vector storage, mixed row-and-embedding tables, and
SQLite internal objects. A table that stores both ordinary columns and an `embedding BLOB` is kept
in `mixed_embedding_bytes`: SQLite cannot attribute bytes within a shared page to one column, so
the field is an upper bound for the embedding-bearing table, not a fabricated pure-vector byte
count. Object detail is deterministic and capped at 4,096 rows; `objects_truncated` and
`objects_omitted` make cap pressure explicit while the aggregate class totals still cover every
object returned by `dbstat`. `size_composition_error` explains an unavailable report.

`graph_edge_integrity` reports `duplicate_edge_id_groups`, `graph_edges_rows`,
`graph_edges_seq_rows`, and `pre_v14_duplicate_edge_state_detected`. A non-zero duplicate group
count is the legacy cross-namespace duplicate-ID state that can make a multi-namespace edge cursor
walk lossy. The two row counts are raw evidence, not a parity verdict: list-sequence rows
intentionally survive hard deletion, so the ledger can legitimately contain more rows than the
live edge table. `graph_edge_integrity_error` explains a missing integrity section.

The handler additionally annotates `graph_edge_integrity` with four derived fields:
`graph_edges_rows_scope` (`{"namespaces": "all", "rows": "live_and_soft_deleted"}`),
`graph_edges_seq_rows_scope` (`{"namespaces": "all", "rows":
"inserted_ids_retained_after_hard_delete"}`), `graph_edges_seq_minus_graph_edges` (the signed
ledger delta), and `graph_edges_seq_relationship` — one of
`ledger_ahead_consistent_with_hard_deletes`, `equal`,
`ledger_behind_pre_v14_duplicate_edge_state` (a negative delta while the report flags the pre-V14
duplicate-edge state), or `ledger_behind_unexpected` (a negative delta with no known legacy
explanation).
Sections that cannot be collected (in-memory backend, missing file, unsupported platform) carry
explicit reasons rather than being silently omitted.

```
request(ops="db_diagnostics()")
```

### `verbs` — Assertive

List all MCP-callable verbs registered on this server. Internal subhandlers are
excluded.

| Param      | Type   | Required | Notes                                                                                                                                 |
| ---------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `category` | string | no       | Filter: `Assertive`\|`Commissive`\|`Declaration`\|`Directive`.                                                                        |
| `pack`     | string | no       | Filter by pack name (`kg`, `gtd`, `memory`, `brain`, `comm`, `schedule`, `knowledge`, `session`, `git`, `code`, `workspace`, `blob`). |

```
request(ops="verbs()")
```

The result includes the filtered `verbs` array and `total`, plus an unfiltered
`pack_counts` object for every loaded pack. Zero-verb packs remain present in
`pack_counts`, so callers can distinguish an ontology-only pack from one that
was not loaded.

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
| `depends_on`        | array\<uuid\>   | no       | Blocking task complete UUIDs or unique 8+ hex prefixes resolved in the caller's primary namespace.                                                                   |
| `context_entity_id` | uuid            | no       | Full UUID of a related KG entity. Prefixes are rejected because the stored relationship is an explicit stable reference; Agent responses preserve it canonically.    |
| `tags`              | array\<string\> | no       | Tag list.                                                                                                                                                            |

```
request(ops="gtd.assign(title=\"Ship API reference\", priority=\"p1\", assignee=\"agent:docs\")")
```

### `gtd.next` — Assertive

List actionable tasks (status `next` or `active`) by priority. By default, tasks with
unfinished or structurally broken dependencies are omitted.

| Param             | Type    | Required | Notes                                                                             |
| ----------------- | ------- | -------- | --------------------------------------------------------------------------------- |
| `limit`           | integer | no       | Default 10.                                                                       |
| `assignee`        | string  | no       | Filter to this assignee.                                                          |
| `include_blocked` | boolean | no       | Include blocked/broken candidates after ready work for diagnosis (default false). |

```
request(ops="gtd.next(assignee=\"agent:docs\", limit=10)")
```

### `gtd.complete` — Declaration

Mark a task done (or cancelled) with an optional result note.
Every non-terminal GTD state may move directly to either terminal state. Successful
state changes include `audit_persisted`; `false` means the task committed but the
best-effort lifecycle-audit append failed.

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

Each task reports `dependency_state` (`ready`, `blocked`, or `broken`), `actionable`,
and a `blocked_by` array whose entries carry a `state` of `pending`, `cancelled`,
`soft_deleted`, `missing`, `invalid`, `different_namespace`, or `wrong_kind`.

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
| `namespace`         | string  | no       | Exact-match read scope; absent uses the caller's visible namespace set.   |

Each result carries `serve_attribution` (`profile`, `unattributed`, or
`unspecified`). `profile` also carries `served_by_profile_id`; `unattributed`
means a selected profile record was unreadable and downstream feedback must not
fall back to a current binding/default.
Each result also carries canonical `full_id`; pass it directly to
`memory.feedback(target_id=...)` in a later request without an extra `get`.
`full_id` is present when the resolved output format is `json`, the builtin
default, under any presentation mode. The `auto` and `table` formats omit it
unless the request sets `presentation=verbose`.

```
request(ops="memory.recall(query=\"ADR-016 DSL grammar\", limit=5, min_score=0.3)")
```

### `memory.feedback` — Commissive

Emit explicit feedback on a recalled entity; updates recall-domain posteriors.

| Param       | Type   | Required | Notes                                                                                                                                                                           |
| ----------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `target_id` | uuid   | yes      | Full UUID of the recalled entity or memory. Prefixes are rejected because feedback must identify one exact record. The acknowledgement returns canonical `target_id` for reuse. |
| `signal`    | string | yes      | `useful`\|`not_useful`\|`wrong`\|`explicit_positive`\|`explicit_negative`\|`implicit_positive`\|`implicit_negative`\|`correction`.                                              |

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

## `brain` pack — 16 verbs

Recall-tuning profiles: Beta-posterior scoring, profile lifecycle, and the actor/
namespace/consumer-kind resolution table that picks which profile serves a given
caller. Optional; load with `KHIVE_PACKS=kg,brain`.

### `brain.event_counts` — Assertive

Windowed event counts grouped by kind, actor, and verb over the event plane (ADR-103
Stage 1, #724 Ask A). `feedback_explicit` events are additionally split by
`served_by_profile_id`, signal, and `payload.originating_verb` (direct
`brain.feedback` versus `brain.auto_feedback`). Legacy feedback rows without the origin
marker fall back to their stored event verb. Events carrying a `work_class` (today:
`phase_started`/`phase_completed`/`phase_cancelled` payloads, or `payload.resource.work_class`
on a dispatch audit row) split by `counts_by_work_class`. Events carrying
`payload.resource.cost_unit` (ADR-103 Amendment 1, stamped on every successful verb dispatch
since PR #927) sum into `total_cost_unit` and `cost_unit_by_verb`; both are omitted, not
zero-filled, when no event in the window carries `cost_unit`. Events without a `cost_unit`
(pre-Amendment-1 events, or errored/denied dispatches) simply do not contribute. When
`truncated` is `true`, these sums are computed over the fetched page only, same as the other
`counts_by_*` fields.

| Param   | Type   | Required | Notes                                                                                                                                                       |
| ------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `since` | string | yes      | Window start, ISO-8601/RFC-3339 datetime. Inclusive.                                                                                                        |
| `until` | string | no       | Window end, ISO-8601/RFC-3339 datetime. Exclusive. Defaults to now.                                                                                         |
| `actor` | string | no       | Filter to a single actor. Stored actor strings are prefixed (`actor:lambda:khive`); bare (`lambda:khive`) or prefixed form both match. Omit for all actors. |
| `kind`  | string | no       | Filter to a single EventKind (e.g. `"recall_executed"`). Omit for all.                                                                                      |

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

Move a profile to Active. This is a lifecycle transition; serving reads profile state
per request and no background update loop is started.

| Param        | Type   | Required | Notes                |
| ------------ | ------ | -------- | -------------------- |
| `profile_id` | string | yes      | Profile to activate. |

```
request(ops="brain.activate(profile_id=\"implementer-recall-v1\")")
```

### `brain.deactivate` — Commissive

Move a profile to Inactive (lifecycle transition; retain state).

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

| Param                  | Type   | Required | Notes                                                                                                                                              |
| ---------------------- | ------ | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| `target_id`            | uuid   | yes      | Memory note or entity the feedback applies to.                                                                                                     |
| `signal`               | string | yes      | Same signal set as `memory.feedback`.                                                                                                              |
| `served_by_profile_id` | string | no       | Profile that served the rated result.                                                                                                              |
| `serve_attribution`    | string | no       | `profile`\|`unattributed`\|`unspecified`; unattributed implicit feedback is forced to zero weight, while explicit/correction feedback is rejected. |
| `section_signals`      | object | no       | Per-section signals for `knowledge_compose` profiles: `{"section_name": "useful"\|"not_useful"\|"wrong"}`.                                         |
| `scorer_run_id`        | string | no       | ADR-081 scorer-pass id; must pair with `serve_ledger_id`.                                                                                          |
| `serve_ledger_id`      | string | no       | ADR-081 `brain_serve_ledger` row id; must pair with `scorer_run_id`.                                                                               |

```
request(ops="brain.feedback(target_id=\"<uuid>\", signal=\"useful\")")
```

### `brain.auto_feedback` — Commissive

Emit caller-attributed feedback for one recall result — the convenience verb to call
right after `memory.recall` instead of hand-building `brain.feedback`.

| Param                  | Type   | Required    | Notes                                                                  |
| ---------------------- | ------ | ----------- | ---------------------------------------------------------------------- |
| `query`                | string | yes         | The recall query that produced the results.                            |
| `results`              | array  | yes         | Recall result objects retained as candidate context.                   |
| `target_id`            | string | with signal | Full UUID or compact id; must exactly equal one `results[].id`.        |
| `signal`               | string | no          | Omission abstains: no feedback event or posterior update.              |
| `served_by_profile_id` | string | no          | Profile that served the recall.                                        |
| `serve_attribution`    | string | no          | Serve-time tri-state; otherwise copied from the selected result.       |
| `scorer_run_id`        | string | no          | Forwarded verbatim to `brain.feedback`; pairs with `serve_ledger_id`.  |
| `serve_ledger_id`      | string | no          | Forwarded verbatim to `brain.feedback`; pairs with `scorer_run_id`.    |
| `namespace`            | string | no          | Exact namespace for the event and posterior fold; invalid values fail. |

Top-level serve-attribution fields are one pair and take precedence over the selected
result's pair. If neither top-level field is supplied, both fields are copied from the
selected result together. Feedback events retain the canonical event verb
`brain.feedback` and record the originating feedback handler in
`payload.originating_verb`.

```
request(ops="memory.recall(query=\"x\", limit=5) | brain.auto_feedback(query=\"x\", results=[{\"id\": $prev[0].id}], target_id=$prev[0].id, signal=\"implicit_positive\")")
```

### `brain.mark_turn` — Commissive

Emit a `PhaseStarted` event with `work_class="actor_turn"` carrying the calling actor and
a timestamp. Callers invoke it once per bounded unit of work (a wake, a turn) so
`brain.event_counts`'s `counts_by_work_class["actor_turn"]`, grouped by actor, gives a
per-actor denominator (e.g. `feedback_explicit / actor_turn`) that is not biased toward
whichever actor issues the most raw verb calls. Reuses the existing ADR-103 Stage 1
`PhaseStarted`/`work_class` vocabulary rather than a new event kind. Best-effort — never
fails the caller's turn.

| Param   | Type   | Required | Notes                                                                                                                                             |
| ------- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `label` | string | no       | Free-form label for this unit of work (e.g. `"wake"`, `"turn"`), recorded in the event payload's `phase` field. Does not affect the `work_class`. |

```
request(ops="brain.mark_turn(label=\"wake\")")
```

### `brain.bind` — Declaration

Write a row in the profile resolution table.

| Param           | Type    | Required | Notes                                                                                                                    |
| --------------- | ------- | -------- | ------------------------------------------------------------------------------------------------------------------------ |
| `profile_id`    | string  | yes      | Must exist.                                                                                                              |
| `actor`         | string  | no       | Default `*` (all actors).                                                                                                |
| `namespace`     | string  | no       | Default `*` (all namespaces).                                                                                            |
| `consumer_kind` | string  | no       | Default `*`; specific values must be declared by a loaded consumer pack. Unknown values are rejected with the valid set. |
| `priority`      | integer | no       | Higher wins on multiple matches (default 0).                                                                             |

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

The result includes `removed`, the number of matching bindings deleted. A successful
request that matched nothing returns `removed: 0`. The legacy `unbound` field carries
the same count for compatibility.

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

## `comm` pack — 10 verbs

Actor-to-actor messaging with threading. Optional; load with `KHIVE_PACKS=kg,comm`.

### `comm.send` — Commissive

Send a message, optionally threaded.

The atomic outbound/inbound write preserves the full body on both notes. If either copy's
embedding input is bounded, the successful response includes the standard `warnings` advisory.

| Param       | Type   | Required | Notes                                                                                                                                                                                           |
| ----------- | ------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `to`        | string | yes      | Actor label, e.g. `"lambda:leo"`. Both copies land in the caller's namespace; no cross-namespace write occurs.                                                                                  |
| `content`   | string | yes      | Non-empty message body.                                                                                                                                                                         |
| `subject`   | string | no       | Optional subject line.                                                                                                                                                                          |
| `thread_id` | uuid   | no       | Optional full thread UUID. Prefixes are rejected because a thread root is an explicit stable reference. Accepted complete spellings normalize to canonical lowercase dashed form.               |
| `self_send` | bool   | no       | Default false. Required when `to` matches the configured sender actor; otherwise the send is rejected. The anonymous `local` fallback is exempt. Use true only for an intentional note to self. |

```
request(ops="comm.send(to=\"lambda:leo\", subject=\"PR ready\", content=\"#600 is open for review\")")
```

Returns `{id, full_id, thread_id, ...}`. `full_id` and `thread_id` remain canonical
36-character UUIDs in Agent mode; pass the returned `thread_id` unchanged to a later send.

### `comm.delivered` — Assertive

Confirm the internal inbound sibling for a `comm.send` or `comm.reply`
outbound UUID. This is a read-only exact correlation lookup; it does not infer
delivery from content and does not report later SMTP or other external
transport status. The matching inbound note must belong to the caller's
namespace and carry the caller as `from_actor`.

| Param | Type | Required | Notes                                                                                                                                                              |
| ----- | ---- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `id`  | uuid | yes      | Full `full_id` from a successful send/reply, or `outbound_id` surfaced by an ambiguous atomic-write error. Prefixes are rejected; the UUID is the correlation key. |

Returns `{id, status, delivered, inbound_count}`. A successful lookup is
conclusive: `status` is `delivered` when `inbound_count > 0`, otherwise
`undelivered`. A lookup error leaves the delivery outcome uncertain.
Ordinary atomic-write failures leave neither copy and do not require this
lookup. Loss of the entire MCP response also loses the generated UUID and is
outside this operation's contract.

```
request(ops="comm.delivered(id=\"<full-outbound-uuid>\")")
```

### `comm.inbox` — Assertive

List and page through the caller's filtered inbound messages (default) or sent
history (`box="sent"`).
The response keeps the inbox envelope. `unread_count` is the exact mailbox-wide
unread count for the caller — independent of the page window and of `status`
and sender filters — and is zero for sent rows.
With `wait_ms`, an initially empty fully filtered page waits for a newly
committed matching message and otherwise returns at the deadline.

| Param                | Type    | Required | Notes                                                                     |
| -------------------- | ------- | -------- | ------------------------------------------------------------------------- |
| `limit`              | integer | no       | Default 20, max 200.                                                      |
| `box`                | string  | no       | `inbox` (default)\|`sent`. Sent rows are scoped to the caller.            |
| `offset`             | integer | no       | Default 0; offset after every supplied filter.                            |
| `status`             | string  | no       | Inbox-only: `unread` (default)\|`read`\|`all`.                            |
| `wait_ms`            | integer | no       | Long-poll only when the initial page is empty; default 0, max 30,000.     |
| `from_actor`         | string  | no       | Exact sender; mutually exclusive with `from_prefix`.                      |
| `from_prefix`        | string  | no       | Sender prefix; mutually exclusive with `from_actor`.                      |
| `exclude_from_actor` | string  | no       | Exclude an exact sender actor label.                                      |
| `to_actor`           | string  | no       | Sent-only exact recipient actor filter.                                   |
| `since`              | string  | no       | Inclusive RFC 3339 lower bound on top-level `created_at`.                 |
| `before`             | string  | no       | Exclusive RFC 3339 upper bound on top-level `created_at`.                 |
| `subject_contains`   | string  | no       | Case-insensitive non-empty subject substring; null subjects do not match. |
| `content_contains`   | string  | no       | Case-insensitive non-empty content substring.                             |
| `fields`             | array   | no       | Non-empty message-field projection shared with `comm.thread`.             |

```
request(ops="comm.inbox(limit=10)")
request(ops="comm.inbox(status=\"all\", content_contains=\"timeout\", offset=200)")
request(ops="comm.inbox(box=\"sent\", to_actor=\"lambda:leo\", since=\"2026-08-01T00:00:00Z\", fields=[\"id\",\"subject\",\"sent_at\"])")
request(ops="comm.inbox(limit=10, wait_ms=30000)")
```

The long-poll wake is process-local and carries no payload. Every wake re-runs
the same scoped query, and the response shape is identical to an immediate inbox call.

Every returned message uses the hyphenated full UUID for `id`, so the value is
always accepted unchanged by `comm.read`, `comm.reply`, or `comm.thread`, even
when two messages share an eight-character prefix. `full_id` remains an alias
for compatibility, while `short_id` is the compact display-only prefix.
Responses also carry `offset`, `has_more`, and `next_offset`; repeat the same
filtered call with each non-null `next_offset` to enumerate every match without
marking it read. All filters are ANDed. Time bounds use response `created_at`,
not optional transport `sent_at` metadata.

`fields` accepts the ordinary top-level message keys plus stable property
aliases (`comm_schema_version`, `from_actor`, `to_actor`, `thread_id`,
`sent_at`, `outbound_ref`, `sent_by_process`). Unknown names and an empty list
are errors. Omit it for the existing full-body response.

### `comm.unread` — Assertive

Count-only view of the caller's unread inbound messages — the same filter as
`comm.inbox(status="unread")`, without message payloads. Takes no parameters.

```
request(ops="comm.unread()")
```

### `comm.read` — Declaration

Compatibility mark-read surface for one or more inbound messages. It does not retrieve message
content; use `comm.inbox` or `comm.thread` for that. Outbound messages cannot be marked read. Mark writes
are best-effort: validation errors (not found, wrong kind, outbound direction, wrong addressee)
remain fatal, but a post-read mark failure returns `status: "failed"`, `read: false`, and
`mark_error`. A write whose execution seam terminated after being accepted (so it may already
have applied) instead returns `status: "unknown"`, `read: null`, and `mark_error` — check the
message's current state through `comm.inbox` before re-issuing; re-issuing is safe, since marking
a message read is idempotent. Successful items carry `status: "success"`; inspect each result and
re-issue failures (or unresolved unknowns) later.

| Param | Type            | Required    | Notes                                                                   |
| ----- | --------------- | ----------- | ----------------------------------------------------------------------- |
| `id`  | string          | conditional | One 8-char prefix or full UUID; mutually exclusive with `ids`.          |
| `ids` | array of string | conditional | 1-500 IDs; mutually exclusive with `id`. All targets validate up front. |

```
request(ops="comm.read(id=\"<message-id>\")")
request(ops="comm.read(ids=[\"<message-id-1>\", \"<message-id-2>\"])")
```

Exactly one of `id` or `ids` is required. The bulk response contains ordered
`results` plus `requested_count`, `unique_count`, `marked_count`, `unknown_count`, and
`failed_count`, with aggregate `status=success|partial|failed|unknown`. Bulk updates are not atomic
across messages: validation errors
reject the call before any write, while later storage errors appear in each
item's `read` and optional `mark_error`.

### `comm.mark_read` — Declaration

Canonical named bulk mark-read. It accepts the same inbound targets and returns the same bulk
summary shape as `comm.read(ids=[...])`, while adding an all-or-nothing mutation mode.

| Param    | Type            | Required | Notes                                                                                            |
| -------- | --------------- | -------- | ------------------------------------------------------------------------------------------------ |
| `ids`    | array of string | yes      | 1-500 prefixes or full UUIDs. All targets validate up front; duplicate resolved IDs update once. |
| `atomic` | bool            | no       | Default false. True commits every unique mark in one transaction or rolls the full set back.     |

```
request(ops="comm.mark_read(ids=[\"<message-id-1>\", \"<message-id-2>\"])")
request(ops="comm.mark_read(ids=[\"<message-id-1>\", \"<message-id-2>\"], atomic=true)")
```

With the default `atomic=false`, complete prevalidation is followed by the existing best-effort
per-target storage updates; inspect `read` and `mark_error`. With `atomic=true`, every target is
rechecked for namespace, message kind, inbound direction, and addressee inside one transaction.
A failed recheck or transaction statement returns an operation error and leaves every target
unchanged. A `side_effects_unknown` storage error means the transaction stayed indivisible but its
commit outcome could not be confirmed; callers must not blindly retry that case. The affected writer
is retired instead of being reused.
Retrieve content separately through `comm.inbox` or `comm.thread`.

### `comm.reply` — Commissive

Reply to a message, threading linkage.

Replies use the same full-source storage and embedding-truncation `warnings` contract as
`comm.send`.

| Param     | Type   | Required | Notes                                                       |
| --------- | ------ | -------- | ----------------------------------------------------------- |
| `id`      | string | yes      | 8-char prefix or full UUID of the message being replied to. |
| `content` | string | yes      | Non-empty reply body.                                       |

```
request(ops="comm.reply(id=\"<message-id>\", content=\"On it.\")")
```

### `comm.thread` — Assertive

Retrieve all messages in a conversation thread, ordered chronologically.

| Param    | Type    | Required | Notes                                                               |
| -------- | ------- | -------- | ------------------------------------------------------------------- |
| `id`     | string  | yes      | Thread root: 8-char prefix or full UUID of the originating message. |
| `limit`  | integer | no       | Default 100, max 500.                                               |
| `order`  | string  | no       | `asc` (default)\|`desc`.                                            |
| `after`  | string  | no       | Message-id or RFC 3339 cursor in the chosen order.                  |
| `fields` | array   | no       | Same strict message-field projection as `comm.inbox`.               |

```
request(ops="comm.thread(id=\"<thread-root-id>\")")
request(ops="comm.thread(id=\"<thread-root-id>\", fields=[\"id\",\"from_actor\",\"sent_at\"])")
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
every known channel, including `poll_interval_secs`, nullable advisory `stalled`, and
the live `quarantined_count`. Top-level `quarantined_count` covers the namespace-wide
parked backlog; `unattributed_quarantined_count` reports legacy rows that lack a complete
channel identity. Quarantine-only channel entries have nullable heartbeat fields and do
not fabricate daemon ownership. The channel union is capped at 200: heartbeat rows take
precedence, then quarantine-only identities fill remaining capacity in lexical channel
identity order. Top-level quarantine totals remain namespace-wide when entries are omitted.
For current rows with no known failure, `stalled` becomes true after three missed nominal
intervals; it is null for legacy/malformed rows or active failure/backoff state. This is
not a computed healthy or authoritative supervisor verdict. Health judgment belongs to
the caller. Rows are read from the caller's injected namespace (`namespace=`, defaulting
to `local` like every other comm verb). The shipped poll loop explicitly writes its
heartbeats to `local`; authorized per-tenant writers can write their own namespace. The
response echoes the namespace actually read in a `namespace` field, so an empty
`channels` array is scoped unambiguously. See the
[communication guide](communication.md) for the full response contract.

To recover, page `comm.inbox(status="all")`, inspect full rows for
`properties.quarantined`, and fetch a selected row with `get(id=...)`.
`delete(id=...)` removes it from the parked count; `delete(id=..., hard=true)`
permanently purges it. There is deliberately no automatic "release as trusted" path.
Generic message `create`/`update` mutations cannot set `channel_kind`, `channel_slug`, or
`quarantined`; those transport-owned fields are established only by `comm.ingest`.

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

| Param     | Type   | Required | Notes                                                                                                 |
| --------- | ------ | -------- | ----------------------------------------------------------------------------------------------------- |
| `content` | string | yes      | Non-empty reminder message.                                                                           |
| `at`      | string | yes      | RFC 3339 trigger time, e.g. `"2026-06-01T09:00:00Z"`.                                                 |
| `repeat`  | string | no       | `daily`\|`weekly`\|`monthly`. Cron expressions are rejected because the executor cannot advance them. |

```
request(ops="schedule.remind(content=\"check PR #600 CI\", at=\"2026-07-05T09:00:00Z\")")
```

### `schedule.schedule` — Commissive

Schedule a future verb dispatch.

| Param    | Type   | Required | Notes                                                               |
| -------- | ------ | -------- | ------------------------------------------------------------------- |
| `action` | string | yes      | One replayable verb call, e.g. `"gtd.assign(title=\"follow up\")"`. |
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

| Param | Type   | Required | Notes                                                                                                                    |
| ----- | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------ |
| `id`  | string | yes      | Complete UUID or unique 8+ hex prefix of the scheduled event. Prefix resolution searches the caller's primary namespace. |

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

Fetch a single atom or domain by full UUID, exact slug, or unique short prefix, in that
order. Exact slug lookup uses the caller namespace; UUID and prefix forms are
namespace-agnostic by-ID reads.

| Param              | Type   | Required | Notes                                                                                                                                                                                                                                                                           |
| ------------------ | ------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`               | string | yes      | Atom/domain full UUID, exact caller-namespace slug, or unique 8+ hex UUID prefix.                                                                                                                                                                                               |
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

The response includes `truncation_by_model`, keyed by every model that completed embedding work.
Each value contains `truncated` and `discarded_bytes` counters derived from the actual embedding
outcomes; atom source content remains complete in SQL and FTS.

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

| Param        | Type            | Required | Notes                                                     |
| ------------ | --------------- | -------- | --------------------------------------------------------- |
| `domain_ids` | array\<string\> | no       | Domain UUIDs/slugs whose member atoms to include.         |
| `atom_ids`   | array\<string\> | no       | Atom UUIDs/slugs to include directly.                     |
| `query`      | string          | yes      | Reranks the selected atom bodies.                         |
| `namespace`  | string          | no       | Exact namespace for all compose and profile-weight reads. |

```
request(ops="knowledge.compose(query=\"FastAPI JWT middleware validation patterns\", domain_ids=[\"attention\"])")
```

### `knowledge.edit` — Commissive

Upsert sections for an atom without wiping other sections.

The response combines the inline section and atom refresh outcomes in `truncation_by_model` and
includes the standard `warnings` advisory when any model bounded an embedding input. Stored section
and atom content remains complete.

| Param      | Type            | Required | Notes                                                                                                                                                                                                                                                                             |
| ---------- | --------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `id`       | string          | yes      | Atom UUID or slug.                                                                                                                                                                                                                                                                |
| `sections` | array\<object\> | yes      | `[{section_type, content, heading?, sort_order?}]`. `section_type` is a closed enum: `overview`\|`core_model`\|`boundary_conditions`\|`formalism`\|`operational_guidance`\|`examples`\|`failure_modes`\|`expert_lens`\|`references`\|`other`. `content` must be >= 80 characters. |

```
request(ops="[{\"tool\":\"knowledge.edit\",\"args\":{\"id\":\"rope\",\"sections\":[{\"section_type\":\"overview\",\"content\":\"Rotary position embedding rotates query/key vectors by an angle proportional to position...\"}]}}]")
```

### `knowledge.import` — Commissive

Validate and ingest atlas markdown file(s) with stable root-relative identity.

| Param            | Type   | Required | Notes                                                                           |
| ---------------- | ------ | -------- | ------------------------------------------------------------------------------- |
| `path`           | string | yes      | Filesystem path to a `.md` file or bounded directory tree.                      |
| `format`         | string | no       | Only `atlas_md` supported (default).                                            |
| `chunk_strategy` | string | no       | `section` (atom plus section rows) or `atom` (whole markdown, no section rows). |

```
request(ops="knowledge.import(path=\"/path/to/atlas/rope.md\")")
```

Directory slugs use normalized root-relative components joined by `--`; source paths are
retained in `properties.source_path`. Traversal and source validation complete before writes,
normalization collisions fail closed, and symlinks are not followed. Root directory symlinks are
rejected with or without a trailing separator. Entry, depth, and file-limit errors include the
exact failing path plus current and configured traversal counts. Successful responses add
`entries_visited`, `files_discovered`, `files_skipped`, `traversal_errors`, `sections_discovered`,
and `sections_skipped` to the existing import counters.

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
Every summary includes canonical `full_id` for direct reuse with
`session.resume` or `session.export` across requests. As with other records,
`full_id` is present under the default `json` output format and is omitted by
`format=auto` and `format=table` unless the request sets `presentation=verbose`.

| Param      | Type    | Required | Notes                                               |
| ---------- | ------- | -------- | --------------------------------------------------- |
| `limit`    | integer | no       | 1–200, default 20.                                  |
| `offset`   | integer | no       | Default 0.                                          |
| `provider` | string  | no       | Exact filter on `properties.provider`.              |
| `agent_id` | string  | no       | Exact filter on legacy `properties.agent_id`.       |
| `since`    | string  | no       | Inclusive RFC 3339 lower bound on session creation. |

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

Git-history ingester plus a hardened write surface (ADR-088,
[ADR-088 Amendment 1](../adr/ADR-088-amendment-1-git-digest.md),
[ADR-088 Amendment 2](../adr/ADR-088-amendment-2-anchor-identity.md), ADR-108).
Optional; load with `KHIVE_PACKS=kg,git`. Also registers the `commit` /
`issue` / `pull_request` note kinds, used by `git.digest` below and by the `kkernel
git-ingest` CLI (both drive the same underlying ingest core, so ingest enrichment —
readable `name`s, `Closes #N` reference edges, parent→child commit `precedes` edges —
applies identically either way).

### `git.digest` — Commissive

Walk a local repository path or clone/fetch a remote `https://` URL, then ingest commits
and (when source-bound `gh repo view <owner/repo>` resolves the GitHub repository derived
from the canonical source or local `origin`) issues and pull requests as provenance notes,
resolving or auto-creating the repo-anchor
`project` entity. Bounded and cursor-resumable: call again with the same
`source`/`project` while the response's `done` field is `false`.

| Param       | Type            | Required | Notes                                                                                                                                                                                                                                                                                                                                 |
| ----------- | --------------- | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `source`    | string          | yes      | A local filesystem path (must contain `.git`) or an `https://` URL. Any `https` host is accepted; issue/PR work requires a successful source-bound GitHub probe, otherwise the pass degrades to commits-only with structured skips. `ssh://`, `git://`, `http://`, and scp-shorthand (`user@host:path`) sources are rejected.         |
| `project`   | string          | no       | UUID or 8+ hex prefix of the repo-anchor `project` entity. When absent, resolution is slug-first through `properties.repo_slug`, then exact and normalized `properties.repo_url` reconciliation; a new anchor is created only when no identity evidence matches. Names are never a match key. See `project_id` and `project_created`. |
| `max_items` | integer         | no       | Bounded work for this call, counted across commits + issues + PRs (default 500, clamped to 1..=2000). Cursor-resumable: call again while the response's `done` field is `false`.                                                                                                                                                      |
| `include`   | array\<string\> | no       | Which record kinds to ingest this call: any of `commits` \| `issues` \| `pull_requests` (default: all three).                                                                                                                                                                                                                         |

```
request(ops="git.digest(source=\"https://github.com/org/repo\", max_items=500)")
```

The result includes `writes_refused`, a per-call count of record writes blocked by the
secret gate, and `write_refusals`, one safe structured diagnostic per refusal. Each
diagnostic names the attempted `verb`, the provenance `record_kind` and natural
`record_key`, plus the detector and a masked excerpt; rejected content is never returned.
Because a digest continues after a per-record refusal, callers that require a clean run
should assert `writes_refused == 0` in addition to waiting for `done == true`.

Per-source coverage is machine-readable via `sources` and `history_exhausted`. Every
source requested by `include` reports one of `completed`, `stopped_early` (with a
`reason`: budget exhausted, incomplete `gh` paging window, or a frozen cursor), or
`skipped` (with a `reason`: budget exhausted before the source was reached, `gh` CLI
absent, or a `gh` failure) — so "this repo has no issues/PRs" is distinguishable from
"issues/PRs were never reached" without parsing `warnings[]`. `history_exhausted` is
`true` only when every requested source completed: it separates "the walk visited
everything" from "the walk stopped before the end", a distinction `done`'s
budget-cursor semantics do not carry.

`gh_available` is `true` only after the probe explicitly targets and returns the
`owner/repo` derived from the canonical source or configured `origin`; every list call
pins that value with `--repo`. Argument-less repository selection is never used, so an
alternate remote selected by `gh repo set-default` cannot redirect ingestion. It is `false`
for an absent, unauthenticated, or repository-incompatible `gh`, and `null`
when neither issues nor pull requests were requested. A failed probe marks
each requested remote source `skipped` and does not expose `gh` stderr.

Every successful response also carries `receipt_id`, the UUID of a durable
schema-v2 audit event whose `payload.result` is the exact complete response
and whose target is `project_id`. The runtime appends this receipt before
returning. `git.digest` is `AlwaysVerbose`, so omitted/default MCP presentation
still returns the full UUID and exact stored result. If persistence cannot be
confirmed, the call returns `git_digest_receipt_persist_failed` and warns that
writes may already have committed instead of returning an unqualified success. If malformed handler
output prevents receipt construction, the runtime still appends one generic
Error audit when the gate audit and event store are available.

For response-loss recovery, record `request_started_at_us` before dispatch. One recovery
attempt freezes `since=request_started_at_us.saturating_sub(1)` and
`until=recovery_query_time_us + 1`; event-list bounds are strict
`created_at > since AND created_at < until`. Query with top-level
`presentation="verbose"` and otherwise-identical filters at offsets 0, 1000, 2000, …:

```text
request(
  presentation="verbose",
  ops="list(namespace=\"<original namespace>\", kind=\"event\", event_kind=\"audit\", verb=\"git.digest\", since=<since>, until=<frozen until>, limit=1000, offset=<offset>)"
)
```

Advance by the returned row count and stop only on a page shorter than 1000. The frozen
upper bound prevents newly completed receipts from shifting newest-first offsets. Match
`payload.result.project_id` and, when available, `payload.resource.request_id`; the exact
report is `payload.result`. Explicit namespace constrains this multi-record discovery to
the digest's attribution namespace. Under `AllowAllGate`, `get(id=<event id>)` remains
namespace-agnostic per ADR-007; repeating namespace can provide Gate/routing context but
does not make the by-ID storage lookup namespace-filtered. A namespace-less `list` uses
the caller's configured visible-namespace scope, not an isolation guarantee.

`request_id` groups an entire request, not an individual operation. Batch and chain
members therefore share it. Enumerate all matching receipt rows and treat each event's
`receipt_id` plus `payload.result` as the operation-unique recovery record. If one request
contains multiple digests for the same project, event order does not identify their input
positions; inspect every result, or issue one digest per request when one-to-one mapping is
required. If the client timed out before the daemon finished,
the receipt appears only when that pass completes; restart at offset zero with a newly
frozen `until` on a later attempt rather than
treating temporary absence as proof that nothing committed.

There is no 300-second `git.digest` or daemon-side dispatch deadline. The
observed 300,000 ms bound is the MCP client's request default, so large
`max_items` values can outlive a particular caller's wait while the daemon
continues the pass. The durable receipt is the recovery contract; the item
bound is not silently clamped to a transport-specific duration.

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

## `code` pack — 1 verb

Deterministic source-code map ingest (ADR-085 Amendment 2, PR #1039). Loaded by default;
set `KHIVE_PACKS=kg,code` to select only the base and code packs. Also registers the
`finding` note kind used by the `kkernel code-ingest` admin CLI's `findings.json` batch
ingest (not reachable via this MCP verb surface).

### `code.ingest` — Commissive

Walk a source folder and ingest L1 manifest-declared dependency edges
(`Cargo.toml` / `pyproject.toml` / `package.json`) plus L1.5 regex-based import-scan
module and project edges, into a dedicated map database — never the shared production
graph. A folder with no governing manifest anywhere above its source files still
ingests, using the basename of the ingested folder as its `source_project` identity.
Idempotent: entity and edge ids are `uuid5`-derived from identity, so re-ingesting the
same path upserts rather than duplicates, and a synchronous re-resolve pass
materializes edges for any import that only resolves once a later-scanned file's module
becomes known.

| Param       | Type            | Required | Notes                                                                                                                                                                                                                                        |
| ----------- | --------------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `path`      | string          | yes      | Folder to ingest — a monorepo subtree (a single crate/package) is first-class, not a special case of whole-repo ingest.                                                                                                                      |
| `db`        | string          | no       | Target map database path. Defaults to `<path>/.khive/code-map.db`. The shared production database — its default `$HOME/.khive/khive.db` location and the calling server's actual configured database — is always rejected, with no override. |
| `languages` | array\<string\> | no       | Restrict ingest to a subset of `rust` \| `python` \| `typescript`. Omission accepts all three; the success report lists only languages observed under `path`.                                                                                |
| `tiers`     | array\<string\> | no       | Select any of `l1` \| `l1.5` \| `l2`. Defaults to L1 and L1.5; L2 is opt-in and currently scans Rust sources only.                                                                                                                           |

```
request(ops="code.ingest(path=\"/repo/crates/my-crate\")")
```

The argument object is closed: unknown names are rejected before filesystem or database access.
The success report's sorted `languages` array describes languages observed by a selected tier,
rather than echoing the caller's filter. It also includes `fts_indexed`, the number of entity
documents written to the map's full-text index. Entity and FTS writes are a single success
postcondition for this verb: an FTS failure makes the ingest fail rather than returning a
structurally populated but unsearchable map.

The map database uses the ordinary khive schema. To explore it with the generic KG read verbs,
select it as a backend in a dedicated config:

```toml
[[backends]]
name = "main"
kind = "sqlite"
path = "/absolute/path/to/code-map.db"
```

```sh
kkernel exec --config /absolute/path/to/code-map.toml \
  'search(kind="entity", query="my-crate")'
kkernel exec --config /absolute/path/to/code-map.toml \
  'resolve(refs=["my-crate"])'
```

Use `--config` without `--db` for this read path. With `[[backends]]` configured, a conflicting
concrete `--db` override is refused; `:memory:` remains an explicit ephemeral override, and a path
that canonically matches the declared `main` backend is normalized as a no-op. A warning that a
daemon has a different configuration and local fallback is required is expected and prevents
accidentally serving the production database. `kkernel code-audit` is the distinct policy-driven
reporting surface over a code-map database.

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
byte range. Metadata preflight rejects an object reported above the 64 MiB ceiling before
hydration; the backend's streaming actual-byte bound remains authoritative when metadata is
stale or false-small. A requested slice that would base64-encode past the daemon's IPC frame
cap is also rejected. Concurrent `blob.get` hydration is bounded by the runtime's shared
weighted raw-byte admission; range responses still hydrate and verify the complete object
before slicing.

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
