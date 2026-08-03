# ADR-139: Read Access to Registered Code Map Databases

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

The code pack writes L1 manifest and L1.5 import-scan output to a dedicated map database rather than the shared production graph, and its default target is `<path>/.khive/code-map.db`. [Source: `crates/khive-pack-code/src/vocab.rs:9-40` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

The current code-pack handler table contains one handler, `code.ingest`; its dispatch implementation accepts that verb only, so no code-map read verb exists on the public surface. [Source: `crates/khive-pack-code/src/vocab.rs:9-40` and `crates/khive-pack-code/src/pack.rs:106-120` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

The code-pack's granularity fence keeps exhaustive symbol and call graphs in dedicated map databases rather than `khive.db`. [Source: ADR-085, D6.1 and Amendment 2, B7]

ADR-085 Amendment 4 specifies analysis operations with a caller-supplied `db` parameter and a substantial path and file-identity fence. [Source: ADR-085, Amendment 4, E1-E9]

ADR-028 defines a pack-scoped backend as an operator-declared named SQLite file, assigns each pack instance exactly one backend, and marks the multi-backend configuration and boot path as deferred in the shipped runtime; the shipped configuration parser does not parse a `code_maps` or `code_map_root` key, and it ignores unknown keys. ADR-028 is rationale for this ADR's design shape, not a prerequisite contract: nothing below depends on the deferred multi-backend mechanism, and the configuration record this ADR requires is defined by this ADR itself. [Source: ADR-028, §§1, 2, and 4, and "Deferred" list; `crates/khive-runtime/src/engine_config.rs:341-401` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

L1 and L1.5 maps contain `project` entities and project-to-project `depends_on` edges alongside the four code subtypes. [Source: ADR-085, Amendment 2 (L1 manifest edges, L1.5 import-scan edges); `crates/khive-pack-code/src/source_ingest.rs:296-312` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

This ADR addresses read access to code maps without changing the storage placement established by ADR-085. [Source: ADR-085, D6.1 and Amendment 2, B7]

## Decision

The code pack SHALL add `code.list`, `code.search`, and `code.deps` as read-only verbs over a required opaque `map` name. [Source: ADR-085, D6.1 and Amendment 2, B7; `crates/khive-pack-code/src/vocab.rs:9-40` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

The three verbs SHALL NOT accept a database path, URI, connection string, file descriptor, or other caller-controlled location. [Source: ADR-085, Amendment 4, E7; ADR-018]

The `map` argument SHALL resolve only through an operator-maintained code-map registry, defined by this ADR and loaded from daemon configuration. [Source: ADR-018; ADR-028, §1 and §8]

### Registry configuration contract

The registry's source of truth is a `[[code_maps]]` array in the daemon's configuration file, defined by this ADR: each entry declares the registered `name` (string, unique across entries), the absolute `path` of the map database file, and the operator-declared absolute `source_root` the map's contents describe. The shipped configuration parser does not currently parse `code_maps` or `code_map_root`, so implementing this ADR includes adding both keys to the parser; until then the registry is empty and every read verb fails closed at map resolution. [Source: ADR-028, §1 and "Deferred" list; `crates/khive-runtime/src/engine_config.rs:341-401` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

Beside the `[[code_maps]]` array, the configuration SHALL define a scalar `code_map_root` key: a string holding an absolute path to an existing directory, canonicalized at load time. Relative paths, URI syntax, missing directories, non-directories, and paths failing canonicalization are invalid values. `code_map_root` bounds where registered map database files may live; it has no relationship to any entry's `source_root`, which names the source tree a map's contents describe and is never a storage boundary. When at least one `[[code_maps]]` entry is present and `code_map_root` is absent or invalid, the loader SHALL register no entries: each entry is rejected with the missing-or-invalid-root reason under the per-entry logging rule below. Containment is checked per entry: an entry is accepted only when its canonicalized `path` resolves inside the canonicalized `code_map_root` by whole path components; string-prefix comparison without a component boundary SHALL NOT be used. [Source: ADR-018; ADR-085, Amendment 4, E7]

The registry loads at exactly two points — daemon startup and an explicit operator-initiated configuration reload — and no runtime verb, request, or map content can create, alter, or remove an entry. Registration is a configuration edit followed by a reload or restart; deregistration is removal of the entry followed by the same, and it removes the map from the read surface without deleting or changing the database file. A reload takes effect for every request admitted after it; an in-flight request holds its already-opened handle at most until its short read transaction ends. [Source: ADR-018; ADR-028, §8]

Validation is per entry, at load time, fail closed: an entry failing any check in "Registered map lifecycle" below SHALL NOT be registered, and the loader SHALL log that entry's name and the failing check for the operator. A rejected entry aborts neither daemon startup nor the reload nor the loading of other entries — an unregistered map is simply unreadable, which is this surface's intended failure direction. A read request naming an unknown `map` receives one per-op validation error that does not distinguish never-configured from rejected-at-load; that distinction is operator-facing, in the load log, never caller-facing. [Source: ADR-018]

### Registered map lifecycle

Each registry entry SHALL contain a unique stable `name`, an absolute configured `path`, and an operator-declared `source_root`; the registry loader SHALL reject duplicate names, relative paths, URI syntax, missing files, non-regular files, and symlinks. [Source: ADR-028, §1; ADR-085, Amendment 4, E7]

The registry loader SHALL reject an entry whose opened-file identity matches the shared production database or one of its SQLite companion files. [Source: ADR-085, Amendment 4, E7]

The loader SHALL canonicalize each accepted entry, record its opened-file identity, and accept it only when its canonical path is contained within the configured `code_map_root` as defined in the Registry configuration contract above. [Source: ADR-028, §8; ADR-085, Amendment 4, E7]

Each successful registration SHALL stamp the entry with a registration generation drawn from a monotonically increasing counter: every load event that registers an entry — initial startup or a reload, whether or not the file changed — assigns a fresh generation, and the counter never repeats a value within a daemon run. The generation is what makes registration lifecycle events observable to cursor validation below; a daemon restart additionally invalidates all outstanding cursors, and the implementation SHALL make pre-restart cursors rejectable, for example by folding a per-run boot identifier into the generation. [Source: ADR-085, Amendment 4, E2]

`code.ingest` MAY create or update a dedicated map database, but it SHALL NOT make that database readable through the public read surface. [Source: `crates/khive-pack-code/src/handlers.rs:62-94` and `crates/khive-pack-code/src/db_target.rs:58-105` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

An operator SHALL register a map through configuration and a normal configuration reload or restart after the map database exists. [Source: ADR-028, §1 and §8]

Each read request SHALL resolve the map name before any database open, revalidate the configured file against its registered identity at open time, and fail closed if the file has been replaced, removed, or no longer satisfies the registry checks. [Source: ADR-085, Amendment 4, E7]

Deregistering a map SHALL remove it from the public read surface without deleting or changing its database file. [Source: ADR-028, Open Question 5]

### Read-only open contract

After registry resolution, a read verb SHALL open only the registered file through a read-only SQLite constructor that neither creates a missing file nor runs migrations. [Source: ADR-085, Amendment 4, E7]

Registry resolution replaces caller path selection and nothing else; it SHALL NOT weaken any per-open protection of ADR-085 Amendment 4 E7, all of which apply to every open of a registered map. The open SHALL go through a no-follow VFS for the main database file and for every SQLite companion file the connection touches (`-journal`, `-wal`, `-shm`). After open and before any row is read, three distinct checks apply: the opened main database descriptor's `(device, inode)` identity SHALL equal the map's registered identity; every opened descriptor — the main file and each companion alike — SHALL match no identity in the shared production database's identity set; and a writable companion whose link count exceeds one SHALL be rejected. A companion descriptor is never compared against the registered main-file identity — a `-wal`, `-shm`, or `-journal` file is a distinct file with its own identity, and the registered-identity check applies to the main descriptor only. Any failed check SHALL close the handle and fail the request, fail closed. [Source: ADR-085, Amendment 4, E7]

The implementation SHALL carry a post-registration companion-swap acceptance test: register a valid map, then replace one of its companion files with a symlink to — or a hard link sharing identity with — a production companion, and assert that the next read of that map fails closed without reading through the link. A registry check at load time cannot observe a swap that happens after load; only the per-open fence can, which is why registration does not retire it. [Source: ADR-085, Amendment 4, E7]

Beside that rejection arm, the implementation SHALL carry a positive acceptance test: register a valid WAL-mode map whose legitimate `-wal` and `-shm` companions exist on disk, and assert that a read succeeds and returns rows. An implementation that rejects every companion descriptor for failing to equal the main-file identity passes the swap-rejection arm and fails this one; the pair is what makes the main-versus-companion distinction executable. [Source: ADR-085, Amendment 4, E7]

The read path SHALL use a short read transaction and SHALL close the map handle before returning a response. [Source: ADR-085, Amendment 4, E3-E4]

The implementation SHALL treat the registry as the authorization boundary for map selection; map data cannot grant access to another database. [Source: ADR-018; ADR-028, §1]

### Verb contracts

#### The `kind` discriminator

Read requests and responses use a normalized five-value discriminator named `kind`: `project`, `module`, `function`, `datatype`, or `interface`. `kind` is defined by projection from storage, not stored as one column. A row projects to `kind: "project"` exactly when its base entity kind is `project`; it projects to one of the four code values exactly when its base entity kind is `concept` and its registered `EntityTypeRegistry` token is that value. This is the accepted taxonomy unchanged — repositories and crates are base `project` entities, and the four code subtypes are `concept` rows carrying an `entity_type` token — and this surface adds a read-side projection over it, never a fifth `entity_type` token. [Source: ADR-085, D1-D2; `crates/khive-pack-code/src/source_ingest.rs:296-312` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

A `kind` filter maps to storage by the same projection: `kind=project` filters to rows whose base entity kind is `project`; a code value filters to rows whose base entity kind is `concept` and whose `entity_type` token equals that value. A row matching neither projection arm — possible only when a foreign writer placed rows this pack's ingest path does not produce — is outside the read surface's row population under every filter and under no filter alike: a deterministic population rule, not truncation of contract rows. A `project` row's own project-subtype token (`repository`, `library`, `tool`), when present in storage, is not exposed by this surface and never occupies `kind`. [Source: ADR-085, D1-D2 and Amendment 2]

#### Verb signatures

`code.list(map, kind?, project_id?, cursor?, limit?)` SHALL return a stable, cursor-paginated list of live code entities from the selected map. [Source: ADR-085, D2-D3 and Amendment 4, E1]

`kind`, when supplied to `code.list`, SHALL be one of the five discriminator values above; `project_id`, when supplied, SHALL be a map-local project identifier. [Source: ADR-085, D2-D3 and Amendment 2; Context]

The same five-value set governs responses, filtered or not: L1 and L1.5 maps contain `project` entities and project-to-project `depends_on` edges (Context), and the read surface SHALL represent every kind the map holds rather than silently omit rows an unfiltered request matched. A row whose `kind` is `project` SHALL carry `project_id: null`; a code-value row's `project_id` names its containing project. [Source: ADR-085, Amendment 2; `crates/khive-pack-code/src/source_ingest.rs:296-312` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`]

`code.search(map, query, kind?, project_id?, cursor?, limit?)` SHALL return a bounded, ranked list of live code entities from the selected map and SHALL scope every lookup to that map. [Source: ADR-085, D2-D3 and Amendment 4, E1]

`code.deps(map, id, direction?, max_depth?, limit?)` SHALL traverse live `depends_on` edges from a map-local entity identifier and SHALL return a bounded subgraph from the selected map only. `id` MAY name a `project` entity, and project-to-project `depends_on` edges traverse exactly as code-subtype edges do. [Source: ADR-085, D3 and Amendment 2, B8]

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

| Verb          | Parameter             | Type                                                                    | Required | Default                                       |
| ------------- | --------------------- | ----------------------------------------------------------------------- | -------- | --------------------------------------------- |
| `code.list`   | `map`                 | string (registered map name)                                            | yes      | —                                             |
| `code.list`   | `kind`                | string, one of `project`, `module`, `function`, `datatype`, `interface` | no       | none (no filter applied)                      |
| `code.list`   | `project_id`          | string (map-local identifier)                                           | no       | none (no filter applied)                      |
| `code.list`   | `cursor`              | string (opaque token)                                                   | no       | none (first page)                             |
| `code.list`   | `limit`               | integer                                                                 | no       | 100                                           |
| `code.search` | `map`                 | string (registered map name)                                            | yes      | —                                             |
| `code.search` | `query`               | string, non-empty                                                       | yes      | —                                             |
| `code.search` | `kind`                | string, one of `project`, `module`, `function`, `datatype`, `interface` | no       | none (no filter applied)                      |
| `code.search` | `project_id`          | string (map-local identifier)                                           | no       | none (no filter applied)                      |
| `code.search` | `cursor`              | string (opaque token)                                                   | no       | none (first page); bound per "Cursor binding" |
| `code.search` | `limit`               | integer                                                                 | no       | 20                                            |
| `code.deps`   | `map`                 | string (registered map name)                                            | yes      | —                                             |
| `code.deps`   | `id`                  | string (map-local entity identifier)                                    | yes      | —                                             |
| `code.deps`   | `direction`           | string, one of `outgoing`, `incoming`, `both`                           | no       | `both`                                        |
| `code.deps`   | `max_depth`           | integer                                                                 | no       | 2                                             |
| `code.deps`   | `limit` (total nodes) | integer                                                                 | no       | 200                                           |

#### Response field types

Every response field below is typed; a field marked nullable returns JSON `null` rather than an absent key.

| Verb          | Field                | Type                                                                    | Nullable                 |
| ------------- | -------------------- | ----------------------------------------------------------------------- | ------------------------ |
| `code.list`   | `map`                | string                                                                  | no                       |
| `code.list`   | `items[].id`         | string                                                                  | no                       |
| `code.list`   | `items[].kind`       | string, one of `project`, `module`, `function`, `datatype`, `interface` | no                       |
| `code.list`   | `items[].name`       | string                                                                  | no                       |
| `code.list`   | `items[].project_id` | string                                                                  | yes                      |
| `code.list`   | `next_cursor`        | string (opaque token)                                                   | yes (`null` = last page) |
| `code.search` | `map`                | string                                                                  | no                       |
| `code.search` | `items[].id`         | string                                                                  | no                       |
| `code.search` | `items[].kind`       | string, one of `project`, `module`, `function`, `datatype`, `interface` | no                       |
| `code.search` | `items[].name`       | string                                                                  | no                       |
| `code.search` | `items[].project_id` | string                                                                  | yes                      |
| `code.search` | `items[].score`      | number (floating point, higher ranks first)                             | no                       |
| `code.search` | `next_cursor`        | string (opaque token)                                                   | yes (`null` = last page) |
| `code.deps`   | `map`                | string                                                                  | no                       |
| `code.deps`   | `root`               | string                                                                  | no                       |
| `code.deps`   | `nodes[].id`         | string                                                                  | no                       |
| `code.deps`   | `nodes[].kind`       | string, one of `project`, `module`, `function`, `datatype`, `interface` | no                       |
| `code.deps`   | `nodes[].name`       | string                                                                  | no                       |
| `code.deps`   | `edges[].source`     | string (an `id` present in `nodes`)                                     | no                       |
| `code.deps`   | `edges[].target`     | string (an `id` present in `nodes`)                                     | no                       |
| `code.deps`   | `truncated`          | boolean                                                                 | no                       |

#### `code.list` response

The success value is `{ "map": <resolved map name>, "items": [ { "id", "kind", "name", "project_id" } ], "next_cursor": <string> | null }`. Rows order by `name` ascending, ties broken by `id` ascending — a total order, so two identical calls against an unchanged map return rows in the same sequence. [Source: ADR-085, Amendment 4, E2 and E9, applying the same total-order pattern used for `code.coupling`.]

`cursor` is an opaque token encoding the `(name, id)` of the last row returned on the prior page, under the bindings in "Cursor binding" below; a `cursor` that does not decode to that shape, or that does not correspond to a value the selected map could have produced, SHALL be rejected with a validation error rather than treated as "start of list."

#### `code.search` response

The success value is `{ "map": <resolved map name>, "items": [ { "id", "kind", "name", "project_id", "score" } ], "next_cursor": <string> | null }`. Rows order by `score` descending, ties broken by `id` ascending. `cursor` encodes the `(score, id)` of the last row returned, under the bindings in "Cursor binding" below — ranked order is not guaranteed stable across distinct queries, which is why the query is a bound value.

The search contract is deliberately minimal and deterministic. The indexed field is the entity `name`; `query` is a literal string with no operator grammar, and a `query` that is empty or entirely whitespace SHALL be rejected before the map is opened. Matching is case-insensitive: a row is returned only when `query` matches its `name` exactly, as a prefix, or as an interior substring. `score` is a floating-point number whose scale is implementation-defined; its ordering is not: an exact name match SHALL rank strictly above every prefix match, and a prefix match strictly above every interior-substring match, with ties inside a band broken by `id` ascending. A consumer SHALL rely only on the ordering and band guarantees, never on score values or their differences. The implementation SHALL carry a fixture map whose entity names exercise all three match bands plus a non-matching control, asserting the relative order — the fixture is this contract's executable form, and a richer query grammar would be a superseding amendment, not an implementation liberty. [Source: ADR-085, Amendment 4, E2 and E9]

#### `code.deps` response

The success value is `{ "map": <resolved map name>, "root": <id>, "nodes": [ { "id", "kind", "name" } ], "edges": [ { "source", "target" } ], "truncated": <bool> }`. Traversal SHALL proceed breadth-first from `root`, ordering nodes by depth ascending and, within a depth, by `id` ascending — a total order over the traversal itself, independent of `limit`. When the `limit` bound on total returned nodes is reached before traversal completes, the response SHALL set `truncated: true` and stop expanding rather than silently omit edges among already-returned nodes. `id` values not resolvable in the selected map SHALL produce a distinct not-found validation error before any traversal begins.

#### Cursor binding

Every cursor token binds the full context that produced it: the resolved map name, the map's registration generation as defined in "Registered map lifecycle", and every result-affecting parameter of the issuing request — for `code.list`, `kind` and `project_id`; for `code.search`, `query`, `kind`, and `project_id`. Replaying a cursor with any bound value differing from the replaying request SHALL be rejected with a per-op validation error naming the cursor as stale, never treated as "start of list" and never silently applied to the new context. Deregistering a map ends its registration; re-registering it — even at the same path over an unchanged file — assigns a fresh registration generation and therefore invalidates every outstanding cursor for that map; the caller restarts from the first page. A cursor is short-lived pagination state, not a durable bookmark. [Source: ADR-085, Amendment 4, E2]

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
- `crates/khive-runtime/src/engine_config.rs:341-401` at commit `9442ec2c52290120c5bf4a4c8a1dc771102658dd`.
