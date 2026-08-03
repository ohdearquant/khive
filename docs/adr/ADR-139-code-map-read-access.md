# ADR-139: Read Access to Registered Code Map Databases

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

The code pack writes L1 manifest and L1.5 import-scan output to a dedicated map database rather than the shared production graph, and its default target is `<path>/.khive/code-map.db`. [Source: `crates/khive-pack-code/src/vocab.rs:9-40` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

The current code-pack handler table contains one handler, `code.ingest`; its dispatch implementation accepts that verb only, so no code-map read verb exists on the public surface. [Source: `crates/khive-pack-code/src/vocab.rs:9-40` and `crates/khive-pack-code/src/pack.rs:106-120` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

The code-pack's granularity fence keeps exhaustive symbol and call graphs in dedicated map databases rather than `khive.db`. [Source: ADR-085, D6.1 and Amendment 2, B7]

ADR-085 Amendment 4 specifies analysis operations with a caller-supplied `db` parameter and a substantial path and file-identity fence. [Source: ADR-085, Amendment 4, E1-E9]

ADR-028 defines a pack-scoped backend as an operator-declared named SQLite file, assigns each pack instance exactly one backend, and marks the multi-backend configuration and boot path as deferred in the shipped runtime; the shipped configuration parser reads only engine and actor settings and ignores unknown keys. ADR-028 is rationale for this ADR's design shape, not a prerequisite contract: nothing below depends on the deferred multi-backend mechanism, and the configuration record this ADR requires is defined by this ADR itself. [Source: ADR-028, §§1, 2, and 4, and "Deferred" list]

L1 and L1.5 maps contain `project` entities and project-to-project `depends_on` edges alongside the four code subtypes. [Source: ADR-085, Amendment 2 (L1 manifest edges, L1.5 import-scan edges); `crates/khive-pack-code/src/source_ingest.rs:296-312` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

This ADR addresses read access to code maps without changing the storage placement established by ADR-085. [Source: ADR-085, D6.1 and Amendment 2, B7]

## Decision

The code pack SHALL add `code.list`, `code.search`, and `code.deps` as read-only verbs over a required opaque `map` name. [Source: ADR-085, D6.1 and Amendment 2, B7; `crates/khive-pack-code/src/vocab.rs:9-40` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

The three verbs SHALL NOT accept a database path, URI, connection string, file descriptor, or other caller-controlled location. [Source: ADR-085, Amendment 4, E7; ADR-018]

The `map` argument SHALL resolve only through an operator-maintained code-map registry loaded with daemon configuration. [Source: ADR-028, §1 and §8]

### Registered map lifecycle

Each registry entry SHALL contain a unique stable `name`, an absolute configured `path`, and an operator-declared `source_root`; the registry loader SHALL reject duplicate names, relative paths, URI syntax, missing files, non-regular files, and symlinks. [Source: ADR-028, §1; ADR-085, Amendment 4, E7]

The registry loader SHALL reject an entry whose opened-file identity matches the shared production database or one of its SQLite companion files. [Source: ADR-085, Amendment 4, E7]

The loader SHALL canonicalize each accepted entry, record its opened-file identity, and accept it only when its canonical path is within an operator-configured code-map root. [Source: ADR-028, §8; ADR-085, Amendment 4, E7]

`code.ingest` MAY create or update a dedicated map database, but it SHALL NOT make that database readable through the public read surface. [Source: `crates/khive-pack-code/src/handlers.rs:62-94` and `crates/khive-pack-code/src/db_target.rs:58-105` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

An operator SHALL register a map through configuration and a normal configuration reload or restart after the map database exists. [Source: ADR-028, §1 and §8]

Each read request SHALL resolve the map name before any database open, revalidate the configured file against its registered identity at open time, and fail closed if the file has been replaced, removed, or no longer satisfies the registry checks. [Source: ADR-085, Amendment 4, E7]

Deregistering a map SHALL remove it from the public read surface without deleting or changing its database file. [Source: ADR-028, Open Question 5]

### Read-only open contract

After registry resolution, a read verb SHALL open only the registered file through a read-only SQLite constructor that neither creates a missing file nor runs migrations. [Source: ADR-085, Amendment 4, E7]

The read path SHALL use a short read transaction and SHALL close the map handle before returning a response. [Source: ADR-085, Amendment 4, E3-E4]

The implementation SHALL treat the registry as the authorization boundary for map selection; map data cannot grant access to another database. [Source: ADR-018; ADR-028, §1]

### Verb contracts

`code.list(map, entity_type?, project_id?, cursor?, limit?)` SHALL return a stable, cursor-paginated list of live code entities from the selected map. [Source: ADR-085, D2-D3 and Amendment 4, E1]

`entity_type`, when supplied to `code.list`, SHALL be one of `module`, `function`, `datatype`, or `interface`; `project_id`, when supplied, SHALL be a map-local project identifier. [Source: ADR-085, D2-D3]

`code.search(map, query, entity_type?, project_id?, cursor?, limit?)` SHALL return a bounded, ranked list of live code entities from the selected map and SHALL scope every lookup to that map. [Source: ADR-085, D2-D3 and Amendment 4, E1]

`code.deps(map, id, direction?, max_depth?, limit?)` SHALL traverse live `depends_on` edges from a map-local entity identifier and SHALL return a bounded subgraph from the selected map only. [Source: ADR-085, D3 and Amendment 2, B8]

`direction`, when supplied, SHALL be the closed set `outgoing`, `incoming`, or `both`; when omitted, `direction` SHALL default to `both`. `max_depth` and `limit` SHALL have explicit documented maxima and SHALL reject invalid values before the map is opened. [Source: ADR-085, Amendment 4, E8]

Each response SHALL echo the resolved map name and SHALL never return the configured filesystem path. [Source: ADR-018; ADR-085, Amendment 4, E7]

The implementation SHALL define deterministic ordering for equal-ranked search results and for every list and dependency traversal response. [Source: ADR-085, Amendment 4, E2 and E9]

Every response envelope SHALL be the verb-specific `result` value of the request DSL's per-op envelope (`ok`/`tool`/`result`), not a bare array. [Source: ADR-016, lines 203-225 and 376-390]

#### Numeric parameter bounds

Every bounded numeric or cursor-adjacent parameter below is validated before the map is opened; an out-of-range, non-integer, or malformed value SHALL produce a per-op validation error naming the parameter and its valid range or shape. [Source: ADR-085, Amendment 4, E8]

| Verb          | Parameter             | Minimum | Maximum | Default |
| ------------- | --------------------- | ------- | ------- | ------- |
| `code.list`   | `limit`               | 1       | 500     | 100     |
| `code.search` | `limit`               | 1       | 200     | 20      |
| `code.deps`   | `max_depth`           | 1       | 10      | 2       |
| `code.deps`   | `limit` (total nodes) | 1       | 1000    | 200     |

#### Request field types

Every parameter below is typed and validated before the map is opened; an omitted optional parameter behaves per its Default column, never per an implementation-chosen convention.

| Verb          | Parameter             | Type                                                         | Required | Default                                                                                                   |
| ------------- | --------------------- | ------------------------------------------------------------ | -------- | --------------------------------------------------------------------------------------------------------- |
| `code.list`   | `map`                 | string (registered map name)                                 | yes      | —                                                                                                         |
| `code.list`   | `entity_type`         | string, one of `module`, `function`, `datatype`, `interface` | no       | none (no filter applied)                                                                                  |
| `code.list`   | `project_id`          | string (map-local identifier)                                | no       | none (no filter applied)                                                                                  |
| `code.list`   | `cursor`              | string (opaque token)                                        | no       | none (first page)                                                                                         |
| `code.list`   | `limit`               | integer                                                      | no       | 100                                                                                                       |
| `code.search` | `map`                 | string (registered map name)                                 | yes      | —                                                                                                         |
| `code.search` | `query`               | string, non-empty                                            | yes      | —                                                                                                         |
| `code.search` | `entity_type`         | string, one of `module`, `function`, `datatype`, `interface` | no       | none (no filter applied)                                                                                  |
| `code.search` | `project_id`          | string (map-local identifier)                                | no       | none (no filter applied)                                                                                  |
| `code.search` | `cursor`              | string (opaque token)                                        | no       | none (first page); valid only when replayed against an identical `query`, `entity_type`, and `project_id` |
| `code.search` | `limit`               | integer                                                      | no       | 20                                                                                                        |
| `code.deps`   | `map`                 | string (registered map name)                                 | yes      | —                                                                                                         |
| `code.deps`   | `id`                  | string (map-local entity identifier)                         | yes      | —                                                                                                         |
| `code.deps`   | `direction`           | string, one of `outgoing`, `incoming`, `both`                | no       | `both`                                                                                                    |
| `code.deps`   | `max_depth`           | integer                                                      | no       | 2                                                                                                         |
| `code.deps`   | `limit` (total nodes) | integer                                                      | no       | 200                                                                                                       |

#### Response field types

Every response field below is typed; a field marked nullable returns JSON `null` rather than an absent key.

| Verb          | Field                 | Type                                                         | Nullable                 |
| ------------- | --------------------- | ------------------------------------------------------------ | ------------------------ |
| `code.list`   | `map`                 | string                                                       | no                       |
| `code.list`   | `items[].id`          | string                                                       | no                       |
| `code.list`   | `items[].entity_type` | string, one of `module`, `function`, `datatype`, `interface` | no                       |
| `code.list`   | `items[].name`        | string                                                       | no                       |
| `code.list`   | `items[].project_id`  | string                                                       | yes                      |
| `code.list`   | `next_cursor`         | string (opaque token)                                        | yes (`null` = last page) |
| `code.search` | `map`                 | string                                                       | no                       |
| `code.search` | `items[].id`          | string                                                       | no                       |
| `code.search` | `items[].entity_type` | string, one of `module`, `function`, `datatype`, `interface` | no                       |
| `code.search` | `items[].name`        | string                                                       | no                       |
| `code.search` | `items[].project_id`  | string                                                       | yes                      |
| `code.search` | `items[].score`       | number (floating point, higher ranks first)                  | no                       |
| `code.search` | `next_cursor`         | string (opaque token)                                        | yes (`null` = last page) |
| `code.deps`   | `map`                 | string                                                       | no                       |
| `code.deps`   | `root`                | string                                                       | no                       |
| `code.deps`   | `nodes[].id`          | string                                                       | no                       |
| `code.deps`   | `nodes[].entity_type` | string, one of `module`, `function`, `datatype`, `interface` | no                       |
| `code.deps`   | `nodes[].name`        | string                                                       | no                       |
| `code.deps`   | `edges[].source`      | string (an `id` present in `nodes`)                          | no                       |
| `code.deps`   | `edges[].target`      | string (an `id` present in `nodes`)                          | no                       |
| `code.deps`   | `truncated`           | boolean                                                      | no                       |

#### `code.list` response

The success value is `{ "map": <resolved map name>, "items": [ { "id", "entity_type", "name", "project_id" } ], "next_cursor": <string> | null }`. Rows order by `name` ascending, ties broken by `id` ascending — a total order, so two identical calls against an unchanged map return rows in the same sequence. [Source: ADR-085, Amendment 4, E2 and E9, applying the same total-order pattern used for `code.coupling`.]

`cursor` is an opaque token encoding the `(name, id)` of the last row returned on the prior page; a `cursor` that does not decode to that shape, or that does not correspond to a value the selected map could have produced, SHALL be rejected with a validation error rather than treated as "start of list."

#### `code.search` response

The success value is `{ "map": <resolved map name>, "items": [ { "id", "entity_type", "name", "project_id", "score" } ], "next_cursor": <string> | null }`. Rows order by `score` descending, ties broken by `id` ascending. `cursor` encodes the `(score, id)` of the last row returned; it is valid only when replayed against an identical `query`, `entity_type`, and `project_id` — a cursor replayed against a different query SHALL be rejected with a validation error, because ranked order is not guaranteed stable across distinct queries.

#### `code.deps` response

The success value is `{ "map": <resolved map name>, "root": <id>, "nodes": [ { "id", "entity_type", "name" } ], "edges": [ { "source", "target" } ], "truncated": <bool> }`. Traversal SHALL proceed breadth-first from `root`, ordering nodes by depth ascending and, within a depth, by `id` ascending — a total order over the traversal itself, independent of `limit`. When the `limit` bound on total returned nodes is reached before traversal completes, the response SHALL set `truncated: true` and stop expanding rather than silently omit edges among already-returned nodes. `id` values not resolvable in the selected map SHALL produce a distinct not-found validation error before any traversal begins.

### Relationship to the existing map-analysis design

This ADR supersedes the caller-facing `db` parameter in ADR-085 Amendment 4 for `code.coupling`, `code.health`, and `code.cycles`; those verbs SHALL take the same registered `map` argument when implemented. [Source: ADR-085, Amendment 4, E2-E4 and E7]

This ADR does not change `code.ingest`'s write-target contract. [Source: `crates/khive-pack-code/src/vocab.rs:9-40` and `crates/khive-pack-code/src/db_target.rs:58-105` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

## Consequences

The public read surface can select several independently built code maps without exposing arbitrary filesystem opens to callers. [Source: ADR-085, D6.1; ADR-028, §1]

Map availability becomes an explicit operator lifecycle with registration, reload, revalidation, and deregistration steps. [Source: ADR-028, §1 and §8]

An ingested map is not automatically queryable, so operators must register a map before clients can use the new read verbs. [Source: `crates/khive-pack-code/src/handlers.rs:62-94` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`; ADR-028, §1]

Replacing or relocating a map requires registry revalidation or re-registration before it can be read again. [Source: ADR-085, Amendment 4, E7]

The implementation needs a small configuration registry and read-only map opener, but it does not require the deferred multi-backend boot path or cross-backend coordinator routing. [Source: ADR-028, §§1, 4, 8, and 9]

## Alternatives considered

### Mount every map as a queryable backend through the multi-backend mechanism

This alternative would make each map a named backend and would use the existing multi-backend model for map selection. [Source: ADR-028, §§2, 4, and 8]

It is rejected because ADR-028 assigns each pack instance exactly one backend, while the code read surface must select multiple independently built maps through one code-pack interface. [Source: ADR-028, §4]

It is also rejected because the shipped configuration and boot path do not yet implement backend declarations, per-pack backend assignment, or per-pack runtime instances. [Source: ADR-028, §1]

The registered-map design preserves ADR-028's distinction between operator-declared topology and pack behavior while avoiding a new routing and coordinator requirement for map reads. [Source: ADR-028, §§4, 8, and 9]

### Accept a caller-supplied `db` path on each read verb

ADR-085 Amendment 4 specifies this form for planned analysis verbs and defines path, symlink, hard-link, and production-database protections. [Source: ADR-085, Amendment 4, E7 and E9]

It is rejected because the caller would still select the database location, whereas the registered-map contract makes map selection an operator-controlled name lookup. [Source: ADR-018; ADR-028, §1]

The registry also creates an explicit map lifecycle and prevents a new read endpoint from becoming a general filesystem-query interface. [Source: ADR-028, §1; ADR-085, D6.1]

### Query code structure through the shared production graph

This alternative is rejected because ADR-085 reserves exhaustive code-map data for dedicated map databases rather than the shared production graph. [Source: ADR-085, D6.1 and Amendment 2, B7]

The generic code subtype filters read the shared production database, while `code.ingest` writes to a dedicated map database that the daemon does not attach, so code-map rows are structurally unreachable through those filters. [Source: ADR-085, D6.1; `crates/khive-pack-code/src/db_target.rs:58-105` and `crates/khive-pack-code/src/handlers.rs:62-94` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

## References

- ADR-018: Authorization Gate.
- ADR-028: Pack-Scoped Backends and Per-Pack Schema Declaration.
- ADR-085: Code Pack, including Amendments 2 and 4.
- `crates/khive-pack-code/src/vocab.rs:9-40` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`.
- `crates/khive-pack-code/src/handlers.rs:62-94` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`.
- `crates/khive-pack-code/src/db_target.rs:58-105` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`.
- `crates/khive-pack-code/src/pack.rs:106-120` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`.
- `crates/khive-pack-code/src/source_ingest.rs:296-312` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`.
