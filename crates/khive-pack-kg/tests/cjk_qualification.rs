use khive_pack_kg::KgPack;
use khive_runtime::{
    curation::note_fts_document, KhiveRuntime, Namespace, RequestIdentity, RuntimeConfig,
    RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::Note;
use serde_json::{json, Value};
use uuid::Uuid;

const ACTOR: &str = "cjk-kg-qualification";
const KIND: &str = "observation";
const ID_KEY: &str = "id";
fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let actor = ACTOR;
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        actor_id: Some(actor.into()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.into()));
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}
async fn query(registry: &VerbRegistry, query: &str) -> Result<Value, RuntimeError> {
    registry
        .dispatch(
            "search",
            json!({"kind": "note", "query": query, "source": "text", "limit": 50}),
        )
        .await
}

async fn query_identity_other(registry: &VerbRegistry, query: &str) -> Result<Value, RuntimeError> {
    registry
        .dispatch_with_identity(
            "search",
            json!({"kind": "note", "query": query, "source": "text", "limit": 50}),
            Some(RequestIdentity {
                actor_id: Some(ACTOR.into()),
                namespace: "other".into(),
                ..Default::default()
            }),
        )
        .await
}

async fn query_explicit_other(registry: &VerbRegistry, query: &str) -> Result<Value, RuntimeError> {
    registry
        .dispatch_with_identity(
            "search",
            json!({"kind": "note", "query": query, "source": "text", "limit": 50, "namespace": "other"}),
            Some(RequestIdentity {
                actor_id: Some(ACTOR.into()),
                namespace: "other".into(),
                ..Default::default()
            }),
        )
        .await
}

const A_BASE: u128 = 1;
const B_BASE: u128 = 2;
const A_REPEAT: u128 = 3;
const A_SINGLE: u128 = 4;
const B_REPEAT: u128 = 5;
// Seeded with U+F963, the CJK compatibility form of U+5317. It stays an escape in
// the strings below so that Unicode normalization cannot turn it into U+5317.
const A_COMPAT: u128 = 6;
const A_TRAD: u128 = 7;
const A_SIMPL: u128 = 8;

async fn seed(runtime: &KhiveRuntime, namespace: &str, id: u128, content: &str) {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    let mut note = Note::new(namespace, KIND, content);
    note.id = Uuid::from_u128(id);
    note.salience = Some(0.5);
    note.decay_factor = Some(0.005);
    note.properties = Some(json!({"memory_type": "semantic"}));
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    runtime
        .text_for_notes(&token)
        .unwrap()
        .upsert_document(note_fts_document(&note))
        .await
        .unwrap();
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .is_some());
}

async fn baseline(runtime: &KhiveRuntime, registry: &VerbRegistry) {
    seed(runtime, "local", A_BASE, "北京城").await;
    seed(runtime, "other", B_BASE, "北京城").await;
    let response = query(registry, "北京城")
        .await
        .expect("three-scalar baseline must dispatch");
    let rows = response.as_array().expect("baseline response array");
    assert!(
        contains(rows, A_BASE),
        "local positive baseline missing: {response}"
    );
    assert!(
        !contains(rows, B_BASE),
        "cross-namespace baseline leak: {response}"
    );
    let identity_response = query_identity_other(registry, "北京城")
        .await
        .expect("identity-only baseline must dispatch");
    let identity_rows = identity_response
        .as_array()
        .expect("identity-only baseline response array");
    assert!(
        contains(identity_rows, A_BASE),
        "identity-only local baseline missing: {identity_response}"
    );
    assert!(
        !contains(identity_rows, B_BASE),
        "identity-only other result unexpected: {identity_response}"
    );
    let other_response = query_explicit_other(registry, "北京城")
        .await
        .expect("other-namespace baseline must dispatch");
    let other_rows = other_response
        .as_array()
        .expect("other baseline response array");
    assert!(
        contains(other_rows, B_BASE),
        "other-namespace positive baseline missing: {other_response}"
    );
    assert!(
        !contains(other_rows, A_BASE),
        "reverse cross-namespace baseline leak: {other_response}"
    );
}

fn contains(rows: &[Value], id: u128) -> bool {
    let id = Uuid::from_u128(id).to_string();
    rows.iter()
        .any(|row| row[ID_KEY].as_str() == Some(id.as_str()))
}

async fn qualify(query_text: &str, wanted: &[u128], forbidden: &[u128]) {
    let (runtime, registry) = fixture();
    baseline(&runtime, &registry).await;
    for (namespace, id, content) in [
        ("local", A_REPEAT, "人人乐"),
        ("local", A_SINGLE, "京剧"),
        ("other", B_REPEAT, "人人乐"),
        ("local", A_COMPAT, "\u{F963}京城"),
        ("local", A_TRAD, "臺北"),
        ("local", A_SIMPL, "台北"),
    ] {
        seed(&runtime, namespace, id, content).await;
    }
    let response = query(&registry, query_text)
        .await
        .unwrap_or_else(|err| panic!("query rejected for {query_text:?}: {err}"));
    let rows = response.as_array().expect("qualification response array");
    for id in [B_BASE, B_REPEAT] {
        assert!(
            !contains(rows, id),
            "cross-namespace leak for {query_text:?}: {response}"
        );
    }
    for id in wanted {
        assert!(contains(rows, *id), "expected result missing for {query_text:?}, id {id}; final hits do not identify the cause: {response}");
    }
    for id in forbidden {
        assert!(
            !contains(rows, *id),
            "unexpected result for {query_text:?}, id {id}: {response}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn three_scalar_positive_baseline_and_namespace_isolation() {
    let (runtime, registry) = fixture();
    baseline(&runtime, &registry).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn short_two_distinct() {
    qualify("北京", &[A_BASE], &[]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn short_single() {
    qualify("京", &[A_BASE, A_SINGLE], &[]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn short_repeated() {
    qualify("人人", &[A_REPEAT], &[]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: a CJK compatibility ideograph is not folded to its unified form, so the compatibility and unified spellings do not match"]
async fn compatibility_forward() {
    qualify("北京城", &[A_BASE, A_COMPAT], &[]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: a CJK compatibility ideograph is not folded to its unified form, so the compatibility and unified spellings do not match"]
async fn compatibility_reverse() {
    qualify("\u{F963}京城", &[A_BASE, A_COMPAT], &[]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn no_simplified_traditional_conversion() {
    qualify("台北", &[A_SIMPL], &[A_TRAD]).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn no_traditional_simplified_conversion() {
    qualify("臺北", &[A_TRAD], &[A_SIMPL]).await;
}
