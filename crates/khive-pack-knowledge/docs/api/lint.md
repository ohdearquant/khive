# Graph lint

`knowledge.lint` inspects the caller-visible graph of entities, notes and edges.
Knowledge corpus atoms, domains and sections are separate records and do not
enter its statistics merely because the knowledge pack owns them. The request
token supplies caller identity and visibility.

Omit `rules` to evaluate all thirteen built-ins, or supply a nonempty array of
exact rule IDs. Repeated IDs are evaluated once. `severity` is the minimum level
reported: `error`, `warn` (default), or `info`. `fix` defaults to `false`.
Nulls, invalid values and wrong types are refused.

| Rule                     | Severity | Checks                                                      |
| ------------------------ | -------- | ----------------------------------------------------------- |
| `orphan-entity`          | error    | Live entity has no visible incident edges                   |
| `dangling-edge`          | error    | Edge has a confirmed soft-deleted endpoint                  |
| `min-edge-density`       | error    | Concept degree below 4, project below 3, or person below 1  |
| `direction-violation`    | error    | Resolved endpoint types violate current shared link rules   |
| `missing-entity-type`    | warn     | Project/resource lacks a nonempty canonical entity type     |
| `missing-introduced-by`  | warn     | Legacy concept paper lacks a valid live outgoing introducer |
| `missing-implements`     | warn     | Project lacks a valid live implements edge to a concept     |
| `duplicate-name`         | warn     | Live entities share exact namespace, kind and name          |
| `superseded-not-marked`  | warn     | Live superseded entity lacks deprecated status              |
| `concept-no-parent`      | warn     | Concept lacks a valid live parent edge                      |
| `paper-missing-metadata` | info     | Legacy concept paper lacks authors, year or source members  |
| `low-weight-edge`        | info     | Edge weight is strictly below 0.3                           |
| `note-no-annotates`      | info     | Note lacks a valid live outgoing annotation                 |

`rules=["stats-only"]` is a sole selector and cannot request fixes. It returns
exactly `{stats: ...}`. Other selections return `violations`, `summary` and
`stats`. Statistics always cover the complete visible observation before fixes,
regardless of rule selection or severity. Density counts distinct incident live
edge IDs, including a dangling edge at its live end and a self-loop once. Property
coverage counts top-level member presence, including explicit nulls.

`fix=true` adds a `fixes` array and `summary.fixed`, counting confirmed committed
actions. The three families soft-delete an unchanged dangling edge, propose a
question about observed duplicates, and mark an unchanged superseded entity
deprecated. They preserve ordinary mutation authorization and events. A duplicate
proposal never merges entities or selects a winner.

All candidates are prepared before the first mutation, and each action checks its
observed revisions and witnesses within one owning database transaction. A changed
predicate produces `conflict`; ambiguous ownership, a witness in another database,
or missing atomic support produces `unsupported`. These receipts retain the
original violations and statistics. Earlier confirmed actions remain committed
if a later action fails.

A policy/storage failure during actions returns a typed error containing
`details.lint_fix`: the failed action, confirmed receipts and the frozen report.
An unknown write settlement stops processing without retry or a rollback claim.
The enclosing multi-action error reports unknown domain disposition and is not
retryable; its per-action receipts describe confirmed effects.

Graph pages, endpoint types and visibility are checked before reports or counts
disclose records. Read, decoding and deadline failures refuse the inspection
instead of returning partial success. This is a captured observation across
registered graph backends, not a cross-database snapshot guarantee.

The verb is classified Write because its strongest requested effect can mutate
records. An inspection does not change domain rows; normal dispatch auditing still
applies. Custom rules and `knowledge.lint_config` remain deferred.

The governing contract is [ADR-048, section 9](../../../../docs/adr/ADR-048-knowledge-section-profiles.md#9-kg-lint--configurable-graph-hygiene-rules-deferred).
