use super::*;
use khive_types::Pack;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

/// A probe hook shaped like a real kind-owning pack: `normalize_note_update`
/// moves a caller-supplied top-level field into `properties`, and
/// `validate_note_update` refuses based on what it finds there. The value
/// `validate_note_update` inspects does not exist in `properties` until
/// `normalize_note_update` puts it there, so a passing refusal assertion
/// proves both the ordering and that normalize's mutation reached validate.
#[derive(Debug, Default)]
struct SequencerProbeHook {
    normalize_calls: AtomicUsize,
    validate_calls: AtomicUsize,
    validate_saw_marker: StdMutex<Option<bool>>,
    effects_calls: AtomicUsize,
    effects: StdMutex<Vec<NoteUpdateEffect>>,
}

#[async_trait]
impl KindHook for SequencerProbeHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn normalize_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        self.normalize_calls.fetch_add(1, Ordering::SeqCst);
        let Some(raw) = args.get("raw_marker").and_then(Value::as_bool) else {
            return Ok(());
        };
        let root = args.as_object_mut().expect("probe test args are an object");
        root.remove("raw_marker");
        let mut properties = serde_json::Map::new();
        properties.insert("marker".into(), Value::Bool(raw));
        root.insert("properties".into(), Value::Object(properties));
        Ok(())
    }

    async fn validate_note_update(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        properties: Option<&Value>,
    ) -> Result<(), RuntimeError> {
        self.validate_calls.fetch_add(1, Ordering::SeqCst);
        let marker = properties
            .and_then(|value| value.get("marker"))
            .and_then(Value::as_bool);
        *self.validate_saw_marker.lock().unwrap() = marker;
        if marker == Some(true) {
            return Err(RuntimeError::InvalidInput(
                "probe validator refuses marker=true".into(),
            ));
        }
        Ok(())
    }
    async fn note_update_effects(
        &self,
        _runtime: &KhiveRuntime,
        _token: &NamespaceToken,
        _note: &khive_storage::Note,
        _patch: &crate::NotePatch,
    ) -> Result<Vec<NoteUpdateEffect>, RuntimeError> {
        self.effects_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.effects.lock().unwrap().clone())
    }
}

struct ProbePack(Arc<SequencerProbeHook>);

impl Pack for ProbePack {
    const NAME: &'static str = "sequencer-probe";
    const NOTE_KINDS: &'static [&'static str] = &["probe-note"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for ProbePack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        (kind == "probe-note").then(|| self.0.clone() as Arc<dyn KindHook>)
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "ProbePack has no verbs: {verb}"
        )))
    }
}

/// The arm the issue reported: a normalizer that moves a caller field into
/// `properties` and a validator that refuses it must both run by the time
/// `prepare_note_update_hook` returns its error.
///
/// This arm was confirmed to be load-bearing by a mutation run before the
/// change landed, recorded here as a result rather than as a procedure. With
/// the sequencing reduced to a single `validate_note_update` call — the
/// pre-fix shape, and what a caller that skips the normalizer reproduces —
/// `normalize_note_update` did not run, `marker` did not reach `properties`,
/// `validate_note_update` observed `None` where it expects `Some(true)`, and
/// the call returned `Ok` instead of the expected `Err`, reddening the first
/// assertion below.
#[tokio::test]
async fn prepare_note_update_hook_runs_normalize_before_validate_and_validate_can_refuse() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let hook = Arc::new(SequencerProbeHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ProbePack(hook.clone()));
    let registry = builder.build().expect("registry builds");

    let note = khive_storage::Note::new("local", "probe-note", "body");
    let mut args = serde_json::json!({"raw_marker": true});

    let result = registry
        .prepare_note_update_hook(&runtime, &token, &note, &mut args)
        .await;

    let error = result.expect_err("the refusal must fire");
    assert!(
        error.to_string().contains("marker=true"),
        "the error must be the probe validator's own refusal: {error}"
    );
    assert_eq!(
        args["properties"]["marker"],
        serde_json::json!(true),
        "normalization must land in args even on the arm that ends in refusal"
    );
    assert_eq!(
        hook.normalize_calls.load(Ordering::SeqCst),
        1,
        "normalize must run even though validate goes on to refuse"
    );
    assert_eq!(
        hook.validate_calls.load(Ordering::SeqCst),
        1,
        "validate must run exactly once"
    );
    assert_eq!(
        *hook.validate_saw_marker.lock().unwrap(),
        Some(true),
        "validate must see the property normalize just wrote, not the caller's raw field"
    );
}

/// Mirror of the arm above: the same probe, with input that makes the
/// validator accept. `prepare_note_update_hook` must still return `Ok`
/// AND normalization must still have landed in `args` — an accepting
/// validator is not a reason to have skipped normalization.
///
/// Under the same mutation described on the arm above,
/// `args["properties"]["marker"]` was left unset — `properties` did not
/// exist at all — reddening the first assertion below.
#[tokio::test]
async fn prepare_note_update_hook_runs_normalize_before_validate_and_validate_can_accept() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local namespace");
    let hook = Arc::new(SequencerProbeHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ProbePack(hook.clone()));
    let registry = builder.build().expect("registry builds");

    let note = khive_storage::Note::new("local", "probe-note", "body");
    let mut args = serde_json::json!({"raw_marker": false});

    registry
        .prepare_note_update_hook(&runtime, &token, &note, &mut args)
        .await
        .expect("an accepting validator must not refuse");

    assert_eq!(
        args["properties"]["marker"],
        serde_json::json!(false),
        "normalization must land in args even when validation accepts"
    );
    assert_eq!(hook.normalize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hook.validate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*hook.validate_saw_marker.lock().unwrap(), Some(false));
}
async fn effects_fixture() -> (
    KhiveRuntime,
    NamespaceToken,
    VerbRegistry,
    Arc<SequencerProbeHook>,
    khive_storage::Note,
) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let hook = Arc::new(SequencerProbeHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ProbePack(hook.clone()));
    let registry = builder.build().unwrap();
    let note = khive_storage::Note::new("local", "probe-note", "before");
    let id = note.id;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    (runtime, token, registry, hook, note)
}

#[tokio::test]
async fn note_update_effects_are_not_computed_for_invalid_or_known_stale_patches() {
    use crate::atomic_prepare::prepare_update_from_note_snapshot;
    use serde_json::json;
    let (runtime, token, registry, hook, snapshot) = effects_fixture().await;
    let mut invalid = json!({"id": snapshot.id, "raw_marker": true});
    registry
        .prepare_note_update_hook(&runtime, &token, &snapshot, &mut invalid)
        .await
        .expect_err("kind validator refuses");
    assert_eq!(hook.effects_calls.load(Ordering::SeqCst), 0);
    for extra in [
        json!({"salience": 2.0}),
        json!({"expected_version": snapshot.version + 1}),
    ] {
        let mut args = json!({"id": snapshot.id, "raw_marker": false});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let policy = registry
            .prepare_note_update_policy(&runtime, &token, &snapshot, &mut args)
            .await
            .unwrap();
        prepare_update_from_note_snapshot(
            &runtime,
            &token,
            &args,
            None,
            snapshot.clone(),
            policy,
            &registry,
        )
        .await
        .expect_err("invalid/stale update must be refused before effects");
        assert_eq!(hook.effects_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(snapshot.id)
                .await
                .unwrap()
                .unwrap(),
            snapshot
        );
    }
    runtime
        .update_note(
            &token,
            snapshot.id,
            crate::NotePatch::new(None, Some("winner".into()), None, None, None),
        )
        .await
        .unwrap();
    let current = runtime
        .notes(&token)
        .unwrap()
        .get_note(snapshot.id)
        .await
        .unwrap()
        .unwrap();
    let mut args = json!({"id": snapshot.id, "raw_marker": false});
    let policy = registry
        .prepare_note_update_policy(&runtime, &token, &snapshot, &mut args)
        .await
        .unwrap();
    prepare_update_from_note_snapshot(
        &runtime,
        &token,
        &args,
        None,
        snapshot.clone(),
        policy,
        &registry,
    )
    .await
    .expect_err("known stale snapshot");
    assert_eq!(hook.effects_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(snapshot.id)
            .await
            .unwrap()
            .unwrap(),
        current
    );
}

#[tokio::test]
async fn note_update_effects_missing_target_must_fail_without_note_changes() {
    use serde_json::json;
    let (runtime, token, registry, hook, snapshot) = effects_fixture().await;
    *hook.effects.lock().unwrap() = vec![NoteUpdateEffect::Link(LinkSpec {
        namespace: None,
        source_id: snapshot.id,
        target_id: uuid::Uuid::new_v4(),
        relation: khive_storage::EdgeRelation::Annotates,
        weight: 1.0,
        metadata: None,
        resurrect: false,
    })];
    let mut args = json!({"id": snapshot.id, "raw_marker": false});
    let policy = registry
        .prepare_note_update_policy(&runtime, &token, &snapshot, &mut args)
        .await
        .unwrap();
    let error = runtime
        .update_note_from_snapshot_with_kind_effects(
            &token,
            snapshot.clone(),
            &args,
            policy,
            &registry,
        )
        .await
        .expect_err("missing link target must not become a successful note update");
    assert!(matches!(error, RuntimeError::NotFound(_)), "{error}");
    assert_eq!(hook.effects_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(snapshot.id)
            .await
            .unwrap()
            .unwrap(),
        snapshot
    );
}

#[tokio::test]
async fn note_update_effects_do_not_replace_a_live_annotation_that_appeared_during_prepare() {
    use khive_storage::EdgeRelation;
    use serde_json::json;
    let (runtime, token, registry, hook, snapshot) = effects_fixture().await;
    let target = runtime
        .create_entity(&token, "concept", None, "new target", None, None, vec![])
        .await
        .unwrap();
    runtime
        .link(
            &token,
            snapshot.id,
            target.id,
            EdgeRelation::Annotates,
            0.3,
            Some(json!({"keep": true})),
        )
        .await
        .unwrap();
    let before = runtime
        .get_edge_by_natural_key_including_deleted(
            &token,
            "local",
            snapshot.id,
            target.id,
            EdgeRelation::Annotates,
        )
        .await
        .unwrap()
        .unwrap();
    // Models an owner's absent-edge observation followed by another writer
    // creating the natural key before the runtime prepares the Link request.
    *hook.effects.lock().unwrap() = vec![NoteUpdateEffect::Link(LinkSpec {
        namespace: None,
        source_id: snapshot.id,
        target_id: target.id,
        relation: EdgeRelation::Annotates,
        weight: 1.0,
        metadata: None,
        resurrect: true,
    })];
    let mut args = json!({"id": snapshot.id, "raw_marker": false});
    let policy = registry
        .prepare_note_update_policy(&runtime, &token, &snapshot, &mut args)
        .await
        .unwrap();
    let error = runtime
        .update_note_from_snapshot_with_kind_effects(
            &token,
            snapshot.clone(),
            &args,
            policy,
            &registry,
        )
        .await
        .expect_err("typed create must not replace an intervening live edge");
    assert!(error.to_string().contains("live edge appeared"), "{error}");
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(snapshot.id)
            .await
            .unwrap()
            .unwrap(),
        snapshot
    );
    let after = runtime
        .get_edge_by_natural_key_including_deleted(
            &token,
            "local",
            snapshot.id,
            target.id,
            EdgeRelation::Annotates,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}

#[tokio::test]
async fn note_update_effects_commit_guards_roll_back_note_and_prior_edge_effects() {
    use crate::atomic_prepare::prepare_update_from_note_snapshot;
    use crate::{run_atomic_unit, AtomicRunOutcome, EdgeListFilter};
    use khive_storage::EdgeRelation;
    use serde_json::json;
    for stale_note in [false, true] {
        let (runtime, token, registry, hook, snapshot) = effects_fixture().await;
        let a = runtime
            .create_entity(&token, "concept", None, "A", None, None, vec![])
            .await
            .unwrap();
        let b = runtime
            .create_entity(&token, "concept", None, "B", None, None, vec![])
            .await
            .unwrap();
        let a_id = a.id;
        let b_id = b.id;
        runtime
            .link(
                &token,
                snapshot.id,
                a_id,
                EdgeRelation::Annotates,
                1.0,
                None,
            )
            .await
            .unwrap();
        let old = runtime
            .get_edge_by_natural_key_including_deleted(
                &token,
                "local",
                snapshot.id,
                a_id,
                EdgeRelation::Annotates,
            )
            .await
            .unwrap()
            .unwrap();
        *hook.effects.lock().unwrap() = vec![
            NoteUpdateEffect::DeleteEdge(old.clone()),
            NoteUpdateEffect::Link(LinkSpec {
                namespace: None,
                source_id: snapshot.id,
                target_id: b_id,
                relation: EdgeRelation::Annotates,
                weight: 1.0,
                metadata: None,
                resurrect: false,
            }),
        ];
        let mut args = json!({"id": snapshot.id, "raw_marker": false});
        let policy = registry
            .prepare_note_update_policy(&runtime, &token, &snapshot, &mut args)
            .await
            .unwrap();
        let (_, plan) = prepare_update_from_note_snapshot(
            &runtime,
            &token,
            &args,
            None,
            snapshot.clone(),
            policy,
            &registry,
        )
        .await
        .unwrap();
        let expected = if stale_note {
            runtime
                .update_note(
                    &token,
                    snapshot.id,
                    crate::NotePatch::new(None, Some("concurrent winner".into()), None, None, None),
                )
                .await
                .unwrap()
        } else {
            runtime.delete_entity(&token, b_id, false).await.unwrap();
            snapshot.clone()
        };
        let outcome = run_atomic_unit(runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap();
        assert!(
            matches!(
                outcome,
                AtomicRunOutcome::RolledBack {
                    failed_op_index: 0,
                    ..
                }
            ),
            "missing endpoint / stale note MUST refuse: {outcome:?}"
        );
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(snapshot.id)
                .await
                .unwrap()
                .unwrap(),
            expected
        );
        let edges = runtime
            .list_edges(
                &token,
                EdgeListFilter {
                    source_id: Some(snapshot.id),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(edges).unwrap(), json!([old]));
        assert!(runtime
            .get_edge_by_natural_key_including_deleted(
                &token,
                "local",
                snapshot.id,
                b_id,
                EdgeRelation::Annotates,
            )
            .await
            .unwrap()
            .is_none());
    }
}
