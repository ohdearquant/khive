# PR #341 Review - ADR-004/009/014 Event observable + provenance

## Verdict

REQUEST CHANGES

Local gates pass, but the PR does not complete cluster-06. The remaining problems are accepted ADR contract violations, not style issues:

- ADR-014 curation operations still do not emit typed curation events.
- `brain.emit` creates `FeedbackExplicit` events that do not project the required `Signal` provenance row.
- The event list wire surface cannot express event kind, session, observed, or selected filters even though storage implements them.

Findings: 0 critical, 3 major, 0 minor.

## Findings

### Major 1. ADR-014 curation audit trail is still missing for update/delete/merge paths

ADR-014 requires every curation operation to emit an `EventStore` event: `update_entity -> entity_updated`, `update_edge -> edge_updated`, `update_note -> note_updated`, `merge_entity -> entity_merged`, `delete_entity -> entity_deleted`, `delete_edge -> edge_deleted`, and `delete_note -> note_deleted` (`docs/adr/ADR-014-curation-operations.md:353`).

That is not what this implementation does:

- `update_entity` mutates storage, reindexes, and returns `Ok(entity)` with no event append (`crates/khive-runtime/src/curation.rs:109`, `crates/khive-runtime/src/curation.rs:145`, `crates/khive-runtime/src/curation.rs:151`).
- `merge_entity` commits the merge and returns `Ok(summary)` with no `EntityMerged` event (`crates/khive-runtime/src/curation.rs:164`, `crates/khive-runtime/src/curation.rs:197`, `crates/khive-runtime/src/curation.rs:212`).
- `delete_note`, `delete_entity`, `update_edge`, and `delete_edge` all return after mutating their stores without appending the typed lifecycle event (`crates/khive-runtime/src/operations.rs:1287`, `crates/khive-runtime/src/operations.rs:1348`, `crates/khive-runtime/src/operations.rs:1404`, `crates/khive-runtime/src/operations.rs:1451`, `crates/khive-runtime/src/operations.rs:1515`, `crates/khive-runtime/src/operations.rs:1539`, `crates/khive-runtime/src/operations.rs:1552`, `crates/khive-runtime/src/operations.rs:1591`).
- The KG pack handlers only dispatch to those runtime methods and serialize the result; they do not emit events around the successful mutation (`crates/khive-pack-kg/src/handlers.rs:958`, `crates/khive-pack-kg/src/handlers.rs:964`, `crates/khive-pack-kg/src/handlers.rs:990`, `crates/khive-pack-kg/src/handlers.rs:998`, `crates/khive-pack-kg/src/handlers.rs:1005`, `crates/khive-pack-kg/src/handlers.rs:1028`).
- The registry-level event is only a generic `EventKind::Audit` gate event, not the required typed curation state transition (`crates/khive-runtime/src/pack.rs:491`).

Impact: event consumers cannot reconstruct or observe actual curation state transitions. This also leaves F037 unaddressed for the changed public behavior. A passing audit gate event is not equivalent to `EntityUpdated`, `EdgeDeleted`, or `EntityMerged`.

Fix: emit typed events after successful curation mutations, with the acted-on record as `target_id`, correct `SubstrateKind`, payload fields matching ADR-014 (`id`, `namespace`, `changed_fields`, `hard`, merge policy, rewired edge counts), and projection rows per ADR-041. Add tests that call `update`, `delete`, and `merge` through the KG verb surface and assert the typed events are queryable.

### Major 2. `brain.emit` feedback events silently lose their `Signal` provenance

ADR-041 says `FeedbackExplicit` emitters MUST project a `Signal` role for the entity or note the feedback is about (`docs/adr/ADR-041-event-provenance-projection.md:172`, `docs/adr/ADR-041-event-provenance-projection.md:183`).

The brain pack appends a `FeedbackExplicit` event with the target stored only in `event.target_id` and payload `{"signal": signal}` (`crates/khive-pack-brain/src/lib.rs:224`, `crates/khive-pack-brain/src/lib.rs:231`, `crates/khive-pack-brain/src/lib.rs:232`). The projection decoder, however, only looks for `payload.about_id`; when it is absent, it returns `Ok(Vec::new())` (`crates/khive-db/src/stores/event.rs:417`, `crates/khive-db/src/stores/event.rs:418`, `crates/khive-db/src/stores/event.rs:419`).

Impact: `brain.emit` succeeds and persists an event, but inserts no `event_observations` row for the feedback target. Any provenance query using the required `Signal` role will miss these events.

Fix: make the emitter and decoder agree on the referent. Either include `about_id` in the payload, or make `decode_signal_observation` fall back to `event.target_id`. Also use the correct referent kind/substrate for note feedback instead of always creating the event with `SubstrateKind::Event` (`crates/khive-pack-brain/src/lib.rs:228`). Add a regression test that `brain.emit` writes exactly one `event_observations` row with `role = signal` for the target.

### Major 3. Event list API drops the new event/provenance query contract

ADR-022 defines event-list wire filters for event kind and maps them to `EventFilter.kinds` (`docs/adr/ADR-022-events-query-surface.md:84`, `docs/adr/ADR-022-events-query-surface.md:88`, `docs/adr/ADR-022-events-query-surface.md:89`). The same ADR defines the v1 `EventFilter` fields for `kinds`, `session_id`, `observed`, and `selected` (`docs/adr/ADR-022-events-query-surface.md:171`, `docs/adr/ADR-022-events-query-surface.md:175`, `docs/adr/ADR-022-events-query-surface.md:181`, `docs/adr/ADR-022-events-query-surface.md:182`, `docs/adr/ADR-022-events-query-surface.md:183`).

Storage implements those fields (`crates/khive-storage/src/event.rs:157`, `crates/khive-storage/src/event.rs:159`, `crates/khive-storage/src/event.rs:165`, `crates/khive-storage/src/event.rs:166`, `crates/khive-storage/src/event.rs:167`). The KG wire params do not expose them: `ListParams` only has `verb`, `verbs`, `outcome`, `actor`, `substrate`, `since`, and `until` for events (`crates/khive-pack-kg/src/handlers.rs:207`, `crates/khive-pack-kg/src/handlers.rs:221`). `event_filter_from_params` fills only verbs, substrates, actors, and time bounds, then defaults the rest (`crates/khive-pack-kg/src/handlers.rs:508`, `crates/khive-pack-kg/src/handlers.rs:527`, `crates/khive-pack-kg/src/handlers.rs:533`).

Impact: callers cannot list only `EntityUpdated` events, cannot filter by `session_id`, and cannot use the provenance indexes added by this PR through the public verb surface. That leaves the event observability feature only partially reachable.

Fix: add unambiguous wire parameters for event kind(s), `session_id`, `observed`, and `selected` to the event list handler, parse them into `EventFilter`, and test each filter through `list(kind="event", ...)`. Because ADR-022 uses `kind` for event kind while the unified verb also uses `kind="event"` for record type, this PR should either implement a compatible spelling such as `event_kind`/`event_kinds` with an ADR note, or resolve the collision directly in the wire layer.

## Looks Right

- F031/F032 are addressed in storage: event filtering is no longer NoteKind-based, and `EventFilter` carries `EventKind` and `SubstrateKind` (`crates/khive-storage/src/event.rs:157`).
- The SQLite event schema/migration now has typed event columns, payload/profile/session fields, aggregate fields, `event_observations`, and event ordering indexes.
- `append_event` and `append_events` project observations inside a write transaction and rollback when projection decoding fails.
- Event ordering uses `created_at` plus event id as the deterministic tie-breaker, matching the canonical ADR-004 ordering shape.
- Event records are treated as immutable through KG update/delete handlers.

## Commands Run

- `git diff --name-status integration/v1-adr-alignment...HEAD`
- `cd crates && RUSTC_WRAPPER= cargo test --workspace` - passed
- `cd crates && RUSTC_WRAPPER= cargo clippy --workspace --all-targets -- -D warnings` - passed
- `cd crates && cargo fmt --all -- --check` - passed
- `git diff --check integration/v1-adr-alignment...HEAD` - passed

## What I Did Not Check

- I did not inspect remote GitHub Actions beyond the local gates above.
- I did not run coverage; no coverage gate was requested.
- I did not run ignored/heavy tests.
- I did not post this review to GitHub.
- I did not do a live MCP end-to-end smoke test through an external client; the findings are from ADRs, the PR diff, and local tests.

Domain utility: SKIPPED - No lore/suggest tools were available in this Codex environment; the ADRs and repository code provided the needed review contract.
