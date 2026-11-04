use std::sync::Arc;

use khive_db::StorageBackend;
use khive_storage::types::TextSearchHit;
use khive_storage::{
    SubstrateKind, TextDocument, TextFilter, TextQueryMode, TextSearch, TextSearchRequest,
};
use uuid::Uuid;

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
const LIMIT: u32 = 64; // Greater than the full eight-document corpus.

struct Fixture {
    _dir: tempfile::TempDir,
    backend: StorageBackend,
    index: Arc<dyn TextSearch>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("private temporary fixture directory");
        let backend = StorageBackend::sqlite_for_test(dir.path().join("lexical.db"))
            .expect("temporary SQLite backend");
        let index = backend.text("notes").expect("note text index");
        Self {
            _dir: dir,
            backend,
            index,
        }
    }

    async fn seed(&self, namespace: &str, id: u128, body: &str) {
        self.index
            .upsert_document(TextDocument {
                subject_id: Uuid::from_u128(id),
                kind: SubstrateKind::Note,
                record_kind: Some("memory".into()),
                namespace: namespace.into(),
                title: None,
                body: body.into(),
                tags: vec![],
                metadata: None,
                updated_at: chrono::Utc::now(),
            })
            .await
            .expect("synthetic text document setup");
    }

    async fn search(&self, namespace: &str, query: &str) -> Vec<TextSearchHit> {
        let request = TextSearchRequest {
            query: query.into(),
            mode: TextQueryMode::Plain,
            filter: Some(TextFilter {
                namespaces: vec![namespace.into()],
                kinds: vec![SubstrateKind::Note],
                record_kinds: vec!["memory".into()],
                ..TextFilter::default()
            }),
            top_k: LIMIT,
            snippet_chars: 0,
        };
        let hits = self.index.search(request).await.unwrap_or_else(|error| {
            panic!("text search rejected {query:?} in {namespace}: {error}")
        });
        eprintln!("CJK_LEXICAL query={query:?} namespace={namespace:?} mode=Plain limit={LIMIT} count={} hits={hits:?}", hits.len());
        assert!(
            hits.len() < LIMIT as usize,
            "result window reached for {query:?}: {hits:?}"
        );
        hits
    }

    async fn baseline(&self) {
        self.seed("local", A_BASE, "北京城").await;
        self.seed("other", B_BASE, "北京城").await;
        let local = self.search("local", "北京城").await;
        let other = self.search("other", "北京城").await;
        assert!(
            contains(&local, A_BASE),
            "local baseline missing: {local:?}"
        );
        assert!(
            !contains(&local, B_BASE),
            "cross-namespace local leak: {local:?}"
        );
        assert!(
            contains(&other, B_BASE),
            "other baseline missing: {other:?}"
        );
        assert!(
            !contains(&other, A_BASE),
            "cross-namespace other leak: {other:?}"
        );
    }

    async fn shutdown(self) {
        let Self {
            _dir,
            backend,
            index,
        } = self;
        drop(index);
        let join = backend.pool().take_writer_task_join();
        drop(backend);
        if let Some(join) = join {
            join.await.expect("fixture writer shutdown");
        }
        drop(_dir);
    }
}

fn contains(hits: &[TextSearchHit], id: u128) -> bool {
    hits.iter().any(|hit| hit.subject_id == Uuid::from_u128(id))
}

async fn qualify(query: &str, wanted: &[u128], forbidden: &[u128]) {
    let fixture = Fixture::new();
    fixture.baseline().await;
    for (namespace, id, body) in [
        ("local", A_REPEAT, "人人乐"),
        ("local", A_SINGLE, "京剧"),
        ("other", B_REPEAT, "人人乐"),
        ("local", A_COMPAT, "\u{F963}京城"),
        ("local", A_TRAD, "臺北"),
        ("local", A_SIMPL, "台北"),
    ] {
        fixture.seed(namespace, id, body).await;
    }
    let hits = fixture.search("local", query).await;
    for id in [B_BASE, B_REPEAT] {
        assert!(
            !contains(&hits, id),
            "cross-namespace leak for {query:?}: {hits:?}"
        );
    }
    for id in wanted {
        assert!(
            contains(&hits, *id),
            "expected lexical result {id} missing for {query:?}: {hits:?}"
        );
    }
    for id in forbidden {
        assert!(
            !contains(&hits, *id),
            "forbidden lexical result {id} for {query:?}: {hits:?}"
        );
    }
    fixture.shutdown().await;
}

#[tokio::test]
async fn three_scalar_positive_baseline_and_reciprocal_namespace_isolation() {
    let fixture = Fixture::new();
    fixture.baseline().await;
    fixture.shutdown().await;
}

#[tokio::test]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn short_two_distinct() {
    qualify("北京", &[A_BASE], &[]).await;
}

#[tokio::test]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn short_single() {
    qualify("京", &[A_BASE, A_SINGLE], &[]).await;
}

#[tokio::test]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn short_repeated() {
    qualify("人人", &[A_REPEAT], &[]).await;
}

#[tokio::test]
#[ignore = "#1113: a CJK compatibility ideograph is not folded to its unified form, so the compatibility and unified spellings do not match"]
async fn compatibility_forward() {
    qualify("北京城", &[A_BASE, A_COMPAT], &[]).await;
}

#[tokio::test]
#[ignore = "#1113: a CJK compatibility ideograph is not folded to its unified form, so the compatibility and unified spellings do not match"]
async fn compatibility_reverse() {
    qualify("\u{F963}京城", &[A_BASE, A_COMPAT], &[]).await;
}

#[tokio::test]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn no_simplified_traditional_conversion() {
    qualify("台北", &[A_SIMPL], &[A_TRAD]).await;
}

#[tokio::test]
#[ignore = "#1113: the trigram text index has no term for a query shorter than three characters, so this CJK query returns no lexical result"]
async fn no_traditional_simplified_conversion() {
    qualify("臺北", &[A_TRAD], &[A_SIMPL]).await;
}
