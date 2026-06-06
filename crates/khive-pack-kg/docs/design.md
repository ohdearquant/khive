# khive-pack-kg Design Notes

**Scope**: KG verb pack -- 16 verb handlers for entity/note CRUD, graph traversal,
hybrid search, event-sourced proposals. First-party pack shipped with the khive binary.

## Primary Links

- Source: [`src/`](../src/)
  - [`handlers/mod.rs`](../src/handlers/mod.rs) -- 16 verb handlers
  - [`apply_worker/mod.rs`](../src/apply_worker/mod.rs) -- proposal apply pipeline
  - [`projection_worker/mod.rs`](../src/projection_worker/mod.rs) -- proposals_open projection
  - [`dispatch.rs`](../src/dispatch.rs) -- PackRuntime impl + self-registration
  - [`handler_defs.rs`](../src/handler_defs.rs) -- HandlerDef table (16 entries)
  - [`vocab.rs`](../src/vocab.rs) -- pack-owned entity/note vocabulary
  - [`entity_type_registry.rs`](../src/entity_type_registry.rs) -- entity subtype validation
  - [`pack.rs`](../src/pack.rs) -- KgPack struct + edge endpoint rules
- Tests: [`tests/integration.rs`](../tests/integration.rs)
- Benchmarks: none (no `benches/` target)

## ADR References

- [ADR-001: Entity Kind Taxonomy](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)
- [ADR-002: Edge Ontology](../../../docs/adr/ADR-002-edge-ontology.md)
- [ADR-013: Note Kind Taxonomy](../../../docs/adr/ADR-013-note-kind-taxonomy.md)
- [ADR-014: Curation Operations](../../../docs/adr/ADR-014-curation-operations.md)
- [ADR-017: Pack Standard](../../../docs/adr/ADR-017-pack-standard.md)
- [ADR-027: Dynamic Pack Loading](../../../docs/adr/ADR-027-dynamic-pack-loading.md)
- [ADR-038: Bulk Link](../../../docs/adr/ADR-038-bulk-link-operation.md)
- [ADR-045: Timestamp Normalization](../../../docs/adr/ADR-045-timestamp-normalization.md)
- [ADR-046: Event-Sourced Proposals](../../../docs/adr/ADR-046-event-sourced-proposals.md)
- [ADR-048: Resource Entity Kind](../../../docs/adr/ADR-048-knowledge-section-profiles.md)
- [ADR-050: Namespace Token Contract](../../../docs/adr/ADR-050-namespace-token-contract.md)

---

## ADR Compliance

### ADR-001: Entity Kind Taxonomy

- This pack declares eight canonical entity kinds: `concept`, `document`, `dataset`, `project`,
  `person`, `org`, `artifact`, `service`.
- A 9th kind, `resource`, is a pack-local extension (see `vocab.rs`) for actionable content
  that agents consume (atoms, domains, skills, tools). It is not in the wire-level enum but
  is accepted via `FromStr` aliasing.
- `benchmark` belongs to `Dataset`, not `Concept` -- it evaluates models, it is not a conceptual
  idea. `entity_type_registry.rs` enforces this at apply-worker and handler boundaries.
- `model_family` is the canonical type name for the concept kind; `model` is the accepted alias
  to distinguish from a trained model instance (which is an `artifact`).
- `Person` has no standard subtypes -- roles are stored as metadata, not as registered subtypes.
- Apply worker validates entity kinds against the closed taxonomy before committing any changeset
  step. Proposals cannot bypass taxonomy validation.

### ADR-002: Edge Ontology (15 relations)

- The pack extends the base entity-entity allowlist with pack-level endpoint rules for person-org
  and org-org pairs (see `pack.rs` `KG_EDGE_RULES`). These are additive only.
- When `runtime.link()` returns an allowlist error, `enrich_allowlist_error` in `handlers.rs`
  fetches entity kinds and appends the valid relations for that endpoint pair to the error message.
- The sentinel substring matched inside `handlers.rs` is `"not in the base endpoint allowlist"`.
- Error messages from `parse_relation` enumerate all 15 relations (derived from
  `EdgeRelation::ALL`), not from a hardcoded string. Tests assert `derived_from` and `precedes`
  appear in the error.

### ADR-004: Note Status Remapping (Option A)

- `remap_note_status` in `handlers.rs` lifts pack-owned lifecycle status from `properties.status`
  to the top-level `status` field. The storage-layer `status` (row visibility) moves to `lifecycle`.
- Applied at all response boundaries: `get`, `list`, `create` for note substrates.

### ADR-013: Note Kind Taxonomy

- Five canonical note kinds: `observation`, `insight`, `question`, `decision`, `reference`.
- Aliases are rejected -- only canonical names are accepted via `FromStr`.
- Validation is enforced in `canonical_note_kind` in `handlers.rs`.

### ADR-014: UUID-only Operations (substrate inference)

- `update` and `delete` accept an optional `kind` hint. When absent, the substrate is inferred
  by probing entity -> note -> edge in order via `infer_kind_from_uuid`.
- `get` resolves across all substrates plus proposal lookup as a final fallback.

### ADR-025 / ADR-060: Illocutionary Classification

- All 16 KG handlers are classified as Assertive, Commissive, or Declaration per Searle (1976).
- `verbs` lists verbs by category; `propose`/`review`/`withdraw` are Commissive/Declaration.

### ADR-027: Dynamic Pack Loading / Inventory Self-Registration

- The `KgPackFactory` registered via `inventory::submit!` in `dispatch.rs` self-registers the
  KG pack with the VerbRegistry at binary startup without explicit wiring.

### ADR-038: Bulk Link

- `handle_link` in `handlers.rs` accepts both singleton and bulk-link (`links: [...]`) forms.
- The `atomic` flag (default true) wraps the entire batch in a transaction.

### ADR-040: Message-Filter Scan Cap

- When message-specific filters are active on `list(kind=note)`, the handler paginates up to
  `MAX_SCAN_TOTAL = 10_000` notes before stopping. This prevents pathological scans on large
  note stores. For deep mailboxes, `comm.inbox` (uncapped) or `comm.thread` (thread-indexed) are
  preferred alternatives. See `handlers.rs` `handle_list` note-substrate branch.

### ADR-045: Timestamp Normalization (§5 Handler Invariant)

- All `i64`/`u64` microsecond epoch timestamps must be converted to ISO-8601 strings before
  crossing the MCP boundary. `walk_timestamps` in `handlers.rs` does this recursively at any
  nesting depth. The key set is `TIMESTAMP_KEYS`.
- `normalize_entity_timestamps` applies to entity and note responses.
- `normalize_event_timestamps` applies to event responses.
- Proposal listing converts `created_at`, `updated_at`, `expiry` before returning rows.
- Note create was previously missing normalization (Blocker C1 fix).

### ADR-046: Event-Sourced Proposals

- Three proposal verbs: `propose` (Commissive), `review` (Declaration), `withdraw` (Commissive).
- `propose` emits a `ProposalCreated` event and inserts a row into `proposals_open`.
  Projection is maintained by `ProposalsProjectionWorker`, not inline in the handler.
- `review` runs a CAS UPDATE + event INSERT in a single `BEGIN IMMEDIATE` transaction
  (`reviewed_and_emit`), so projection and event log always advance together.
- `withdraw` similarly runs `withdrawn_and_emit` atomically.
- Apply worker fires on approval threshold (v1: 1 approve, 0 rejects). It runs a pre-apply
  CAS (`status: approved -> applying`) before touching the KG, closing the apply/withdraw race.
- Hard-state proposals (applied/rejected/withdrawn) are retained in `proposals_open` for audit.
  `list(kind=proposal)` returns ALL rows when no status filter is supplied.
- `get(id=<proposal_id>)` resolves to the `ProposalCreated` event payload (not a projection row).
- All-or-nothing write budget: if `max_new_entries` is exceeded, `ProposalApplied{Failed}` is
  emitted and status stays `approved` (no KG mutation).
- Failed applies revert `status: applying -> approved` so proposals are not permanently stuck.
- Self-approval is forbidden except in OSS local mode (single-user, `actor == "local"`).
- `BUG-6`: `parent_id` is validated against `proposals_open` before creating an amendment.

### ADR-048: Resource Entity Kind

- `resource` is the 9th entity kind: actionable content agents consume (atoms, domains, skills,
  tools, templates, prompts, runbooks). It lives in `vocab.rs` as a pack-local extension.
- Accepted aliases in `FromStr`: `atom`, `runbook`, `template`, `prompt`, `skill`, `tool`.

### ADR-050: Namespace Token Contract

- The KG pack honors the `NamespaceToken` received from the `VerbRegistry::dispatch` caller.
- Entity/edge operations use the graph token; note/event operations use the caller token.
- Cross-namespace reads return `NotFound` (indistinguishable from absence) -- fail-closed.
- The dispatch test suite covers: tenant-a creates are visible to tenant-a and opaque to tenant-b;
  OSS default namespace entities co-locate under the `local` namespace.

---

## Verb Visibility Audit (Issue #497)

All 16 KG handlers are `Visibility::Verb` (agent-callable via the MCP `request` DSL). There are
no `Visibility::Subhandler` entries in this pack. Rationale:

- The KG pack has no operator-only introspection or internal plumbing that needs to be hidden from
  the agent surface. Every verb is a first-class operation that agents are expected to invoke
  directly.
- `propose` / `review` / `withdraw` are deliberate agent-facing verbs: proposals flow
  from agents who must be able to call all three steps. If a future requirement introduces an
  operator-only "admin_apply" or "force_reject" escape hatch it should be added as
  `Visibility::Subhandler` at that point.
- The `verbs` introspection verb is agent-callable by design: it implements the self-describing
  verb catalog (ue-help-introspection H5) and explicitly filters out `Subhandler` entries before
  responding, so adding any future `Subhandler` entries here would automatically hide them from
  that output.

Packs with confirmed `Subhandler` entries as of this audit:

- `pack-memory`: `recall_embed`, `recall_candidates`, `recall_fuse`, `recall_rerank`,
  `recall_score`
- `pack-brain`: `brain.state`, `brain.config`, `brain.events`, `brain.emit` (deprecated)

*Source pointer*: `src/handler_defs.rs` -- the comment block above `static KG_HANDLERS` summarises this.

---

## Proposal Projection CAS / Event-Insert Proof (`reviewed_and_emit`)

*Source pointer*: `src/projection_worker.rs` -- `ProposalsProjectionWorker::reviewed_and_emit`.

`reviewed_and_emit` atomically runs the CAS UPDATE on `proposals_open` and a conditional
`ProposalReviewed` event INSERT in a single `BEGIN IMMEDIATE` / `COMMIT` transaction via
`execute_batch`.

### CAS guard: `changes() = 1`

`changes()` returns the row count from the immediately-preceding statement on the same connection.
Since `execute_batch` runs both statements on the same connection with no intervening operations,
`changes()` at INSERT time is exactly the UPDATE's row count.

- If the UPDATE matched 1 row (this connection won the CAS): `changes() = 1` is true -> the INSERT
  runs.
- If the UPDATE matched 0 rows (CAS lost): `changes() = 0` -> the INSERT is skipped.

This replaces the round-3 `updated_at = <now>` subquery guard, which was unsafe under
same-microsecond concurrent calls: two callers can compute identical `now` values before either
holds the writer lock, so the loser's guard could match the winner's committed `updated_at` and
insert a duplicate event. `changes()` is connection-local and requires no timestamp uniqueness
assumption.

Return value: `Ok((cas_hit, event_id))`.

- `cas_hit = true`: projection row was updated AND the event was inserted (total_rows == 2 for
  state-changing decisions).
- `cas_hit = false`: no projection update, no event written.
- For `Comment` decisions (no state change), `cas_hit` is always true.

---

## Message-Filter Scan Cap

*Source pointer*: `src/handlers.rs` `handle_list` -- note-substrate branch with `has_msg_filter`.

When message-specific property filters (`thread_id`, `direction`, `from`, `to`, `read`) are
active on `list(kind=note)`, the storage layer does not push those predicates to SQL -- only
namespace + kind are indexed. The handler must apply them in-memory after retrieval.

To avoid pathological scans on large note stores, a paginated scan loop caps total rows examined
at `MAX_SCAN_TOTAL = 10_000`. Pages are fetched in batches of `PAGE_SIZE = 200`.

Alternatives for deep mailboxes:
- `comm.inbox` -- dedicated inbox query with no cap.
- `comm.thread` -- thread-indexed lookup, O(1) per message.

---

## Consistency Notes

- `handlers.rs` sentinel string for allowlist errors (`"not in the base endpoint allowlist"`)
  must match the string produced by `khive-runtime`'s `operations.rs`. If that string changes,
  update both files.
- `entity_type_registry.rs` validates `entity_type` at the handler boundary; the apply worker
  also validates at apply time. Both must stay in sync with `EntityKind::ALL`.
- `KG_EDGE_RULES` in `pack.rs` are additive over the base contract in `operations.rs`.
  If a new person-org or org-org relation is needed, add it here. Removing existing rules is a
  breaking change -- coordinate with the runtime team.

---

## Invariants and Failure Modes

- Entity and note kinds are closed sets. Any unrecognized kind string is rejected with an error
  listing valid values. Proposals bypass the same validation at apply time.
- Edge weights must be finite numbers in `[0.0, 1.0]`. Out-of-range or non-finite weights are
  rejected at the handler boundary (not silently clamped).
- Namespace isolation is enforced at the runtime layer. ID-based operations verify
  `record.namespace == caller_namespace` after fetching by UUID. Cross-namespace reads return
  `NotFound` (indistinguishable from absence).
- Proposal state transitions use CAS (compare-and-swap) guards on `proposals_open` to prevent
  race conditions between concurrent approve/withdraw/apply operations.
- The apply worker exclusively owns the `applying` state. Failed applies revert to `approved`.
- The `changes()` SQL guard (not `updated_at` equality) prevents duplicate event insertion
  under same-microsecond timestamp collisions.

---

Last reviewed: 2026-06-06
