// ── ADR-017 / ADR-021 2026-09-22 amendment: the AddNote memory exception is withdrawn ──
//
// The test this section replaced proved the opposite of what the amendment now requires: an
// approved `AddNote(kind="memory")` changeset used to admit the memory kind, leaving
// `salience` and `decay_factor` unset because `prepare_add_note` never ran the shared create
// hook. The memory pack now registers `KindHook::validate_proposal_note` (ADR-017), and the
// same refusal that already covers shared `create`, `stream.batch`, and `stream.append` now
// covers this route too, at both `propose` and apply.

/// Assert a `registry.dispatch` error carries the canonical proposal-note admission-refusal
/// shape: `details.reason = "kind_admission_refused"`, `details.kind = "memory"`,
/// `details.route = "proposal_add_note"`, and a message naming `memory.remember`.
fn assert_memory_proposal_admission_refused(error: &RuntimeError) {
    let RuntimeError::Khive(khive_error) = error else {
        panic!("expected RuntimeError::Khive carrying structured details, got {error:?}");
    };
    let details = khive_error
        .details()
        .expect("refusal must carry structured details");
    assert_eq!(details.get("reason"), Some("kind_admission_refused"));
    assert_eq!(details.get("kind"), Some("memory"));
    assert_eq!(details.get("route"), Some("proposal_add_note"));
    assert!(
        khive_error.to_string().contains("memory.remember"),
        "refusal must name the writer to use instead: {khive_error}"
    );
}

/// Read one `SELECT COUNT(*) ...`-shaped statement back as an integer, mirroring the
/// `count` helper `khive-runtime`'s own `keyed_memory_tests.rs` uses for the same purpose.
async fn scalar_count(rt: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> i64 {
    let mut reader = rt.sql().reader().await.expect("sql reader");
    match reader
        .query_scalar(SqlStatement {
            sql: sql.to_string(),
            params,
            label: Some("test.scalar_count".into()),
        })
        .await
        .expect("count rows")
    {
        Some(SqlValue::Integer(value)) => value,
        other => panic!("unexpected count result: {other:?}"),
    }
}

/// A namespace-scoped `notes` row count for one kind, including soft-deleted rows (no
/// `deleted_at` filter): the shape the apply-refusal test needs to prove nothing was written
/// even transiently.
async fn notes_kind_count_including_deleted(rt: &KhiveRuntime, ns: &str, kind: &str) -> i64 {
    scalar_count(
        rt,
        "SELECT COUNT(*) FROM notes WHERE namespace = ?1 AND kind = ?2",
        vec![
            SqlValue::Text(ns.to_string()),
            SqlValue::Text(kind.to_string()),
        ],
    )
    .await
}

fn row_text(row: &khive_storage::types::SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn row_i64(row: &khive_storage::types::SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

/// The full `proposals_open` row for one proposal, every column the schema declares.
async fn proposals_open_row(
    rt: &KhiveRuntime,
    ns: &str,
    proposal_id: Uuid,
) -> khive_storage::types::SqlRow {
    let mut reader = rt.sql().reader().await.expect("sql reader");
    reader
        .query_row(SqlStatement {
            sql: "SELECT proposal_id, namespace, proposer, title, status, created_at, \
                  updated_at, expiry, last_decision, review_count, approve_count, reject_count \
                  FROM proposals_open WHERE proposal_id = ?1 AND namespace = ?2"
                .to_string(),
            params: vec![
                SqlValue::Text(proposal_id.to_string()),
                SqlValue::Text(ns.to_string()),
            ],
            label: Some("test.proposals_open_row_snapshot".into()),
        })
        .await
        .expect("read proposals_open row")
        .expect("seeded proposals_open row must exist")
}

/// Every column `revert_applying_to_approved` (`sql/proposals_revert_to_approved.sql`, read at
/// source) is documented to touch: `SET status = 'approved', updated_at = ?1 WHERE ... AND
/// status = 'applying'`. The CAS this test drives through is `approved -> applying ->
/// approved` (`pre_apply_cas` then the revert on failure, `sql/proposals_mark_applying.sql`
/// touches the same two columns), so `status` nets out unchanged and `updated_at` is the only
/// column expected to differ.
const PROPOSALS_OPEN_COLUMNS_UNTOUCHED_BY_THE_FAILURE_LIFECYCLE: &[&str] = &[
    "proposal_id",
    "namespace",
    "proposer",
    "title",
    "created_at",
    "expiry",
    "last_decision",
    "review_count",
    "approve_count",
    "reject_count",
];

/// The full `payload_proposal_id`-filtered event history for one proposal, in the store's own
/// order (`created_at DESC, id DESC`, per `query_events_orders_by_created_at_then_id_desc`):
/// newest first.
async fn proposal_events_newest_first(
    rt: &KhiveRuntime,
    tok: &NamespaceToken,
    proposal_id: Uuid,
) -> Vec<khive_storage::event::Event> {
    rt.events(tok)
        .expect("event store")
        .query_events(
            khive_storage::EventFilter {
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 50,
            },
        )
        .await
        .expect("query events for the seeded proposal")
        .items
}

/// A whole-store `ProposalCreated` event count. Every creation-refusal test below runs against
/// its own fresh in-memory runtime, so this is a safe unscoped count: nothing else in that
/// store can emit this event kind.
async fn proposal_created_event_count(rt: &KhiveRuntime, tok: &NamespaceToken) -> usize {
    rt.events(tok)
        .expect("event store")
        .query_events(
            khive_storage::EventFilter {
                kinds: vec![khive_types::EventKind::ProposalCreated],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                offset: 0,
                limit: 50,
            },
        )
        .await
        .expect("query ProposalCreated events")
        .items
        .len()
}

/// A bare `AddNote(kind="memory")` proposal is refused at `propose`, before any proposal row
/// exists.
#[tokio::test]
async fn propose_add_note_changeset_refuses_memory_kind_before_any_proposal_row() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ns = tok.namespace().as_str().to_owned();

    let before = registry
        .dispatch("list", json!({"kind": "proposal"}))
        .await
        .expect("list(kind=proposal) must succeed");
    let before_count = before["items"].as_array().map(Vec::len).unwrap_or(0);
    let before_created_events = proposal_created_event_count(&rt, &tok).await;
    let before_notes = notes_kind_count_including_deleted(&rt, &ns, "memory").await;

    let refusal = registry
        .dispatch(
            "propose",
            json!({
                "title": "a fresh memory add_note proposal",
                "description": "must be refused before a proposal row is created",
                "changeset": {"kind": "add_note", "note": {"kind": "memory", "content": "a bare memory draft written through a fresh proposal"}}
            }),
        )
        .await
        .expect_err("propose must refuse a bare AddNote(kind=memory) changeset");
    assert_memory_proposal_admission_refused(&refusal);

    let after = registry
        .dispatch("list", json!({"kind": "proposal"}))
        .await
        .expect("list(kind=proposal) must succeed");
    let after_count = after["items"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(
        before_count, after_count,
        "a refused propose must not create a proposal row"
    );
    assert_eq!(
        before_created_events,
        proposal_created_event_count(&rt, &tok).await,
        "a refused propose must not append a ProposalCreated event"
    );
    assert_eq!(
        before_notes,
        notes_kind_count_including_deleted(&rt, &ns, "memory").await,
        "a refused propose must not write a memory note"
    );
}

/// A draft carrying values that look like a complete `memory.remember` call is refused with the
/// same shape: these fields do not supply the derivation, actor routing, or keyed-replay
/// contract only `memory.remember` provides.
#[tokio::test]
async fn propose_add_note_changeset_refuses_memory_kind_with_complete_looking_defaults() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ns = tok.namespace().as_str().to_owned();
    let before_created_events = proposal_created_event_count(&rt, &tok).await;
    let before_notes = notes_kind_count_including_deleted(&rt, &ns, "memory").await;

    let refusal = registry
        .dispatch(
            "propose",
            json!({
                "title": "a memory add_note proposal with explicit defaults",
                "description": "complete-looking defaults must not bypass the refusal",
                "changeset": {
                    "kind": "add_note",
                    "note": {
                        "kind": "memory",
                        "content": "a memory draft with explicit defaults written through a proposal",
                        "properties": {
                            "memory_type": "episodic",
                            "salience": 0.3,
                            "decay_factor": 0.02
                        }
                    }
                }
            }),
        )
        .await
        .expect_err("propose must refuse AddNote(kind=memory) even with complete-looking defaults");
    assert_memory_proposal_admission_refused(&refusal);
    assert_eq!(
        before_created_events,
        proposal_created_event_count(&rt, &tok).await,
        "a refused propose must not append a ProposalCreated event"
    );
    assert_eq!(
        before_notes,
        notes_kind_count_including_deleted(&rt, &ns, "memory").await,
        "a refused propose must not write a memory note"
    );
}

/// The refusal reaches an `AddNote(kind="memory")` nested inside a single-step `Compound`
/// changeset, not just a top-level `AddNote`.
#[tokio::test]
async fn propose_add_note_changeset_refuses_memory_kind_nested_in_compound() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ns = tok.namespace().as_str().to_owned();
    let before_created_events = proposal_created_event_count(&rt, &tok).await;
    let before_notes = notes_kind_count_including_deleted(&rt, &ns, "memory").await;

    let refusal = registry
        .dispatch(
            "propose",
            json!({
                "title": "a memory add_note nested in a compound changeset",
                "description": "the walk must reach AddNote nested inside Compound",
                "changeset": {
                    "kind": "compound",
                    "steps": [
                        {"kind": "add_note", "note": {"kind": "memory", "content": "a memory draft nested inside a compound changeset"}}
                    ]
                }
            }),
        )
        .await
        .expect_err("propose must refuse a memory AddNote nested inside a Compound changeset");
    assert_memory_proposal_admission_refused(&refusal);
    assert_eq!(
        before_created_events,
        proposal_created_event_count(&rt, &tok).await,
        "a refused propose must not append a ProposalCreated event"
    );
    assert_eq!(
        before_notes,
        notes_kind_count_including_deleted(&rt, &ns, "memory").await,
        "a refused propose must not write a memory note"
    );
}

/// An `AddNote(kind="observation")` proposal is unaffected: it creates, and an approve review
/// applies it end to end. This is a different invariant from the trait's default body: `kg`,
/// which owns `observation`, registers no `KindHook` at all, so `find_kind_hook` returns `None`
/// and `check_note_proposal_admission` returns early, before any `KindHook::validate_proposal_note`
/// method, default or overridden, ever runs for this kind. The genuine default-body witness is
/// `propose_add_note_changeset_admits_task_kind_via_the_proposal_note_hooks_default_body` below:
/// `task`'s owning pack does register a hook, and that hook does not override this method, so the
/// trait's default `Ok(())` is the one that runs.
#[tokio::test]
async fn propose_add_note_changeset_admits_observation_kind_unaffected_by_the_memory_refusal() {
    let registry = make_registry(make_runtime());

    let propose = registry
        .dispatch(
            "propose",
            json!({
                "title": "an observation add_note proposal",
                "description": "a kind with no proposal-note hook must be unaffected",
                "changeset": {"kind": "add_note", "note": {"kind": "observation", "content": "an observation written through a proposal"}}
            }),
        )
        .await
        .expect("propose must succeed for an AddNote(kind=observation) changeset");
    let proposal_id = propose["id"]
        .as_str()
        .expect("propose returns id")
        .to_string();

    registry
        .dispatch("review", json!({"id": proposal_id, "decision": "approve"}))
        .await
        .expect("approving an observation add_note changeset must succeed");

    let found = registry
        .dispatch(
            "list",
            json!({"kind": "note", "note_kind": "observation", "limit": 50}),
        )
        .await
        .expect("list must succeed");
    let items = found["items"].as_array().expect("items array");
    assert!(
        items
            .iter()
            .any(|item| item["content"] == "an observation written through a proposal"),
        "the approved observation note must exist after apply: {items:?}"
    );
}

/// The seam's genuine default-body witness: `task` (gtd's `TaskHook`) is a note kind whose
/// owning pack does register a `KindHook`, unlike `observation` above, but that hook's
/// `impl KindHook` for `TaskHook` (`crates/khive-pack-gtd/src/hook.rs`) does not override
/// `validate_proposal_note`. `find_kind_hook("task")` therefore returns `Some(TaskHook)`, and
/// `check_note_proposal_admission` calls `validate_proposal_note` on it, reaching the trait's
/// own default `Ok(())` body rather than short-circuiting before any hook exists. `prepare_add_note`
/// dispatches no pack hook and only requires a registered note kind, so a raw `AddNote(kind="task")`
/// changeset materializes without gtd's task-specific create defaulting; this test only needs the
/// note to exist after apply, not the full task lifecycle.
#[tokio::test]
async fn propose_add_note_changeset_admits_task_kind_via_the_proposal_note_hooks_default_body() {
    let rt = make_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    let registry = builder.build().expect("registry builds");

    let propose = registry
        .dispatch(
            "propose",
            json!({
                "title": "a task add_note proposal",
                "description": "a kind whose hook keeps the proposal-note default must be admitted",
                "changeset": {"kind": "add_note", "note": {"kind": "task", "content": "a task written through a proposal, admitted by the hook's default validate_proposal_note"}}
            }),
        )
        .await
        .expect("propose must succeed for an AddNote(kind=task) changeset");
    let proposal_id = propose["id"]
        .as_str()
        .expect("propose returns id")
        .to_string();

    registry
        .dispatch("review", json!({"id": proposal_id, "decision": "approve"}))
        .await
        .expect(
            "approving a task add_note changeset must succeed through TaskHook's inherited default validate_proposal_note",
        );

    let found = registry
        .dispatch("list", json!({"kind": "task", "limit": 50}))
        .await
        .expect("list must succeed");
    let items = found["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|item| {
            item["content"] == "a task written through a proposal, admitted by the hook's default validate_proposal_note"
        }),
        "the approved task note must exist after apply: {items:?}"
    );
}
