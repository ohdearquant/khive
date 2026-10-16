use super::*;
use khive_runtime::Namespace;

const REVISION: &str = "1111111111111111111111111111111111111111";
const AMBIGUOUS_PATH: &str = "crates/ambig/src/lib.rs";

fn rt_and_token() -> (KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    (rt, token)
}

async fn create_module(rt: &KhiveRuntime, token: &NamespaceToken, name: &str, path: &str) -> Uuid {
    rt.create_entity_with_embedding_report(
        token,
        "concept",
        Some("module"),
        name,
        None,
        Some(json!({
            "source_path": path,
            "source_revision": REVISION
        })),
        vec![],
    )
    .await
    .map(|(row, _report)| row)
    .unwrap()
    .id
}

/// A live module row whose `id` does not parse as a UUID — a shape the
/// normal `create` path never writes, inserted raw to prove the loader
/// still counts it toward ambiguity.
async fn insert_unparsable_module_row(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    name: &str,
    path: &str,
) {
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO entities \
                      (id, namespace, kind, entity_type, name, properties, created_at, updated_at) \
                      VALUES (?1, ?2, 'concept', 'module', ?3, ?4, 0, 0)"
                .into(),
            params: vec![
                SqlValue::Text("not-a-parseable-uuid".to_string()),
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(name.to_string()),
                SqlValue::Text(
                    json!({
                        "source_path": path,
                        "source_revision": REVISION
                    })
                    .to_string(),
                ),
            ],
            label: Some("test_insert_unparsable_module_row".into()),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn single_valid_module_row_binds() {
    let (rt, token) = rt_and_token();
    let id = create_module(&rt, &token, "solo_module", AMBIGUOUS_PATH).await;

    let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
        .await
        .unwrap();
    assert_eq!(index.get(AMBIGUOUS_PATH), Some(&Some(id)));
}

/// Contract: more than one live module with the same
/// `(source_revision, source_path)` is ambiguous and annotates none. An
/// unparsable-id row is still a live row for that key, so it must mark
/// the pair ambiguous even though it can never bind itself.
#[tokio::test]
async fn unparsable_module_row_counts_toward_ambiguity() {
    let (rt, token) = rt_and_token();
    let valid_id = create_module(&rt, &token, "valid_module", AMBIGUOUS_PATH).await;
    insert_unparsable_module_row(&rt, &token, "malformed_module", AMBIGUOUS_PATH).await;

    let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
        .await
        .unwrap();
    assert_eq!(
        index.get(AMBIGUOUS_PATH),
        Some(&None),
        "a malformed row is evidence of a second live module for \
             {AMBIGUOUS_PATH}; the valid row {valid_id} must not be selected"
    );
}

#[tokio::test]
async fn two_parseable_module_rows_fold_to_ambiguity() {
    let (rt, token) = rt_and_token();
    let first_id = create_module(&rt, &token, "first_module", AMBIGUOUS_PATH).await;
    let second_id = create_module(&rt, &token, "second_module", AMBIGUOUS_PATH).await;

    let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
        .await
        .unwrap();
    assert_eq!(
        index.get(AMBIGUOUS_PATH),
        Some(&None),
        "two live parseable rows ({first_id}, {second_id}) must not select a winner"
    );
}

#[tokio::test]
async fn unparsable_module_row_alone_never_binds() {
    let (rt, token) = rt_and_token();
    insert_unparsable_module_row(&rt, &token, "lonely_malformed", AMBIGUOUS_PATH).await;

    let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
        .await
        .unwrap();
    assert_eq!(
        index.get(AMBIGUOUS_PATH),
        Some(&None),
        "an unparsable id can never serve as an annotation target"
    );
}

/// A failed index load must surface its cause: the caller includes this
/// error text in the degradation warning, so persistent SQL/schema
/// problems stay diagnosable.
#[tokio::test]
async fn load_failure_surfaces_error_text() {
    let (rt, token) = rt_and_token();
    // Break the substrate directly: the index query then fails with a
    // real SQL error that must reach the caller.
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "DROP TABLE entities".into(),
            params: vec![],
            label: Some("test_drop_entities".into()),
        })
        .await
        .unwrap();

    let err = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
        .await
        .expect_err("a failed index load must return Err");
    assert!(
        format!("{err}").contains("no such table"),
        "the error must carry the underlying SQL cause: {err}"
    );
}
