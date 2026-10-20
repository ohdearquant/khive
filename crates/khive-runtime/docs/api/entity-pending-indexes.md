# Deferred entity index publication

An entity update that publishes required indexes after committing its row now records
its indexing obligation in that same transaction. If the obligation cannot be stored,
the entity update rolls back. This implements the first slice of
[ADR-014 Amendment 7](../../../../docs/adr/ADR-014-curation-operations.md#amendment-7-recover-unfinished-indexes-after-a-committed-entity-update-4755).

`update_entity_with_index_report` returns the committed entity together with embedding
truncation accounting and an `indexing` report. The public `update` response includes
that report:

- `complete`: every target for this update was confirmed published.
- `unfinished`: `unfinished` names each target and its failure, blocking, supersession,
  or unknown-outcome stage.
- `not_requested`: this patch did not request indexing. This is not a claim that no
  older indexing obligation exists.

An unfinished target does not ask the caller to repeat the entity mutation. The older
embedding-report-only Rust methods instead return a structured, non-retryable
`post_commit_degraded` error with the committed record ID and unfinished targets.
Event-append degradation is also reported after the entity commits.

The record is keyed by entity ID within its domain store. It stores the observed
namespace and version, a new generation, first-pending time, and per-target state.
FTS is always required. One pre-transaction registry snapshot supplies model storage
names and dimensions. A later indexed update carries unfinished model obligations;
conflicting dimensions remain blocked without losing either definition or retry
history. Property-only and tag-only patches create no new obligation.

Each target publishes in its own transaction after checking the generation, unfinished
state, and live entity revision. Vector replacement, provenance clearing, ANN-log
updates and target completion commit together. A failed target leaves healthy targets
intact. The final completion removes the record. A schema trigger removes pending
work on every physical entity deletion, including connections with foreign keys off.
An unknown publication outcome remains unknown even if the record subsequently
appears absent.

This slice adds no background recovery. Failed targets remain durable until a later
covered update or a future recovery component handles them. Note updates, merges,
model-revision tracking and backfill are unchanged. Namespace-move integration is
a required follow-on: the existing move census refuses moves from a namespace with
pending rows until a subject-keyed disposition and generation refresh are added.
